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

/* 128 bytes, 64-byte aligned, only ever read and written uncached: a line each side writes. Job
 * seq's words are job[seq & 3], arg[seq & 3] and res[seq & 3]: the SC has at most GPU_ME_WALKS
 * jobs in flight, fewer than the 4 slots, so a slot is free again before it is written. */
#define ME_SLOT(s) ((s) & 3u)
typedef struct {
    u32 seq, job[4], arg[4][2], sc_us, rpc_ack, rpc_res;
    u32 alive, beat, ack, done, err, res[4], st_seq, st_id, st_addr, rpc_seq, rpc_op, rpc_arg, me_pad[1];
} MeCtl;

static volatile MeCtl *ctl; /* written by the SC before the boot, only read after */

/* The SPU on the ME (core/psx.h, SpuMe): the line after MeCtl, uncached too. The SC asks for the
 * back (on), publishes how far the CPU is (the beacon: lo before hi, so a torn read is never past
 * it) and asks for a wait (req: the back applied every event and ran up to req_hi:lo); the ME
 * answers on_ack and ack, applies the ring between jobs and mixes up to the beacon, SPU_STEP samples
 * a look, and puts what the SPU gives out in aout (aw), which the SC hands to the host (ar). */
typedef struct {
    u32 on, b_lo, b_hi, req, req_lo, req_hi, ar, wr, sc_pad[8];
    u32 on_ack, ack, aw, rd, drops, steps, me_pad[10];
} SpuCtl;

#define SPU_STEP 64   /* samples the ME mixes between two looks at a job */
#define SPU_RING 4096 /* events */
#define AOUT 4096     /* stereo samples from the ME's SPU to the SC, a power of two */
#define AOUT_CYCLES (368u * 768u) /* the SC hands the ME's audio to the host every half frame or so */
#define UNC(p) ((void *)(0x40000000u | (u32)(p)))

static volatile SpuCtl *spc;
static SpuMe sme; /* the SC writes it; the ME only reads its constants */
static SpuEv spu_ring[SPU_RING] __attribute__((aligned(64))); /* uncached on both sides */
static u32 aout[AOUT] __attribute__((aligned(64)));           /* L | R << 16; uncached on both sides */
static struct {
    Psx psx;
} __attribute__((aligned(64))) sbk; /* the back: only the ME writes it while the SPU is on */
static PsxHost sbk_host;            /* the back's: the audio to aout, nothing else */

/* The SC's: the SPU's back is on (sme in spu_me), it runs on the SC because the ME stopped */
static int spu_on, spu_local;
static u32 spu_req;
static u64 aout_at;
static struct {
    u32 wait_us, wait_max, ev0, drops0, steps0, lost;
} ss;

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

static void me_job(volatile MeCtl *c, u32 k)
{
    switch (c->job[k]) {
    case JOB_PING:
        c->res[k] = c->arg[k][0] + 1;
        break;
    case JOB_WALK:
        c->res[k] = dma_list_walk((Psx *)c->arg[k][0], c->arg[k][1]);
        break;
    default:
        c->err = 0xBAD70B;
    }
}

/* The back's audio (PsxHost.audio, on the ME): into aout; what finds it full is dropped, counted */
static void me_spu_out(void *user, const s16 *s, int n)
{
    (void)user;
    volatile SpuCtl *p = spc;
    volatile u32 *o = UNC(aout);
    u32 w = p->aw, r = p->ar;
    for (int i = 0; i < n; i++, w++) {
        if (w - r >= AOUT && w - (r = p->ar) >= AOUT) {
            p->drops += (u32)(n - i);
            break;
        }
        o[w & (AOUT - 1)] = (u16)s[2 * i] | (u32)(u16)s[2 * i + 1] << 16;
    }
    __asm__ volatile("sync");
    p->aw = w;
}

/* One look at the SPU between jobs: an on or off answered (the caches cleared, so the ME reads
 * what the SC set up and the SC what the ME wrote), a wait answered (every event applied and the
 * mix up to req, all of it in RAM before the ack), or SPU_STEP samples more up to the beacon. The
 * beacon is read before the ring's index, so every event earlier than it is seen. 0: nothing to do. */
