/*
 * psxrt core: the GTE (COP2), bit-exact to the hardware as documented in psx-spx: 44-bit MAC
 * accumulation with per-step overflow flags, the UNR division table, the RTP IR3 flag quirk, the
 * MVMVA far-color bug and the garbage matrix 3.
 */
#include "psx.h"

#define D (psx->gte->d)
#define C (psx->gte->c)

/* data register indices */
enum { RGBC = 6, OTZ = 7, IR0 = 8, SXY0 = 12, SXYP = 15, SZ0 = 16, RGB0 = 20, RES1 = 23, MAC0 = 24, LZCS = 30, LZCR = 31 };
/* control register indices */
enum { RT = 0, TR = 5, LLM = 8, BK = 13, LCM = 16, FC = 21, OFX = 24, OFY = 25, H = 26, DQA = 27, DQB = 28, ZSF3 = 29, ZSF4 = 30, FLAG = 31 };

#define IR(n) D[IR0 + (n)]
#define MAC(n) D[MAC0 + (n)]

static u8 unr[0x101];
static int unr_ready;

static void flag(Psx *psx, int bit) { C[FLAG] |= 1u << bit; }

/* element k (0..8) of the 3x3 matrix whose first control register is `base` */
static s32 mat(Psx *psx, int base, int k)
{
    u32 w = (u32)C[base + k / 2];
    return (k & 1) ? (s16)(w >> 16) : (s16)w;
}

/* Hot paths test the high word only: v fits 44 bits when v >> 32 is in [-0x800, 0x7FF]. */
static s64 mac_chk(Psx *psx, int n, s64 v)
{
    s32 h = (s32)((u64)v >> 32);
    if (__builtin_expect((u32)(h + 0x800) >= 0x1000u, 0)) {
        flag(psx, h < 0 ? 28 - n : 31 - n);
        return (s64)((u64)v << 20) >> 20; /* wraps to 44 bits */
    }
    return v;
}

/* v >> sf for sf 0 or 12, low word: a constant shift instead of a 64-bit variable one */
static s32 shr_sf(s64 v, int sf) { return sf ? (s32)(v >> 12) : (s32)v; }

static s32 ir_clamp(Psx *psx, int n, s32 v, int lm)
{
    s32 lo = lm ? 0 : -0x8000;
    if (v < lo) {
        flag(psx, 25 - n);
        return lo;
    }
    if (v > 0x7FFF) {
        flag(psx, 25 - n);
        return 0x7FFF;
    }
    return v;
}

static s32 ir_sat(Psx *psx, int n, s64 v, int lm)
{
    s64 lo = lm ? 0 : -0x8000;
    if (v < lo) {
        flag(psx, 25 - n);
        return (s32)lo;
    }
    if (v > 0x7FFF) {
        flag(psx, 25 - n);
        return 0x7FFF;
    }
    return (s32)v;
}

static void set_mac_ir(Psx *psx, int n, s64 v, int sf, int lm)
{
    s32 m = shr_sf(v, sf);
    MAC(n) = m;
    IR(n) = ir_clamp(psx, n, m, lm);
}

static s32 mac0_chk(Psx *psx, s64 v)
{
    s32 lo = (s32)v, hi = (s32)((u64)v >> 32);
    if (__builtin_expect(hi != lo >> 31, 0))
        flag(psx, hi < 0 ? 15 : 16);
    return lo;
}

static u32 color_sat(Psx *psx, int n, s32 v)
{
    if (v < 0) {
        flag(psx, 22 - n);
        return 0;
    }
    if (v > 0xFF) {
        flag(psx, 22 - n);
        return 0xFF;
    }
    return (u32)v;
}

static void push_rgb(Psx *psx)
{
    D[RGB0] = D[RGB0 + 1];
    D[RGB0 + 1] = D[RGB0 + 2];
    D[RGB0 + 2] = (s32)(color_sat(psx, 1, MAC(1) >> 4) | color_sat(psx, 2, MAC(2) >> 4) << 8 | color_sat(psx, 3, MAC(3) >> 4) << 16 | ((u32)D[RGBC] & 0xFF000000u));
}

