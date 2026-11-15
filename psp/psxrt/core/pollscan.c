/*
 * psxrt core: may a polling loop's read of a root counter jump more than one tick?
 *
 * Time that jumps forward is time the CPU spent stalled, which the PS1 does too (an interrupt, a
 * DMA). What must not move is when the loop leaves. A loop waiting for an exact value would miss
 * it and wait a whole turn of the counter; a loop comparing a difference with a limit leaves late
 * if the jump passes the limit. So the code that consumes the value is read from guest RAM, from
 * the load on. The value (the counter, x) may pass through copies, sums and differences with
 * registers the path does not write, and through a return (`jr ra`, as PsyQ's GetRCnt does). It
 * must reach a slt/sltu/slti/sltiu against such a value. An affine function of x without 32-bit
 * wrap is monotonic, so that comparison flips at most once before the counter's own wrap, and the
 * jump lands on the flip. After the comparison, on every path, whatever still holds the value must
 * be overwritten before any other read. Anything else: 0, and the caller moves one tick at a time.
 */
#include "psx.h"

#define MAX_STEPS 64 /* instructions read, all paths */
#define MAX_PATHS 8
#define ARGS 0xF0u                /* a0-a3 */
#define CALLER_SAVED 0x0300FFFEu /* at, v0-v1, a0-a3, t0-t9: a call overwrites them */

enum { PLAIN, BRANCH, JUMP, CALL, JR, BAD };

typedef struct {
    Psx *psx;
    const u32 *gpr;
    u32 tm;     /* registers holding s * x + c */
    u32 wm;     /* registers the path wrote: gpr no longer holds their value */
    s32 s[32];  /* 1 or -1 */
    u32 c[32];  /* mod 2^32 */
    int steps;
} Scan;

typedef struct {
    s32 s;
    u32 c, k;   /* s * x + c compared with k */
    int x_left; /* x < k, else k < x */
    int sign;
} Cmp;

static int word(Scan *z, u32 pc, u32 *w)
{
    u32 p = pc & 0x1FFFFFFFu;
    if ((pc & 3) || p >= 0x800000 || ++z->steps > MAX_STEPS)
        return 0;
    const u8 *m = z->psx->ram + (p & (RAM_SIZE - 1));
    *w = m[0] | (u32)m[1] << 8 | (u32)m[2] << 16 | (u32)m[3] << 24;
    return 1;
}

/* The kind of instruction w, the registers it reads (bit mask, not r0) and the one it writes */
static int decode(u32 w, u32 *reads, u32 *wr)
{
    u32 op = w >> 26, rs = w >> 21 & 31, rt = w >> 16 & 31, rd = w >> 11 & 31, f = w & 63;
    u32 both = 1u << rs | 1u << rt;
    int kind = BAD;
    *reads = 0;
    *wr = 0;
    switch (op) {
    case 0x00:
        if (f <= 0x03 && f != 0x01)
            *reads = 1u << rt, *wr = rd, kind = PLAIN; /* sll srl sra */
        else if (f == 0x04 || f == 0x06 || f == 0x07 || (f >= 0x20 && f <= 0x27) || f == 0x2A || f == 0x2B)
            *reads = both, *wr = rd, kind = PLAIN;
        else if (f == 0x08)
            *reads = 1u << rs, kind = JR;
        else if (f == 0x09)
            *reads = 1u << rs, *wr = rd, kind = CALL; /* jalr */
        else if (f == 0x10 || f == 0x12)
            *wr = rd, kind = PLAIN; /* mfhi mflo */
        else if (f == 0x11 || f == 0x13)
            *reads = 1u << rs, kind = PLAIN; /* mthi mtlo */
        else if (f >= 0x18 && f <= 0x1B)
            *reads = both, kind = PLAIN; /* mult div */
        break;
    case 0x01: /* bltz bgez; the linking ones are calls */
        *reads = 1u << rs;
        kind = rt & 0x10 ? BAD : BRANCH;
        break;
    case 0x02: kind = JUMP; break;
    case 0x03: *wr = 31, kind = CALL; break;
    case 0x04: case 0x05: *reads = both, kind = BRANCH; break;
    case 0x06: case 0x07: *reads = 1u << rs, kind = BRANCH; break;
    case 0x0F: *wr = rt, kind = PLAIN; break;
    case 0x10: case 0x12: /* COP0, GTE: moves, or commands that touch no GPR (bc branches: BAD) */
        if (rs == 0 || rs == 2)
            *wr = rt, kind = PLAIN;
        else if (rs == 4 || rs == 6)
            *reads = 1u << rt, kind = PLAIN;
        else if (rs & 0x10)
            kind = PLAIN;
        break;
    case 0x32: case 0x3A: *reads = 1u << rs, kind = PLAIN; break; /* lwc2 swc2 */
    default:
        if (op >= 0x08 && op <= 0x0E)
            *reads = 1u << rs, *wr = rt, kind = PLAIN;
        else if (op >= 0x20 && op <= 0x26) /* loads; lwl lwr merge into rt */
            *reads = 1u << rs | (op == 0x22 || op == 0x26 ? 1u << rt : 0), *wr = rt, kind = PLAIN;
        else if (op >= 0x28 && op <= 0x2E && op != 0x2C && op != 0x2D)
            *reads = both, kind = PLAIN;
    }
    *reads &= ~1u;
    return kind;
}

