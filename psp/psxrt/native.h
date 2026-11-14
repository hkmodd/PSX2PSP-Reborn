/* psxrt: running the translated code of DATA.PSAR (native.c, trampolines.S). */
#ifndef PSXRT_NATIVE_H
#define PSXRT_NATIVE_H

#include "disc.h"
#include "psx.h"

enum { IO_GPU, IO_DMA, IO_CD, IO_SPU, IO_TIMER, IO_IRQ, IO_MDEC, IO_PAD, IO_GTE, IO_GTEC, IO_COP0, IO_OTHER, IO_KINDS };
#define IO_KIND_NAMES {"gpu", "dma", "cd", "spu", "timer", "irq", "mdec", "pad", "gte", "gtecmd", "cop0", "other"}

typedef struct NativeStats {
    u32 calls;  /* host calls (I/O, COP0, GTE, syscalls, polls) */
    u32 misses; /* dispatch-table misses */
    u32 steps;  /* instructions interpreted */
    u32 loads;  /* translated executables loaded */
    u32 hashes; /* texts hashed to recognise an executable */
    u32 poll_skips; /* timer reads in a polling loop that let PS1 time pass */
    /* microseconds: in the interpreter (with what it calls), host-call instructions (device
     * I/O, DMA), device events (vblank, IRQs, guest callbacks), presenting, sleeping */
    u32 us_interp, us_io, us_update, us_present, us_sleep;
    /* host-call instructions by what they touch (IO_KIND_NAMES): count and microseconds */
    u32 n_io[IO_KINDS], us_io_kind[IO_KINDS];
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