static void push_sz(Psx *psx, s32 v)
{
    if (v < 0) {
        flag(psx, 18);
        v = 0;
    } else if (v > 0xFFFF) {
        flag(psx, 18);
        v = 0xFFFF;
    }
    D[SZ0] = D[SZ0 + 1];
    D[SZ0 + 1] = D[SZ0 + 2];
    D[SZ0 + 2] = D[SZ0 + 3];
    D[SZ0 + 3] = (s32)v;
}

static s32 sxy_sat(Psx *psx, int bit, s32 v)
{
    if (v < -0x400) {
        flag(psx, bit);
        return -0x400;
    }
    if (v > 0x3FF) {
        flag(psx, bit);
        return 0x3FF;
    }
    return (s32)v;
}

static void push_sxy(Psx *psx, s32 x, s32 y)
{
    D[SXY0] = D[SXY0 + 1];
    D[SXY0 + 1] = D[SXY0 + 2];
    D[SXY0 + 2] = (s32)(((u32)x & 0xFFFF) | (u32)y << 16);
}

static u32 divide(Psx *psx, u32 h, u32 sz3)
{
    if (h >= sz3 * 2) {
        flag(psx, 17);
        return 0x1FFFF;
    }
    if (!unr_ready) {
        for (int i = 0; i < 0x101; i++) {
            int v = (0x40000 / (i + 0x100) + 1) / 2 - 0x101;
            unr[i] = (u8)(v < 0 ? 0 : v);
        }
        unr_ready = 1;
    }
    /* sz3 is 1..0xFFFF here (h >= 0 = 2 * 0); h < 2 * sz3, so h << z stays under 0x20000 */
    int z = __builtin_clz(sz3) - 16;
    u32 n = h << z;
    u32 d = sz3 << z;
    u32 u = unr[(d - 0x7FC0) >> 7] + 0x101u;
    d = (0x2000080u - d * u) >> 8;
    d = (0x0000080u + d * u) >> 8;
    u64 r = ((u64)n * d + 0x8000) >> 16;
    return r > 0x1FFFF ? 0x1FFFF : (u32)r;
}

static void vec(Psx *psx, int v, s32 out[3])
{
    if (v == 3) {
        out[0] = IR(1), out[1] = IR(2), out[2] = IR(3);
        return;
    }
    out[0] = (s16)D[v * 2];
    out[1] = (s16)(D[v * 2] >> 16);
    out[2] = (s16)D[v * 2 + 1];
}

static void rtp(Psx *psx, int v, int sf, int lm, int last)
{
    s32 V[3];
    vec(psx, v, V);
    s64 z = 0;
    for (int i = 0; i < 3; i++) {
        s64 a = mac_chk(psx, i + 1, ((s64)C[TR + i] << 12) + (s64)mat(psx, RT, i * 3) * V[0]);
        a = mac_chk(psx, i + 1, a + (s64)mat(psx, RT, i * 3 + 1) * V[1]);
        a = mac_chk(psx, i + 1, a + (s64)mat(psx, RT, i * 3 + 2) * V[2]);
        if (i < 2)
            set_mac_ir(psx, i + 1, a, sf, lm);
        else
            z = a;
    }
    /* IR3: saturated from MAC3, but its flag follows MAC3 >> 12 whatever sf is (z is 44
     * bits, so z >> 12 fits a word) */
    s32 m3 = shr_sf(z, sf), z12 = (s32)(z >> 12);
    MAC(3) = m3;
    if (z12 < -0x8000 || z12 > 0x7FFF)
        flag(psx, 22);
    s32 lo = lm ? 0 : -0x8000;
    IR(3) = m3 < lo ? lo : m3 > 0x7FFF ? 0x7FFF : m3;
    push_sz(psx, z12);
    s32 q = (s32)divide(psx, (u16)C[H], (u32)D[SZ0 + 3]);
    /* q < 2^17, |IR| <= 2^15: one widening multiply each, and the sums stay under 2^33 */
    s64 sx = (s64)q * IR(1) + C[OFX], sy = (s64)q * IR(2) + C[OFY];
    mac0_chk(psx, sx);
    mac0_chk(psx, sy);
    push_sxy(psx, sxy_sat(psx, 14, (s32)(sx >> 16)), sxy_sat(psx, 13, (s32)(sy >> 16)));
    if (last) {
        s64 m = (s64)q * (s16)C[DQA] + C[DQB];
        MAC(0) = mac0_chk(psx, m);
        s32 i0 = (s32)(m >> 12);
        if (i0 < 0) {
            flag(psx, 12);
            i0 = 0;
        } else if (i0 > 0x1000) {
            flag(psx, 12);
            i0 = 0x1000;
        }
        IR(0) = (s32)i0;
    }
}

