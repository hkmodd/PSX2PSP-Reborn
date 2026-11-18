/*
 * me_probe: probe 0 of the ME translator, the main CPU's side (me_probe.h). One run, one log:
 * me_probe.log beside the EBOOT, a line per measurement, written and closed at once, so a crash
 * keeps everything before it. tools/me_probe_verdict.py reads it.
 *
 *   info, mem      the PSP, its clocks and eDRAM, the memory the probe got
 *   ge cal         the GE's time for a draw of the busy list (100 textured, blended full-screen sprites)
 *   wb             the cost of writing back 16 KB of dirty data cache, the GE idle and drawing
 *   C0, C1         the translator on the CPU, the ME in reset; the GE idle, then drawing
 *   me boot        the ME out of reset into me.c; its clock against the CPU's (me calib)
 *   b              the ME reading 1 MB of main RAM: uncached, cached, copied to its eDRAM, and the
 *                  same copy by the DMACplus
 *   M0             the translator on the ME alone
 *   C2, C3, C4     the CPU's translator, the GE drawing, the ME idle, copying at full rate, and
 *                  running the translator itself (M1) beside it
 *   me prof, me edram, me end  what the ME sees at the profiler and past 2 MB of eDRAM; whether it
 *                  still runs at the end (no firmware took it back)
 *
 * Kernel words through kubridge (ARK-4). The ME is put back in reset before the probe exits; the
 * PSP should be restarted after it all the same.
 *
 * The DMACplus transfer follows mcidclan's me-dmacplus-sc2me-then-me2sc (MIT, Copyright (c)
 * 2024-2026 m-c/d; THIRD-PARTY-LICENSES.md): its channel registers, control word and LLI chain.
 */
#include <malloc.h>
#include <stdarg.h>
#include <stdio.h>
#include <string.h>

#include <pspctrl.h>
#include <pspdebug.h>
#include <pspge.h>
#include <pspgu.h>
#include <pspkernel.h>
#include <psppower.h>
#include <pspsysmem.h>
#include <psputils.h>

#include "me_probe.h"

PSP_MODULE_INFO("me_probe", 0, 1, 0);
PSP_MAIN_THREAD_ATTR(PSP_THREAD_ATTR_USER | PSP_THREAD_ATTR_VFPU);
PSP_HEAP_SIZE_KB(-1024);

/* KUBridge (libpspkubridge; ARK-4's NIDs) */
int kuKernelGetModel(void);
unsigned int kuKernelPeekw(void *addr);
void kuKernelPokew(void *addr, unsigned int value);
#define KW(a) ((void *)(a))

#define RESETEN 0xBC10004Cu
#define BUSCLKEN 0xBC100050u
#define ME_RESET 0xBFC00000u
#define PROF(k) (0xBC400000u + 4 * (k))
#define DMA_CH 0xBC800180u /* src, dst, next, ctrl, status */

#define BSRC_BYTES (1024 * 1024)
#define ARENA_BYTES (11 * 1024 * 1024)

static u32 sc_now(void) { return sceKernelGetSystemTimeLow(); }

/* ---- the log ---- */

static char log_path[256];
static int log_lines;

