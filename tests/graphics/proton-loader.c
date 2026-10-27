/* SPDX-License-Identifier: MIT */
/* Own synthetic PE: no game, account, authentication or entitlement operation. */
#define COBJMACROS
#define INITGUID
#define WIDL_using_Windows_Data_Json
#include <windows.h>
#include <unknwn.h>
#include <roapi.h>
#include <winstring.h>
#include <windows.data.json.h>
#include <stdio.h>
#include <string.h>

typedef HRESULT (WINAPI *Query)(const GUID *, const GUID *, void **);
typedef HRESULT (WINAPI *Initialize)(ULONG, ULONG, char, const void *);
typedef HRESULT (WINAPI *Shutdown)(void);
static const GUID threading_iid = {0x073b7dcb,0x1fcf,0x4030,{0x94,0xbe,0xe3,0xc9,0xeb,0x62,0x34,0x28}};
static const GUID user_class = {0x01acd177,0x91f9,0x4763,{0xa3,0x8e,0xcc,0xbb,0x55,0xce,0x32,0xe0}};
static const GUID user_iid = {0x5131d685,0x4394,0x4ee6,{0x8c,0x18,0xbf,0xb5,0xd4,0xae,0xf1,0xff}};

/* The successful HTTP response must be readable by the game-side user bridge.
 * A stock Proton factory can load successfully while Parse is E_NOTIMPL. */
static int auth_json(void)
{
    IJsonValueStatics *statics = NULL;
    IJsonValue *value = NULL;
    IJsonObject *object = NULL, *claims = NULL, *identity = NULL;
    IJsonArray *users = NULL;
    HSTRING name = NULL, input = NULL, key = NULL, token = NULL;
    const WCHAR data[] = L"{\"Token\":\"synthetic\",\"NotAfter\":\"2099-01-01T00:00:00Z\",\"DisplayClaims\":{\"xui\":[{\"uhs\":\"test\",\"xid\":\"1\"}]}}";
    const char *stage = "RoInitialize";
    HRESULT hr = RoInitialize(RO_INIT_MULTITHREADED);
    if (FAILED(hr)) goto failed;
    stage = "JsonValue factory";
    hr = WindowsCreateString(L"Windows.Data.Json.JsonValue", 27, &name);
    if (FAILED(hr)) goto cleanup;
    hr = RoGetActivationFactory(name, &IID_IJsonValueStatics, (void **)&statics);
    if (FAILED(hr)) goto cleanup;
    stage = "Parse";
    hr = WindowsCreateString(data, wcslen(data), &input);
    if (FAILED(hr)) goto cleanup;
    hr = IJsonValueStatics_Parse(statics, input, &value);
    if (FAILED(hr)) goto cleanup;
    stage = "GetObject";
    hr = IJsonValue_GetObject(value, &object);
    if (FAILED(hr)) goto cleanup;
    stage = "GetNamedString";
    hr = WindowsCreateString(L"Token", 5, &key);
    if (FAILED(hr)) goto cleanup;
    hr = IJsonObject_GetNamedString(object, key, &token);
    if (FAILED(hr)) goto cleanup;
    if (wcscmp(WindowsGetStringRawBuffer(token, NULL), L"synthetic")) { hr = E_FAIL; goto cleanup; }
    WindowsDeleteString(key); key = NULL;
    stage = "GetNamedObject";
    hr = WindowsCreateString(L"DisplayClaims", 13, &key);
    if (FAILED(hr)) goto cleanup;
    hr = IJsonObject_GetNamedObject(object, key, &claims);
    if (FAILED(hr)) goto cleanup;
    WindowsDeleteString(key); key = NULL;
    stage = "GetNamedArray";
    hr = WindowsCreateString(L"xui", 3, &key);
    if (FAILED(hr)) goto cleanup;
    hr = IJsonObject_GetNamedArray(claims, key, &users);
    if (FAILED(hr)) goto cleanup;
    stage = "GetObjectAt";
    hr = IJsonArray_GetObjectAt(users, 0, &identity);
cleanup:
    if (identity) IJsonObject_Release(identity);
    if (users) IJsonArray_Release(users);
    if (claims) IJsonObject_Release(claims);
    if (object) IJsonObject_Release(object);
    if (value) IJsonValue_Release(value);
    if (statics) IJsonValueStatics_Release(statics);
    WindowsDeleteString(token); WindowsDeleteString(key); WindowsDeleteString(input); WindowsDeleteString(name);
    RoUninitialize();
failed:
    if (FAILED(hr)) printf("Xbox response JSON failed: %s hr=%08lx\n", stage, (unsigned long)hr);
    else puts("PASS: Xbox response JSON parsing, token strings and nested identity arrays");
    return FAILED(hr);
}