/* MAC = T * 0x1000 + M * V (T may be NULL), IR = saturate(MAC) */
static void mul(Psx *psx, const s32 M[9], const s32 *T, const s32 V[3], int sf, int lm)
{
    for (int i = 0; i < 3; i++) {
        s64 a = mac_chk(psx, i + 1, (T ? (s64)T[i] << 12 : 0) + (s64)M[i * 3] * V[0]);
        a = mac_chk(psx, i + 1, a + (s64)M[i * 3 + 1] * V[1]);
        a = mac_chk(psx, i + 1, a + (s64)M[i * 3 + 2] * V[2]);
        set_mac_ir(psx, i + 1, a, sf, lm);
    }
}

static void load_mat(Psx *psx, int base, s32 M[9])
{
    for (int k = 0; k < 9; k++)
        M[k] = mat(psx, base, k);
}

static void load_tr(Psx *psx, int base, s32 T[3])
{
    T[0] = C[base], T[1] = C[base + 1], T[2] = C[base + 2];
}

/* [MAC] = in + (FC*0x1000 - in) * IR0, then IR = saturate(MAC, lm) and push the colour */
static void interpolate(Psx *psx, const s64 in[3], int sf, int lm)
{
    for (int i = 0; i < 3; i++)
        set_mac_ir(psx, i + 1, mac_chk(psx, i + 1, ((s64)C[FC + i] << 12) - in[i]), sf, 0);
    for (int i = 0; i < 3; i++)
        set_mac_ir(psx, i + 1, mac_chk(psx, i + 1, (s64)IR(i + 1) * IR(0) + in[i]), sf, lm);
    push_rgb(psx);
}

static void color_from_ir(Psx *psx, s64 out[3])
{
    u32 c = (u32)D[RGBC];
    for (int i = 0; i < 3; i++)
        out[i] = ((s64)((c >> (8 * i)) & 0xFF) * IR(i + 1)) << 4;
}

/* NCS/NCCS/NCDS core for vertex v: IR = LCM * (LLM * V) + BK */
static void light(Psx *psx, int v, int sf, int lm)
{
    s32 M[9], T[3], V[3];
    load_mat(psx, LLM, M);
    vec(psx, v, V);
    mul(psx, M, 0, V, sf, lm);
    load_mat(psx, LCM, M);
    load_tr(psx, BK, T);
    V[0] = IR(1), V[1] = IR(2), V[2] = IR(3);
    mul(psx, M, T, V, sf, lm);
}

static void cc_tail(Psx *psx, int sf, int lm)
{
    s64 m[3];
    color_from_ir(psx, m);
    for (int i = 0; i < 3; i++)
        set_mac_ir(psx, i + 1, mac_chk(psx, i + 1, m[i]), sf, lm);
    push_rgb(psx);
}

static void mvmva(Psx *psx, u32 cmd, int sf, int lm)
{
    int mx = (cmd >> 17) & 3, v = (cmd >> 15) & 3, cv = (cmd >> 13) & 3;
    s32 M[9], T[3], V[3];
    if (mx == 3) {
        s32 r = (s32)(D[RGBC] & 0xFF) << 4;
        M[0] = -r, M[1] = r, M[2] = IR(0);
        M[3] = M[4] = M[5] = mat(psx, RT, 2);
        M[6] = M[7] = M[8] = mat(psx, RT, 4);
    } else
        load_mat(psx, mx == 0 ? RT : mx == 1 ? LLM : LCM, M);
    vec(psx, v, V);
    if (cv == 2) { /* far colour: the first column only reaches the flags */
        load_tr(psx, FC, T);
        for (int i = 0; i < 3; i++) {
            s64 a = mac_chk(psx, i + 1, ((s64)T[i] << 12) + (s64)M[i * 3] * V[0]);
            ir_sat(psx, i + 1, a >> sf, 0);
            a = mac_chk(psx, i + 1, (s64)M[i * 3 + 1] * V[1]);
            a = mac_chk(psx, i + 1, a + (s64)M[i * 3 + 2] * V[2]);
            set_mac_ir(psx, i + 1, a, sf, lm);
        }
        return;
    }
    if (cv == 3)
        mul(psx, M, 0, V, sf, lm);
    else {
        load_tr(psx, cv == 0 ? TR : BK, T);
        mul(psx, M, T, V, sf, lm);
    }
}

