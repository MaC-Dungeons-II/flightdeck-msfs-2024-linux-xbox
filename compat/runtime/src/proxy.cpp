/* SPDX-License-Identifier: LGPL-2.1-or-later */
#include <initguid.h>
#include "compat.h"
#include <string>
#include <atomic>
#include <mutex>
#include <limits>
#include "NetworkingState.h"
#include "UserBridge.h"
#include "StoreBridge.h"
#include "GameSaveBridge.h"
#include "RuntimeDiagnostics.h"
#include "MacUser.h"
#include <cstdlib>
#include <cstring>

static INIT_ONCE base_once = INIT_ONCE_STATIC_INIT;
static HMODULE base_module;
static DWORD base_error;
namespace {
struct RuntimeLifetime {
    std::mutex lock;
    unsigned long long clients = 0;
    bool terminal = false;
};
RuntimeLifetime &lifetime() {
    static RuntimeLifetime *state = new RuntimeLifetime;
    return *state;
}
template<typename Initialize>
HRESULT initialize_client(ULONG gdk, ULONG services, char mode, const void *options,
                          unsigned argc, Initialize initialize) {
    auto &state = lifetime();
    std::lock_guard<std::mutex> lock(state.lock);
    HRESULT hr;
    if (state.terminal) hr = E_UNEXPECTED;
    else if (state.clients == std::numeric_limits<unsigned long long>::max()) hr = E_OUTOFMEMORY;
    else {
        hr = initialize();
        /* mac chain: the Store/User module starts lazily on first use; starting it
         * here shut the game down before its window opened. */
        if (SUCCEEDED(hr)) RememberUserRuntimeArgs(gdk, services, mode, options);
        if (SUCCEEDED(hr)) ++state.clients;
    }
    static std::atomic<unsigned> logs{0};
    if (logs.fetch_add(1) < 64)
        std::fprintf(stderr,"xodus-runtime-lifetime: initialize argc=%u clients=%llu result=%08lx\n",
            argc,state.clients,static_cast<ULONG>(hr));
    return hr;
}
}
static BOOL CALLBACK load_base(PINIT_ONCE, PVOID, PVOID*) {
    HMODULE self;
    if (!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                           reinterpret_cast<LPCWSTR>(&load_base), &self)) {
        base_error = GetLastError(); return TRUE;
    }
    wchar_t path[32768];
    DWORD count = GetModuleFileNameW(self, path, 32768);
    if (!count || count >= 32768) { base_error = ERROR_FILENAME_EXCED_RANGE; return TRUE; }
    std::wstring full(path, count);
    auto slash = full.find_last_of(L"\\/");
    if (slash == std::wstring::npos) { base_error = ERROR_BAD_PATHNAME; return TRUE; }
    full.resize(slash + 1);
    full += L"xgameruntime_original.dll";
    base_module = LoadLibraryExW(full.c_str(), nullptr, LOAD_WITH_ALTERED_SEARCH_PATH);
    if (!base_module) base_error = GetLastError();
    return TRUE;
}
static FARPROC base_proc(const char *name) {
    InitOnceExecuteOnce(&base_once, load_base, nullptr, nullptr);
    if (!base_module) { SetLastError(base_error ? base_error : ERROR_MOD_NOT_FOUND); return nullptr; }
    return GetProcAddress(base_module, name);
}
extern IXNetworkingImpl *x_networking_impl;
static bool TraceQueryApi();
static HRESULT QueryApiRoute(const GUID *clsid, REFIID iid, void **out);
extern "C" HRESULT WINAPI QueryApiImpl(const GUID *clsid, REFIID iid, void **out) {
    if (!clsid || !out) return E_POINTER;
    *out = nullptr;
    HRESULT hr = QueryApiRoute(clsid, iid, out);
    if (TraceQueryApi())
        std::fprintf(stderr, "xodus-queryapi: clsid=%08lx-%04x-%04x-%02x%02x-%02x%02x%02x%02x%02x%02x iid=%08lx result=%08lx\n",
            clsid->Data1, clsid->Data2, clsid->Data3, clsid->Data4[0], clsid->Data4[1], clsid->Data4[2], clsid->Data4[3],
            clsid->Data4[4], clsid->Data4[5], clsid->Data4[6], clsid->Data4[7], iid.Data1, static_cast<ULONG>(hr));
    return hr;
}
/* Diagnostics: XODUS_NETWORKING=genuine leaves XNetworking (and its feature
 * report) to the genuine runtime underneath, as before this runtime answered it. */
