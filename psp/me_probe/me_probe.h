/*
 * me_probe: probe 0 of the ME translator (me-translator design note), on a real PSP. main.c runs
 * on the main CPU (the SC), me.c's jobs on the Media Engine. They share one block of words,
 * uncached on both sides (MeCtl), and nothing else they both write.
 *
 * ge.c, gpu.c, ge_host_pc.c, gel_hash.c and replay.c are linked into seven copies (Makefile),
 * each with its own statics and its own memory (an Arena): copies 0-4 run on the SC, 5 and 6 on
 * the ME. The ME's copies have their statics on 64-byte lines of their own (pad_*.S), so that the
 * ME's data cache, written back, never overwrites what the SC wrote beside them.
 */
#ifndef ME_PROBE_H
#define ME_PROBE_H

#include "ge.h"
#include "replay.h"

#define TR_COPIES 7
#define ME_ALIVE 0x4D454F4Bu /* "MEOK", the ME's first write */

enum {
    JOB_NONE,
    JOB_READ_UNCACHED, /* arg0 words at arg1 (an uncached address): res0 sum, res1 count ticks */
    JOB_READ_CACHED,   /* the same through the ME's data cache, cold */
    JOB_FILL,          /* eDRAM [arg0, arg0 + arg1) = arg2 */
    JOB_COPY,          /* arg1 bytes from arg0 into eDRAM at arg2, then written back: res0 eDRAM's sum, res1 ticks */
    JOB_SUM,           /* res0: the sum of eDRAM [arg0, arg0 + arg1) */
    JOB_STREAM,        /* JOB_COPY again and again until stop: res0 passes, res1 ticks */
    JOB_CALIB,         /* count ticks until stop: res0 ticks */
    JOB_REPLAY,        /* copy arg0 (5, 6) runs the dump arg2 (arg3 words) on Psx arg1 into PrRun arg4: res0 ok */
    JOB_PROF,          /* the profiler at 0xBC400000 as the ME sees it: res0 word 0, res1 word 2's change, res2 ticks */
    JOB_EDRAM,         /* uncached eDRAM at 1 MB and 3 MB written 0x11111111, 0x22222222: res0, res1 read back */
};

/* 128 bytes, 64-byte aligned, only ever read and written uncached */
typedef struct {
    /* the SC's */
    u32 seq, job, arg[6], stop, fcr31, sc_pad[6];
    /* the ME's */
    u32 alive, beat, ack, done, progress, err, res[10];
} MeCtl;

/* An arena's head (64 bytes), then its size bytes */
typedef struct {
    u32 used, size, peak, pad[13];
} Arena;
void *arena_calloc(Arena *a, u32 n, u32 s); /* 64-byte aligned and zeroed; NULL when full */
extern Arena *tr_arena[TR_COPIES];          /* each copy's calloc */

/* one replay's frames: clock ticks between frame ends (the first frame, ge.c from cold, apart) */
#define PR_FRAMES 32
typedef struct {
    u32 frame, polys, t, match, h_hi, h_lo, mem, mh_hi, mh_lo;
} PrFrame;
typedef struct {
    u32 (*now)(void);
    volatile MeCtl *ctl; /* the ME's progress, or NULL */
    u32 t_last, nf, polys, t, matched;
    int ok;
    ReplayResult r;
    PrFrame f[PR_FRAMES];
} PrRun;
void pr_frame(void *user, const ReplayFrame *f);

typedef int (*TrRun)(Psx *psx, const u32 *d, u32 n, int mode, const ReplayHooks *h, ReplayResult *r);
int tr0_run(Psx *, const u32 *, u32, int, const ReplayHooks *, ReplayResult *);
int tr1_run(Psx *, const u32 *, u32, int, const ReplayHooks *, ReplayResult *);
int tr2_run(Psx *, const u32 *, u32, int, const ReplayHooks *, ReplayResult *);
int tr3_run(Psx *, const u32 *, u32, int, const ReplayHooks *, ReplayResult *);
int tr4_run(Psx *, const u32 *, u32, int, const ReplayHooks *, ReplayResult *);
int tr5_run(Psx *, const u32 *, u32, int, const ReplayHooks *, ReplayResult *);
int tr6_run(Psx *, const u32 *, u32, int, const ReplayHooks *, ReplayResult *);

/* me.c */
extern volatile MeCtl *me_ctl; /* the uncached alias, set before the ME starts */
void me_handler(void); /* _me_section: copied to 0xBFC00000, where the ME starts */
u32 me_now(void);      /* the ME's cp0 Count */

#endif
