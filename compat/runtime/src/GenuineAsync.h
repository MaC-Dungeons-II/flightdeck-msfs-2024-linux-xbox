#pragma once
/* mac chain: the game's task queues and async blocks belong to the genuine
 * runtime (XThreading is forwarded to it). Async work and queue calls made by
 * this proxy on the game's behalf (Store, GameSave, NetworkSecurity, license
 * events) must go through the genuine IXThreadingImpl, not this proxy's own
 * XAsync/XTaskQueue copy, whose objects have a different layout.
 * Include this after all other headers. */
#include <windows.h>
#include <xasyncprovider.h>

HRESULT genuine_XAsyncBegin(XAsyncBlock *a, void *c, const void *i, const char *n, XAsyncProvider *p);
HRESULT genuine_XAsyncSchedule(XAsyncBlock *a, UINT32 d);
void genuine_XAsyncComplete(XAsyncBlock *a, HRESULT hr, SIZE_T n);
HRESULT genuine_XAsyncGetStatus(XAsyncBlock *a, BOOLEAN w);
HRESULT genuine_XAsyncGetResultSize(XAsyncBlock *a, SIZE_T *n);
HRESULT genuine_XAsyncGetResult(XAsyncBlock *a, const void *i, SIZE_T n, void *b, SIZE_T *u);
HRESULT genuine_XTaskQueueDuplicateHandle(XTaskQueueHandle q, XTaskQueueHandle *out);
void genuine_XTaskQueueCloseHandle(XTaskQueueHandle q);
HRESULT genuine_XTaskQueueSubmitCallback(XTaskQueueHandle q, XTaskQueuePort port, void *ctx, XTaskQueueCallback *cb);
bool genuine_XTaskQueueGetCurrentProcessTaskQueue(XTaskQueueHandle *out);

#define XAsyncBegin genuine_XAsyncBegin
#define XAsyncSchedule genuine_XAsyncSchedule
#define XAsyncComplete genuine_XAsyncComplete
#define XAsyncGetStatus genuine_XAsyncGetStatus
#define XAsyncGetResultSize genuine_XAsyncGetResultSize
#define XAsyncGetResult genuine_XAsyncGetResult
#define XTaskQueueDuplicateHandle genuine_XTaskQueueDuplicateHandle
#define XTaskQueueCloseHandle genuine_XTaskQueueCloseHandle
#define XTaskQueueSubmitCallback genuine_XTaskQueueSubmitCallback
#define XTaskQueueGetCurrentProcessTaskQueue genuine_XTaskQueueGetCurrentProcessTaskQueue
