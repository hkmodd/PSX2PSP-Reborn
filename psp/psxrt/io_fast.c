/*
 * psxrt: device reads from translated code without the host-call path, through psxrt_io_read
 * (trampolines.S; layout.rs IO_READ). Built like gte_fast.c: the caller-saved registers other
 * than at, v0 and a0 are callee-saved here (Makefile), so the trampoline saves only those, sp,
 * gp, ra and HI/LO. For that to hold nothing here calls other code, nor anything that becomes
 * a library call (no 64-bit division).
 *
 * Served: naturally aligned reads of the pad port (SIO0, core/sio.c), the DMA registers, GPUSTAT
 * and GPUREAD outside a VRAM->CPU transfer, word reads of I_STAT, I_MASK and the root
 * counters' count, mode and target, for a counter on the system clock, timer 2's eighth of it or
 * timer 1's hblanks and not stopped, when no device event is due, no interrupt is pending and the
 * read raises none. What they return and
 * leave behind is what psx_read's would (native_init checks it on random counters, both video
 * modes). Anything else, and a counter read at which native.c poll_skip lets PS1 time pass (a
 * polling loop), goes on as a host call.
 */
#include "native.h"

Psx *io_fast_psx;
u32 io_fast_shift; /* native.c cpu_shift */

#define CTX ((volatile u32 *)0x09A00400u)
#define C_BUDGET (0x90 / 4)
#define C_SITE (0x94 / 4) /* CTX.GTE_RET: the trampoline parks the site there */

/* CPU cycles a count of counter i, as core/timers.c cpt() (psx.c line_cycles() for hblanks);
 * 0 on the dot clock and for timer 2 in a sync mode. */
static u32 tmr_cpt(const Psx *psx, int i)
{
    u32 m = psx->tmr.t[i].mode, src = (m >> 8) & 3;
    if ((i == 0 && (src & 1)) || (i == 2 && (m & 1)))
        return 0;
    if (i == 1 && (src & 1))
        return (psx->gpu.stat >> 20) & 1 ? 2168 : 2153;
    return i == 2 && (src & 2) ? 8 : 1;
}

/* Counter i moved to the clock as core/timers.c sync() moves it, in 32 bits. Nonzero, with
 * nothing changed: 1 for a counter on the dot clock, timer 2 in a sync mode or a counter last
 * moved 2^31 cycles ago or more, 2 for a move that raises an interrupt, 3 for one that reaches
 * an interrupt a one-shot counter has already raised. */
static int tmr_sync(Psx *psx, int i)
{
    Timer *t = &psx->tmr.t[i];
    u32 m = t->mode, k = tmr_cpt(psx, i);
    u64 d = psx->cycles - t->base;
    if (!k || d >> 31)
        return 1;
    u32 ticks = k == 1 ? (u32)d : (u32)d / k;
    if (!ticks)
        return 0;
    u32 per = (m & 8) && t->target ? t->target + 1 : 0x10000, c = t->count % per;
    u32 to_t = (t->target + per - c) % per, to_f = (0xFFFF + per - c) % per;
    int hit_target = ticks >= (to_t ? to_t : per) && t->target < per;
    int hit_ffff = per == 0x10000 && ticks >= (to_f ? to_f : per);
    if ((hit_target && (m & 0x10)) || (hit_ffff && (m & 0x20)))
        return (m & 0x40) || (m & 0x400) ? 2 : 3;
    t->base += ticks * k;
    t->count = (t->count + ticks) % per;
    t->mode = m | (hit_target ? 0x800u : 0) | (hit_ffff ? 0x1000u : 0);
    return 0;
}

/* The word psx_read gives for the device register at physical `p` (word aligned for I_STAT,
 * I_MASK and the counters) and what the read leaves behind, before the size's mask; nonzero
 * (tmr_sync's codes; 1 for GPUREAD in a VRAM->CPU transfer), with nothing changed, if not
 * served. */
