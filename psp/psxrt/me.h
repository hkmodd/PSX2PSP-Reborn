/*
 * psxrt's Media Engine (the ME translator design note, step 2b). Slice 2b-1: with psxrt.me beside
 * the EBOOT the ME leaves the firmware for psxrt's own loop (me.c), idles there and answers one
 * ping job a second; the GE path stays on the CPU. sceSasCore runs on the ME, so main.c has the
 * SPU mix every voice when psxrt.me is there.
 *
 * Kernel words through KUBridge (kubridge.S, ARK-4). The ME goes back in reset when psxrt exits;
 * the firmware does not take it back, so the PSP is best restarted after a run.
 */
#ifndef PSXRT_ME_H
#define PSXRT_ME_H

/* 1 when the ME runs psxrt's loop; msg gets the boot's line for the log either way */
int me_start(char *msg, int cap);
/* once a second: the "-- me" line (beat, pings, the clock check), its length; 0 when the ME is off */
int me_log(char *line, int cap);
/* the ME back in reset (the exit callback); nothing when it is off */
void me_stop(void);

#endif