bool GenuineNetworking() {
    static const bool on = [] { const char *v = std::getenv("XODUS_NETWORKING"); return v && std::strcmp(v, "genuine") == 0; }();
    return on;
}
static HRESULT QueryApiRoute(const GUID *clsid, REFIID iid, void **out) {
    /* mac chain: XThreading and XGame stay with the genuine runtime underneath
     * (the game's task queues are its objects); Store, GameSave, XNetworking,
     * diagnostics and XUser (the launcher's session file with
     * XODUS_MAC_SESSION, else with XODUS_USER_RUNTIME=1 the Xodus user
     * runtime) are answered here.
     * Everything else is forwarded. */
    if (XodusGameSaveLocalEnabled() && IsGameSaveRuntimeClass(clsid))
        return QueryGameSaveRuntime(clsid,iid,out);
    if (IsStoreRuntimeClass(clsid)) return QueryStoreRuntime(clsid,iid,out);
    if (*clsid == __uuidof(IXNetworkingImpl) && !GenuineNetworking()) return x_networking_impl->QueryInterface(iid,out);
    if (IsRuntimeDiagnosticsClass(clsid)) return QueryRuntimeDiagnostics(clsid,iid,out);
    /* only XUser: XGame (title id and the like) stays with the genuine
     * runtime, which reads the title's config; the game quit on the user
     * runtime's answers before it got to its saves */
    static const GUID xuser = {0x01acd177,0x91f9,0x4763,{0xa3,0x8e,0xcc,0xbb,0x55,0xce,0x32,0xe0}};
    /* the launcher's session file (MacUser.cpp), when it provides one */
    if (MacUserEnabled() && *clsid == xuser) return QueryMacUser(iid,out);
    if (UserRuntimeEnabled() && *clsid == xuser) {
        /* the user runtime starts lazily (see initialize_client) */
        if (HRESULT ready = EnsureStoreModule(); FAILED(ready)) return ready;
        return QueryUserRuntime(clsid,iid,out);
    }
    return QueryOriginalApi(clsid,iid,out);
}
/* Per-call tracing; the game calls QueryApi constantly, so only with
 * XODUS_TRACE_QUERYAPI=1. */
static bool TraceQueryApi() {
    static const bool on = [] { const char *v = std::getenv("XODUS_TRACE_QUERYAPI"); return v && *v == '1'; }();
    return on;
}
HRESULT QueryOriginalApi(const GUID *clsid, REFIID iid, void **out) {
    if (TraceQueryApi()) std::fprintf(stderr, "xodus-taskqueue: forward QueryApi clsid=%08lx-%04x-%04x-%02x%02x-%02x%02x%02x%02x%02x%02x\n", clsid->Data1, clsid->Data2, clsid->Data3, clsid->Data4[0], clsid->Data4[1], clsid->Data4[2], clsid->Data4[3], clsid->Data4[4], clsid->Data4[5], clsid->Data4[6], clsid->Data4[7]);
    using Fn = HRESULT(WINAPI *)(const GUID *, REFIID, void **);
    /* the game calls QueryApi constantly; look the genuine entry point up once */
    static std::atomic<Fn> cached{nullptr};
    Fn fn = cached.load(std::memory_order_acquire);
    if (!fn && (fn = reinterpret_cast<Fn>(base_proc("QueryApiImpl")))) cached.store(fn, std::memory_order_release);
    HRESULT hr=fn ? fn(clsid, iid, out) : HRESULT_FROM_WIN32(GetLastError());
    if (TraceQueryApi()) std::fprintf(stderr,"xodus-taskqueue: forwarded QueryApi result=%08lx\n",static_cast<ULONG>(hr));
    return hr;
}
/* mac chain: the genuine runtime does not find the title's
 * MicrosoftGame.config under Wine, so callers that pass no options get one
 * pointing at the game root (this DLL sits in <root>\Dungeons\Binaries\WinGDK).
 * All three entry points go through InitializeApiImplEx2. Like
 * mcd2-crossover's adapter (MIT, Copyright (c) 2026 Wanzho). */