int io_fast_dev(Psx *psx, u32 p, u32 *v)
{
    u32 s = p - 0x1F801040;
    if (s < 0x20) { /* core/sio.c sio_read */
        Sio *o = &psx->sio;
        switch (s) {
        case 0x00:
            *v = o->rx_full ? o->rx : 0xFFu;
            o->rx_full = 0;
            break;
        case 0x04: *v = 0x5u | (u32)o->rx_full << 1 | (o->stat & 0x200u); break;
        case 0x08: *v = o->mode; break;
        case 0x0A: *v = o->ctrl; break;
        case 0x0E: *v = o->baud; break;
        case 0x14: *v = 0x5; break;
        default: *v = 0;
        }
        return 0;
    }
    if (p - 0x1F801080 < 0x80) { /* core/dma.c dma_read, shifted as core/mem.c io_read */
        const Dma *d = &psx->dma;
        u32 w = p & ~3u, ch = (w - 0x1F801080) >> 4, r = 0;
        if (w == 0x1F8010F0)
            r = d->dpcr;
        else if (w == 0x1F8010F4)
            r = d->dicr;
        else if (ch <= 6)
            r = (w & 0xC) == 0 ? d->ch[ch].madr : (w & 0xC) == 4 ? d->ch[ch].bcr : (w & 0xC) == 8 ? d->ch[ch].chcr : 0;
        *v = r >> (8 * (p & 3));
        return 0;
    }
    if (p - 0x1F801810 < 8) { /* core/gpu.c gpu_read; GPUREAD of a VRAM->CPU transfer moves pixels */
        const Gpu *g = &psx->gpu;
        if (!(p & 4)) {
            if (g->xfer == 2)
                return 1;
            *v = g->gpuread;
            return 0;
        }
        u32 s = (g->stat | 0x1C000000u) & ~(1u << 25), dir = (s >> 29) & 3;
        if (dir == 1 || dir == 2 || (dir == 3 && g->xfer == 2))
            s |= 1u << 25;
        *v = s;
        return 0;
    }
    if (p == 0x1F801070 || p == 0x1F801074) {
        *v = p == 0x1F801070 ? psx->i_stat : psx->i_mask;
        return 0;
    }
    u32 r = p - 0x1F801100;
    if (r >= 0x30 || (r & 0xF) > 8)
        return 1;
    int no = tmr_sync(psx, (int)(r >> 4));
    if (no)
        return no;
    Timer *t = &psx->tmr.t[r >> 4];
    if ((r & 0xF) == 4) {
        *v = t->mode;
        t->mode &= ~0x1800u; /* reached bits clear on read */
    } else
        *v = (r & 0xF) ? t->target : t->count;
    return 0;
}

static int irq_pending(const Psx *psx)
{
    const Cpu *c = &psx->cpu;
    if (!(c->sr & 1))
        return 0;
    u32 ip = (c->cause & 0x300u) | ((psx->i_stat & psx->i_mask) ? 0x400u : 0);
    return (ip & c->sr & 0x700u) != 0;
}

/* IO_READ with the guest address `a`: 0 when the loaded value is in CTX, else the site goes on
 * as a host call. The cycles of the loops since the last runtime entry are charged first, as
 * every entry does, unless the alarm asked for one. A counter read served is noted in
 * native_poll as poll_skip notes it; one at which poll_skip would move the clock is not served. */
u32 native_io_read(u32 a)
{
    Psx *psx = io_fast_psx;
    volatile u32 *ctx = CTX;
    const u32 *site = (const u32 *)ctx[C_SITE];
    u32 insn = site[1], op = insn >> 26, p = a & 0x1FFFFFFFu, b = ctx[C_BUDGET];
    u32 size = op == 0x23 ? 4 : (op & 1) ? 2 : 1;
    int sio = p - 0x1F801040 < 0x20, dma = p - 0x1F801080 < 0x80, gpu = p - 0x1F801810 < 8;
    if ((p & (size - 1)) || (!sio && !dma && !gpu && ((p & 3) || (p - 0x1F801070 >= 8 && p - 0x1F801100 >= 0x30))) ||
        (s32)b < -(s32)(NATIVE_FORCED / 2))
        return 1;
    psx->cycles += (u32)((s32)NATIVE_BUDGET - (s32)b) >> io_fast_shift;
    ctx[C_BUDGET] = NATIVE_BUDGET;
    int timer = p - 0x1F801100 < 0x30;
    NativePoll q = native_poll;
    u32 v, tick = timer && !(p & 0xF) ? tmr_cpt(psx, (int)((p >> 4) & 3)) : 0;
    int fall = psx->cycles >= psx->next_event ? 0
             : irq_pending(psx)              ? 1
             : timer && native_poll_note(&q, site[0], tick, psx->cycles, psx->next_event) ? 2
                                             : -1;
    if (fall < 0) {
        int no = io_fast_dev(psx, p, &v);
        fall = no ? 2 + no : -1;
    }
    if (fall >= 0) {
        native_stats.io_fall[fall]++;
        return 1;
    }
    if (timer)
        native_poll = q;
    if (op == 0x20)
        v = (u32)(s32)(s8)v;
    else if (op == 0x21)
        v = (u32)(s32)(s16)v;
    else if (op == 0x24)
        v &= 0xFF;
    else if (op == 0x25)
        v &= 0xFFFF;
    ctx[(insn >> 16) & 31] = v;
    psx->cycles += 2;
    native_stats.calls++;
    native_stats.n_io[timer ? IO_TIMER : sio ? IO_PAD : dma ? IO_DMA : gpu ? IO_GPU : IO_IRQ]++;
    native_stats.io_lean++;
    return 0;
}
