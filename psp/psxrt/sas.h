/*
 * psxrt: SPU voices on the PSP's sceSasCore (the Media Engine). See sas.c.
 */
#ifndef PSXRT_SAS_H
#define PSXRT_SAS_H

#include "psx.h"

/* Loads the modules and starts the mixer for `psx`'s SPU (mode 1: it plays the voices it can;
 * 2: it plays them beside the SPU, which mixes them too, and only their levels are compared).
 * alloc: the runtime's partition allocator. NULL, or why it could not. */
const char *sas_init(Psx *psx, int mode, void *(*alloc)(const char *name, u32 size));

/* The emulation thread: spu_hw_mode's voice and tick, `t` being the sample of the audio
 * stream the event is at. */
int sas_voice(int voice, int ev, u32 t);
void sas_tick(u32 t);

/* The audio thread: mixes the voices over the grains of `ring` (stereo, `size` samples, a power
 * of two) up to sample `wr`. Returns the sample it has mixed up to. */
u32 sas_mix(s16 *ring, u32 size, u32 wr);

/* A "-- sas" line of the second's counters; *us gets the Media Engine's time this second. */
int sas_log(char *line, int cap, u32 *us);

#endif
