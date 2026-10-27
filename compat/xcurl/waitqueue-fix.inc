/* In-memory fix for the wait-queue use-after-return in Wine's RtlWaitOnAddress
 * (WineHQ bug 60397, item 1), for loading inside the game process.
 *
 * After its wait, Wine's waiter tests entry.addr WITHOUT the queue lock and, if a
 * waker already cleared it, returns at once. The waker is still unlinking that
 * stack-resident entry, so a quick re-wait on another address corrupts two
 * buckets. Turning the 2-byte "je skip_lock" into a 2-byte nop makes the waiter
 * always take the queue lock, which the waker holds until it is done with the
 * entry; the existing locked re-check then skips the unlink. No other behaviour
 * changes (stale alerts and spurious wakeups, items 2-3 of the bug, remain).
 *
 * The patch is applied only when the exact instruction sequence is found once:
 *   call NtWaitForAlertByThreadId ; cmpq $0,0x50(%rsp) ; movl %eax,%esi ;
 *   je L ; xorl %ecx,%ecx ; movl $-1,%edx ; ... L-4: xorl %eax,%eax ; xchgl %eax,(%rbx)
 * Any other build, or real Windows, is left untouched.
 *
 * Returns 1 patched, 2 already patched, 0 not applicable, -1 protection failure.
 * The includer must declare GetModuleHandleA, GetProcAddress, VirtualProtect,
 * GetCurrentProcess and FlushInstructionCache. */
static int waitqueue_fix_apply(void)
{
	static const unsigned char after_call[] = {0x48,0x83,0x7c,0x24,0x50,0x00,0x89,0xc6};
	static const unsigned char after_je[] = {0x31,0xc9,0xba,0xff,0xff,0xff,0xff};
	void *ntdll = GetModuleHandleA("ntdll.dll");
	unsigned char *f, *site = 0;
	unsigned char *alert;
	unsigned long old;
	int matches = 0, already = 0;

	if (!ntdll || !GetProcAddress(ntdll, "wine_get_version")) return 0;
	f = (unsigned char *)GetProcAddress(ntdll, "RtlWaitOnAddress");
	alert = (unsigned char *)GetProcAddress(ntdll, "NtWaitForAlertByThreadId");
	if (!f || !alert) return 0;
	for (int i = 0; i < 0x300; i++)
	{
		unsigned char *p = f + i;
		int k, ok = 1;
		if (p[0] != 0xe8 || p + 5 + *(int *)(p + 1) != alert) continue;
		for (k = 0; k < (int)sizeof(after_call); k++) if (p[5 + k] != after_call[k]) ok = 0;
		for (k = 0; k < (int)sizeof(after_je); k++) if (p[15 + k] != after_je[k]) ok = 0;
		if (!ok) continue;
		if (p[13] == 0x66 && p[14] == 0x90) { already++; continue; }
		if (p[13] != 0x74) continue;
		{
			unsigned char *target = p + 15 + (signed char)p[14];
			if (target[-4] != 0x31 || target[-3] != 0xc0 || target[-2] != 0x87 || target[-1] != 0x03) continue;
		}
		site = p + 13;
		matches++;
	}
	if (already && !matches) return 2;
	if (matches != 1) return 0;
	/* site is 2-byte aligned in the examined builds; the store is a single 16-bit write. */
	if (!VirtualProtect(site, 2, 0x40 /* PAGE_EXECUTE_READWRITE */, &old)) return -1;
	*(volatile unsigned short *)site = 0x9066; /* bytes 66 90: two-byte nop */
	VirtualProtect(site, 2, old, &old);
	FlushInstructionCache(GetCurrentProcess(), site, 2);
	return 1;
}
