/* psxrt: running the translated code of DATA.PSAR (native.c, trampolines.S). */
#ifndef PSXRT_NATIVE_H
#define PSXRT_NATIVE_H

#include "disc.h"
#include "psx.h"

/* IO_VLC: the DecDCTvlc recipe's calls and their time, all of it (not sampled) */
enum { IO_GPU, IO_DMA, IO_CD, IO_SPU, IO_TIMER, IO_IRQ, IO_MDEC, IO_PAD, IO_GTE, IO_GTEC, IO_COP0, IO_OTHER, IO_VLC, IO_KINDS };
#define IO_KIND_NAMES {"gpu", "dma", "cd", "spu", "timer", "irq", "mdec", "pad", "gte", "gtecmd", "cop0", "other", "vlc"}

typedef struct NativeStats {
    u32 calls;  /* host calls (I/O, COP0, GTE, syscalls, polls) */
    u32 misses; /* dispatch-table misses */
    u32 steps;  /* guest instructions interpreted */
    u32 bios;   /* BIOS calls (vectors A0/B0/C0) served by the HLE on the interpreter's path */
    u32 loads;  /* translated executables loaded */
    u32 hashes; /* texts hashed to recognise an executable */
    u32 poll_skips; /* timer reads in a polling loop that let PS1 time pass */
    u32 poll_far;   /* of those, the ones past one tick (pollscan.c) */
    u32 ra_scans;   /* searches of a host ra's guest return site (guest_ra) */
    u32 hooks;      /* recipes run (hooks.rs) */
    /* microseconds: in the interpreter (with what it calls), host-call instructions (device
     * I/O, DMA), device events (vblank, IRQs, guest callbacks), presenting, sleeping */
    u32 us_interp, us_io, us_update, us_present, us_sleep;
    /* host-call instructions by what they touch (IO_KIND_NAMES): count and microseconds */
    u32 n_io[IO_KINDS], us_io_kind[IO_KINDS];
    u32 n_gte[64]; /* GTE commands by opcode */
    /* PS1 vblanks that waited for the PSP's, came after it, forgiven (PS1 time slowed), owed
     * (the next one comes early); waited us */
    u32 vs_wait, vs_late, vs_adv, vs_short, us_vs_wait, us_late, us_late_max;
    u32 updates; /* device updates (psx_update, the stall's included) */
    u32 io_lean; /* of the host calls, device reads served by io_fast.c (IO_READ) and device writes by native_io_write (IO_WRITE) */
    u32 io_fall[6]; /* timer/IRQ/pad accesses those left to the host call: event due, IRQ pending (or unmasked by the write), poll, then tmr_sync 1-3 */
    u32 io_host; /* timer/IRQ/pad instructions run as host calls (recipes' own reads not counted) */
    u32 vsync_passes; /* v_wait passes counted down at once by hook_vsync_wait (not run as guest code) */
    u32 fl_places, fl_reused, fl_drops, fl_full; /* floating code: units placed, placements reinstalled, dropped, refused for room */
    u32 us_loaded; /* microseconds checking the translations of what DMAs and BIOS reads loaded (on_ram_loaded); main.c zeroes it */
    /* owed-vblank stalls held by guest_owes: on an interrupt the CPU takes, a raised timer bit,
     * a queued sector; the I_STAT bits and CD interrupt masks seen then (main.c zeroes them);
     * PS1 kcycles of stall asked and run; CD sectors replaced while it ran; parts ended on 1024
     * events, events found due and not run; PS1 cycles the CD drive waited in it for the guest */
    u32 st_cut[3], st_bits, st_cdmask, st_ask_k, st_done, st_repl, st_jump[2], st_cdwait;
    u32 st_iec; /* updates that left the stall to the next one: the CPU was in a handler or a critical section (SR.IEc 0) */
    u32 io_forced; /* lean device writes that raised an interrupt: taken at the next poll, forced (native_io_write) */
} NativeStats;

