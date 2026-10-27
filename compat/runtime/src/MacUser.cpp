/* SPDX-License-Identifier: MIT
 *
 * mac chain: XUser for the macOS launcher (M(a)C Dungeons II). One account:
 * the one signed in by the launcher. The host tool (`macd2 mac-session
 * --serve`) keeps a session file with that account's identity and its Xbox
 * Live tokens for the title's services; this provider reads it. Tokens are
 * handed to the game as-is (no signature); ForceRefresh asks the host tool
 * for a renewed file. Nothing here signs in, and no refresh credential enters
 * Wine.
 *
 * Enabled when XODUS_MAC_SESSION holds the session folder (Windows path).
 *
 * The interface layout (50 XUser slots, the gamertag interface) and the
 * behaviour follow mcd2-crossover's user provider (https://github.com/Wanzho/
 * mcd2-crossover, MIT, Copyright (c) 2026 Wanzho), rewritten for this runtime.
 * Its Wine builtin user runtime (UserBridge, XODUS_USER_RUNTIME) answered the
 * game's calls, but the game quit before loading its saves with it. */
#include "compat.h"
#include <atomic>
#include <cstdio>
#include <cstring>
#include "MacUser.h"
#include "GenuineAsync.h"

namespace {
constexpr HRESULT kFail = static_cast<HRESULT>(0x80004005);
constexpr HRESULT kInvalid = static_cast<HRESULT>(0x80070057);
constexpr HRESULT kNotImpl = static_cast<HRESULT>(0x80004001);
constexpr HRESULT kNoUser = static_cast<HRESULT>(0x89245100);
constexpr HRESULT kSmall = static_cast<HRESULT>(0x8007007a);
constexpr unsigned long long kMagic = 0x3247444952425858ULL;      // session file
constexpr unsigned long long kRequestMagic = 0x3151455242434447ULL; // refresh request

/* Written by `macd2 mac-session` (xodus fork, mac_session.rs). Token index:
 * 0 http://xboxlive.com, 1 rp://api.minecraftservices.com/,
 * 2 http://playfab.xboxlive.com/ */
#pragma pack(push, 1)
struct Packet {
    unsigned long long magic, expires, xuid;
    unsigned age, privKnown;
    unsigned char priv[32];
    char tags[4][128];
    char tokens[3][16384];
    unsigned long long generation;
};
#pragma pack(pop)
static_assert(sizeof(Packet) == 49736, "session file layout");

Packet cred;
bool authenticated = false;
int handleMarker;                      // the one user handle: its address
SRWLOCK credLock = SRWLOCK_INIT;
ULONGLONG lastCheck = 0;
SRWLOCK requestLock = SRWLOCK_INIT;
unsigned long long requestCounter = 0;

void log(const char *what, long value) {
    static std::atomic<unsigned> lines{0};
    if (lines.fetch_add(1) < 256) std::fprintf(stderr, "macd2-user: %s %ld\n", what, value);
}

bool sessionPath(char *out, size_t cap, const char *leaf) {
    char dir[1024] = {};
    DWORD n = GetEnvironmentVariableA("XODUS_MAC_SESSION", dir, sizeof(dir));
    if (!n || n >= sizeof(dir)) return false;
    return std::snprintf(out, cap, "%s\\%s", dir, leaf) < static_cast<int>(cap);
}

bool bounded(const char *s, size_t cap) { return std::memchr(s, 0, cap) != nullptr; }

/* Reads and validates the session file; the account may not change. */
HRESULT readAuth() {
    char path[1100];
    if (!sessionPath(path, sizeof(path), "session.bin")) return kNoUser;
    HANDLE f = CreateFileA(path, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                           nullptr, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, nullptr);
    if (f == INVALID_HANDLE_VALUE) { authenticated = false; return kNoUser; }
    auto *p = static_cast<Packet *>(HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, sizeof(Packet)));
    if (!p) { CloseHandle(f); return kFail; }
    DWORD n = 0;
    BOOL ok = ReadFile(f, p, sizeof(*p), &n, nullptr);
    CloseHandle(f);
    FILETIME ft; GetSystemTimeAsFileTime(&ft);
    unsigned long long now = (static_cast<unsigned long long>(ft.dwHighDateTime) << 32) | ft.dwLowDateTime;
    HRESULT r = S_OK;
    if (!ok || n != sizeof(*p) || p->magic != kMagic || !p->xuid || !p->generation || p->expires <= now || p->age > 3)
        r = kFail;
    for (unsigned i = 0; r == S_OK && i < 4; i++) if (!bounded(p->tags[i], 128)) r = kFail;
    for (unsigned i = 0; r == S_OK && i < 3; i++) if (!p->tokens[i][0] || !bounded(p->tokens[i], 16384)) r = kFail;
    if (r == S_OK && cred.xuid && p->xuid != cred.xuid) r = kNoUser;
    if (r == S_OK) { std::memcpy(&cred, p, sizeof(cred)); authenticated = true; }
    else authenticated = false;
    SecureZeroMemory(p, sizeof(*p));
    HeapFree(GetProcessHeap(), 0, p);
    return r;
}

