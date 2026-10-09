#pragma once
/* mac chain: route provider-side XAsync calls to the threading runtime that
 * owns the game's task queues (genuine runtime in the mcd2 chain). */
#include <windows.h>
#include <xasyncprovider.h>
#include "MacMode.h"
extern IXThreadingImpl *x_threading_impl;

/* mac chain: XThreading stays with the genuine runtime, so the game's task
 * queues are genuine objects. Drive Store async blocks through the genuine
 * IXThreadingImpl instead of this proxy's own XAsync/XTaskQueue copy. */
HRESULT QueryOriginalApi(const GUID *clsid, REFIID iid, void **out);
namespace {
IXThreadingImpl *genuine_threading() {
    static IXThreadingImpl *impl = [] {
        IXThreadingImpl *p = nullptr;
        if (FAILED(QueryOriginalApi(&__uuidof(IXThreadingImpl), __uuidof(IXThreadingImpl), reinterpret_cast<void **>(&p)))) p = nullptr;
        return p;
    }();
    return impl;
}
#define G_THREADING() (MacPureMode() ? x_threading_impl : genuine_threading())
HRESULT g_begin(XAsyncBlock *a, void *c, const void *i, const char *n, XAsyncProvider *p) { auto t=G_THREADING(); return t ? t->XAsyncBegin(a,c,i,n,p) : E_UNEXPECTED; }
HRESULT g_schedule(XAsyncBlock *a, UINT32 d) { auto t=G_THREADING(); return t ? t->XAsyncSchedule(a,d) : E_UNEXPECTED; }
void g_complete(XAsyncBlock *a, HRESULT hr, SIZE_T n) { if (auto t=G_THREADING()) t->XAsyncComplete(a,hr,n); }
HRESULT g_status(XAsyncBlock *a, BOOLEAN w) { auto t=G_THREADING(); return t ? t->XAsyncGetStatus(a,w) : E_UNEXPECTED; }
HRESULT g_result_size(XAsyncBlock *a, SIZE_T *n) { auto t=G_THREADING(); return t ? t->XAsyncGetResultSize(a,n) : E_UNEXPECTED; }
HRESULT g_result(XAsyncBlock *a, const void *i, SIZE_T n, void *b, SIZE_T *u) { auto t=G_THREADING(); return t ? t->XAsyncGetResult(a,i,n,b,u) : E_UNEXPECTED; }
}
#define XAsyncBegin g_begin
#define XAsyncSchedule g_schedule
#define XAsyncComplete g_complete
#define XAsyncGetStatus g_status
#define XAsyncGetResultSize g_result_size
#define XAsyncGetResult g_result
