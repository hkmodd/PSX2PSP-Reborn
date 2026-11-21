/*
 * psxrt's Media Engine (the ME translator design note, step 2b). With psxrt.me beside the EBOOT
 * the ME leaves the firmware for psxrt's own loop (me.c) and answers one ping job a second
 * (slice 2b-1); the GPU's linked lists walk there, through dma.c's dma_list_hook (2b-2), while the
 * CPU goes on with the game until an entry of the GPU's needs the walk's result (2b-3, through
 * gpu_sync_hook). sceSasCore runs on the ME, so main.c has the SPU mix every voice when psxrt.me
 * is there.
 *
 * Kernel words through KUBridge (kubridge.S, ARK-4). The ME goes back in reset when psxrt exits.
 * No restart is needed after a run: every run that took the ME found it out of reset at boot
 * (RESETEN 0x40 in me_probe's 3 logs and in 2b-1/2b-2's, with no restart in between).
 */
#ifndef PSXRT_ME_H
#define PSXRT_ME_H

/* 1 when the ME runs psxrt's loop (and the hooks are set); msg gets the boot's line either way */
int me_start(char *msg, int cap);
/* once a second, from the vblank: the "-- me" line (beat, pings, the walks, the CPU's waits for
 * them by cause, list calls, stalls), its length; 0 when the ME is off */
int me_log(char *line, int cap);
/* The SPU's mix to the ME (core/psx.h, SpuMe) from the Psx's state now: 1 when it went (the ME
 * runs psxrt's loop and the SPU's IRQ is off); after a boot or a snapshot's load. spu_me_off
 * brings it back inline (before a snapshot's save). */
struct Psx;
int me_spu_on(struct Psx *psx);
/* the ME back in reset and the hooks NULL (the exit callback); nothing when it is off */
void me_stop(void);

#endif