unsigned long long nowFiletime() {
    FILETIME ft; GetSystemTimeAsFileTime(&ft);
    return (static_cast<unsigned long long>(ft.dwHighDateTime) << 32) | ft.dwLowDateTime;
}

/* The session is current: re-read at most once a second, or once expired. */
bool live() {
    AcquireSRWLockExclusive(&credLock);
    ULONGLONG tick = GetTickCount64();
    unsigned long long now = nowFiletime();
    if (!lastCheck || tick - lastCheck >= 1000 || cred.expires <= now) { readAuth(); lastCheck = tick; }
    bool r = authenticated && cred.expires > now;
    ReleaseSRWLockExclusive(&credLock);
    return r;
}

bool valid(const void *u) { return u == &handleMarker && live(); }

HRESULT loadAuth() {
    AcquireSRWLockExclusive(&credLock);
    HRESULT r = readAuth();
    lastCheck = GetTickCount64();
    ReleaseSRWLockExclusive(&credLock);
    return r;
}

/* ForceRefresh: write refresh-<nonce>.req {magic, generation}; the host tool
 * renews the session and answers refresh-<nonce>.res {hresult, generation}. */
HRESULT forceRenew() {
    AcquireSRWLockExclusive(&requestLock);
    unsigned long long serial = ++requestCounter;
    ReleaseSRWLockExclusive(&requestLock);
    unsigned long long nonce = (GetTickCount64() << 24) ^ (static_cast<unsigned long long>(GetCurrentProcessId()) << 8) ^ serial;
    char leaf[64], request[1100], response[1100];
    std::snprintf(leaf, sizeof(leaf), "refresh-%016llx.req", nonce);
    if (!sessionPath(request, sizeof(request), leaf)) return kFail;
    std::snprintf(leaf, sizeof(leaf), "refresh-%016llx.res", nonce);
    if (!sessionPath(response, sizeof(response), leaf)) return kFail;
    unsigned long long data[2] = {kRequestMagic, 0};
    AcquireSRWLockExclusive(&credLock);
    data[1] = cred.generation;
    unsigned long long id = cred.xuid;
    ReleaseSRWLockExclusive(&credLock);
    if (!id) return kNoUser;
    HANDLE f = CreateFileA(request, GENERIC_WRITE, 0, nullptr, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, nullptr);
    if (f == INVALID_HANDLE_VALUE) return kFail;
    DWORD n = 0;
    BOOL ok = WriteFile(f, data, sizeof(data), &n, nullptr);
    CloseHandle(f);
    if (!ok || n != sizeof(data)) { DeleteFileA(request); return kFail; }
    ULONGLONG until = GetTickCount64() + 120000;
    HRESULT result = kFail;
    while (GetTickCount64() < until) {
        HANDLE reply = CreateFileA(response, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                                   nullptr, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, nullptr);
        if (reply != INVALID_HANDLE_VALUE) {
            unsigned long long answer[2] = {};
            n = 0;
            ok = ReadFile(reply, answer, sizeof(answer), &n, nullptr);
            CloseHandle(reply);
            if (ok && n == sizeof(answer)) {
                result = static_cast<HRESULT>(answer[0]);
                if (SUCCEEDED(result)) {
                    result = loadAuth();
                    AcquireSRWLockExclusive(&credLock);
                    // accept only a newer session of the same account
                    if (SUCCEEDED(result) && (cred.xuid != id || cred.generation == data[1] || cred.generation < answer[1]))
                        result = kNoUser;
                    ReleaseSRWLockExclusive(&credLock);
                }
                break;
            }
        }
        Sleep(50);
    }
    DeleteFileA(request);
    DeleteFileA(response);
    log("force refresh result", result);
    return result;
}

