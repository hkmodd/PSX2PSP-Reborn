/*
 * psxrt's Media Engine (me.h), both sides. The SC copies me_handler to 0xBFC00000, writes into its
 * copy the unlock flag, me_main's cached address and the shared block's uncached address, and
 * lets the ME out of reset. The handler turns on the ME's buses and FPU, clears its caches, puts
 * its stack at the top of its eDRAM and jumps to me_main, which runs the SC's jobs one by one from
 * MeCtl. Nothing on the ME's side calls the firmware.
 *
 * Slice 2b-2: the GPU's linked lists (dma.c's dma_list_walk, through gpu.c into ge.c) are a job,
 * through dma_list_hook. The SC waits on the job and serves it: the clock it publishes in MeCtl,
 * the list calls ge_host.h sends here, the GE's stall from the word the ME moves. Both data caches
 * go to RAM and are dropped at the job's edges and around every list call, so each side reads what
 * the other wrote; between those points no line the ME writes is written by the SC.
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
#include <pspge.h>
#include <pspkernel.h>
#include <psputils.h>
#include <stdio.h>
#include <string.h>

#include "ge_host.h"
#include "me.h"
#include "psx.h"

/* KUBridge (kubridge.S) */
unsigned int kuKernelPeekw(void *addr);
void kuKernelPokew(void *addr, unsigned int value);
#define KW(a) ((void *)(a))

#define RESETEN 0xBC10004Cu /* bit 0x04: the ME held in reset */
#define ME_RESET_BIT 0x04u
#define ME_VECTOR 0xBFC00000u /* where the ME starts out of reset */
#define ME_ALIVE 0x4D454F4Bu  /* "MEOK", the ME's first write */
#define ME_TIMEOUT 1000000u   /* us without an answer or a list call: the ME is off */

enum {
    JOB_NONE,
    JOB_PING, /* res0 = arg0 + 1 */
    JOB_WALK, /* res0 = dma_list_walk(arg0, arg1) */
};
enum { RPC_START = 1, RPC_FINISH, RPC_WAIT }; /* geh_list_start(arg), geh_list_finish(), geh_list_wait() */

/* 128 bytes, 64-byte aligned, only ever read and written uncached: a line each side writes */
typedef struct {
    u32 seq, job, arg[2], sc_us, rpc_ack, rpc_res, sc_pad[9];
    u32 alive, beat, ack, done, err, res[3], st_seq, st_id, st_addr, rpc_seq, rpc_op, rpc_arg, me_pad[2];
} MeCtl;

static volatile MeCtl *ctl; /* written by the SC before the boot, only read after */

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

/* ge_host.h's calls on the ME. The clock is the one the SC publishes while it waits. */
u32 me_geh_us(void) { return ctl->sc_us; }
void me_geh_wbinv_all(void) { me_dcache_wbinv_all(); }

/* The GE may run the list up to buf + n: the SC applies the newest stall it finds. ge.c wrote the
 * list back first, so any address the SC reads here has its bytes in RAM. */
void me_geh_list_stall(int id, void *buf, u32 n)
{
    volatile MeCtl *c = ctl;
    __asm__ volatile("sync");
    c->st_id = (u32)id, c->st_addr = (u32)GEH_UNCACHED(buf) + n;
    __asm__ volatile("sync");
    c->st_seq = c->st_seq + 1;
}

/* A call the SC makes for the ME: everything the ME wrote is in RAM first and nothing of RAM stays
 * in its cache, so the ME reads what the SC's call wrote (the SC writes it back before the ack). */
static u32 me_rpc(u32 op, u32 arg)
{
    volatile MeCtl *c = ctl;
    me_dcache_wbinv_all();
    c->rpc_op = op, c->rpc_arg = arg;
    __asm__ volatile("sync");
    u32 s = c->rpc_seq + 1;
    c->rpc_seq = s;
    while (c->rpc_ack != s)
        ;
    return c->rpc_res;
}