static void out(int screen, const char *fmt, va_list ap)
{
    char b[512];
    int n = vsnprintf(b, sizeof b - 1, fmt, ap);
    if (n < 0)
        return;
    if (n > (int)sizeof b - 2)
        n = sizeof b - 2;
    b[n++] = '\n';
    SceUID fd = sceIoOpen(log_path, PSP_O_WRONLY | PSP_O_CREAT | (log_lines++ ? PSP_O_APPEND : PSP_O_TRUNC), 0777);
    if (fd >= 0) {
        sceIoWrite(fd, b, n);
        sceIoClose(fd);
    }
    if (screen) {
        b[n] = 0;
        pspDebugScreenPrintf("%s", b);
    }
}
/* a line in the log and on the screen */
static void say(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
static void say(const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    out(1, fmt, ap);
    va_end(ap);
}
/* a line in the log only */
static void note(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
static void note(const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    out(0, fmt, ap);
    va_end(ap);
}

/* ---- the ME ---- */

static u8 ctl_mem[128] __attribute__((aligned(64)));
static u32 me_seq;
static int me_on, me_dead;

static void me_reset(void)
{
    if (me_on)
        kuKernelPokew(KW(RESETEN), kuKernelPeekw(KW(RESETEN)) | 4);
    me_on = 0;
}

static void me_kick(u32 job, u32 a0, u32 a1, u32 a2, u32 a3, u32 a4)
{
    me_ctl->job = job, me_ctl->arg[0] = a0, me_ctl->arg[1] = a1, me_ctl->arg[2] = a2, me_ctl->arg[3] = a3, me_ctl->arg[4] = a4;
    me_ctl->stop = 0;
    sceKernelDcacheWritebackInvalidateAll(); /* everything the job reads is in RAM */
    me_ctl->seq = ++me_seq;
}
static int me_busy(void) { return me_ctl->done != me_seq; }
/* 1 when the job ended within us; the ME's writes are then what the CPU reads */
static int me_wait(u32 us)
{
    u32 t = sc_now();
    while (me_busy()) {
        if (sc_now() - t > us) {
            me_dead = 1;
            say("me timeout job=%u seq=%u ack=%u done=%u progress=%u err=%u", (unsigned)me_ctl->job, (unsigned)me_seq,
                (unsigned)me_ctl->ack, (unsigned)me_ctl->done, (unsigned)me_ctl->progress, (unsigned)me_ctl->err);
            return 0;
        }
        sceKernelDelayThread(100);
    }
    sceKernelDcacheWritebackInvalidateAll();
    return 1;
}
static int me_job(u32 job, u32 a0, u32 a1, u32 a2, u32 a3, u32 a4, u32 us)
{
    if (me_dead)
        return 0;
    me_kick(job, a0, a1, a2, a3, a4);
    return me_wait(us);
}

extern char __start__me_section[], __stop__me_section[];

static int me_boot(int unlock)
{
    const u32 *h = (const u32 *)__start__me_section;
    u32 words = (u32)(__stop__me_section - __start__me_section) / 4, bad = 0;
    memset(ctl_mem, 0, sizeof ctl_mem);
    me_ctl = (volatile MeCtl *)(0x40000000u | (u32)ctl_mem);
    sceKernelDcacheWritebackInvalidateAll();
    for (u32 i = 0; i < words; i++)
        kuKernelPokew(KW(ME_RESET + 4 * i), i == 2 ? (u32)unlock : h[i]);
    for (u32 i = 0; i < words; i++)
        bad += kuKernelPeekw(KW(ME_RESET + 4 * i)) != (i == 2 ? (u32)unlock : h[i]);
    u32 r = kuKernelPeekw(KW(RESETEN));
    kuKernelPokew(KW(RESETEN), r | 4);
    kuKernelPokew(KW(RESETEN), r & ~4u);
    me_on = 1;
    u32 t = sc_now(), us;
    while ((us = sc_now() - t) < 500000 && me_ctl->alive != ME_ALIVE)
        sceKernelDelayThread(100);
    int alive = me_ctl->alive == ME_ALIVE;
    say("me boot unlock=%d words=%u readback_bad=%u alive=%d us=%u", unlock, (unsigned)words, (unsigned)bad, alive,
        (unsigned)us);
    if (!alive)
        me_reset();
    return alive;
}

/* ---- the translator's runs ---- */

static const TrRun tr_run[TR_COPIES] = {tr0_run, tr1_run, tr2_run, tr3_run, tr4_run, tr5_run, tr6_run};
static const u32 *dump;
static u32 dump_words;

void cpu_abort(void) __attribute__((noreturn));
void cpu_abort(void)
{
    say("abort on the cpu");
    me_reset();
    sceKernelExitGame();
    for (;;)
        ;
}

static Arena *arena_new(void)
{
    Arena *a = (Arena *)memalign(64, sizeof(Arena) + ARENA_BYTES);
    if (a)
        memset(a, 0, sizeof *a), a->size = ARENA_BYTES;
    return a;
}

/* a Psx as replay_run wants it, in the arena, which starts over */
static Psx *psx_new(Arena *a)
{
    a->used = a->peak = 0;
    Psx *p = (Psx *)arena_calloc(a, 1, sizeof(Psx));
    PsxHost *h = (PsxHost *)arena_calloc(a, 1, sizeof(PsxHost));
    u16 *vram = (u16 *)arena_calloc(a, VRAM_W * VRAM_H, 2);
    u8 *ram = (u8 *)arena_calloc(a, RAM_SIZE, 1);
    if (!p || !h || !vram || !ram)
        return NULL;
    p->host = h, p->gpu.vram = vram, p->ram = ram;
    return p;
}

static void log_run(const char *name, const char *on, int ge, const char *me, const PrRun *r, u32 wall, u32 tail,
                    u32 peak)
{
    char err[64];
    snprintf(err, sizeof err, "%s", r->r.err ? r->r.err : "none");
    for (char *c = err; *c; c++)
        if (*c == ' ')
            *c = '_';
    say("run name=%s on=%s ge=%d me=%s ok=%d frames=%u polys=%u t=%u matched=%u unit=%s wall_us=%u ge_tail_us=%u "
        "peak=%u events=%u err=%s",
        name, on, ge, me, r->ok, (unsigned)r->nf, (unsigned)r->polys, (unsigned)r->t, (unsigned)r->matched,
        strcmp(on, "me") ? "us" : "count", (unsigned)wall, (unsigned)tail, (unsigned)peak, (unsigned)r->r.events, err);
    for (u32 i = 0; i < r->nf && i < PR_FRAMES; i++) {
        const PrFrame *f = &r->f[i];
        note("frame name=%s n=%u frame=%u polys=%u t=%u match=%u h=%08X%08X mem=%u mh=%08X%08X", name, (unsigned)i,
             (unsigned)f->frame, (unsigned)f->polys, (unsigned)f->t, (unsigned)f->match, (unsigned)f->h_hi,
             (unsigned)f->h_lo, (unsigned)f->mem, (unsigned)f->mh_hi, (unsigned)f->mh_lo);
    }
}

/* the CPU profiler (psxrt main.c's): 2 cpu ck, 4 memory stall, 13 i miss, 14 d miss */
static u32 prof[20];
static void prof_start(void)
{
    kuKernelPokew(KW(PROF(0)), 1);
    for (int k = 1; k < 20; k++)
        kuKernelPokew(KW(PROF(k)), 0);
}
static void prof_stop(const char *name)
{
    for (int k = 1; k < 20; k++)
        prof[k] = kuKernelPeekw(KW(PROF(k)));
    char b[400];
    int n = snprintf(b, sizeof b, "prof name=%s", name);
    for (int k = 1; k < 20 && n < (int)sizeof b; k++)
        n += snprintf(b + n, sizeof b - n, " r%d=%u", k, (unsigned)prof[k]);
    note("%s", b);
}

static PrRun cpu_pr;
/* the translator copy `copy` over the dump on the CPU, memory from a; its wall time */
static u32 sc_run(const char *name, int copy, Arena *a)
{
    Psx *psx = psx_new(a);
    memset(&cpu_pr, 0, sizeof cpu_pr);
    if (!psx) {
        say("run name=%s on=cpu alloc_failed=1", name);
        return 0;
    }
    cpu_pr.now = sc_now;
    ReplayHooks h = {NULL, pr_frame, &cpu_pr};
    tr_arena[copy] = a;
    prof_start();
    u32 t = sc_now();
    cpu_pr.t_last = t;
    cpu_pr.ok = tr_run[copy](psx, dump, dump_words, GE_NORMAL, &h, &cpu_pr.r);
    u32 wall = sc_now() - t;
    prof_stop(name);
    return wall;
}

/* ---- the GE, kept busy ---- */

typedef struct {
    u16 u, v;
    u32 color;
    s16 x, y, z, pad;
} Vtx;
#define SPRITES 100
static Vtx vtx[2 * SPRITES] __attribute__((aligned(64)));
static u16 tex[256 * 256] __attribute__((aligned(64)));
static unsigned int glist[256 * 1024] __attribute__((aligned(64)));
#define GLIST_UNCACHED ((void *)(0x40000000u | (u32)glist))
#define GE_DRAWS_MAX 30000u

static void ge_setup(void)
{
    for (int i = 0; i < 256 * 256; i++)
        tex[i] = (u16)(i * 40503u) | (i & 1 ? 0x8000 : 0);
    for (int i = 0; i < SPRITES; i++) {
        Vtx *a = &vtx[2 * i], *b = a + 1;
        a->u = 0, a->v = 0, a->color = 0x80FFFFFFu, a->x = 0, a->y = 0, a->z = 0;
        b->u = 256, b->v = 256, b->color = 0x80FFFFFFu, b->x = 480, b->y = 272, b->z = 0;
    }
    sceKernelDcacheWritebackAll();
    sceGuInit();
    sceGuStart(GU_DIRECT, GLIST_UNCACHED);
    sceGuDrawBuffer(GU_PSM_5650, (void *)0x100000, 512);
    sceGuOffset(2048 - 240, 2048 - 136);
    sceGuViewport(2048, 2048, 480, 272);
    sceGuScissor(0, 0, 480, 272);
    sceGuEnable(GU_SCISSOR_TEST);
    sceGuDisable(GU_DEPTH_TEST);
    sceGuFinish();
    sceGuSync(0, 0);
}

/* queues `draws` draws of SPRITES sprites each; the GE starts at once */
static void ge_busy(u32 draws)
{
    if (draws > GE_DRAWS_MAX)
        draws = GE_DRAWS_MAX;
    sceGuStart(GU_DIRECT, GLIST_UNCACHED);
    sceGuEnable(GU_TEXTURE_2D);
    sceGuTexMode(GU_PSM_5551, 0, 0, 0);
    sceGuTexImage(0, 256, 256, 256, tex);
    sceGuTexFunc(GU_TFX_MODULATE, GU_TCC_RGBA);
    sceGuTexFilter(GU_LINEAR, GU_LINEAR);
    sceGuEnable(GU_BLEND);
    sceGuBlendFunc(GU_ADD, GU_SRC_ALPHA, GU_ONE_MINUS_SRC_ALPHA, 0, 0);
    for (u32 i = 0; i < draws; i++)
        sceGuDrawArray(GU_SPRITES, GU_TEXTURE_16BIT | GU_COLOR_8888 | GU_VERTEX_16BIT | GU_TRANSFORM_2D, 2 * SPRITES, 0,
                       vtx);
    sceGuFinish();
}
/* the wait for the GE to end the busy list: > 0 when it drew all along */
static u32 ge_tail(void)
{
    u32 t = sc_now();
    sceGuSync(0, 0);
    return sc_now() - t;
}
static u32 ge_draw_us = 1000;
static u32 ge_draws_for(u32 us) { return us / ge_draw_us + 2; }

/* ---- e: the data cache's writeback ---- */

static u8 wbuf[16384] __attribute__((aligned(64)));
#define WB_R 2000

static void dirty(u32 r)
{
    for (u32 i = 0; i < sizeof wbuf; i += 64)
        wbuf[i] = (u8)r;
}
static void wb_measure(int ge)
{
    u32 t, d, w16, wall, clean;
    t = sc_now();
    for (u32 r = 0; r < WB_R; r++)
        dirty(r);
    d = sc_now() - t;
    t = sc_now();
    for (u32 r = 0; r < WB_R; r++)
        dirty(r), sceKernelDcacheWritebackRange(wbuf, sizeof wbuf);
    w16 = sc_now() - t;
    t = sc_now();
    for (u32 r = 0; r < WB_R; r++)
        dirty(r), sceKernelDcacheWritebackAll();
    wall = sc_now() - t;
    sceKernelDcacheWritebackRange(wbuf, sizeof wbuf);
    t = sc_now();
    for (u32 r = 0; r < WB_R; r++)
        sceKernelDcacheWritebackRange(wbuf, sizeof wbuf);
    clean = sc_now() - t;
    u32 tail = ge ? ge_tail() : 0;
    say("wb ge=%d r=%u dirty_us=%u wb16k_us=%u wball_us=%u clean_us=%u ge_tail_us=%u", ge, WB_R, (unsigned)d,
        (unsigned)w16, (unsigned)wall, (unsigned)clean, (unsigned)tail);
}

/* ---- b: the DMACplus, main RAM to the ME's eDRAM ---- */

typedef struct {
    u32 src, dst, next, ctrl, status;
} __attribute__((aligned(16))) DmaLli;
#define DMA_CTRL(idst, isrc, mdst, msrc, width, size)                                                                  \
    ((u32)(idst) << 27 | (u32)(isrc) << 26 | (u32)(mdst) << 25 | (u32)(msrc) << 24 | (u32)(width) << 21 |            \
     (u32)(width) << 18 | 1u << 15 | 1u << 12 | (u32)(size))
#define DMA_SC2ME(width, size) DMA_CTRL(0, 0, 1, 0, width, size)
#define DMA_MAX 4095u
static DmaLli lli[24] __attribute__((aligned(64)));

/* the chain for bytes from src to eDRAM offset dst: blocks of 4095 transfers, widths 16 to 2,
 * then the rest a byte at a time; its length */
static u32 dma_chain(u32 src, u32 dst, u32 bytes)
{
    static const u8 width[4] = {16, 8, 4, 2}, code[4] = {4, 3, 2, 1};
    u32 n = 0, off = 0;
    for (int w = 0; w < 4; w++)
        for (u32 block = width[w] * DMA_MAX; bytes - off >= block && n < 24; off += block, n++)
            lli[n] = (DmaLli){src + off, dst + off, 0, DMA_SC2ME(code[w], DMA_MAX), 1};
    if (off < bytes && n < 24)
        lli[n] = (DmaLli){src + off, dst + off, 0, DMA_SC2ME(0, bytes - off), 1}, n++;
    for (u32 i = 0; i + 1 < n; i++)
        lli[i].next = (u32)&lli[i + 1];
    sceKernelDcacheWritebackInvalidateRange(lli, sizeof lli);
    return n;
}

/* ---- main ---- */

static int exit_cb(int a, int b, void *c)
{
    (void)a, (void)b, (void)c;
    me_reset();
    sceKernelExitGame();
    return 0;
}
static int cb_thread(SceSize n, void *a)
{
    (void)n, (void)a;
    sceKernelRegisterExitCallback(sceKernelCreateCallback("exit", exit_cb, NULL));
    sceKernelSleepThreadCB();
    return 0;
}

static u32 sum_words(const u32 *p, u32 words)
{
    u32 s = 0;
    for (u32 i = 0; i < words; i++)
        s += p[i];
    return s;
}

int main(int argc, char **argv)
{
    SceUID th = sceKernelCreateThread("cb", cb_thread, 0x11, 0x1000, 0, NULL);
    if (th >= 0)
        sceKernelStartThread(th, 0, NULL);
    pspDebugScreenInit();
    const char *self = argc > 0 ? argv[0] : "ms0:/PSP/GAME/ME_PROBE/EBOOT.PBP";
    const char *slash = strrchr(self, '/');
    int dl = slash ? (int)(slash - self) + 1 : 0;
    snprintf(log_path, sizeof log_path, "%.*sme_probe.log", dl, self);
    char dump_path[256];
    snprintf(dump_path, sizeof dump_path, "%.*ssheep.gedp", dl, self);

    scePowerSetClockFrequency(333, 333, 166);
    u32 fcr31;
    __asm__ volatile("cfc1 %0, $31" : "=r"(fcr31));
    say("info model=%d fw=%08X cpu=%d bus=%d edram=%u free=%u maxfree=%u fcr31=%08X reseten=%08X busclken=%08X",
        kuKernelGetModel(), (unsigned)sceKernelDevkitVersion(), scePowerGetCpuClockFrequencyInt(),
        scePowerGetBusClockFrequencyInt(), sceGeEdramGetSize(), (unsigned)sceKernelTotalFreeMemSize(),
        (unsigned)sceKernelMaxFreeMemSize(), (unsigned)fcr31, kuKernelPeekw(KW(RESETEN)), kuKernelPeekw(KW(BUSCLKEN)));

    /* memory: the dump, 1 MB to read, an arena for the CPU's runs and one for the ME's */
    SceUID fd = sceIoOpen(dump_path, PSP_O_RDONLY, 0);
    int bytes = fd >= 0 ? (int)sceIoLseek(fd, 0, PSP_SEEK_END) : -1;
    u32 *d = bytes > 0 ? (u32 *)memalign(64, (u32)bytes) : NULL;
    if (d) {
        sceIoLseek(fd, 0, PSP_SEEK_SET);
        if (sceIoRead(fd, d, (u32)bytes) != bytes)
            free(d), d = NULL;
    }
    if (fd >= 0)
        sceIoClose(fd);
    dump = d, dump_words = d ? (u32)bytes / 4 : 0;
    u32 *bsrc = (u32 *)memalign(64, BSRC_BYTES);
    Arena *A = arena_new(), *B = arena_new();
    say("mem dump=%d bsrc=%d arenas=%d arena=%u psx=%u", d ? bytes : -1, bsrc != NULL, !!A + !!B, ARENA_BYTES,
        (unsigned)sizeof(Psx));
    if (!d || !bsrc || !A) {
        say("done missing=1");
        sceKernelDelayThread(10 * 1000 * 1000);
        sceKernelExitGame();
        return 0;
    }
    for (u32 i = 0; i < BSRC_BYTES / 4; i++)
        bsrc[i] = i * 2654435761u;
    u32 bsum = sum_words(bsrc, BSRC_BYTES / 4);
    sceKernelDcacheWritebackInvalidateAll();

    /* the GE's speed */
    ge_setup();
    u32 t = sc_now();
    ge_busy(10);
    ge_tail();
    u32 cal = sc_now() - t; /* the list's build counted in */
    ge_draw_us = cal / 10 ? cal / 10 : 1;
    say("ge cal draws=10 sprites=%d us=%u", SPRITES, (unsigned)cal);

    /* e */
    t = sc_now();
    wb_measure(0);
    ge_busy(ge_draws_for(3 * (sc_now() - t)));
    wb_measure(1);

    /* C0, C1: the ME in reset */
    u32 w0 = sc_run("C0", 0, A);
    log_run("C0", "cpu", 0, "off", &cpu_pr, w0, 0, A->peak);
    u32 busy_us = 4 * (w0 ? w0 : 1000000);
    ge_busy(ge_draws_for(busy_us));
    u32 w = sc_run("C1", 1, A);
    log_run("C1", "cpu", 1, "off", &cpu_pr, w, ge_tail(), A->peak);

    /* a: the ME up, and its clock */
    if (me_boot(0) || me_boot(1)) {
        me_ctl->fcr31 = fcr31;
        me_kick(JOB_CALIB, 0, 0, 0, 0, 0);
        t = sc_now();
        sceKernelDelayThread(200 * 1000);
        me_ctl->stop = 1;
        u32 us = sc_now() - t;
        if (me_wait(1000000))
            say("me calib us=%u count=%u", (unsigned)us, (unsigned)me_ctl->res[0]);
    } else
        me_dead = 1;

    /* b */
    if (me_job(JOB_READ_UNCACHED, BSRC_BYTES / 4, 0x40000000u | (u32)bsrc, 0, 0, 0, 5000000))
        say("b read_unc words=%u count=%u sum_ok=%d", BSRC_BYTES / 4, (unsigned)me_ctl->res[1], me_ctl->res[0] == bsum);
    if (me_job(JOB_READ_CACHED, BSRC_BYTES / 4, (u32)bsrc, 0, 0, 0, 5000000))
        say("b read_cached words=%u count=%u sum_ok=%d", BSRC_BYTES / 4, (unsigned)me_ctl->res[1], me_ctl->res[0] == bsum);
    if (me_job(JOB_COPY, (u32)bsrc, BSRC_BYTES, 0, 0, 0, 5000000))
        say("b copy words=%u count=%u sum_ok=%d", BSRC_BYTES / 4, (unsigned)me_ctl->res[1], me_ctl->res[0] == bsum);
    if (me_job(JOB_FILL, 0, BSRC_BYTES, 0xDEADBEEFu, 0, 0, 5000000)) {
        u32 n = dma_chain((u32)bsrc, 0, BSRC_BYTES);
        u32 st0 = kuKernelPeekw(KW(DMA_CH + 0x10)), st = st0, us = 0;
        if (!st0) {
            kuKernelPokew(KW(DMA_CH + 0x0), lli[0].src);
            kuKernelPokew(KW(DMA_CH + 0x4), lli[0].dst);
            kuKernelPokew(KW(DMA_CH + 0x8), lli[0].next);
            kuKernelPokew(KW(DMA_CH + 0xC), lli[0].ctrl);
            t = sc_now();
            kuKernelPokew(KW(DMA_CH + 0x10), lli[0].status);
            while ((st = kuKernelPeekw(KW(DMA_CH + 0x10))) != 0 && (us = sc_now() - t) < 100000)
                ;
            us = sc_now() - t;
        }
        int ok = me_job(JOB_SUM, 0, BSRC_BYTES, 0, 0, 0, 5000000);
        say("b dma bytes=%u llis=%u st0=%u st=%u us=%u sum_ok=%d", BSRC_BYTES, (unsigned)n, (unsigned)st0, (unsigned)st,
            (unsigned)us, ok && me_ctl->res[0] == bsum);
    }

    /* M0: the translator on the ME alone */
    PrRun *mr = NULL;
    Psx *mp = me_dead ? NULL : psx_new(B ? B : A);
    Arena *MA = B ? B : A;
    if (mp && (mr = (PrRun *)arena_calloc(MA, 1, sizeof(PrRun))) != NULL) {
        tr_arena[5] = MA;
        t = sc_now();
        int ok = me_job(JOB_REPLAY, 5, (u32)mp, (u32)dump, dump_words, (u32)mr, 30000000);
        u32 wall = sc_now() - t;
        if (ok)
            log_run("M0", "me", 0, "replay", mr, wall, 0, MA->peak);
    }

    /* C2: the ME idle */
    ge_busy(ge_draws_for(busy_us));
    w = sc_run("C2", 2, A);
    log_run("C2", "cpu", 1, me_dead ? "dead" : "idle", &cpu_pr, w, ge_tail(), A->peak);

    /* C3: the ME copying 1 MB to its eDRAM, again and again */
    if (!me_dead) {
        ge_busy(ge_draws_for(busy_us));
        me_kick(JOB_STREAM, (u32)bsrc, BSRC_BYTES, 0, 0, 0);
        w = sc_run("C3", 3, A);
        u32 tail = ge_tail();
        me_ctl->stop = 1;
        if (me_wait(5000000)) {
            log_run("C3", "cpu", 1, "stream", &cpu_pr, w, tail, A->peak);
            say("stream passes=%u bytes=%u count=%u", (unsigned)me_ctl->res[0], BSRC_BYTES, (unsigned)me_ctl->res[1]);
        }
    }

    /* C4: the ME running the translator (M1) beside the CPU's */
    if (!me_dead && B) {
        mp = psx_new(B);
        mr = mp ? (PrRun *)arena_calloc(B, 1, sizeof(PrRun)) : NULL;
        if (mr) {
            tr_arena[6] = B;
            ge_busy(ge_draws_for(busy_us));
            me_kick(JOB_REPLAY, 6, (u32)mp, (u32)dump, dump_words, (u32)mr);
            u32 tm = sc_now();
            w = sc_run("C4", 4, A);
            u32 tail = ge_tail();
            int busy = me_busy();
            u32 prog = me_ctl->progress;
            log_run("C4", "cpu", 1, "replay", &cpu_pr, w, tail, A->peak);
            say("c4 me_busy_at_end=%d me_progress=%u", busy, (unsigned)prog);
            if (me_wait(30000000))
                log_run("M1", "me", 1, "replay", mr, sc_now() - tm, 0, B->peak);
        }
    } else if (!B)
        say("c4 skipped=1 arenas=1");

    /* what else the ME sees */
    if (me_job(JOB_EDRAM, 0, 0, 0, 0, 0, 1000000))
        say("me edram a=%08X b=%08X", (unsigned)me_ctl->res[0], (unsigned)me_ctl->res[1]);
    if (me_job(JOB_PROF, 0, 0, 0, 0, 0, 5000000))
        say("me prof en=%u d=%u count=%u", (unsigned)me_ctl->res[0], (unsigned)me_ctl->res[1], (unsigned)me_ctl->res[2]);

    /* the ME still ours at the end */
    if (me_on) {
        u32 b0 = me_ctl->beat;
        sceKernelDelayThread(50 * 1000);
        u32 b1 = me_ctl->beat;
        say("me end alive=%d beat_ok=%d err=%u dead=%d", me_ctl->alive == ME_ALIVE, b1 != b0, (unsigned)me_ctl->err, me_dead);
    }
    me_reset();
    say("me reset reseten=%08X", kuKernelPeekw(KW(RESETEN)));
    say("done");

    pspDebugScreenPrintf("\nme_probe.log written. X or 20 s to exit; then restart the PSP.\n");
    SceCtrlData pad;
    t = sc_now();
    do {
        sceCtrlPeekBufferPositive(&pad, 1);
        sceKernelDelayThread(50 * 1000);
    } while (!(pad.Buttons & PSP_CTRL_CROSS) && sc_now() - t < 20000000);
    sceKernelExitGame();
    return 0;
}
