/* SPDX-License-Identifier: MIT
 *
 * XCurl.dll for the macOS (Wine) build: the GDK's libcurl (curl-compat.dll,
 * next to this DLL) with three fixes the game needs under Wine. Everything
 * not overridden here is forwarded by xcurl.def.
 *
 * - Trust: every handle verifies peers and hosts against curl-ca-bundle.crt
 *   next to this DLL (the GDK libcurl has no CA store under Wine), and
 *   attempts to lower verification are refused.
 * - SSL_CTX: Unreal passes an OpenSSL 1.1 SSL_CTX callback; this libcurl uses
 *   LibreSSL, so the option is refused (CURLE_NOT_BUILT_IN) instead of handing
 *   it an incompatible context.
 * - Framing: when the caller's headers carry exactly one valid Content-Length
 *   and no Transfer-Encoding, curl is told the same body size, so a streamed
 *   POST is not sent chunked (PlayFab sign-in rejects that). The caller's size
 *   is restored after the transfer.
 *
 * DllMain also applies the Wine wait-queue fix (waitqueue-fix.inc).
 *
 * Behaviour follows mcd2-crossover's XCurl wrapper (https://github.com/Wanzho/
 * mcd2-crossover, MIT, Copyright (c) 2026 Wanzho); rewritten without its
 * diagnostics. */
typedef unsigned long DWORD;
typedef void *HANDLE;
typedef struct { void *ptr; } SRWLOCK;
__declspec(dllimport) HANDLE __stdcall LoadLibraryA(const char *);
__declspec(dllimport) void *__stdcall GetProcAddress(HANDLE, const char *);
__declspec(dllimport) DWORD __stdcall GetModuleFileNameA(HANDLE, char *, DWORD);
__declspec(dllimport) HANDLE __stdcall GetModuleHandleA(const char *);
__declspec(dllimport) int __stdcall VirtualProtect(void *, unsigned long long, DWORD, DWORD *);
__declspec(dllimport) HANDLE __stdcall GetCurrentProcess(void);
__declspec(dllimport) int __stdcall FlushInstructionCache(HANDLE, const void *, unsigned long long);
__declspec(dllimport) void __stdcall AcquireSRWLockExclusive(SRWLOCK *);
__declspec(dllimport) void __stdcall ReleaseSRWLockExclusive(SRWLOCK *);
#include "waitqueue-fix.inc"

void *memset(void *p, int v, unsigned long long n) { volatile unsigned char *c = p; while (n--) *c++ = (unsigned char)v; return p; }

/* libcurl option and info ids used here */
enum {
    OPT_URL = 10002, OPT_HTTPHEADER = 10023, OPT_CAINFO = 10065,
    OPT_SSL_VERIFYPEER = 64, OPT_SSL_VERIFYHOST = 81,
    OPT_SSL_CTX_FUNCTION = 20108, OPT_SSL_CTX_DATA = 10109,
    OPT_POSTFIELDSIZE = 60, OPT_POSTFIELDSIZE_LARGE = 30120,
    OPT_INFILESIZE = 14, OPT_INFILESIZE_LARGE = 30115, OPT_UPLOAD = 46,
    CURLE_NOT_BUILT_IN = 4, CURLMSG_DONE = 1,
};

static HANDLE self, curl;
static void *fn(const char *name) { if (!curl) curl = LoadLibraryA("curl-compat.dll"); return curl ? GetProcAddress(curl, name) : 0; }
typedef int (*setopt_fn)(void *, int, ...);

/* --- trust --------------------------------------------------------------- */
static char ca[2048];
static void trust(void *easy)
{
    setopt_fn set = (setopt_fn)fn("curl_easy_setopt");
    if (!ca[0]) {
        static const char name[] = "curl-ca-bundle.crt";
        DWORD n = GetModuleFileNameA(self, ca, sizeof(ca));
        if (!n || n >= sizeof(ca)) { ca[0] = 0; return; }
        while (n && ca[n - 1] != '\\' && ca[n - 1] != '/') n--;
        if (n + sizeof(name) > sizeof(ca)) { ca[0] = 0; return; }
        for (unsigned i = 0; i < sizeof(name); i++) ca[n + i] = name[i];
    }
    set(easy, OPT_CAINFO, ca);
    set(easy, OPT_SSL_VERIFYPEER, (void *)1);
    set(easy, OPT_SSL_VERIFYHOST, (void *)2);
}