int me_geh_list_start(void *buf) { return (int)me_rpc(RPC_START, (u32)buf); }
u32 me_geh_list_finish(void) { return me_rpc(RPC_FINISH, 0); }
void me_geh_list_wait(void) { me_rpc(RPC_WAIT, 0); }

static void me_job(volatile MeCtl *c)
{
    switch (c->job) {
    case JOB_PING:
        c->res[0] = c->arg[0] + 1;
        break;
    case JOB_WALK:
        c->res[0] = dma_list_walk((Psx *)c->arg[0], c->arg[1]);
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
        me_dcache_wbinv_all(); /* no line of the SC's memory older than RAM */
        me_job(c);
        me_dcache_wbinv_all(); /* everything the job wrote is in RAM before done */
        __asm__ volatile("sync");
        c->done = seq;
    }
}

/* ---- the SC's side ---- */

static u8 ctl_mem[128] __attribute__((aligned(64)));
static int on, dead;
static u32 seq, kicked_job, beat0, pings, pings_ok, late, n_dead;
static u32 walks, walk_us, rpcs, stalls, st_seen, rpc_seen;
static PsxHost me_host; /* the game's host for a walk on the ME: no log, the published clock */

static u32 now(void) { return sceKernelGetSystemTimeLow(); }

static void me_reset(void) { kuKernelPokew(KW(RESETEN), kuKernelPeekw(KW(RESETEN)) | ME_RESET_BIT); }

static void kick(u32 job, u32 a0, u32 a1)
{
    ctl->job = job, ctl->arg[0] = a0, ctl->arg[1] = a1;
    __asm__ volatile("sync");
    ctl->seq = ++seq;
    kicked_job = job;
}

/* The ME stopped answering: back in reset, and the lists walk on the SC from now on. */
static void me_dead(void)
{
    me_reset();
    dma_list_hook = NULL;
    dead = 1, n_dead++;
}

/* A ping's answer, counted once */
static void ping_check(void)
{
    if (kicked_job == JOB_PING)
        pings_ok += ctl->res[0] == seq + 1;
    kicked_job = JOB_NONE;
}

/* The last job done within ME_TIMEOUT; 0, and the ME off, when it was not. */
static int me_idle(void)
{
    u32 t = now();
    while (ctl->done != seq)
        if (now() - t > ME_TIMEOUT) {
            me_dead();
            return 0;
        }
    ping_check();
    return 1;
}

static u32 me_usec(void *user)
{
    (void)user;
    return ctl->sc_us;
}

/* dma_list_hook: the walk runs on the ME while the SC serves it. Before the kick the SC's cache
 * goes to RAM and is dropped, so the ME reads the SC's latest; after done it is dropped again (the
 * ME's writes are in RAM). While it waits the SC writes its stack, this file's counters and MeCtl;
 * the list calls it makes for the ME are written back before the ack. With a trace on the walk stays
 * on the SC: the ME has no log, and the traces' statics (gpu.c's last image) share lines with the
 * disc thread's. */