static void kill(Scan *z, u32 r)
{
    z->tm &= ~(1u << r);
    z->wm |= 1u << r;
}

static void set(Scan *z, u32 r, s32 s, u32 c)
{
    if (!r)
        return;
    z->tm |= 1u << r;
    z->wm |= 1u << r;
    z->s[r] = s;
    z->c[r] = c;
}

static int known(const Scan *z, u32 r, u32 *v)
{
    if (!r) {
        *v = 0;
        return 1;
    }
    if ((z->tm | z->wm) >> r & 1)
        return 0;
    *v = z->gpr[r];
    return 1;
}

/* A plain instruction on the way to the comparison: 0 it goes on, 1 it is the comparison (in q),
 * -1 it does something else with x */
static int flow(Scan *z, u32 w, Cmp *q)
{
    u32 reads, wr, v;
    if (decode(w, &reads, &wr) != PLAIN)
        return -1;
    if (!(reads & z->tm)) {
        if (wr)
            kill(z, wr);
        return 0;
    }
    u32 op = w >> 26, rs = w >> 21 & 31, rt = w >> 16 & 31, f = w & 63, imm = (u32)(s32)(s16)w;
    int a = z->tm >> rs & 1;
    if (op == 0) {
        if (a && (z->tm >> rt & 1))
            return -1;
        u32 xr = a ? rs : rt;
        if (!known(z, a ? rt : rs, &v))
            return -1;
        s32 s = z->s[xr];
        u32 c = z->c[xr];
        switch (f) {
        case 0x25: /* or with r0: a copy */
            if (v)
                return -1;
            /* fall through */
        case 0x20: case 0x21:
            set(z, wr, s, c + v);
            return 0;
        case 0x22: case 0x23:
            if (a)
                set(z, wr, s, c - v);
            else
                set(z, wr, -s, v - c);
            return 0;
        case 0x2A: case 0x2B:
            *q = (Cmp){s, c, v, a, f == 0x2A};
            kill(z, wr);
            return 1;
        }
        return -1;
    }
    switch (op) {
    case 0x08: case 0x09:
        set(z, wr, z->s[rs], z->c[rs] + imm);
        return 0;
    case 0x0A: case 0x0B:
        *q = (Cmp){z->s[rs], z->c[rs], imm, 1, op == 0x0A};
        kill(z, wr);
        return 1;
    }
    return -1;
}

