/* SPDX-License-Identifier: MIT */
#pragma once
#include "compat.h"

/* mac chain: XUser from the launcher's session file (MacUser.cpp). */
bool MacUserEnabled();
bool MacUserHandle(const void *user);
HRESULT QueryMacUser(REFIID iid, void **out);