int main(int argc, char **argv)
{
    WCHAR path[32768], *name;
    HMODULE proxy, original, builtin, helper;
    IUnknown *threading = NULL, *user = NULL;
    Query query;
    Initialize initialize;
    Shutdown shutdown;
    int (*marker)(void);
    HRESULT hr;
    if (argc != 2 || strcmp(argv[1], "-FastLaunch")) {
        puts("MSFS FastLaunch argument did not reach the Windows process"); return 8;
    }
    /* UI engines and delayed imports also resolve from the working directory.
     * The previous probe covered only the executable's temporary directory. */
    if (!GetCurrentDirectoryW(32768, path) || wcslen(path) > 32000) return 6;
    wcscat(path, L"\\nested\\probe.dll");
    helper = LoadLibraryW(path);
    if (!helper || !(marker = (void *)GetProcAddress(helper, "probe")) || marker() != 42) {
        printf("Working-directory mapped DLL failed: %lu\n", GetLastError()); return 7;
    }
    FreeLibrary(helper);
    if (!GetModuleFileNameW(NULL, path, 32768) || !(name = wcsrchr(path, '\\'))) return 1;
    wcscpy(name + 1, L"nested\\probe.dll");
    helper = LoadLibraryW(path);
    if (!helper || !(marker = (void *)GetProcAddress(helper, "probe")) || marker() != 42) {
        printf("Mapped DLL failed: %lu\n", GetLastError()); return 2;
    }
    proxy = LoadLibraryW(L"xgameruntime.dll");
    original = LoadLibraryW(L"xgameruntime_original.dll");
    builtin = LoadLibraryW(L"xodus_store_test.dll");
    if (!proxy || !original || !builtin) { printf("Store DLL load failed: %lu\n", GetLastError()); return 3; }
    query = (void *)GetProcAddress(proxy, "QueryApiImpl");
    if (!query) return 4;
    hr = query(&threading_iid, &threading_iid, (void **)&threading);
    if (FAILED(hr) || !threading) { printf("Threading interface failed: %#lx\n", (unsigned long)hr); return 5; }
    IUnknown_Release(threading);
    initialize = (void *)GetProcAddress(proxy, "InitializeApiImplEx2");
    shutdown = (void *)GetProcAddress(proxy, "UninitializeApiImpl");
    if (!initialize || !shutdown) return 9;
    /* Find MicrosoftGame.config through the real temporary image view, as the
     * game does. Initialization sends only a ping to the local probe broker. */
    hr = initialize(250600, 0, 0, NULL);
    if (FAILED(hr)) { printf("Xbox runtime initialization failed: %#lx\n", (unsigned long)hr); return 10; }
    hr = query(&user_class, &user_iid, (void **)&user);
    if (FAILED(hr) || !user) { printf("Xbox user interface failed: %#lx\n", (unsigned long)hr); return 11; }
    IUnknown_Release(user);
    if (auth_json()) return 13;
    hr = shutdown();
    if (FAILED(hr)) { printf("Xbox runtime shutdown failed: %#lx\n", (unsigned long)hr); return 12; }
    puts("PASS: Xbox runtime initialization, user interface and shutdown with synthetic title and ping-only broker");
    puts("PASS: FastLaunch argument, memfd executable, working-directory and module-directory DLLs, Store libraries and threading interface");
    return 0;
}
