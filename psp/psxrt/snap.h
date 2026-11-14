/* psxrt: snapshots of the whole machine, to start a run where an earlier one was saved
 * (docs/recomp/S9.md). */
#ifndef PSXRT_SNAP_H
#define PSXRT_SNAP_H

#include "disc.h"
#include "psx.h"

/* Writes the PS1 machine, the translation state and `host_pc` (where translated code goes on)
 * to `path`. The GE's pixels must already be in RAM's copy of VRAM. 0, or -1. */
int snap_save(const char *path, Psx *psx, const DiscInfo *info, u32 host_pc, u32 frames);
/* Restores a snapshot of this EBOOT into `psx` (after native_init, instead of psx_boot): 0 with
 * the host address to enter and the frame count; -1 no file; -2 made by another EBOOT or
 * runtime layout (nothing changed); -3 unreadable after the machine was partly overwritten. */
int snap_load(const char *path, Psx *psx, const DiscInfo *info, u32 *host_pc, u32 *frames);

#endif