extern NativeStats native_stats;

/* CTX.BUDGET is refilled to NATIVE_BUDGET at every runtime entry (1 ms of loops between them);
 * the interrupt alarm subtracts NATIVE_FORCED to make the next back-edge enter. */
#define NATIVE_BUDGET (PSX_CLOCK / 1000)
#define NATIVE_FORCED 0x40000000u
/* Timer reads of a polling loop (native.c poll_skip): a pair's second read comes within
 * NATIVE_POLL_PAIR cycles of its first, a loop's next pass within NATIVE_POLL_GAP. */
#define NATIVE_POLL_PAIR 16u
#define NATIVE_POLL_GAP 2048u

/* poll_skip's track of the counter reads, kept by io_fast.c's reads too: the loop's site, how
 * many passes in a row, when it last passed and how long that pass took, when any counter was
 * last read. */
typedef struct { u32 hot, streak; u64 hot_at, period, last_at; } NativePoll;
extern NativePoll native_poll;

/* The track after a counter read at guest `pc` at `now`, `tick` cycles a count (0: not a running
 * count register): nonzero when poll_skip moves the clock on at this read. */
static inline int native_poll_note(NativePoll *q, u32 pc, u32 tick, u64 now, u64 next_event)
{
    u64 since = now - q->last_at;
    q->last_at = now;
    if (!tick)
        return 0;
    if (pc != q->hot) {
        if (since >= NATIVE_POLL_PAIR)
            q->hot = pc, q->hot_at = now, q->streak = 0;
        return 0;
    }
    u64 period = q->period = now - q->hot_at;
    q->hot_at = now;
    if (period >= NATIVE_POLL_GAP) {
        q->streak = 0;
        return 0;
    }
    return ++q->streak >= 4 && tick > period && next_event > now;
}

/* main.c, with psxrt.prof: the profiler's counters around the runtime's own work, sampled, each
 * kept sample counted `weight` times. PB_CALL host calls of translated code, PB_UPDATE device
 * updates, PB_EMPTY a bracket around nothing taken like PB_CALL's (what the counter reads add to
 * each sample), PB_VBLANK every vblank's update, weight 1: the devices' vblank work, the host's
 * (present, the log second, sleeps) and the other threads that run while it waits, less the GPU
 * DMAs inside it. prof_begin returns 0 when not sampling, and when another bracket is open (then
 * the sample is counted dropped); `drop`: other bracketed work ran inside. */
enum { PB_CALL, PB_UPDATE, PB_EMPTY, PB_VBLANK, PB_PARTS };
#define PB_EVERY 64u
int prof_begin(int part, u32 weight);
void prof_end(int part, int drop);
/* main.c, with psxrt.prof: a host call at guest `pc` (its flag bits kept), counted for the log */
void prof_site(u32 pc);

/* Takes over `psx` (after psx_init, before psx_boot): guest calls, RAM-load tracking, the
 * dispatch table, the stubs and the interrupt alarm. The code region must be reserved. NULL,
 * or what is wrong. */
const char *native_init(Psx *psx, const DiscInfo *info);
/* Runs the guest from psx->cpu.pc, or enters translated code at `host_pc` (a snapshot's) with
 * the context as it is; does not return. */
void native_run(u32 host_pc) __attribute__((noreturn));
/* Called at each PS1 vblank once its frame is presented: paces PS1 frames to the PSP display. */
void native_vsync(void);
/* Called at each PS1 vblank before its frame is presented: a frame early for its PSP vblank
 * waits for it. */
void native_vsync_early(void);
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
/* The ten guest pcs where the interpreter ran most instructions since the last call (psxrt.trace
 * bit 8), as one log line into `buf`; clears the counts. Its length. */
int native_entries_log(char *buf, int len);
/* The guest loops that took most PS1 cycles since the last call (sampled at the budget polls), as
 * one log line into `buf`; clears the counts. Its length. */
int native_hot_log(char *buf, int len);

#endif