/* --- per-handle request size --------------------------------------------- */
struct slist { const char *data; struct slist *next; };
struct handle {
    void *easy;
    struct slist *headers;
    long long post_size, upload_size;   /* the caller's, -1 when unset */
    int upload;
    int override;                       /* 0 none, 1 post size, 2 upload size */
};
static struct handle handles[128];
static SRWLOCK lock;

static struct handle *find(void *easy, int create)
{
    struct handle *free_slot = 0, *found = 0;
    if (!easy) return 0;
    AcquireSRWLockExclusive(&lock);
    for (int i = 0; i < 128 && !found; i++) {
        if (handles[i].easy == easy) found = &handles[i];
        else if (!handles[i].easy && !free_slot) free_slot = &handles[i];
    }
    if (!found && create && free_slot) {
        memset(free_slot, 0, sizeof(*free_slot));
        free_slot->easy = easy;
        free_slot->post_size = free_slot->upload_size = -1;
        found = free_slot;
    }
    ReleaseSRWLockExclusive(&lock);
    return found;
}

static int header_is(const char *p, unsigned long long n, const char *name)
{
    unsigned long long i = 0;
    for (; i < n && name[i]; i++) {
        char c = p[i];
        if (c >= 'A' && c <= 'Z') c += 32;
        if (c != name[i]) return 0;
    }
    return i == n && !name[i];
}

static void apply_size(void *easy)
{
    struct handle *h = find(easy, 0);
    unsigned long long length = 0;
    int seen = 0, invalid = 0, transfer = 0;
    if (!h) return;
    for (struct slist *l = h->headers; l; l = l->next) {
        const char *p = l->data;
        unsigned long long colon = 0;
        if (!p) continue;
        while (p[colon] && p[colon] != ':') colon++;
        if (p[colon] != ':') continue;
        if (header_is(p, colon, "transfer-encoding")) { transfer = 1; continue; }
        if (!header_is(p, colon, "content-length")) continue;
        seen++;
        p += colon + 1;
        while (*p == ' ' || *p == '\t') p++;
        unsigned long long value = 0;
        int digits = 0;
        while (*p >= '0' && *p <= '9') {
            unsigned digit = (unsigned)(*p++ - '0');
            if (value > (0x7fffffffffffffffULL - digit) / 10) { invalid = 1; break; }
            value = value * 10 + digit;
            digits++;
        }
        while (*p == ' ' || *p == '\t') p++;
        if (!digits || *p) invalid = 1;
        length = value;
    }
    if (seen != 1 || invalid || transfer) return;
    setopt_fn set = (setopt_fn)fn("curl_easy_setopt");
    if (!set(easy, h->upload ? OPT_INFILESIZE_LARGE : OPT_POSTFIELDSIZE_LARGE, (long long)length))
        h->override = h->upload ? 2 : 1;
}

static void restore_size(void *easy)
{
    struct handle *h = find(easy, 0);
    if (!h || !h->override) return;
    setopt_fn set = (setopt_fn)fn("curl_easy_setopt");
    if (h->override == 2 ? !set(easy, OPT_INFILESIZE_LARGE, h->upload_size)
                         : !set(easy, OPT_POSTFIELDSIZE_LARGE, h->post_size))
        h->override = 0;
}

/* --- exports ------------------------------------------------------------- */
__declspec(dllexport) void *curl_easy_init(void)
{
    void *(*call)(void) = (void *(*)(void))fn("curl_easy_init");
    void *easy = call ? call() : 0;
    if (easy) { trust(easy); find(easy, 1); }
    return easy;
}

