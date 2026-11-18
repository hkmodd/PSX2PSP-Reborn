/*
 * me_probe: a GE translator dump (psp/psxrt/ge_dump.h) run again through ge.c, on whatever runs
 * this: a PC, the PSP's main CPU, its Media Engine. The host of ge.c is ge_host_pc.c everywhere
 * (lists hashed, no GE, a clock that counts its reads), so every run makes the same choices.
 */
#ifndef REPLAY_H
#define REPLAY_H

#include "ge_dump.h"

typedef struct {
    GehFrame got;
    const u32 *want; /* GEDP_FRAME's words, as the sim had them */
    int match;       /* got == want (GE_SHADOW only: the mode the sim ran) */
    u32 polys;       /* polygons given to ge.c this frame */
    u32 t_calls;     /* clock ticks in the renderer calls this frame */
    u32 t_end;       /* and in the frame's end (the lists finished and hashed) */
} ReplayFrame;

typedef struct {
    u32 (*clock)(void); /* optional */
    void (*frame)(void *user, const ReplayFrame *f); /* optional: after each frame */
    void *user;
    int no_hash; /* timing: geh_hash(0), so no frame is the sim's */
} ReplayHooks;

typedef struct {
    u32 events, frames, frames_bad, rets_bad;
    u32 polys, t_calls, t_end; /* over the frames after the first (ge.c warm) */
    const char *err;           /* NULL: the dump read through to its end */
} ReplayResult;

/* psx: zeroed, with gpu.vram (VRAM_W * VRAM_H), ram (2 MB) and host (zeroed) set. mode: GE_SHADOW
 * as the sim ran (each frame's hash and each call's return checked), or GE_NORMAL as the PSP runs
 * (no software drawing beside ge.c: its own hashes). Once a process: ge.c is one. */
int replay_run(Psx *psx, const u32 *d, u32 n, int mode, const ReplayHooks *h, ReplayResult *r);

#endif
