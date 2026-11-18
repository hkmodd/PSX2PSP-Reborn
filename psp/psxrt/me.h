/*
 * psxrt's Media Engine (the ME translator design note, step 2b). With psxrt.me beside the EBOOT
 * the ME leaves the firmware for psxrt's own loop (me.c) and answers one ping job a second
 * (slice 2b-1); the GPU's linked lists walk there, the CPU waiting on each (slice 2b-2, through
 * dma.c's dma_list_hook). sceSasCore runs on the ME, so main.c has the SPU mix every voice when
 * psxrt.me is there.
 *
 * Kernel words through KUBridge (kubridge.S, ARK-4). The ME goes back in reset when psxrt exits;
 * the firmware does not take it back, so the PSP is best restarted after a run.
 */
#ifndef PSXRT_ME_H
#define PSXRT_ME_H

/* 1 when the ME runs psxrt's loop (and dma_list_hook is set); msg gets the boot's line either way */
int me_start(char *msg, int cap);
/* once a second: the "-- me" line (beat, pings, the walks and their list calls), its length; 0 when the ME is off */
int me_log(char *line, int cap);
/* the ME back in reset and dma_list_hook NULL (the exit callback); nothing when it is off */
void me_stop(void);

#endif