/* --- async operations (on the genuine runtime's async blocks) ------------- */
char addIdentity, tokenIdentity;
struct TokenData { unsigned long long tokenSize, signatureSize; void *token; void *signature; };
struct Op { int kind; /* 0 add user, 1 token, 2 token after refresh */ int wide, index; char token[16384]; };

HRESULT CALLBACK provider(XAsyncOp op, const XAsyncProviderData *d) {
    auto *c = static_cast<Op *>(d->context);
    switch (op) {
    case XAsyncOp::Begin:
        return XAsyncSchedule(d->async, 0);
    case XAsyncOp::DoWork: {
        HRESULT r = c->kind == 2 ? forceRenew() : S_OK;
        if (SUCCEEDED(r) && !live()) r = kNoUser;
        if (SUCCEEDED(r) && c->kind == 2) {
            AcquireSRWLockExclusive(&credLock);
            std::memcpy(c->token, cred.tokens[c->index], sizeof(c->token));
            ReleaseSRWLockExclusive(&credLock);
        }
        SIZE_T n = c->kind == 0 ? sizeof(void *)
                                : sizeof(TokenData) + (std::strlen(c->token) + 1) * (c->wide ? 2 : 1);
        XAsyncComplete(d->async, r, FAILED(r) ? 0 : n);
        return S_OK;
    }
    case XAsyncOp::GetResult: {
        if (c->kind == 0) { *static_cast<void **>(d->buffer) = &handleMarker; return S_OK; }
        auto *t = static_cast<TokenData *>(d->buffer);
        size_t n = std::strlen(c->token);
        t->tokenSize = n + 1;
        t->signatureSize = 0;
        t->token = t + 1;
        t->signature = nullptr;
        if (c->wide) {
            auto *p = static_cast<unsigned short *>(t->token);
            for (size_t i = 0; i <= n; i++) p[i] = static_cast<unsigned char>(c->token[i]);
        } else {
            std::memcpy(t->token, c->token, n + 1);
        }
        return S_OK;
    }
    case XAsyncOp::Cancel:
        return S_OK;   // the work is short and already scheduled
    case XAsyncOp::Cleanup:
        SecureZeroMemory(c, sizeof(*c));
        HeapFree(GetProcessHeap(), 0, c);
        return S_OK;
    }
    return kInvalid;
}

HRESULT start(XAsyncBlock *a, int kind, int index, int wide) {
    if (!a) return kInvalid;
    auto *c = static_cast<Op *>(HeapAlloc(GetProcessHeap(), HEAP_ZERO_MEMORY, sizeof(Op)));
    if (!c) return E_OUTOFMEMORY;
    c->kind = kind; c->wide = wide; c->index = index;
    if (kind == 1) {
        AcquireSRWLockExclusive(&credLock);
        std::memcpy(c->token, cred.tokens[index], sizeof(c->token));
        ReleaseSRWLockExclusive(&credLock);
    }
    HRESULT r = XAsyncBegin(a, c, kind ? static_cast<const void *>(&tokenIdentity) : &addIdentity,
                            kind ? "MacXboxToken" : "MacXboxUser", provider);
    if (FAILED(r)) { SecureZeroMemory(c, sizeof(*c)); HeapFree(GetProcessHeap(), 0, c); }
    return r;
}