static int me_spu_step(void)
{
    volatile SpuCtl *p = spc;
    u32 o = p->on;
    if (o != p->on_ack) {
        me_dcache_wbinv_all();
        p->on_ack = o;
        return 1;
    }
    if (!o)
        return 0;
    u32 q = p->req;
    if (q != p->ack) {
        spu_me_run(&sme, (u64)p->req_hi << 32 | p->req_lo, 0);
        me_dcache_wbinv_all();
        p->ack = q;
        return 1;
    }
    u32 hi, lo;
    do
        hi = p->b_hi, lo = p->b_lo;
    while (p->b_hi != hi);
    __asm__ volatile("sync");
    u64 c0 = sbk.psx.cycles;
    u32 r0 = p->rd;
    spu_me_run(&sme, (u64)hi << 32 | lo, SPU_STEP);
    if (sbk.psx.cycles == c0 && p->rd == r0)
        return 0;
    p->steps++;
    return 1;
}

static void me_main(volatile MeCtl *c) __attribute__((noreturn, used));
static void me_main(volatile MeCtl *c)
{
    c->beat = 0, c->err = 0;
    c->alive = ME_ALIVE;
    for (;;) {
        u32 seq = c->done;
        if (c->seq == seq) { /* the SPU between jobs, or idle: one uncached look every few microseconds */
            if (!me_spu_step()) {
                c->beat++;
                for (u32 i = 0; i < 1000; i++)
                    __asm__ volatile("nop");
            }
            continue;
        }
        seq++; /* the jobs in order: the one after the last done */
        c->ack = seq;
        me_dcache_wbinv_all(); /* no line of the SC's memory older than RAM: a list queued is read now */
        me_job(c, ME_SLOT(seq));
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
static u8 ctl_mem[sizeof(MeCtl) + sizeof(SpuCtl)] __attribute__((aligned(64)));
static int on, dead, mj_init;
static u32 seq, kicked_job, beat0, pings, pings_ok, late, n_dead;
static u32 st_seen, rpc_seen;

/* The walks in flight (gpu_me_busy, core/psx.h: one, or up to GPU_ME_WALKS with lists queued): the
 * game's Psx, each one's list and the words the CPU counted, the first one's job number, when it was
 * kicked and when all were first seen done (0: not yet); the end of the last list kicked, where a
 * queued one starts (gpu_list_scan's). */
static Psx *walk_psx;
static u32 walk_addr[GPU_ME_WALKS], walk_words[GPU_ME_WALKS], walk_seq, t_kick, t_done;
static GpuScan walk_next;

/* The serve thread and the CPU's own thread take turns at serve_once: the serve thread, at a
 * higher priority, never runs inside the other's turn (it sees in_main and waits for its next
 * look); the CPU's thread runs inside the serve thread's turn only while that thread blocks in a
 * list call, and then sees in_serve and looks again. */
static volatile int in_main, in_serve, quit;
static SceUID kick_sema = -1, serve_thread = -1;

static struct {
    u32 walks, cpu_walks, cpu_busy, queued, reaped, walk_us, head_us, words_bad, rpcs, stalls, polls, lost, unscanned, stat_bad;
    u32 waits[GPU_SYNC_N], wait_us[GPU_SYNC_N];
} st;

static u32 now(void) { return sceKernelGetSystemTimeLow(); }

static void me_reset(void) { kuKernelPokew(KW(RESETEN), kuKernelPeekw(KW(RESETEN)) | ME_RESET_BIT); }

static void kick(u32 job, u32 a0, u32 a1)
{
    u32 k = ME_SLOT(seq + 1);
    ctl->job[k] = job, ctl->arg[k][0] = a0, ctl->arg[k][1] = a1;
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
    if (spu_on && !spu_local) { /* the SPU's back on the SC from here, from what the ME left in RAM */
        spu_local = 1, ss.lost++;
        sme.bk_view = &sbk.psx;
        sme.ram_view = sbk.psx.spu.ram;
    }
}

/* A ping's answer, counted once */
static void ping_check(void)
{
    if (kicked_job == JOB_PING)
        pings_ok += ctl->res[ME_SLOT(seq)] == seq + 1;
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

/* The walks done: the ME's writes are in RAM (it wrote its cache back before done) and no line of
 * them stays in the SC's cache; their GPU state, VRAM writes and GPU interrupt go to the game's
 * Psx, then a GP1 04 kept meanwhile. */
static void walk_end(void)
{
    sceKernelDcacheWritebackInvalidateAll();
    Psx *psx = walk_psx;
    psx->gpu = mj.psx.gpu;
    gpu_writes += mj.writes;
    if (mj.psx.i_stat & (1u << IRQ_GPU))
        psx_raise(psx, IRQ_GPU);
    ge_bind(psx);
    for (int w = 0; w < gpu_me_busy && w < GPU_ME_WALKS; w++)
        st.words_bad += ctl->res[ME_SLOT(walk_seq + (u32)w)] != walk_words[w];
    st.walk_us += (t_done ? t_done : now()) - t_kick;
    kicked_job = JOB_NONE;
    gpu_me_busy = 0;
    gpu_me_gp1_done(psx);
    st.stat_bad += gpu_me_stat_ok && (psx->gpu.stat != gpu_me_stat || psx->gpu.xfer);
}

/* The ME stopped in a walk: back in reset, and the walks again on the SC from the GPU state before
 * the first kick (the game's Psx was not written since: a GP1 04 kept waits for the end), then the
 * GP1 04. What the ME had put in the GE's list stays. */
static void walk_lost(void)
{
    me_dead();
    sceKernelDcacheWritebackInvalidateAll();
    Psx *psx = walk_psx;
    ge_bind(psx);
    kicked_job = JOB_NONE;
    int n = gpu_me_busy < GPU_ME_WALKS ? gpu_me_busy : GPU_ME_WALKS;
    gpu_me_busy = 0;
    st.lost++;
    for (int w = 0; w < n; w++)
        dma_list_walk(psx, walk_addr[w]);
    gpu_me_gp1_done(psx);
}

/* gpu_sync_hook: the CPU waits for the walk in flight and serves it meanwhile. Nothing on the ME
 * (ge.c's entries call it there too). */
static void me_sync(int why)
{
    if (geh_me() || !gpu_me_busy)
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
        while (!quit && gpu_me_busy) {
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

/* dma_list_hook: the walk goes to the ME on a copy of the GPU state and the CPU goes on; the GPU's
 * entries (gpu_sync) wait for it. Before the kick the SC's cache goes to RAM, so the ME reads the
 * list and the state as they are now (a list queued too: the ME's cache is cleared before each job).
 * With fewer than GPU_ME_WALKS in flight and the last one's end known to the scan, the list is
 * queued behind them on the same copy, scanned from that end (GPUSTAT with a GP1 04 kept since), and
 * the CPU goes on: the game read GPUSTAT "ready" from that scan, as the PS1's GPU would have been.
 * A full queue, or a list behind one the scan could not follow, waits (FULL, UNSCANNED); walks the
 * ME has done are taken back first, unwaited.
 * The DMA's end is the scan's words from this kick either way (core/dma.c), as on the SC. With a
 * trace on the walk stays on the SC (the ME has no log, and the traces' statics share lines with
 * the disc thread's). */
static u32 me_walk(Psx *psx, u32 addr)
{
    if (gpu_me_busy && ctl->done == seq) /* all done: back without a wait (the last stall applied) */
        me_sync(-1), st.reaped++;
    /* a walk on the SC only after every walk in flight is drained: the same Gpu on both is lost */
    int queue = gpu_me_busy && gpu_me_busy < GPU_ME_WALKS && !dead && !psx->trace && psx == walk_psx && gpu_me_stat_ok &&
                !walk_next.open;
    if (!queue)
        me_sync(gpu_me_busy >= GPU_ME_WALKS ? GPU_SYNC_FULL : GPU_SYNC_UNSCANNED);
    if (psx->trace || dead || (!queue && !me_idle())) {
        st.cpu_walks++, st.cpu_busy += gpu_me_busy != 0; /* 0: the drain above */
        return dma_list_walk(psx, addr);
    }
    u32 t0 = now();
    /* the list's words, so the DMA's time is the same as with the walk on the SC; GPUSTAT at its end */
    GpuScan sc = walk_next;
    if (queue)
        sc.stat = gpu_me_stat;
    else
        gpu_scan_from(psx, &sc);
    u32 words = gpu_list_scan(psx, addr, &sc);
    walk_next = sc, gpu_me_stat = sc.stat, gpu_me_stat_ok = sc.ok;
    st.unscanned += !sc.ok;
    if (!queue) {
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
        walk_psx = psx, walk_seq = seq + 1;
    }
    walk_addr[gpu_me_busy] = addr, walk_words[gpu_me_busy] = words, t_done = 0;
    ctl->sc_us = now();
    sceKernelDcacheWritebackAll();
    kick(JOB_WALK, (u32)&mj.psx, addr);
    gpu_me_head[gpu_me_busy] = addr;
    gpu_me_busy++;
    u32 t1 = now();
    if (!queue)
        t_kick = t1;
    st.walks++, st.queued += queue, st.head_us += t1 - t0;
    if (kick_sema >= 0) /* the serve thread may have seen the first walk done and gone to wait */
        sceKernelSignalSema(kick_sema, 1);
    return words;
}

/* ---- the SPU's back on the ME (SpuMe's kick, the SC's side) ---- */

/* *a == v within ME_TIMEOUT; the ME off (and the SPU's back on the SC) when not */
static void spu_ack_wait(volatile u32 *a, u32 v)
{
    u32 t = now();
    for (u32 i = 0; *a != v; i++)
        if (!(i & 255) && now() - t > ME_TIMEOUT) {
            if (gpu_me_busy) /* the walks again on the SC, and gpu_me_busy cleared */
                walk_lost();
            else
                me_dead();
            return;
        }
}

/* What the back gave out since the last time, to the host's audio */
static void aout_drain(void)
{
    const PsxHost *h = ((Psx *)sme.user)->host;
    u32 r = spc->ar, w = spc->aw;
    while (r != w) {
        u32 i = r & (AOUT - 1), n = w - r;
        if (n > AOUT - i)
            n = AOUT - i;
        if (h->audio)
            h->audio(h->user, (const s16 *)UNC(aout + i), (int)n);
        r += n;
    }
    spc->ar = r;
}

static void spu_wb(const void *p, u32 n) { sceKernelDcacheWritebackRange(p, n); }

static void spu_kick(SpuMe *m, u64 cycle, int why)
{
    volatile SpuCtl *p = spc;
    switch (why) {
    case SPU_ME_BEACON:
        if (!spu_local) {
            p->b_lo = (u32)cycle;
            __asm__ volatile("sync");
            p->b_hi = (u32)(cycle >> 32);
        }
        if (cycle >= aout_at) {
            aout_at = cycle + AOUT_CYCLES;
            if (spu_local)
                spu_me_run(m, cycle, 0);
            aout_drain();
        }
        return;
    case SPU_ME_OFF:
        if (!spu_local) {
            p->on = 0;
            spu_ack_wait(&p->on_ack, 0);
        }
        aout_drain();
        sceKernelDcacheWritebackInvalidateAll(); /* no line of what the back wrote older than RAM */
        spu_on = 0;
        return;
    }
    /* WAIT, FULL: a walk in flight first (the ME runs one job at a time), then the back up to cycle */
    u32 t0 = now();
    me_sync(GPU_SYNC_OTHER);
    if (!spu_local) {
        p->req_lo = (u32)cycle, p->req_hi = (u32)(cycle >> 32);
        __asm__ volatile("sync");
        p->req = ++spu_req;
        spu_ack_wait(&p->ack, spu_req);
    }
    if (spu_local)
        spu_me_run(m, cycle, 0);
    u32 us = now() - t0;
    ss.wait_us += us;
    if (us > ss.wait_max)
        ss.wait_max = us;
}

int me_spu_on(Psx *psx)
{
    if (!on || dead || spu_me)
        return 0;
    SpuMe *m = &sme;
    m->ring = UNC(spu_ring), m->size = SPU_RING;
    m->wr = &spc->wr, m->rd = &spc->rd;
    m->bk = &sbk.psx, m->bk_view = UNC(&sbk.psx);
    m->ram_view = UNC(psx->spu.ram), m->xa_view = UNC(psx->cd.xa_fifo);
    m->kick = spu_kick, m->wb = spu_wb, m->user = psx;
    if (!spu_me_on(psx, m))
        return 0;
    memset(&sbk_host, 0, sizeof sbk_host);
    sbk_host.audio = me_spu_out;
    sbk.psx.host = &sbk_host;
    spc->ar = spc->aw = 0; /* the ME writes aw only while on */
    spc->b_lo = (u32)psx->cycles, spc->b_hi = (u32)(psx->cycles >> 32);
    aout_at = psx->cycles + AOUT_CYCLES;
    spu_on = 1;
    sceKernelDcacheWritebackInvalidateAll(); /* the back, the views' memory and SPU RAM in RAM for the ME */
    spc->on = 1;
    spu_ack_wait(&spc->on_ack, 1);
    return 1;
}

static int boot(u32 unlock, u32 *us, u32 *bad)
{
    const u32 *h = (const u32 *)(void *)me_handler;
    u32 words = (u32)(me_handler_end - h);
    memset(ctl_mem, 0, sizeof ctl_mem);
    ctl = (volatile MeCtl *)(0x40000000u | (u32)ctl_mem);
    spc = (volatile SpuCtl *)(0x40000000u | (u32)(ctl_mem + sizeof(MeCtl)));
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
    gpu_me_keep04 = 1; /* GP1 04 with a walk in flight: kept for the next scan, written after the drain (core/gpu.c) */
    snprintf(msg, (size_t)cap,
             "ME: psxrt's loop runs (unlock %u, %u us, %u handler words, readback bad %u); RESETEN %08X before, %08X now;"
             " the GPU's lists walk there while the CPU goes on (serve thread %s, a look every %u us)",
             (unsigned)unlock, (unsigned)us, (unsigned)(me_handler_end - (const u32 *)(void *)me_handler), (unsigned)bad,
             (unsigned)r0, kuKernelPeekw(KW(RESETEN)), serve_thread >= 0 ? "on" : "OFF: stalls at the waits only", (unsigned)POLL_US);
    return 1;
}

static const char *const why_name[GPU_SYNC_N] = {"read", "gp0", "gp1", "vblank", "dma", "ge", "other", "otc", "full", "unscanned"};

int me_log(char *line, int cap)
{
    if (!on || cap <= 0)
        return 0;
    int n;
    if (dead) {
        n = snprintf(line, (size_t)cap, "-- me: off, no answer in %u ms (%u times, %u in a walk; job %u, err %X): the lists walk on the CPU",
                     (unsigned)(ME_TIMEOUT / 1000), (unsigned)n_dead, (unsigned)st.lost, (unsigned)kicked_job, (unsigned)ctl->err);
    } else {
        u32 b = ctl->beat;
        if (!gpu_me_busy) { /* the next ping only when no walk is in flight and the last job is done */
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
                     "-- me: beat %u, pings %u/%u, late %u, err %X; walks %u (%u on the CPU, %u of them with walks in flight, %u queued, %u reaped), kick to done %u ms,"
                     " headers %u ms, words bad %u, gp1 04 kept %u; waits %u in %u ms (",
                     (unsigned)(b - beat0), (unsigned)pings_ok, (unsigned)pings, (unsigned)late, (unsigned)ctl->err, (unsigned)st.walks,
                     (unsigned)st.cpu_walks, (unsigned)st.cpu_busy, (unsigned)st.queued, (unsigned)st.reaped, (unsigned)(st.walk_us / 1000),
                     (unsigned)(st.head_us / 1000), (unsigned)st.words_bad, (unsigned)gpu_me_gp1_kept, (unsigned)wn, (unsigned)(wus / 1000));
        for (int k = 0; k < GPU_SYNC_N && n < cap; k++)
            n += snprintf(line + n, (size_t)(cap - n), "%s%s %u/%u", k ? ", " : "", why_name[k], (unsigned)st.waits[k],
                          (unsigned)(st.wait_us[k] / 1000));
        if (n < cap)
            n += snprintf(line + n, (size_t)(cap - n), " n/ms); list calls %u, stalls %u, polls %u; otc kept %u synced %u; reads kept %u, lists unscanned %u, stat bad %u",
                          (unsigned)st.rpcs, (unsigned)st.stalls, (unsigned)st.polls, (unsigned)dma_otc_kept, (unsigned)dma_otc_synced,
                          (unsigned)gpu_me_reads_kept, (unsigned)st.unscanned, (unsigned)st.stat_bad);
        dma_otc_kept = dma_otc_synced = gpu_me_reads_kept = gpu_me_gp1_kept = 0;
        beat0 = b;
    }
    if (n < cap && (spu_on || sme.offs || ss.lost)) { /* the SPU's back: the CPU's waits for it, n and time */
        u32 ev = spc->wr, dr = spc->drops, sp = spc->steps;
        n += snprintf(line + n, (size_t)(cap - n),
                      "; spu %s: waits %u (%u full) in %u ms, max %u us, events %u, ME steps %u, out drops %u, IRQ offs %u, lost %u",
                      !spu_on ? "inline" : spu_local ? "on the CPU" : "on the ME", (unsigned)sme.waits, (unsigned)sme.fulls,
                      (unsigned)(ss.wait_us / 1000), (unsigned)ss.wait_max, (unsigned)(ev - ss.ev0), (unsigned)(sp - ss.steps0),
                      (unsigned)(dr - ss.drops0), (unsigned)sme.offs, (unsigned)ss.lost);
        sme.waits = sme.fulls = 0;
        ss.wait_us = ss.wait_max = 0, ss.ev0 = ev, ss.drops0 = dr, ss.steps0 = sp;
    }
    if (n < cap)
        n += snprintf(line + n, (size_t)(cap - n), "\n");
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