namespace {
struct TitleConfigOptions { unsigned version, flags; const char *file; };
const TitleConfigOptions *title_config_options() {
    static char path[2048];
    static TitleConfigOptions options{1, 0, path};
    static INIT_ONCE once = INIT_ONCE_STATIC_INIT;
    InitOnceExecuteOnce(&once, [](PINIT_ONCE, PVOID, PVOID *) -> BOOL {
        HMODULE self;
        if (!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                                reinterpret_cast<LPCWSTR>(&title_config_options), &self)) return TRUE;
        DWORD n = GetModuleFileNameA(self, path, sizeof(path));
        if (!n || n >= sizeof(path)) { path[0] = 0; return TRUE; }
        while (n && path[n - 1] != '\\' && path[n - 1] != '/') n--;
        static const char tail[] = "..\\..\\..\\MicrosoftGame.config";
        if (n + sizeof(tail) > sizeof(path)) { path[0] = 0; return TRUE; }
        std::memcpy(path + n, tail, sizeof(tail));
        return TRUE;
    }, nullptr, nullptr);
    return path[0] ? &options : nullptr;
}
}
extern "C" HRESULT WINAPI InitializeApiImplEx2(ULONG a, ULONG b, char c, const void *d);
extern "C" HRESULT WINAPI InitializeApiImpl(ULONG a, ULONG b) { return InitializeApiImplEx2(a,b,0,nullptr); }
extern "C" HRESULT WINAPI InitializeApiImplEx(ULONG a, ULONG b, char c) { return InitializeApiImplEx2(a,b,c,nullptr); }
extern "C" HRESULT WINAPI InitializeApiImplEx2(ULONG a, ULONG b, char c, const void *d) {
    if (!d) d = title_config_options();
    return initialize_client(a,b,c,d,4,[&] {
        using Fn=HRESULT(WINAPI *)(ULONG,ULONG,char,const void*); auto fn=reinterpret_cast<Fn>(base_proc("InitializeApiImplEx2"));
        return fn ? fn(a,b,c,d) : HRESULT_FROM_WIN32(GetLastError());
    });
}
extern "C" HRESULT WINAPI DllCanUnloadNow() {
    // Process-lifetime singleton interfaces and the networking timer pin code.
    return S_FALSE;
}
extern "C" HRESULT WINAPI UninitializeApiImpl() {
    auto &state = lifetime();
    {
        std::lock_guard<std::mutex> lock(state.lock);
        if (!state.clients) return S_OK;
        --state.clients;
        static std::atomic<unsigned> logs{0};
        if(logs.fetch_add(1)<64)
            std::fprintf(stderr,"xodus-runtime-lifetime: uninitialize clients=%llu shutdown=%u\n",state.clients,state.clients==0);
        if (state.clients) return S_OK;
        // This private runtime currently has terminal component teardown.
        // Do not claim successful in-process reinitialization of closed state.
        state.terminal = true;
    }
    // Never hold the lifetime mutex while waiting for user callbacks or IPC.
    ShutdownGameSaveRuntime();
    ShutdownStoreRuntime();
    HRESULT hr = ShutdownUserRuntime();
    NetworkRuntimeShutdown();
    // The original snapshot has no initialization resources to release;
    // its export is a Wine raising stub and must not be called.
    return FAILED(hr) ? hr : S_OK;
}
extern "C" void WINAPI XErrorReport(ULONG a, const char *b) {
    using Fn=void(WINAPI *)(ULONG,const char*); auto fn=reinterpret_cast<Fn>(base_proc("XErrorReport"));
    if (fn) fn(a,b);
}