/* --- XUser slots ---------------------------------------------------------- */
void *uservt[50];
void *tagvt[4];
struct Object { void **vt; } userObj{uservt}, tagObj{tagvt};

const GUID userIids[] = {
    {0x01acd177,0x91f9,0x4763,{0xa3,0x8e,0xcc,0xbb,0x55,0xce,0x32,0xe0}},
    {0xeb9bf948,0x18dc,0x4d82,{0xbb,0xcc,0x40,0xe0,0xa8,0x09,0xc4,0xc0}},
    {0x1bf2f8c5,0xd507,0x4e52,{0xbb,0x05,0xf7,0x26,0xd0,0xe7,0x11,0x61}},
    {0x079415e3,0x6727,0x437f,{0x8e,0x9d,0x8f,0x8f,0x9b,0x24,0x39,0xf7}},
    {0x26f3c674,0xa2fe,0x44fa,{0xb6,0xc4,0xa3,0x23,0xbc,0x94,0xff,0x53}},
    {0x5131d685,0x4394,0x4ee6,{0x8c,0x18,0xbf,0xb5,0xd4,0xae,0xf1,0xff}},
};
const GUID tagIid = {0xcef4fac0,0x7676,0x4a94,{0xa1,0x19,0x4c,0x43,0xf9,0xeb,0x5b,0x74}};

bool isUserIid(REFIID g) { for (const GUID &u : userIids) if (g == u) return true; return false; }

HRESULT WINAPI userQI(void *, REFIID g, void **o) {
    if (!o) return kInvalid;
    *o = nullptr;
    if (isUserIid(g) || g == IID_IUnknown) { *o = &userObj; return S_OK; }
    if (g == tagIid) { *o = &tagObj; return S_OK; }
    return E_NOINTERFACE;
}
ULONG WINAPI ref(void *) { return 1; }
HRESULT WINAPI unsupported(void *) { return kNotImpl; }
HRESULT WINAPI dupUser(void *, void *u, void **out) {
    if (!out) return kInvalid;
    *out = nullptr;
    if (!valid(u)) return kNoUser;
    *out = u;
    return S_OK;
}
void WINAPI closeUser(void *, void *) {}
int WINAPI compareUser(void *, void *a, void *b) { return a == b ? 0 : a < b ? -1 : 1; }
HRESULT WINAPI maxUsers(void *, unsigned *out) { if (!out) return kInvalid; *out = 1; return S_OK; }
HRESULT WINAPI addUser(void *, unsigned options, XAsyncBlock *a) {
    log("add user options", options);
    HRESULT r = loadAuth();
    if (FAILED(r)) { log("no valid session", r); return r; }
    return start(a, 0, 0, 0);
}
HRESULT WINAPI addResult(void *, XAsyncBlock *a, void **out) {
    if (!out) return kInvalid;
    *out = nullptr;
    HRESULT r = XAsyncGetResult(a, &addIdentity, sizeof(void *), out, nullptr);
    log("add user result", r);
    return r;
}
/* only the signed-in account itself */
HRESULT WINAPI addById(void *, unsigned long long id, XAsyncBlock *a) {
    if (!a) return kInvalid;
    HRESULT r = loadAuth();
    if (FAILED(r)) return r;
    return id == cred.xuid ? start(a, 0, 0, 0) : kNoUser;
}
HRESULT WINAPI localId(void *, void *u, unsigned long long *out) {
    if (!out) return kInvalid;
    if (!valid(u)) return kNoUser;
    *out = 1;
    return S_OK;
}
HRESULT WINAPI findLocal(void *s, unsigned long long id, void **o) { return id == 1 ? dupUser(s, &handleMarker, o) : kNoUser; }
HRESULT WINAPI getId(void *, void *u, unsigned long long *out) {
    if (!out) return kInvalid;
    if (!valid(u)) return kNoUser;
    *out = cred.xuid;
    return S_OK;
}
HRESULT WINAPI findId(void *s, unsigned long long id, void **o) { return live() && id == cred.xuid ? dupUser(s, &handleMarker, o) : kNoUser; }
HRESULT WINAPI guest(void *, void *u, unsigned char *out) {
    if (!out) return kInvalid;
    if (!valid(u)) return kNoUser;
    *out = 0;
    return S_OK;
}
HRESULT WINAPI state(void *, void *u, unsigned *out) {
    if (!out) return kInvalid;
    *out = valid(u) ? 0 /* signed in */ : 2 /* signed out */;
    return u == &handleMarker ? S_OK : kNoUser;
}
HRESULT WINAPI age(void *, void *u, unsigned *out) {
    if (!out) return kInvalid;
    *out = 0;
    if (!valid(u)) return kNoUser;
    *out = cred.age;
    return S_OK;
}
HRESULT WINAPI privilege(void *, void *u, unsigned opts, unsigned id, unsigned char *has, unsigned *reason) {
    if (!has) return kInvalid;
    *has = 0;
    if (reason) *reason = 0xffffffff;
    if (!valid(u)) return kNoUser;
    if (opts || id >= 256 || !cred.privKnown) return kNotImpl;
    *has = (cred.priv[id / 8] >> (id % 8)) & 1;
    if (reason) *reason = *has ? 0 : 0xffffffff;
    return S_OK;
}
HRESULT WINAPI tag(void *, void *u, unsigned component, SIZE_T size, char *b, SIZE_T *used) {
    if (!valid(u)) return kNoUser;
    if (component >= 4 || !b) return kInvalid;
    const char *t = cred.tags[component];
    if (component != 2 && !t[0]) return kNotImpl;
    size_t n = std::strlen(t) + 1;
    if (used) *used = n;
    if (size < n) return kSmall;
    std::memcpy(b, t, n);
    return S_OK;
}

