#pragma once
#include <windows.h>
/* XODUS_PURE=1: Flightdeck-only chain (no outer mcd2 adapter). The proxy owns
 * XThreading, XNetworking and XUser exactly as upstream Flightdeck does. */
inline bool MacPureMode() {
    static int v = -1;
    if (v < 0) { char b[4]; v = GetEnvironmentVariableA("XODUS_PURE", b, sizeof(b)) == 1 && b[0] == '1'; }
    return v == 1;
}
