/* KUBridge of ARK-4's systemctrl (NIDs from ARK-4 libs/KUBridge/KUBridge.S): a word read or
 * written in kernel mode, for the CPU profiler's registers (main.c, psxrt.prof). Elsewhere the
 * import stays unlinked and its calls return an error, and they are made only on request. */
	.set noreorder

#include "pspimport.s"

	IMPORT_START "KUBridge",0x40090000
	IMPORT_FUNC  "KUBridge",0x7A50075E,kuKernelPeekw
	IMPORT_FUNC  "KUBridge",0x0E73A39D,kuKernelPokew