/* Service tokens by host: the three relying parties of the session file. */
bool domain(const char *h, const char *d) {
    size_t n = std::strlen(h), m = std::strlen(d);
    return std::strcmp(h, d) == 0 || (n > m && h[n - m - 1] == '.' && std::strcmp(h + n - m, d) == 0);
}
int tokenIndex(const char *url) {
    static const char scheme[] = "https://";
    if (!url || std::strncmp(url, scheme, sizeof(scheme) - 1) != 0) return -1;
    url += sizeof(scheme) - 1;
    char h[256] = {};
    unsigned n = 0;
    while (*url && *url != '/' && *url != '?' && *url != '#') {
        char c = *url++;
        if (n >= 254 || c == ':' || c == '@' || c == '\\') return -1;
        h[n++] = (c >= 'A' && c <= 'Z') ? c + 32 : c;
    }
    if (std::strcmp(h, "api.minecraftservices.com") == 0) return 1;
    if (domain(h, "playfabapi.com")) return 2;
    if (domain(h, "xboxlive.com")) return 0;
    return -1;
}
HRESULT tokenRequest(void *u, unsigned opts, const char *url, XAsyncBlock *a, int wide) {
    if (opts & ~1U) return kNotImpl;   // only ForceRefresh
    if (u != &handleMarker || !url || (!(opts & 1) && !valid(u))) return kNoUser;
    int index = tokenIndex(url);
    if (index < 0) {
        /* the host only (no path or query) */
        char host[128] = {};
        const char *h = std::strstr(url, "://");
        h = h ? h + 3 : url;
        for (unsigned i = 0; i < sizeof(host) - 1 && h[i] && h[i] != '/' && h[i] != '?'; i++) host[i] = h[i];
        static std::atomic<unsigned> lines{0};
        if (lines.fetch_add(1) < 32) std::fprintf(stderr, "macd2-user: token refused for host %s\n", host);
        return kNotImpl;
    }
    return start(a, (opts & 1) ? 2 : 1, index, wide);
}
HRESULT WINAPI tokenAsync(void *, void *u, unsigned opts, const char *, const char *url,
                          SIZE_T, const void *, SIZE_T, const void *, XAsyncBlock *a) {
    return tokenRequest(u, opts, url, a, 0);
}
HRESULT WINAPI token16(void *, void *u, unsigned opts, const unsigned short *, const unsigned short *url,
                       SIZE_T, const void *, SIZE_T, const void *, XAsyncBlock *a) {
    char v[2048] = {};
    if (url) {
        unsigned i = 0;
        for (; i < 2047 && url[i]; i++) { if (url[i] > 127) return kNotImpl; v[i] = static_cast<char>(url[i]); }
        if (url[i]) return kInvalid;
    }
    return tokenRequest(u, opts, url ? v : nullptr, a, 1);
}
HRESULT WINAPI tokenSize(void *, XAsyncBlock *a, SIZE_T *n) { return XAsyncGetResultSize(a, n); }
HRESULT WINAPI tokenResult(void *, XAsyncBlock *a, SIZE_T size, void *b, void **ptr, SIZE_T *used) {
    if (!ptr) return kInvalid;
    *ptr = nullptr;
    HRESULT r = XAsyncGetResult(a, &tokenIdentity, size, b, used);
    if (SUCCEEDED(r)) *ptr = b;
    return r;
}
HRESULT WINAPI changeRegister(void *, void *, void *, void *, unsigned long long *token) {
    if (!token) return kInvalid;
    *token = 1;
    return S_OK;
}
BOOLEAN WINAPI changeUnregister(void *, unsigned long long token, BOOLEAN) { return token == 1; }
/* the signed-in account is the one whose Store license the broker verifies */
BOOLEAN WINAPI storeUser(void *, void *u) { return valid(u) ? 1 : 0; }
BOOLEAN WINAPI noSignoutUI(void *) { return 0; }

