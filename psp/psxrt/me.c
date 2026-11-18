/*
 * psxrt's Media Engine (me.h), both sides. The SC copies me_handler to 0xBFC00000, writes into its
 * copy the unlock flag, me_main's cached address and the shared block's uncached address, and
 * lets the ME out of reset. The handler turns on the ME's buses and FPU, clears its caches, puts
 * its stack at the top of its eDRAM and jumps to me_main, which runs the SC's jobs one by one from
 * MeCtl. Nothing on the ME's side calls the firmware, and the ME writes no cached memory.
 *
 * The same boot as me_probe (psp/me_probe, probes 0-0c on a PSP-3000). The handler holds no
 * address of its own, so it needs no relocation in the prx: what it jumps to is in its words.
 *
 * The handler's steps and the cache loops follow mcidclan's Media Engine samples
 * (psp-media-engine-custom-core, me-simple-count; the license below). Changed here: the bus clock
 * and RAM size registers are or-ed, not overwritten; the memory protection is opened only when
 * the SC asks (the word at handler + 8).
 *
 *   MIT License. Copyright (c) 2024-2026 m-c/d
 *
 *   Permission is hereby granted, free of charge, to any person obtaining a copy of this software
 *   and associated documentation files (the "Software"), to deal in the Software without
 *   restriction, including without limitation the rights to use, copy, modify, merge, publish,
 *   distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the
 *   Software is furnished to do so, subject to the following conditions:
 *
 *   The above copyright notice and this permission notice shall be included in all copies or
 *   substantial portions of the Software.
 *
 *   THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING
 *   BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
 *   NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
 *   DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 *   OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
 */
#include <pspkernel.h>
#include <psputils.h>
#include <stdio.h>
#include <string.h>

#include "me.h"

/* KUBridge (kubridge.S) */
unsigned int kuKernelPeekw(void *addr);
void kuKernelPokew(void *addr, unsigned int value);
#define KW(a) ((void *)(a))

#define RESETEN 0xBC10004Cu /* bit 0x04: the ME held in reset */
#define ME_RESET_BIT 0x04u
#define ME_VECTOR 0xBFC00000u /* where the ME starts out of reset */
#define SYSTIME 0xBC600000u   /* the 1 MHz system timer [?]: me_start checks it against the kernel's */
#define ME_ALIVE 0x4D454F4Bu  /* "MEOK", the ME's first write */
#define ME_CLOCK_AT 10        /* the second whose job is JOB_CLOCK instead of a ping */

enum {
    JOB_NONE,
    JOB_PING,  /* res0 = arg0 + 1 */
    JOB_CLOCK, /* SYSTIME as the ME reads it, before and after ~300 us of nops: res0, res1 */
};

/* 128 bytes, 64-byte aligned, only ever read and written uncached: a line each side writes */
typedef struct {
    u32 seq, job, arg[2], sc_pad[12];
    u32 alive, beat, ack, done, err, res[3], me_pad[8];
} MeCtl;

/* ---- the ME's side ---- */

