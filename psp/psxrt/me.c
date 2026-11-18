/*
 * psxrt's Media Engine (me.h), both sides. The SC copies me_handler to 0xBFC00000, writes into its
 * copy the unlock flag, me_main's cached address and the shared block's uncached address, and
 * lets the ME out of reset. The handler turns on the ME's buses and FPU, clears its caches, puts
 * its stack at the top of its eDRAM and jumps to me_main, which runs the SC's jobs one by one from
 * MeCtl. Nothing on the ME's side calls the firmware.
 *
 * Slices 2b-2 and 2b-3: the GPU's linked lists (dma.c's dma_list_walk, through gpu.c into ge.c) are
 * a job, through dma_list_hook, on a copy of the Psx with its own GPU state. The CPU goes on with
 * the game; a serve thread looks at the job every POLL_US (the clock it publishes in MeCtl, the
 * list calls ge_host.h sends here, the GE's stall from the word the ME moves), and every entry
 * that reads or writes the GPU's state (core/psx.h, gpu_sync) waits for the job's end and serves it
 * meanwhile. Both data caches go to RAM at the job's edges and around every list call, so each side
 * reads what the other wrote; between those points no line the ME writes is written by the SC
 * (the link map's check, fshare.py beside the build).
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

#include "ge.h"
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

#define POLL_US 200 /* the serve thread's look at a walk in flight */

/* The walk's Psx: the game's GPU state and RAM, and a host with no log and no clock (the ME has
 * neither). During a walk only the ME writes it, and its lines hold nothing else (the type's
 * alignment pads it to whole lines). */
typedef struct {
    Psx psx;
    PsxHost host;
    u32 writes; /* gpu_writes' count of the walk (core/psx.h, gpu_count_writes) */
} __attribute__((aligned(64))) MeJob;

static MeJob mj;
static u8 ctl_mem[128] __attribute__((aligned(64)));
static int on, dead, mj_init;
static u32 seq, kicked_job, beat0, pings, pings_ok, late, n_dead;
static u32 st_seen, rpc_seen;

/* The walk in flight: the game's Psx, its list, the words the CPU counted, when it was kicked and
 * first seen done (0: not yet). */
static volatile int busy;
static Psx *walk_psx;
static u32 walk_addr, walk_words, t_kick, t_done;

/* The serve thread and the CPU's own thread take turns at serve_once: the serve thread, at a
 * higher priority, never runs inside the other's turn (it sees in_main and waits for its next
 * look); the CPU's thread runs inside the serve thread's turn only while that thread blocks in a
 * list call, and then sees in_serve and looks again. */
static volatile int in_main, in_serve, quit;
static SceUID kick_sema = -1, serve_thread = -1;

static struct {
    u32 walks, cpu_walks, walk_us, head_us, words_bad, rpcs, stalls, polls, lost;
    u32 waits[GPU_SYNC_N], wait_us[GPU_SYNC_N];
} st;

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
    gpu_sync_hook = NULL;
    dead = 1, n_dead++;
}

/* A ping's answer, counted once */
static void ping_check(void)
{
    if (kicked_job == JOB_PING)
        pings_ok += ctl->res[0] == seq + 1;
    kicked_job = JOB_NONE;
}

/* The last job (a ping: walks end in me_sync) done within ME_TIMEOUT; 0, and the ME off, when it
 * was not. */
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

/* One look at the walk: the clock published, the newest stall applied, a list call served. done
 * is read first, so what the ME wrote before it is in by the next two reads. 1 when the walk is
 * done; *moved is the time of the last stall or list call. */
static int serve_once(u32 *moved)
{
    u32 t = now();
    ctl->sc_us = t;
    u32 done = ctl->done, r = ctl->rpc_seq, s = ctl->st_seq;
    if (s != st_seen) {
        st_seen = s, st.stalls++, *moved = t;
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
        rpc_seen = r, st.rpcs++, *moved = now();
        return 0;
    }
    if (done != seq)
        return 0;
    if (!t_done)
        t_done = t;
    return 1;
}

/* The walk done: the ME's writes are in RAM (it wrote its cache back before done) and no line of
 * them stays in the SC's cache; its GPU state, VRAM writes and GPU interrupt go to the game's Psx. */