template<typename F> void *slot(F f) { return reinterpret_cast<void *>(f); }

void initTables() {
    static INIT_ONCE once = INIT_ONCE_STATIC_INIT;
    InitOnceExecuteOnce(&once, [](PINIT_ONCE, PVOID, PVOID *) -> BOOL {
        for (void *&s : uservt) s = slot(unsupported);
        uservt[0] = slot(userQI); uservt[1] = slot(ref); uservt[2] = slot(ref);
        uservt[3] = slot(dupUser); uservt[4] = slot(closeUser); uservt[5] = slot(compareUser);
        uservt[6] = slot(maxUsers); uservt[7] = slot(addUser); uservt[8] = slot(addResult);
        uservt[9] = slot(localId); uservt[10] = slot(findLocal); uservt[11] = slot(getId);
        uservt[12] = slot(findId); uservt[13] = slot(guest); uservt[14] = slot(state);
        uservt[19] = slot(age); uservt[20] = slot(privilege);
        uservt[23] = slot(tokenAsync); uservt[24] = slot(tokenSize); uservt[25] = slot(tokenResult);
        uservt[26] = slot(token16); uservt[27] = slot(tokenSize); uservt[28] = slot(tokenResult);
        uservt[33] = slot(changeRegister); uservt[34] = slot(changeUnregister);
        uservt[37] = slot(addById); uservt[38] = slot(addResult);
        uservt[42] = slot(storeUser); uservt[47] = slot(noSignoutUI);
        tagvt[0] = slot(userQI); tagvt[1] = slot(ref); tagvt[2] = slot(ref); tagvt[3] = slot(tag);
        return TRUE;
    }, nullptr, nullptr);
}
}

bool MacUserEnabled() {
    static const bool on = [] {
        char dir[8];
        return GetEnvironmentVariableA("XODUS_MAC_SESSION", dir, sizeof(dir)) > 0;
    }();
    return on;
}

bool MacUserHandle(const void *user) { return MacUserEnabled() && user == &handleMarker; }

HRESULT QueryMacUser(REFIID iid, void **out) {
    if (!out) return E_POINTER;
    initTables();
    return userQI(nullptr, iid, out);
}