void me_handler(void);
extern const u32 me_handler_end[];
__asm__(
    "	.pushsection .text\n"
    "	.set push\n"
    "	.set noreorder\n"
    "	.set noat\n"
    "	.align 2\n"
    "	.globl me_handler\n"
    "	.globl me_handler_end\n"
    "me_handler:\n"
    "	b 1f\n"
    "	nop\n"
    "	.word 0\n" /* + 8: 1 = open the DDR memory protection first */
    "	.word 0\n" /* + 12: me_main, cached */
    "	.word 0\n" /* + 16: MeCtl, uncached: me_main's argument */
    "1:	lui $k0, 0xbc10\n"
    "	lw $k1, 0x50($k0)\n"
    "	ori $k1, $k1, 0x0f\n"
    "	sw $k1, 0x50($k0)\n" /* bus clocks: the ME, AW A and B, AW eDRAM */
    "	lw $k1, 0x40($k0)\n"
    "	ori $k1, $k1, 0x02\n"
    "	sw $k1, 0x40($k0)\n" /* RAM size: 64 MB */
    "	sync\n"
    "	lui $k0, 0xbfc0\n"
    "	lw $k1, 8($k0)\n"
    "	beqz $k1, 3f\n"
    "	nop\n"
    "	lui $k0, 0xbc00\n"
    "	li $k1, -1\n"
    "	addiu $at, $k0, 0x30\n"
    "2:	sw $k1, 0($k0)\n"
    "	addiu $k0, $k0, 4\n"
    "	bne $k0, $at, 2b\n"
    "	nop\n"
    "	sync\n"
    "3:	lui $k0, 0x3000\n"
    "	mtc0 $k0, $12\n" /* Status: CU0, CU1 (the FPU), kernel mode, interrupts off */
    "	sync\n"
    "	move $k0, $zero\n"
    "	ori $k1, $zero, 8192\n"
    "4:	cache 0x04, 0($k0)\n" /* instruction cache: every line invalid */
    "	cache 0x04, 0($k0)\n"
    "	cache 0x14, 0($k0)\n" /* data cache: every line written back and invalid */
    "	cache 0x14, 0($k0)\n"
    "	addiu $k0, $k0, 64\n"
    "	bne $k0, $k1, 4b\n"
    "	nop\n"
    "	sync\n"
    "	lui $sp, 0x8020\n" /* the top of 2 MB of eDRAM (every model has 2 MB at least) */
    "	lui $k0, 0xbfc0\n"
    "	lw $a0, 16($k0)\n"
    "	lw $k0, 12($k0)\n"
    "	jr $k0\n"
    "	nop\n"
    "me_handler_end:\n"
    "	.set pop\n"
    "	.popsection\n");

static void me_dcache_wbinv_all(void)
{
    __asm__ volatile("sync");
    for (u32 i = 0; i < 8192; i += 64)
        __asm__ volatile("cache 0x14, 0(%0)\n\tcache 0x14, 0(%0)" : : "r"(i));
    __asm__ volatile("sync");
}

static void me_job(volatile MeCtl *c)
{
    volatile const u32 *st = (volatile const u32 *)SYSTIME;
    switch (c->job) {
    case JOB_PING:
        c->res[0] = c->arg[0] + 1;
        break;
    case JOB_CLOCK:
        c->res[0] = *st;
        for (u32 i = 0; i < 100000; i++)
            __asm__ volatile("nop");
        c->res[1] = *st;
        break;
    default:
        c->err = 0xBAD70B;
    }
}

static void me_main(volatile MeCtl *c) __attribute__((noreturn, used));
static void me_main(volatile MeCtl *c)
{
    c->beat = 0, c->err = 0;
    c->alive = ME_ALIVE;
    for (;;) {
        u32 seq = c->seq;
        if (seq == c->done) { /* idle: one uncached look every few microseconds */
            c->beat++;
            for (u32 i = 0; i < 1000; i++)
                __asm__ volatile("nop");
            continue;
        }
        c->ack = seq;
        me_dcache_wbinv_all();
        me_job(c);
        me_dcache_wbinv_all(); /* everything the job wrote is in RAM before done */
        __asm__ volatile("sync");
        c->done = seq;
    }
}

/* ---- the SC's side ---- */

static u8 ctl_mem[128] __attribute__((aligned(64)));
static volatile MeCtl *ctl;
static int on;
static u32 seq, kicked_job, beat0, pings, pings_ok, late, secs, clock_kick;

static u32 now(void) { return sceKernelGetSystemTimeLow(); }

static void me_reset(void) { kuKernelPokew(KW(RESETEN), kuKernelPeekw(KW(RESETEN)) | ME_RESET_BIT); }

static void kick(u32 job, u32 a0)
{
    ctl->job = job, ctl->arg[0] = a0;
    __asm__ volatile("sync");
    ctl->seq = ++seq;
    kicked_job = job;
}

