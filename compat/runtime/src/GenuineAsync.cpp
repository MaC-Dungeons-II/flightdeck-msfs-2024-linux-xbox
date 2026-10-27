/* SPDX-License-Identifier: LGPL-2.1-or-later */
/* mac chain: forwarders to the genuine runtime's IXThreadingImpl, see GenuineAsync.h. */
#define __WINESRC__ 1
#include "compat.h"
#include <windows.h>
#include <xasyncprovider.h>
#include <atomic>

HRESULT QueryOriginalApi(const GUID *clsid, REFIID iid, void **out);

namespace {
/* Looked up on first use; a failed lookup (the genuine runtime not ready yet)
 * is retried on the next call instead of being cached. */
IXThreadingImpl *genuine() {
    static std::atomic<IXThreadingImpl *> cached{nullptr};
    if (IXThreadingImpl *impl = cached.load(std::memory_order_acquire)) return impl;
    IXThreadingImpl *impl = nullptr;
    if (FAILED(QueryOriginalApi(&__uuidof(IXThreadingImpl), __uuidof(IXThreadingImpl), reinterpret_cast<void **>(&impl))) || !impl)
        return nullptr;
    IXThreadingImpl *expected = nullptr;
    if (!cached.compare_exchange_strong(expected, impl, std::memory_order_acq_rel)) impl->Release(), impl = expected;
    return impl;
}
}

IXThreadingImpl *GenuineThreading() { return genuine(); }

HRESULT genuine_XAsyncBegin(XAsyncBlock *a, void *c, const void *i, const char *n, XAsyncProvider *p) { auto t = genuine(); return t ? t->XAsyncBegin(a, c, i, n, p) : E_UNEXPECTED; }
HRESULT genuine_XAsyncSchedule(XAsyncBlock *a, UINT32 d) { auto t = genuine(); return t ? t->XAsyncSchedule(a, d) : E_UNEXPECTED; }
void genuine_XAsyncComplete(XAsyncBlock *a, HRESULT hr, SIZE_T n) { if (auto t = genuine()) t->XAsyncComplete(a, hr, n); }
HRESULT genuine_XAsyncGetStatus(XAsyncBlock *a, BOOLEAN w) { auto t = genuine(); return t ? t->XAsyncGetStatus(a, w) : E_UNEXPECTED; }
HRESULT genuine_XAsyncGetResultSize(XAsyncBlock *a, SIZE_T *n) { auto t = genuine(); return t ? t->XAsyncGetResultSize(a, n) : E_UNEXPECTED; }
HRESULT genuine_XAsyncGetResult(XAsyncBlock *a, const void *i, SIZE_T n, void *b, SIZE_T *u) { auto t = genuine(); return t ? t->XAsyncGetResult(a, i, n, b, u) : E_UNEXPECTED; }
HRESULT genuine_XTaskQueueDuplicateHandle(XTaskQueueHandle q, XTaskQueueHandle *out) { auto t = genuine(); return t ? t->XTaskQueueDuplicateHandle(q, out) : E_UNEXPECTED; }
void genuine_XTaskQueueCloseHandle(XTaskQueueHandle q) { if (auto t = genuine()) t->XTaskQueueCloseHandle(q); }
HRESULT genuine_XTaskQueueSubmitCallback(XTaskQueueHandle q, XTaskQueuePort port, void *ctx, XTaskQueueCallback *cb) { auto t = genuine(); return t ? t->XTaskQueueSubmitCallback(q, port, ctx, cb) : E_UNEXPECTED; }
bool genuine_XTaskQueueGetCurrentProcessTaskQueue(XTaskQueueHandle *out) { auto t = genuine(); return t && t->XTaskQueueGetCurrentProcessTaskQueue(out); }