void gte_command(Psx *psx, u32 cmd)
{
    int sf = (cmd & (1u << 19)) ? 12 : 0;
    int lm = (cmd >> 10) & 1;
    s64 m[3];
    C[FLAG] = 0;
    switch (cmd & 0x3F) {
    case 0x01: /* RTPS */
        rtp(psx, 0, sf, lm, 1);
        break;
    case 0x30: /* RTPT */
        rtp(psx, 0, sf, lm, 0);
        rtp(psx, 1, sf, lm, 0);
        rtp(psx, 2, sf, lm, 1);
        break;
    case 0x06: { /* NCLIP */
        s32 x0 = (s16)D[SXY0], y0 = (s16)(D[SXY0] >> 16);
        s32 x1 = (s16)D[SXY0 + 1], y1 = (s16)(D[SXY0 + 1] >> 16);
        s32 x2 = (s16)D[SXY0 + 2], y2 = (s16)(D[SXY0 + 2] >> 16);
        MAC(0) = mac0_chk(psx, (s64)x0 * y1 + (s64)x1 * y2 + (s64)x2 * y0 - (s64)x0 * y2 - (s64)x1 * y0 - (s64)x2 * y1);
        break;
    }
    case 0x0C: { /* OP */
        s32 d1 = mat(psx, RT, 0), d2 = mat(psx, RT, 4), d3 = mat(psx, RT, 8);
        set_mac_ir(psx, 1, mac_chk(psx, 1, (s64)IR(3) * d2 - (s64)IR(2) * d3), sf, lm);
        set_mac_ir(psx, 2, mac_chk(psx, 2, (s64)IR(1) * d3 - (s64)IR(3) * d1), sf, lm);
        set_mac_ir(psx, 3, mac_chk(psx, 3, (s64)IR(2) * d1 - (s64)IR(1) * d2), sf, lm);
        break;
    }
    case 0x10: /* DPCS */
    case 0x2A: /* DPCT */
        for (int k = 0; k < ((cmd & 0x3F) == 0x2A ? 3 : 1); k++) {
            u32 c = (u32)D[(cmd & 0x3F) == 0x2A ? RGB0 : RGBC];
            for (int i = 0; i < 3; i++)
                m[i] = (s64)((c >> (8 * i)) & 0xFF) << 16;
            interpolate(psx, m, sf, lm);
        }
        break;
    case 0x11: /* INTPL */
        for (int i = 0; i < 3; i++)
            m[i] = (s64)IR(i + 1) << 12;
        interpolate(psx, m, sf, lm);
        break;
    case 0x12: /* MVMVA */
        mvmva(psx, cmd, sf, lm);
        break;
    case 0x13: /* NCDS */
    case 0x16: /* NCDT */
        for (int v = 0; v < ((cmd & 0x3F) == 0x16 ? 3 : 1); v++) {
            light(psx, v, sf, lm);
            color_from_ir(psx, m);
            interpolate(psx, m, sf, lm);
        }
        break;
    case 0x14: { /* CDP */
        s32 M[9], T[3], V[3] = {IR(1), IR(2), IR(3)};
        load_mat(psx, LCM, M);
        load_tr(psx, BK, T);
        mul(psx, M, T, V, sf, lm);
        color_from_ir(psx, m);
        interpolate(psx, m, sf, lm);
        break;
    }
    case 0x1B: /* NCCS */
    case 0x3F: /* NCCT */
        for (int v = 0; v < ((cmd & 0x3F) == 0x3F ? 3 : 1); v++) {
            light(psx, v, sf, lm);
            cc_tail(psx, sf, lm);
        }
        break;
    case 0x1C: { /* CC */
        s32 M[9], T[3], V[3] = {IR(1), IR(2), IR(3)};
        load_mat(psx, LCM, M);
        load_tr(psx, BK, T);
        mul(psx, M, T, V, sf, lm);
        cc_tail(psx, sf, lm);
        break;
    }
    case 0x1E: /* NCS */
    case 0x20: /* NCT */
        for (int v = 0; v < ((cmd & 0x3F) == 0x20 ? 3 : 1); v++) {
            light(psx, v, sf, lm);
            push_rgb(psx);
        }
        break;
    case 0x28: /* SQR */
        for (int i = 0; i < 3; i++)
            set_mac_ir(psx, i + 1, mac_chk(psx, i + 1, (s64)IR(i + 1) * IR(i + 1)), sf, lm);
        break;
    case 0x29: /* DCPL */
        color_from_ir(psx, m);
        interpolate(psx, m, sf, lm);
        break;
    case 0x2D: /* AVSZ3 */
    case 0x2E: { /* AVSZ4 */
        /* SZ are 16-bit: the sum fits a word, the product 34 bits */
        s64 sum = (cmd & 0x3F) == 0x2D ? (s64)(s16)C[ZSF3] * (s32)(D[SZ0 + 1] + D[SZ0 + 2] + D[SZ0 + 3])
                                       : (s64)(s16)C[ZSF4] * (s32)(D[SZ0] + D[SZ0 + 1] + D[SZ0 + 2] + D[SZ0 + 3]);
        MAC(0) = mac0_chk(psx, sum);
        s32 o = (s32)(sum >> 12);
        if (o < 0) {
            flag(psx, 18);
            o = 0;
        } else if (o > 0xFFFF) {
            flag(psx, 18);
            o = 0xFFFF;
        }
        D[OTZ] = (s32)o;
        break;
    }
    case 0x3D: /* GPF */
        for (int i = 0; i < 3; i++)
            set_mac_ir(psx, i + 1, mac_chk(psx, i + 1, (s64)IR(0) * IR(i + 1)), sf, lm);
        push_rgb(psx);
        break;
    case 0x3E: /* GPL */
        for (int i = 0; i < 3; i++)
            set_mac_ir(psx, i + 1, mac_chk(psx, i + 1, ((s64)MAC(i + 1) << sf) + (s64)IR(0) * IR(i + 1)), sf, lm);
        push_rgb(psx);
        break;
    default:
        break;
    }
    if (C[FLAG] & 0x7F87E000)
        C[FLAG] |= (s32)0x80000000u;
}

