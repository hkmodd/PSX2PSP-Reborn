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
    u32 hooks;      /* recipes run (hooks.rs) */
    /* microseconds: in the interpreter (with what it calls), host-call instructions (device
     * I/O, DMA), device events (vblank, IRQs, guest callbacks), presenting, sleeping */
    u32 us_interp, us_io, us_update, us_present, us_sleep;
    /* host-call instructions by what they touch (IO_KIND_NAMES): count and microseconds */
    u32 n_io[IO_KINDS], us_io_kind[IO_KINDS];
    u32 n_gte[64]; /* GTE commands by opcode */
    /* PS1 vblanks that waited for the PSP's, came after it, came a frame or more late; waited us */
    u32 vs_wait, vs_late, vs_adv, us_vs_wait, us_late, us_late_max;
} NativeStats;

extern NativeStats native_stats;

/* Takes over `psx` (after psx_init, before psx_boot): guest calls, RAM-load tracking, the
 * dispatch table, the stubs and the interrupt alarm. The code region must be reserved. NULL,
 * or what is wrong. */
const char *native_init(Psx *psx, const DiscInfo *info);
/* Runs the guest from psx->cpu.pc, or enters translated code at `host_pc` (a snapshot's) with
 * the context as it is; does not return. */
void native_run(u32 host_pc) __attribute__((noreturn));
/* Called at each PS1 vblank once its frame is presented: paces PS1 frames to the PSP display. */
void native_vsync(void);
/* Provided by the runtime: stops with a message; does not return. */
void runtime_halt(const char *why) __attribute__((noreturn));

/* Snapshots (snap.c). Set by the runtime: the next host call of the top-level code (no guest
 * call of the BIOS in progress) calls runtime_snapshot() with the host address it goes on to,
 * the guest state being in the context and psx->cpu. */
extern volatile int native_snap_req;
void runtime_snapshot(u32 host_pc);
/* The context, the translation state and the dispatch table, to and from an open file; loading
 * also brings back the code of the executables that were resident. 0, or -1. */
int native_save(int fd);
int native_load(int fd);

#endif