static u32 me_walk(Psx *psx, u32 addr)
{
    if (psx->trace || !me_idle())
        return dma_list_walk(psx, addr);
    const PsxHost *h0 = psx->host;
    me_host = *h0;
    me_host.log = NULL;
    me_host.usec = h0->usec ? me_usec : NULL;
    psx->host = &me_host;
    u32 t0 = now(), tp = t0, words = 0;
    ctl->sc_us = t0;
    sceKernelDcacheWritebackInvalidateAll();
    kick(JOB_WALK, (u32)psx, addr);
    for (;;) {
        u32 t = now();
        ctl->sc_us = t;
        u32 done = ctl->done, r = ctl->rpc_seq, s = ctl->st_seq; /* in this order: what the ME wrote before is in */
        if (s != st_seen) {
            st_seen = s, stalls++, tp = t;
            sceGeListUpdateStallAddr((int)ctl->st_id, (void *)ctl->st_addr);
        }
        if (r != rpc_seen) {
            u32 a = ctl->rpc_arg, res = 0;
            sceKernelDcacheWritebackInvalidateAll();
            switch (ctl->rpc_op) {
            case RPC_START: res = (u32)geh_list_start((void *)a); break;
            case RPC_FINISH: res = geh_list_finish(); break;
            case RPC_WAIT: geh_list_wait(); break;
            }
            sceKernelDcacheWritebackInvalidateAll();
            ctl->rpc_res = res;
            __asm__ volatile("sync");
            ctl->rpc_ack = r;
            rpc_seen = r, rpcs++, tp = now();
            continue;
        }
        if (done == seq) {
            words = ctl->res[0];
            kicked_job = JOB_NONE;
            break;
        }
        if (t - tp > ME_TIMEOUT) {
            me_dead();
            break;
        }
    }
    sceKernelDcacheWritebackInvalidateAll();
    psx->host = h0;
    walks++, walk_us += now() - t0;
    return words;
}

static int boot(u32 unlock, u32 *us, u32 *bad)
{
    const u32 *h = (const u32 *)(void *)me_handler;
    u32 words = (u32)(me_handler_end - h);
    memset(ctl_mem, 0, sizeof ctl_mem);
    ctl = (volatile MeCtl *)(0x40000000u | (u32)ctl_mem);
    sceKernelDcacheWritebackInvalidateAll(); /* ctl in RAM for the ME; no cached line of ctl_mem left */
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
    u32 us = 0, bad = 0, unlock = 0;
    on = boot(0, &us, &bad) || boot(unlock = 1, &us, &bad);
    if (!on) {
        snprintf(msg, (size_t)cap, "ME: no answer in 500 ms with or without the unlock (RESETEN %08X, readback bad %u): the GPU's lists walk on the CPU",
                 (unsigned)r0, (unsigned)bad);
        return 0;
    }
    beat0 = ctl->beat;
    dma_list_hook = me_walk;
    snprintf(msg, (size_t)cap,
             "ME: psxrt's loop runs (unlock %u, %u us, %u handler words, readback bad %u); RESETEN %08X before, %08X now;"
             " the GPU's lists walk there",
             (unsigned)unlock, (unsigned)us, (unsigned)(me_handler_end - (const u32 *)(void *)me_handler), (unsigned)bad,
             (unsigned)r0, kuKernelPeekw(KW(RESETEN)));
    return 1;
}

int me_log(char *line, int cap)
{
    if (!on || cap <= 0)
        return 0;
    int n;
    if (dead) {
        n = snprintf(line, (size_t)cap, "-- me: off, no answer in %u ms (%u times; job %u, err %X): the lists walk on the CPU\n",
                     (unsigned)(ME_TIMEOUT / 1000), (unsigned)n_dead, (unsigned)kicked_job, (unsigned)ctl->err);
    } else {
        u32 b = ctl->beat;
        if (ctl->done == seq) { /* the next ping only when the last job is done */
            ping_check();
            kick(JOB_PING, seq + 1, 0);
            pings++;
        } else {
            late++;
        }
        n = snprintf(line, (size_t)cap, "-- me: beat %u, pings %u/%u, late %u, err %X; walks %u in %u ms, list calls %u, stalls %u\n",
                     (unsigned)(b - beat0), (unsigned)pings_ok, (unsigned)pings, (unsigned)late, (unsigned)ctl->err,
                     (unsigned)walks, (unsigned)(walk_us / 1000), (unsigned)rpcs, (unsigned)stalls);
        beat0 = b;
    }
    walks = walk_us = rpcs = stalls = 0;
    return n < cap ? n : cap - 1;
}

void me_stop(void)
{
    if (on && !dead)
        me_reset();
    dma_list_hook = NULL;
    on = 0;
}