static u32 lzc(u32 v)
{
    if (v & 0x80000000u)
        v = ~v;
    u32 n = 0;
    while (n < 32 && !(v & (0x80000000u >> n)))
        n++;
    return n;
}

static s32 rgb_from_ir(Psx *psx)
{
    u32 out = 0;
    for (int i = 0; i < 3; i++) {
        s32 c = IR(i + 1) >> 7;
        c = c < 0 ? 0 : c > 0x1F ? 0x1F : c;
        out |= (u32)c << (5 * i);
    }
    return (s32)out;
}

u32 gte_read_data(Psx *psx, int r)
{
    switch (r) {
    case 15: return (u32)D[SXY0 + 2];
    case 28: case 29: return (u32)rgb_from_ir(psx);
    default: return (u32)D[r];
    }
}

void gte_write_data(Psx *psx, int r, u32 v)
{
    switch (r) {
    case 1: case 3: case 5: case 8: case 9: case 10: case 11:
        D[r] = (s16)v;
        break;
    case 7: case 16: case 17: case 18: case 19:
        D[r] = (s32)(v & 0xFFFF);
        break;
    case 15:
        D[SXY0] = D[SXY0 + 1];
        D[SXY0 + 1] = D[SXY0 + 2];
        D[SXY0 + 2] = (s32)v;
        break;
    case 28:
        IR(1) = (s32)((v & 0x1F) << 7);
        IR(2) = (s32)(((v >> 5) & 0x1F) << 7);
        IR(3) = (s32)(((v >> 10) & 0x1F) << 7);
        break;
    case 29: case 31:
        break;
    case 30:
        D[LZCS] = (s32)v;
        D[LZCR] = (s32)lzc(v);
        break;
    default:
        D[r] = (s32)v;
    }
}

u32 gte_read_ctrl(Psx *psx, int r) { return (u32)C[r]; }

void gte_write_ctrl(Psx *psx, int r, u32 v)
{
    switch (r) {
    case 4: case 12: case 20: case 26: case 27: case 29: case 30:
        C[r] = (s16)v; /* H reads back sign-extended: a hardware quirk */
        break;
    case 31:
        v &= 0x7FFFF000u;
        if (v & 0x7F87E000u)
            v |= 0x80000000u;
        C[r] = (s32)v;
        break;
    default:
        C[r] = (s32)v;
    }
}