__declspec(dllexport) int curl_easy_setopt(void *easy, int opt, void *value)
{
    setopt_fn call = (setopt_fn)fn("curl_easy_setopt");
    if ((opt == OPT_SSL_CTX_FUNCTION || opt == OPT_SSL_CTX_DATA) && value) return CURLE_NOT_BUILT_IN;
    if ((opt == OPT_SSL_VERIFYPEER && !value) || (opt == OPT_SSL_VERIFYHOST && (unsigned long long)value < 2)
        || (opt == OPT_CAINFO && !value))
        return CURLE_NOT_BUILT_IN;
    struct handle *h = find(easy, 1);
    if (h && opt == OPT_URL) restore_size(easy);   /* a new request on a reused handle */
    int r = call(easy, opt, value);
    if (r || !h) return r;
    switch (opt) {
    case OPT_HTTPHEADER: h->headers = value; break;
    case OPT_POSTFIELDSIZE: h->post_size = (long)(unsigned long long)value; h->override = 0; break;
    case OPT_POSTFIELDSIZE_LARGE: h->post_size = (long long)value; h->override = 0; break;
    case OPT_INFILESIZE: h->upload_size = (long)(unsigned long long)value; h->override = 0; break;
    case OPT_INFILESIZE_LARGE: h->upload_size = (long long)value; h->override = 0; break;
    case OPT_UPLOAD: h->upload = value != 0; break;
    }
    return r;
}

__declspec(dllexport) int curl_easy_perform(void *easy)
{
    int (*call)(void *) = (int (*)(void *))fn("curl_easy_perform");
    apply_size(easy);
    int r = call(easy);
    restore_size(easy);
    return r;
}

struct msg { int type; void *easy; union { void *p; int code; } data; };
__declspec(dllexport) struct msg *curl_multi_info_read(void *multi, int *remaining)
{
    struct msg *(*call)(void *, int *) = (struct msg *(*)(void *, int *))fn("curl_multi_info_read");
    struct msg *m = call(multi, remaining);
    if (m && m->type == CURLMSG_DONE) restore_size(m->easy);
    return m;
}

__declspec(dllexport) int curl_multi_add_handle(void *multi, void *easy)
{
    int (*call)(void *, void *) = (int (*)(void *, void *))fn("curl_multi_add_handle");
    apply_size(easy);
    int r = call(multi, easy);
    if (r) restore_size(easy);
    return r;
}

__declspec(dllexport) int curl_multi_remove_handle(void *multi, void *easy)
{
    int (*call)(void *, void *) = (int (*)(void *, void *))fn("curl_multi_remove_handle");
    int r = call(multi, easy);
    if (!r) restore_size(easy);
    return r;
}

__declspec(dllexport) void curl_easy_cleanup(void *easy)
{
    void (*call)(void *) = (void (*)(void *))fn("curl_easy_cleanup");
    restore_size(easy);
    call(easy);
    struct handle *h = find(easy, 0);
    if (h) { AcquireSRWLockExclusive(&lock); memset(h, 0, sizeof(*h)); ReleaseSRWLockExclusive(&lock); }
}

__declspec(dllexport) void curl_easy_reset(void *easy)
{
    void (*call)(void *) = (void (*)(void *))fn("curl_easy_reset");
    restore_size(easy);
    call(easy);
    struct handle *h = find(easy, 1);
    if (h) { h->headers = 0; h->post_size = h->upload_size = -1; h->upload = 0; h->override = 0; }
    trust(easy);
}

__declspec(dllexport) void *curl_easy_duphandle(void *easy)
{
    void *(*call)(void *) = (void *(*)(void *))fn("curl_easy_duphandle");
    void *copy = call(easy);
    struct handle *from = find(easy, 0), *to = copy ? find(copy, 1) : 0;
    if (from && to) {
        to->headers = from->headers;
        to->post_size = from->post_size;
        to->upload_size = from->upload_size;
        to->upload = from->upload;
        to->override = 0;
    }
    return copy;
}

int __stdcall DllMain(HANDLE module, DWORD reason, void *reserved)
{
    (void)reserved;
    if (reason == 1) { self = module; waitqueue_fix_apply(); }
    return 1;
}
