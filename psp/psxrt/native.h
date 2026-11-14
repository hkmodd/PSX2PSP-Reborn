/* psxrt: running the translated code of DATA.PSAR (native.c, trampolines.S). */
#ifndef PSXRT_NATIVE_H
#define PSXRT_NATIVE_H

#include "disc.h"
#include "psx.h"

typedef struct NativeStats {
    u32 calls;  /* host calls (I/O, COP0, GTE, syscalls, polls) */
    u32 misses; /* dispatch-table misses */
    u32 steps;  /* instructions interpreted */
    u32 loads;  /* translated executables loaded */
} NativeStats;

extern NativeStats native_stats;

/* Takes over `psx` (after psx_init, before psx_boot): guest calls, RAM-load tracking, the
 * dispatch table, the stubs and the interrupt alarm. The code region must be reserved. NULL,
 * or what is wrong. */
const char *native_init(Psx *psx, const DiscInfo *info);
/* Runs the guest from psx->cpu.pc; does not return. */
void native_run(void) __attribute__((noreturn));
/* Provided by the runtime: stops with a message; does not return. */
void runtime_halt(const char *why) __attribute__((noreturn));

#endif