/* After the comparison: on every path, x is overwritten before it is read again */
static int dies(Scan *z, u32 pc)
{
    u32 at[MAX_PATHS], tms[MAX_PATHS], w, d, reads, wr, dr, dw;
    int n = 0;
    at[n] = pc, tms[n++] = z->tm;
    while (n) {
        n--;
        pc = at[n], z->tm = tms[n];
        while (z->tm) {
            if (!word(z, pc, &w))
                return 0;
            int kind = decode(w, &reads, &wr);
            if (kind == BAD || (reads & z->tm))
                return 0;
            if (kind == PLAIN) {
                z->tm &= ~(1u << wr);
                pc += 4;
                continue;
            }
            if (!word(z, pc + 4, &d) || decode(d, &dr, &dw) != PLAIN || (dr & z->tm))
                return 0;
            z->tm &= ~(1u << dw);
            if (kind == CALL) {
                if (z->tm & (ARGS | ~CALLER_SAVED))
                    return 0;
                break;
            }
            if (kind == JR) {
                if (z->tm)
                    return 0;
                break;
            }
            if (!z->tm)
                break;
            if (kind == JUMP) {
                pc = ((pc + 4) & 0xF0000000u) | (w & 0x03FFFFFFu) << 2;
                continue;
            }
            u32 to = pc + 4 + ((u32)(s32)(s16)w << 2);
            if (w >> 16 == 0x1000) { /* b */
                pc = to;
                continue;
            }
            if (n == MAX_PATHS)
                return 0;
            at[n] = to, tms[n++] = z->tm;
            pc += 8;
        }
    }
    return 1;
}

u32 poll_bound(Psx *psx, u32 pc, const u32 *gpr, u32 raw, u32 room)
{
    Scan z = {psx, gpr, 0, 0, {0}, {0}, 0};
    Cmp q;
    u32 w, reads, wr;
    /* an lhu or lw (a zero-extended count), not in a delay slot, not read in its load delay */
    if (!room || !word(&z, pc - 4, &w) || decode(w, &reads, &wr) != PLAIN || !word(&z, pc, &w))
        return 0;
    u32 rt = w >> 16 & 31;
    if (((w >> 26) != 0x25 && (w >> 26) != 0x23) || !rt)
        return 0;
    if (!word(&z, pc + 4, &w) || (decode(w, &reads, &wr), reads >> rt & 1))
        return 0;
    set(&z, rt, 1, 0);
    for (pc += 4;;) {
        if (!word(&z, pc, &w))
            return 0;
        int kind = decode(w, &reads, &wr), r = 0;
        if (kind == JUMP || (kind == JR && (w >> 21 & 31) == 31)) {
            u32 to = ((pc + 4) & 0xF0000000u) | (w & 0x03FFFFFFu) << 2;
            if (kind == JR) {
                if ((z.tm | z.wm) >> 31 & 1)
                    return 0;
                to = gpr[31];
            }
            if (!word(&z, pc + 4, &w) || flow(&z, w, &q) != 0 || !z.tm)
                return 0;
            pc = to;
            continue;
        }
        if (kind != PLAIN || (r = flow(&z, w, &q)) < 0 || (!z.tm && !r))
            return 0;
        pc += 4;
        if (r)
            break;
    }
    if (!dies(&z, pc))
        return 0;
    /* s * (raw + t) + c for t in 0..room, without 32-bit wrap: monotonic, one flip at most */
    u32 v = (u32)q.s * raw + q.c;
    s64 lo = q.sign ? (s64)(s32)v : (s64)v, hi = lo + (s64)q.s * room;
    s64 k = q.sign ? (s64)(s32)q.k : (s64)q.k;
    if (q.sign ? hi < -0x80000000LL || hi > 0x7FFFFFFFLL : hi < 0 || hi > 0xFFFFFFFFLL)
        return 0;
#define HOLDS(x) (q.x_left ? (x) < k : k < (x))
    int c0 = HOLDS(lo);
    if (HOLDS(hi) == c0)
        return room;
    u32 a = 0, b = room;
    while (b - a > 1) {
        u32 m = a + (b - a) / 2;
        if (HOLDS(lo + (s64)q.s * m) == c0)
            a = m;
        else
            b = m;
    }
#undef HOLDS
    return b;
}