static void walk_end(void)
{
    sceKernelDcacheWritebackInvalidateAll();
    Psx *psx = walk_psx;
    psx->gpu = mj.psx.gpu;
    gpu_writes += mj.writes;
    if (mj.psx.i_stat & (1u << IRQ_GPU))
        psx_raise(psx, IRQ_GPU);
    ge_bind(psx);
    st.words_bad += ctl->res[0] != walk_words;
    st.walk_us += (t_done ? t_done : now()) - t_kick;
    kicked_job = JOB_NONE;
    busy = 0;
}

/* The ME stopped in a walk: back in reset, and the walk again on the SC from the GPU state before
 * the kick (the game's Psx was not written since). What the ME had put in the GE's list stays. */
static void walk_lost(void)
{
    me_dead();
    sceKernelDcacheWritebackInvalidateAll();
    Psx *psx = walk_psx;
    ge_bind(psx);
    kicked_job = JOB_NONE;
    busy = 0;
    st.lost++;
    dma_list_walk(psx, walk_addr);
}

/* gpu_sync_hook: the CPU waits for the walk in flight and serves it meanwhile. Nothing on the ME
 * (ge.c's entries call it there too). */
static void me_sync(int why)
{
    if (geh_me() || !busy)
        return;
    u32 t0 = now(), tp = t0;
    for (;;) {
        in_main = 1;
        __asm__ volatile("" : : : "memory");
        if (in_serve) {
            in_main = 0;
            continue;
        }
        int done = serve_once(&tp);
        in_main = 0;
        if (done) {
            walk_end();
            break;
        }
        if (now() - tp > ME_TIMEOUT) {
            walk_lost();
            break;
        }
    }
    if ((unsigned)why < GPU_SYNC_N)
        st.waits[why]++, st.wait_us[why] += now() - t0;
}

/* The serve thread: from a kick to the walk's done, a look every POLL_US, so the GE gets its
 * stalls and the ME its list calls while the CPU runs the game. */
static int serve_main(SceSize n, void *arg)
{
    (void)n, (void)arg;
    while (!quit) {
        sceKernelWaitSema(kick_sema, 1, NULL);
        while (!quit && busy) {
            u32 tp = 0;
            int done = 0;
            if (!in_main) {
                in_serve = 1;
                __asm__ volatile("" : : : "memory");
                done = serve_once(&tp);
                in_serve = 0;
                st.polls++;
            }
            if (done)
                break;
            sceKernelDelayThread(POLL_US);
        }
    }
    return 0;
}

/* The GPU list's words, from its headers: what dma_list_walk returns for it, so the DMA's time is
 * the same as with the walk on the SC. */
static u32 list_words(const Psx *psx, u32 addr)
{
    u32 words = 0;
    for (int guard = 0; guard < 0x40000; guard++) {
        u32 h;
        memcpy(&h, psx->ram + (addr & (RAM_SIZE - 4)), 4);
        words += (h >> 24) + 1;
        addr = h & 0xFFFFFF;
        if (addr & 0x800000)
            break;
        addr &= 0x1FFFFC;
    }
    return words;
}

/* dma_list_hook: the walk goes to the ME on a copy of the GPU state and the CPU goes on; the GPU's
 * entries (gpu_sync) wait for it. Before the kick the SC's cache goes to RAM, so the ME reads the
 * list and the state as they are now. With a trace on the walk stays on the SC (the ME has no
 * log, and the traces' statics share lines with the disc thread's). */
