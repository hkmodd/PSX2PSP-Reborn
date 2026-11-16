/*
 * psxrt: the GE renderer of the PS1 GPU (docs/recomp/S4.md). PSP VRAM 0x000000-0x0FFFFF holds
 * the GE's copy of PS1 VRAM as a 5551 framebuffer of stride 1024; the core's copy stays in RAM.
 * The GE display list is shared with the host: ge_open() before host GE commands, ge_sync() to
 * run them and wait.
 */
#ifndef PSXRT_GE_H
#define PSXRT_GE_H

#include "psx.h"

#define GE_PS1_VRAM 0x000000u /* offsets in PSP VRAM */
#define GE_DISP_A 0x100000u
#define GE_DISP_B 0x144000u

/* Installs the renderer as psx->gpu.renderer and clears the GE's copy of PS1 VRAM (RAM's copy is
 * all zero after psx_init). compare: the GE and the software rasterizer both draw everything and
 * the copies are compared once per second (psxrt.trace bit 3). */
void ge_init(Psx *psx, int compare);
/* Opens the display list for host commands; the renderer sets its state again afterwards. */
void ge_open(void);
/* Ends the display list and waits for the GE. */
void ge_sync(void);
/* Lets the GE run all of the open list now, without waiting; the list stays open. */
void ge_run(void);
/* Brings the GE's pixels of a PS1 VRAM rectangle into RAM. */
void ge_read(int x, int y, int w, int h);
/* Compare mode: the GE's copy of PS1 VRAM, read back now (RAM's holds the software rasterizer's);
 * NULL otherwise. */
const unsigned short *ge_compare_vram(void);
/* A draw into PS1 VRAM that changes nothing, once per frame (ge.c says why). */
void ge_touch(void);
/* Appends the per-second "-- ge:" lines (counters, and the comparison in compare mode). */
int ge_log(char *buf, int cap);

#endif
