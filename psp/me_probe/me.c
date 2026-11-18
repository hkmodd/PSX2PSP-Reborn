/*
 * me_probe: what runs on the Media Engine (me_probe.h). The SC copies me_handler to 0xBFC00000
 * and lets the ME out of reset; the handler turns on the ME's buses and FPU, clears its caches,
 * puts its stack at the top of its eDRAM and jumps to me_main, which runs the SC's jobs one by
 * one from MeCtl. Nothing here calls the firmware.
 *
 * The handler's steps, the cache loops and the profiler-free timing follow mcidclan's Media
 * Engine samples (me-dmacplus-sc2me-then-me2sc, me-simple-count, psp-media-engine-custom-core;
 * the license below). Changed here: the bus clock and RAM size registers are or-ed, not
 * overwritten; the memory protection is opened only when the SC asks (the word at handler + 8).
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
#include <stddef.h>
#include <string.h>

#include "me_probe.h"

void me_main(void) __attribute__((noreturn, used));

__asm__(
    "	.section _me_section,\"ax\",@progbits\n"
    "	.set push\n"
    "	.set noreorder\n"
    "	.set noat\n"
    "	.globl me_handler\n"
    "	.ent me_handler\n"
    "me_handler:\n"
    "	b 1f\n"
    "	nop\n"
    "	.word 0\n" /* + 8: 1 = open the DDR memory protection first */
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
    "3:	li $k0, 0x30000000\n"
    "	mtc0 $k0, $12\n" /* Status: CU0, CU1 (the FPU), kernel mode, interrupts off */
    "	sync\n"
    "	move $k0, $zero\n"
    "	li $k1, 8192\n"
    "4:	cache 0x04, 0($k0)\n" /* instruction cache: every line invalid */
    "	cache 0x04, 0($k0)\n"
    "	cache 0x14, 0($k0)\n" /* data cache: every line written back and invalid */
    "	cache 0x14, 0($k0)\n"
    "	addiu $k0, $k0, 64\n"
    "	bne $k0, $k1, 4b\n"
    "	nop\n"
    "	sync\n"
    "	lui $sp, 0x8020\n" /* the top of 2 MB of eDRAM (every model has 2 MB at least) */
    "	la $gp, _gp\n"
    "	la $k0, me_main\n"
    "	lui $k1, 0x8000\n"
    "	or $k0, $k0, $k1\n"
    "	jr $k0\n"
    "	nop\n"
    "	.end me_handler\n"
    "	.set pop\n"
    "	.previous\n");

volatile MeCtl *me_ctl;
Arena *tr_arena[TR_COPIES];

static inline u32 count(void)
{
    u32 c;
    __asm__ volatile("mfc0 %0, $9" : "=r"(c));
    return c;
}
u32 me_now(void) { return count(); }

static void dcache_wbinv_all(void)
{
    __asm__ volatile("sync");
    for (u32 i = 0; i < 8192; i += 64)
        __asm__ volatile("cache 0x14, 0(%0)\n\tcache 0x14, 0(%0)" : : "r"(i));
    __asm__ volatile("sync");
}

#define EDRAM(off) ((u32 *)(0x80000000u | (off)))          /* the ME's eDRAM, cached */
#define EDRAM_UNCACHED(off) ((volatile u32 *)(0xA0000000u | (off)))

static u32 sum_cached(const u32 *p, u32 words)
{
    u32 s = 0;
    for (u32 i = 0; i < words; i++)
        s += p[i];
    return s;
}

static u32 sum_uncached(const volatile u32 *p, u32 words)
{
    u32 s = 0;
    for (u32 i = 0; i < words; i++)
        s += p[i];
    return s;
}

static void copy(u32 *d, const u32 *s, u32 bytes)
{
    for (u32 i = 0; i < bytes / 4; i += 4) {
        u32 a = s[i], b = s[i + 1], c = s[i + 2], e = s[i + 3];
        d[i] = a, d[i + 1] = b, d[i + 2] = c, d[i + 3] = e;
    }
}

