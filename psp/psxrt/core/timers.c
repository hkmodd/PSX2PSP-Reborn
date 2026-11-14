/*
 * psxrt core: root counters 0-2. Counts are derived from the cycle clock when read or due; each
 * timer's next interrupt is scheduled. Clock sources: system clock, dot clock (timer 0), hblank
 * (timer 1), system clock / 8 (timer 2). Hblank/vblank synchronisation modes other than timer 2's
 * stop modes are not used by PsyQ and run free.
 */
#include "psx.h"

/* CPU cycles per tick, 16.16 fixed point */
static u64 cpt(Psx *psx, int i)
{
    u32 src = (psx->tmr.t[i].mode >> 8) & 3;
    if (i == 0 && (src & 1)) {
        static const u32 gpu_div[4] = {10, 8, 5, 4}; /* GPU clocks per dot: 256, 320, 512, 640 */
        u32 div = (psx->gpu.stat & 0x10000) ? 7 : gpu_div[(psx->gpu.stat >> 17) & 3];
        return ((u64)div * 7 << 16) / 11; /* GPU clock is 11/7 of the CPU clock */
    }
    if (i == 1 && (src & 1))
        return (u64)psx_line_cycles(psx) << 16;
    if (i == 2 && (src & 2))
        return 8u << 16;
    return 1u << 16;
}

static int paused(Psx *psx, int i)
{
    u32 m = psx->tmr.t[i].mode;
    return i == 2 && (m & 1) && (((m >> 1) & 3) == 0 || ((m >> 1) & 3) == 3);
}

static u32 period(const Timer *t) { return (t->mode & 8) && t->target ? t->target + 1 : 0x10000; }

/* ticks until the counter next equals x (1..period) */
static u32 ticks_to(const Timer *t, u32 x, u32 per)
{
    u32 d = (x + per - t->count % per) % per;
    return d ? d : per;
}

static void irq(Psx *psx, int i)
{
    Timer *t = &psx->tmr.t[i];
    if (!(t->mode & 0x40) && !(t->mode & 0x400))
        return; /* one-shot already fired */
    if (t->mode & 0x80)
        t->mode ^= 0x400; /* toggle */
    else
        t->mode &= ~0x400u;
    psx_raise(psx, IRQ_TMR0 + i);
    if (t->mode & 0x40 && !(t->mode & 0x80))
        t->mode |= 0x400; /* pulse: back to idle */
}

static void sync(Psx *psx, int i)
{
    Timer *t = &psx->tmr.t[i];
    if (paused(psx, i)) {
        t->base = psx->cycles;
        return;
    }
    u64 c = cpt(psx, i);
    u64 ticks = (((psx->cycles - t->base) << 16) / c);
    if (!ticks)
        return;
    t->base += (ticks * c) >> 16;
    u32 per = period(t);
    int hit_target = ticks >= ticks_to(t, t->target, per) && t->target < per;
    int hit_ffff = per == 0x10000 && ticks >= ticks_to(t, 0xFFFF, per);
    t->count = (u32)((t->count + ticks) % per);
    if (hit_target) {
        t->mode |= 0x800;
        if (t->mode & 0x10)
            irq(psx, i);
    }
    if (hit_ffff) {
        t->mode |= 0x1000;
        if (t->mode & 0x20)
            irq(psx, i);
    }
}

void timers_update(Psx *psx)
{
    for (int i = 0; i < 3; i++)
        sync(psx, i);
}

u64 timers_next(Psx *psx)
{
    u64 best = ~0ull;
    for (int i = 0; i < 3; i++) {
        Timer *t = &psx->tmr.t[i];
        if (paused(psx, i) || !(t->mode & 0x30) || (!(t->mode & 0x40) && !(t->mode & 0x400)))
            continue;
        u32 per = period(t), n = per;
        if ((t->mode & 0x10) && t->target < per)
            n = ticks_to(t, t->target, per);
        if ((t->mode & 0x20) && per == 0x10000) {
            u32 f = ticks_to(t, 0xFFFF, per);
            n = f < n ? f : n;
        }
        u64 due = t->base + (((u64)n * cpt(psx, i) + 0xFFFF) >> 16);
        if (due < best)
            best = due;
    }
    return best;
}

void timers_hblank_mode(Psx *psx) { (void)psx; }

u32 timer_read(Psx *psx, u32 addr)
{
    int i = (addr >> 4) & 3;
    if (i > 2)
        return 0;
    Timer *t = &psx->tmr.t[i];
    sync(psx, i);
    switch (addr & 0xF) {
    case 0: return t->count;
    case 4: {
        u32 m = t->mode;
        t->mode &= ~0x1800u; /* reached bits clear on read */
        return m;
    }
    case 8: return t->target;
    }
    return 0;
}

void timer_write(Psx *psx, u32 addr, u32 v)
{
    int i = (addr >> 4) & 3;
    if (i > 2)
        return;
    Timer *t = &psx->tmr.t[i];
    sync(psx, i);
    switch (addr & 0xF) {
    case 0: t->count = v & 0xFFFF; break;
    case 4:
        t->mode = (v & 0x3FF) | 0x400;
        t->count = 0;
        t->base = psx->cycles;
        break;
    case 8: t->target = v & 0xFFFF; break;
    }
    psx_schedule(psx, timers_next(psx));
}