static u32 me_walk(Psx *psx, u32 addr)
{
    me_sync(GPU_SYNC_DMA);
    if (psx->trace || dead || !me_idle()) {
        st.cpu_walks++;
        return dma_list_walk(psx, addr);
    }
    u32 t0 = now();
    u32 words = list_words(psx, addr);
    if (!mj_init)
        mj.psx = *psx, mj_init = 1;
    mj.psx.gpu = psx->gpu;
    mj.psx.ram = psx->ram;
    mj.psx.trace = 0;
    mj.psx.i_stat = 0;
    mj.host = *psx->host;
    mj.host.log = NULL;
    mj.host.usec = NULL;
    mj.psx.host = &mj.host;
    mj.writes = 0;
    ge_bind(&mj.psx);
    walk_psx = psx, walk_addr = addr, walk_words = words, t_done = 0;
    ctl->sc_us = now();
    sceKernelDcacheWritebackAll();
    kick(JOB_WALK, (u32)&mj.psx, addr);
    busy = 1;
    t_kick = now();
    st.walks++, st.head_us += t_kick - t0;
    if (kick_sema >= 0)
        sceKernelSignalSema(kick_sema, 1);
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
    /* the serve thread: above the game's thread (0x20) and the disc's (0x1F), below audio (0x12) */
    kick_sema = sceKernelCreateSema("psxrt_me_kick", 0, 0, 0x7FFFFFFF, NULL);
    serve_thread = kick_sema < 0 ? -1 : sceKernelCreateThread("psxrt_me", serve_main, 0x16, 0x2000, 0, NULL);
    if (serve_thread >= 0 && sceKernelStartThread(serve_thread, 0, NULL) < 0)
        serve_thread = -1;
    gpu_me_psx = &mj.psx;
    gpu_me_writes = &mj.writes;
    gpu_sync_hook = me_sync;
    dma_list_hook = me_walk;
    snprintf(msg, (size_t)cap,
             "ME: psxrt's loop runs (unlock %u, %u us, %u handler words, readback bad %u); RESETEN %08X before, %08X now;"
             " the GPU's lists walk there while the CPU goes on (serve thread %s, a look every %u us)",
             (unsigned)unlock, (unsigned)us, (unsigned)(me_handler_end - (const u32 *)(void *)me_handler), (unsigned)bad,
             (unsigned)r0, kuKernelPeekw(KW(RESETEN)), serve_thread >= 0 ? "on" : "OFF: stalls at the waits only", (unsigned)POLL_US);
    return 1;
}

static const char *const why_name[GPU_SYNC_N] = {"read", "gp0", "gp1", "vblank", "dma", "ge", "other"};

int me_log(char *line, int cap)
{
    if (!on || cap <= 0)
        return 0;
    int n;
    if (dead) {
        n = snprintf(line, (size_t)cap, "-- me: off, no answer in %u ms (%u times, %u in a walk; job %u, err %X): the lists walk on the CPU\n",
                     (unsigned)(ME_TIMEOUT / 1000), (unsigned)n_dead, (unsigned)st.lost, (unsigned)kicked_job, (unsigned)ctl->err);
    } else {
        u32 b = ctl->beat;
        if (!busy) { /* the next ping only when no walk is in flight and the last job is done */
            if (ctl->done == seq) {
                ping_check();
                kick(JOB_PING, seq + 1, 0);
                pings++;
            } else {
                late++;
            }
        }
        u32 wn = 0, wus = 0;
        for (int k = 0; k < GPU_SYNC_N; k++)
            wn += st.waits[k], wus += st.wait_us[k];
        n = snprintf(line, (size_t)cap,
                     "-- me: beat %u, pings %u/%u, late %u, err %X; walks %u (%u on the CPU), kick to done %u ms, headers %u ms, words bad %u;"
                     " waits %u in %u ms (",
                     (unsigned)(b - beat0), (unsigned)pings_ok, (unsigned)pings, (unsigned)late, (unsigned)ctl->err, (unsigned)st.walks,
                     (unsigned)st.cpu_walks, (unsigned)(st.walk_us / 1000), (unsigned)(st.head_us / 1000), (unsigned)st.words_bad,
                     (unsigned)wn, (unsigned)(wus / 1000));
        for (int k = 0; k < GPU_SYNC_N && n < cap; k++)
            n += snprintf(line + n, (size_t)(cap - n), "%s%s %u/%u", k ? ", " : "", why_name[k], (unsigned)st.waits[k],
                          (unsigned)(st.wait_us[k] / 1000));
        if (n < cap)
            n += snprintf(line + n, (size_t)(cap - n), " n/ms); list calls %u, stalls %u, polls %u\n", (unsigned)st.rpcs,
                          (unsigned)st.stalls, (unsigned)st.polls);
        beat0 = b;
    }
    u32 lost = st.lost;
    memset(&st, 0, sizeof st);
    st.lost = lost;
    return n < cap ? n : cap - 1;
}

void me_stop(void)
{
    if (on && !dead)
        me_reset();
    dma_list_hook = NULL;
    gpu_sync_hook = NULL;
    quit = 1;
    if (kick_sema >= 0)
        sceKernelSignalSema(kick_sema, 1);
    on = 0;
}
