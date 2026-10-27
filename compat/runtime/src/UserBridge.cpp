/* SPDX-License-Identifier: LGPL-2.1-or-later */
// Narrow bridge to the separately built, genuine Xodus-backed user runtime.
// LGPL-2.1-or-later, like the surrounding runtime sources.
#include "UserBridge.h"
#include <mutex>
#include <atomic>
#include <string>
#include <cstdio>

namespace {
using BindFn = HRESULT (WINAPI *)(IXThreadingImpl *);
using InitFn = HRESULT (WINAPI *)(ULONG, ULONG, char, const void *);
using QueryFn = HRESULT (WINAPI *)(const GUID *, REFIID, void **);
using ShutdownFn = HRESULT (WINAPI *)();
struct UserState {
    std::mutex lock;
    HMODULE module = nullptr;
    BindFn bind = nullptr;
    InitFn initialize = nullptr;
    QueryFn query = nullptr;
    ShutdownFn shutdown = nullptr;
    bool active = false;
    bool stopping = false;
    bool needs_cleanup = false;
    HRESULT last_error = CO_E_NOTINITIALIZED;
    // Results of interface queries that were already logged.
    HRESULT logged_results[16] = {};
    unsigned logged_count = 0;
};
// The game queries its user interfaces constantly. Each distinct result is
// logged once: a line per call filled most of a session's log.
bool first_result(UserState &user, HRESULT hr) {
    for (unsigned i = 0; i < user.logged_count; ++i)
        if (user.logged_results[i] == hr) return false;
    if (user.logged_count == sizeof(user.logged_results) / sizeof(user.logged_results[0])) return false;
    user.logged_results[user.logged_count++] = hr;
    return true;
}
UserState &state() {
    // The library and its interface vtables stay loaded for process lifetime.
    // Explicit uninitialization shuts down resources, not the DLL's code pages.
    static UserState *value = new UserState;
    return *value;
}
HRESULT load(UserState &user) {
    if (user.module) return S_OK;
    HMODULE module = LoadLibraryW(L"xodus_store_test.dll");
    if (!module) return HRESULT_FROM_WIN32(GetLastError());
    auto bind = reinterpret_cast<BindFn>(GetProcAddress(module, "XodusUserSetThreading"));
    auto initialize = reinterpret_cast<InitFn>(GetProcAddress(module, "InitializeApiImplEx2"));
    auto query = reinterpret_cast<QueryFn>(GetProcAddress(module, "QueryApiImpl"));
    auto shutdown = reinterpret_cast<ShutdownFn>(GetProcAddress(module, "UninitializeApiImpl"));
    if (!bind || !initialize || !query || !shutdown) {
        FreeLibrary(module);
        return HRESULT_FROM_WIN32(ERROR_PROC_NOT_FOUND);
    }
    user.module = module;
    user.bind = bind;
    user.initialize = initialize;
    user.query = query;
    user.shutdown = shutdown;
    return S_OK;
}
}

bool UserRuntimeEnabled() {
    static const bool enabled = [] {
        char value[4] = {};
        return GetEnvironmentVariableA("XODUS_USER_RUNTIME", value, sizeof(value)) == 1 && value[0] == '1';
    }();
    return enabled;
}

bool IsUserRuntimeClass(const GUID *clsid) {
    // CLSIDs from the pinned PR60 xuser.idl and xgame.idl, respectively.
    static const GUID user = {0x01acd177,0x91f9,0x4763,{0xa3,0x8e,0xcc,0xbb,0x55,0xce,0x32,0xe0}};
    static const GUID game = {0x973a344e,0x24bf,0x4d0f,{0x84,0x57,0x56,0xc5,0x34,0x89,0x2b,0x29}};
    return clsid && (*clsid == user || *clsid == game);
}

HRESULT InitializeUserRuntime(ULONG gdk, ULONG services, char mode, const void *options) {
    if (!UserRuntimeEnabled()) return S_OK;
    auto &user = state();
    std::lock_guard<std::mutex> guard(user.lock);
    if (user.stopping) return E_ILLEGAL_METHOD_CALL;
    if (user.active) return S_OK;
    HRESULT hr = load(user);
    if (SUCCEEDED(hr)) {
        hr = user.bind(x_threading_impl);
        if (SUCCEEDED(hr)) user.needs_cleanup = true;
    }
    if (SUCCEEDED(hr)) hr = user.initialize(gdk, services, mode, options);
    user.last_error = hr;
    user.active = SUCCEEDED(hr);
    std::fprintf(stderr, "xodus-user-bridge: initialize result=%08lx\n", static_cast<ULONG>(hr));
    return hr;
}