static int boot(u32 unlock, u32 *us, u32 *bad)
{
    const u32 *h = (const u32 *)(void *)me_handler;
    u32 words = (u32)(me_handler_end - h);
    memset(ctl_mem, 0, sizeof ctl_mem);
    ctl = (volatile MeCtl *)(0x40000000u | (u32)ctl_mem);
    sceKernelDcacheWritebackInvalidateAll(); /* no cached line of ctl_mem left to write back over the ME's */
    *bad = 0;
    for (u32 pass = 0; pass < 2; pass++)
        for (u32 i = 0; i < words; i++) {
            u32 w = i == 2 ? unlock : i == 3 ? 0x80000000u | (u32)me_main : i == 4 ? (u32)ctl : h[i];
            if (!pass)
                kuKernelPokew(KW(ME_VECTOR + 4 * i), w);
            else
                *bad += kuKernelPeekw(KW(ME_VECTOR + 4 * i)) != w;
        }
    u32 r = kuKernelPeekw(KW(RESETEN));
    kuKernelPokew(KW(RESETEN), r | ME_RESET_BIT);
    kuKernelPokew(KW(RESETEN), r & ~ME_RESET_BIT);
    u32 t = now();
    while ((*us = now() - t) < 500000 && ctl->alive != ME_ALIVE)
        sceKernelDelayThread(100);
    if (ctl->alive == ME_ALIVE)
        return 1;
    me_reset();
    return 0;
}

int me_start(char *msg, int cap)
{
    u32 r0 = kuKernelPeekw(KW(RESETEN));
    u32 st_hw = kuKernelPeekw(KW(SYSTIME)), st_k = now();
    u32 us = 0, bad = 0, unlock = 0;
    on = boot(0, &us, &bad) || boot(unlock = 1, &us, &bad);
    if (!on) {
        snprintf(msg, (size_t)cap, "ME: no answer in 500 ms with or without the unlock (RESETEN %08X, readback bad %u): the GE path stays on the CPU",
                 (unsigned)r0, (unsigned)bad);
        return 0;
    }
    beat0 = ctl->beat;
    snprintf(msg, (size_t)cap,
             "ME: psxrt's loop runs (unlock %u, %u us, %u handler words, readback bad %u); RESETEN %08X before, %08X now;"
             " SYSTIME word %u, sceKernelGetSystemTimeLow %u",
             (unsigned)unlock, (unsigned)us, (unsigned)(me_handler_end - (const u32 *)(void *)me_handler), (unsigned)bad,
             (unsigned)r0, kuKernelPeekw(KW(RESETEN)), (unsigned)st_hw, (unsigned)st_k);
    return 1;
}

int me_log(char *line, int cap)
{
    if (!on || cap <= 0)
        return 0;
    int n = 0;
    secs++;
    u32 b = ctl->beat, t = now();
    if (seq) {
        if (ctl->done != seq) {
            late++;
        } else if (kicked_job == JOB_PING) {
            pings_ok += ctl->res[0] == seq + 1;
        } else if (kicked_job == JOB_CLOCK) {
            u32 m0 = ctl->res[0], m1 = ctl->res[1];
            n += snprintf(line + n, (size_t)(cap - n),
                          "-- me clock: the SC kicked at %u, the ME read SYSTIME %u then %u (+%d), the SC saw it done at %u: %s\n",
                          (unsigned)clock_kick, (unsigned)m0, (unsigned)m1, (int)(m1 - m0), (unsigned)t,
                          m0 - clock_kick <= t - clock_kick && m1 - m0 <= t - m0 && m1 != m0 ? "the SC's clock" : "not the SC's clock");
            kicked_job = JOB_NONE;
        }
    }
    if (!seq || ctl->done == seq) { /* the next job only when the last one is done */
        if (secs == ME_CLOCK_AT) {
            clock_kick = now();
            kick(JOB_CLOCK, 0);
        } else {
            kick(JOB_PING, seq + 1);
            pings++;
        }
    }
    if (n < cap)
        n += snprintf(line + n, (size_t)(cap - n), "-- me: beat %u, pings %u/%u, late %u, err %X\n", (unsigned)(b - beat0),
                      (unsigned)pings_ok, (unsigned)pings, (unsigned)late, (unsigned)ctl->err);
    beat0 = b;
    return n < cap ? n : cap - 1;
}

void me_stop(void)
{
    if (on)
        me_reset();
    on = 0;
}