static void job(volatile MeCtl *c)
{
    u32 t;
    switch (c->job) {
    case JOB_READ_UNCACHED:
        t = count();
        c->res[0] = sum_uncached((const volatile u32 *)c->arg[1], c->arg[0]);
        c->res[1] = count() - t;
        break;
    case JOB_READ_CACHED:
        t = count();
        c->res[0] = sum_cached((const u32 *)c->arg[1], c->arg[0]);
        c->res[1] = count() - t;
        break;
    case JOB_FILL:
        for (u32 i = 0; i < c->arg[1] / 4; i++)
            EDRAM(c->arg[0])[i] = c->arg[2];
        break;
    case JOB_COPY:
        t = count();
        copy(EDRAM(c->arg[2]), (const u32 *)c->arg[0], c->arg[1]);
        dcache_wbinv_all();
        c->res[1] = count() - t;
        c->res[0] = sum_cached(EDRAM(c->arg[2]), c->arg[1] / 4);
        break;
    case JOB_SUM:
        c->res[0] = sum_cached(EDRAM(c->arg[0]), c->arg[1] / 4);
        break;
    case JOB_STREAM: {
        u32 passes = 0;
        t = count();
        while (!c->stop) {
            copy(EDRAM(c->arg[2]), (const u32 *)c->arg[0], c->arg[1]);
            c->progress = ++passes;
        }
        c->res[1] = count() - t;
        c->res[0] = passes;
        break;
    }
    case JOB_CALIB:
        t = count();
        while (!c->stop)
            ;
        c->res[0] = count() - t;
        break;
    case JOB_REPLAY: {
        u32 fcr = c->fcr31;
        __asm__ volatile("ctc1 %0, $31" : : "r"(fcr));
        PrRun *run = (PrRun *)c->arg[4];
        run->now = me_now, run->ctl = c, run->t_last = count();
        ReplayHooks h = {NULL, pr_frame, run};
        TrRun fn = c->arg[0] == 6 ? tr6_run : tr5_run;
        run->ok = fn((Psx *)c->arg[1], (const u32 *)c->arg[2], c->arg[3], GE_NORMAL, &h, &run->r);
        c->res[0] = (u32)run->ok;
        break;
    }
    case JOB_PROF: {
        volatile u32 *prof = (volatile u32 *)0xBC400000u;
        u32 p = prof[2];
        t = count();
        for (u32 i = 0; i < 1000000; i++)
            __asm__ volatile("nop");
        c->res[2] = count() - t;
        c->res[1] = prof[2] - p;
        c->res[0] = prof[0];
        break;
    }
    case JOB_EDRAM:
        *EDRAM_UNCACHED(0x100000) = 0x11111111u;
        *EDRAM_UNCACHED(0x300000) = 0x22222222u;
        __asm__ volatile("sync");
        c->res[0] = *EDRAM_UNCACHED(0x100000);
        c->res[1] = *EDRAM_UNCACHED(0x300000);
        break;
    default:
        c->err = 0xBAD70B;
    }
}

void me_main(void)
{
    volatile MeCtl *c = me_ctl;
    c->beat = 0, c->err = 0, c->progress = 0;
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
        dcache_wbinv_all();
        job(c);
        dcache_wbinv_all(); /* everything the job wrote is in RAM before done */
        c->done = seq;
    }
}

/* ---- for the translator's copies (Makefile: their calloc, free, abort and the core's calls) ---- */

void *arena_calloc(Arena *a, u32 n, u32 s)
{
    if (!a || (s && n > 0xFFFFFFFFu / s))
        return NULL;
    u32 bytes = (n * s + 63) & ~63u;
    if (bytes > a->size - a->used)
        return NULL;
    u8 *p = (u8 *)a + sizeof *a + a->used;
    a->used += bytes;
    if (a->used > a->peak)
        a->peak = a->used;
    memset(p, 0, bytes);
    return p;
}

#define TR_ALLOC(n)                                                                                                    \
    void *tr##n##_calloc(size_t c, size_t s);                                                                          \
    void *tr##n##_calloc(size_t c, size_t s) { return arena_calloc(tr_arena[n], (u32)c, (u32)s); }                    \
    void tr##n##_free(void *p);                                                                                        \
    void tr##n##_free(void *p) { (void)p; }
TR_ALLOC(0)
TR_ALLOC(1)
TR_ALLOC(2)
TR_ALLOC(3)
TR_ALLOC(4)
TR_ALLOC(5)
TR_ALLOC(6)

void me_abort(void) __attribute__((noreturn));
void me_abort(void)
{
    me_ctl->err = 0xAB047;
    for (;;)
        ;
}

/* the C library calls with state of their own, on the ME's copies (Makefile ME_TRAPS): err names
 * the call, and the ME stops there */
static void me_trap(u32 k) __attribute__((noreturn));
static void me_trap(u32 k)
{
    me_ctl->err = 0x7A9000u + k;
    for (;;)
        ;
}
#define ME_TRAP(f, k)                                                                                                      void me_trap_##f(void);                                                                                                void me_trap_##f(void) { me_trap(k); }
ME_TRAP(fopen, 1)
ME_TRAP(fwrite, 2)
ME_TRAP(fclose, 3)
ME_TRAP(getenv, 4)
ME_TRAP(snprintf, 5)
ME_TRAP(strtoul, 6)

void pr_log(Psx *psx, const char *fmt, ...);
void pr_log(Psx *psx, const char *fmt, ...) { (void)psx, (void)fmt; }
void pr_raise(Psx *psx, int irq);
void pr_raise(Psx *psx, int irq) { (void)psx, (void)irq; }
void pr_video(Psx *psx);
void pr_video(Psx *psx) { (void)psx; }

void pr_frame(void *user, const ReplayFrame *f)
{
    PrRun *run = (PrRun *)user;
    u32 t = run->now(), dt = t - run->t_last;
    run->t_last = t;
    if (run->nf < PR_FRAMES) {
        PrFrame *o = &run->f[run->nf];
        o->frame = f->got.frame, o->polys = f->polys, o->t = dt, o->match = (u32)f->match;
        o->h_hi = (u32)(f->got.h >> 32), o->h_lo = (u32)f->got.h, o->mem = (u32)f->got.mem;
        o->mh_hi = (u32)(f->got.mh >> 32), o->mh_lo = (u32)f->got.mh;
    }
    if (run->nf++) /* the first frame starts ge.c from cold */
        run->polys += f->polys, run->t += dt, run->matched += (u32)f->match;
    if (run->ctl)
        run->ctl->progress = run->nf;
}