HRESULT QueryUserRuntime(const GUID *clsid, REFIID iid, void **out) {
    if (!out || !clsid) return E_POINTER;
    *out = nullptr;
    auto &user = state();
    std::lock_guard<std::mutex> guard(user.lock);
    if (!user.active) return user.last_error;
    HRESULT hr = user.query(clsid, iid, out);
    if (first_result(user, hr))
        std::fprintf(stderr, "xodus-user-bridge: query result=%08lx\n", static_cast<ULONG>(hr));
    return hr;
}

static std::atomic<bool> runtime_closed{false}; /* mac chain: no lazy start after shutdown */
HRESULT ShutdownUserRuntime() {
    runtime_closed.store(true);
    if (!UserRuntimeEnabled()) return S_OK;
    auto &user = state();
    ShutdownFn shutdown = nullptr;
    {
        std::lock_guard<std::mutex> guard(user.lock);
        if (user.stopping) return S_FALSE;
        if (!user.module || !user.needs_cleanup) return S_OK;
        user.stopping = true;
        user.needs_cleanup = false;
        user.active = false;
        user.last_error = CO_E_NOTINITIALIZED;
        shutdown = user.shutdown;
    }
    // Do not hold the state mutex while draining callbacks/IPC threads.
    HRESULT hr = shutdown();
    {
        std::lock_guard<std::mutex> guard(user.lock);
        user.stopping = false;
    }
    std::fprintf(stderr, "xodus-user-bridge: shutdown result=%08lx\n", static_cast<ULONG>(hr));
    return hr;
}

/* mac chain: defer the xodus module until the first Store request. The init
 * options carry a pointer to the game's MicrosoftGame.config path; copy it. */
namespace {
struct SavedArgs { bool set=false; ULONG gdk=0, services=0; char mode=0; bool has_options=false;
    struct { UINT32 unknown; BOOLEAN inline_config; const char *file; } options{}; /* INITIALIZE_OPTIONS */ std::string file; };
SavedArgs &saved() { static SavedArgs *s = new SavedArgs; return *s; }
std::mutex saved_lock;
}
void RememberUserRuntimeArgs(ULONG gdk, ULONG services, char mode, const void *options) {
    std::lock_guard<std::mutex> guard(saved_lock);
    auto &s = saved();
    if (s.set) return;
    s.set = true; s.gdk = gdk; s.services = services; s.mode = mode;
    if (options) {
        auto o = static_cast<const decltype(s.options) *>(options);
        s.has_options = true; s.options.unknown = o->unknown; s.options.inline_config = o->inline_config;
        if (o->file) { s.file = o->file; s.options.file = s.file.c_str(); }
    }
}
/* A failed start (e.g. the broker is not up) is cached for a few seconds so
 * every Store/GameSave call on game threads doesn't wait out the IPC timeout
 * again; after that it is retried. */
HRESULT EnsureStoreModule() {
    static std::atomic<bool> ready{false};
    static std::atomic<HRESULT> last_failure{S_OK};
    static std::atomic<ULONGLONG> retry_at{0};
    static std::mutex start_lock;
    if (ready.load()) return S_OK;
    if (runtime_closed.load()) return E_ABORT;
    std::lock_guard<std::mutex> start(start_lock);
    if (ready.load()) return S_OK;
    if (GetTickCount64() < retry_at.load()) return last_failure.load();
    { std::lock_guard<std::mutex> guard(saved_lock); if (!saved().set) return CO_E_NOTINITIALIZED; }
    auto &s = saved();
    HRESULT hr = InitializeUserRuntime(s.gdk, s.services, s.mode, s.has_options ? &s.options : nullptr);
    std::fprintf(stderr, "xodus-user-bridge: lazy store init result=%08lx\n", static_cast<ULONG>(hr));
    if (SUCCEEDED(hr)) ready.store(true);
    else { last_failure.store(hr); retry_at.store(GetTickCount64() + 5000); }
    return hr;
}
