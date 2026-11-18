/*
 * psxrt core: the MDEC (psx-spx "Macroblock Decoder"). A decode command's parameter words are kept
 * whole; macroblocks are decoded one at a time as the output is read (DMA1), which is how the
 * games pace it. Run-length decoding, the IDCT and the colour conversion follow the hardware's
 * fixed-point arithmetic.
 */
#include "psx.h"

#include <string.h>

/* zagzig[scan index] = raster position: the inverse of psx-spx's zigzag[] table. */
static const u8 zagzig[64] = {
    0,  1,  8,  16, 9,  2,  3,  10, 17, 24, 32, 25, 18, 11, 4,  5,  12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6,  7,  14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
};

static s32 sext10(u32 v) { return (s32)(v << 22) >> 22; }

static int halfword(Mdec *m, u16 *h)
{
    if (m->hpos >= m->nin * 2)
        return 0;
    u32 w = m->in[m->hpos >> 1];
    *h = (u16)(m->hpos & 1 ? w >> 16 : w);
    m->hpos++;
    return 1;
}

/* Two matrix passes. A coefficient is within +-0x400 and a scale entry within +-0x8000, so a
 * first-pass sum stays below 2^28: exact in 32 bits, and the second pass is 32x32->64 multiply-adds
 * (one MADD each on the PSP). Columns of zero coefficients, most of them in a video, are skipped:
 * their first-pass results are zero. */
static void idct(const Mdec *m, s16 *blk)
{
    s32 tmp[64];
    int cols[8], nc = 0;
    for (int x = 0; x < 8; x++) {
        int any = 0;
        for (int u = 0; u < 8; u++)
            any |= blk[u * 8 + x];
        if (!any)
            continue;
        cols[nc++] = x;
        for (int y = 0; y < 8; y++) {
            s32 sum = 0;
            for (int u = 0; u < 8; u++)
                sum += blk[u * 8 + x] * m->scale[u * 8 + y];
            tmp[x + y * 8] = sum;
        }
    }
    for (int x = 0; x < 8; x++)
        for (int y = 0; y < 8; y++) {
            s64 sum = 0;
            for (int i = 0; i < nc; i++)
                sum += (s64)tmp[cols[i] + y * 8] * m->scale[cols[i] * 8 + x];
            s32 v = (s32)((sum >> 32) + ((sum >> 31) & 1));
            v = (s32)((u32)v << 23) >> 23; /* 9-bit result */
            blk[x + y * 8] = (s16)(v < -128 ? -128 : v > 127 ? 127 : v);
        }
}

/* The standard scale table is 0x8000 * c(u) * cos((2y+1)u pi/16), c(0) = 1/sqrt2: the two passes
 * above then compute the textbook 2-D IDCT, f = 1/4 sum c(u) c(v) F cos cos. With that table the
 * Loeffler-Ligtenberg-Moschytz factorisation does the same in 12 multiplies per 1-D pass instead
 * of 64 (13-bit constants, results within +-1 of the exact passes). */
static int standard_scale(const s16 *s)
{
    static const s32 c[8] = {32768, 32138, 30274, 27245, 23170, 18204, 12539, 6392}; /* 0x8000 cos(k pi/16) */
    for (int u = 0; u < 8; u++)
        for (int y = 0; y < 8; y++) {
            s32 want;
            if (!u)
                want = c[4];
            else {
                int k = ((2 * y + 1) * u) & 31;
                if (k > 16)
                    k = 32 - k;
                want = k > 8 ? -c[16 - k] : c[k];
            }
            s32 d = s[u * 8 + y] - want;
            if (d < -2 || d > 2)
                return 0;
        }
    return 1;
}

#define CB 13 /* constant bits */
#define P1 2  /* extra precision bits between the passes */
#define F(x) ((s32)((x) * (1 << CB) + 0.5))
#define DESCALE(x, n) (((x) + (1 << ((n) - 1))) >> (n))

/* One 1-D IDCT over in[0], in[st], .. in[7 st]; outputs before the final descale. */
#define IDCT_1D(i0, i1, i2, i3, i4, i5, i6, i7)                                                     \
    s32 z1 = ((i2) + (i6)) * F(0.541196100);                                                          \
    s32 e2 = z1 - (i6) * F(1.847759065), e3 = z1 + (i2) * F(0.765366865);                             \
    s32 e0 = ((i0) + (i4)) * (1 << CB), e1 = ((i0) - (i4)) * (1 << CB);                               \
    s32 t10 = e0 + e3, t13 = e0 - e3, t11 = e1 + e2, t12 = e1 - e2;                                   \
    s32 o0 = (i7), o1 = (i5), o2 = (i3), o3 = (i1);                                                   \
    s32 a1 = o0 + o3, a2 = o1 + o2, a3 = o0 + o2, a4 = o1 + o3, a5 = (a3 + a4) * F(1.175875602);      \
    o0 *= F(0.298631336), o1 *= F(2.053119869), o2 *= F(3.072711026), o3 *= F(1.501321110);           \
    a1 *= -F(0.899976223), a2 *= -F(2.562915447), a3 = a3 * -F(1.961570560) + a5,                     \
        a4 = a4 * -F(0.390180644) + a5;                                                               \
    o0 += a1 + a3, o1 += a2 + a4, o2 += a2 + a3, o3 += a1 + a4

static void idct_fast(s16 *blk)
{
    s32 ws[64];
    for (int x = 0; x < 8; x++) { /* columns: frequency u down blk, results down ws */
        const s16 *in = blk + x;
        s32 *o = ws + x;
        if (!(in[8] | in[16] | in[24] | in[32] | in[40] | in[48] | in[56])) {
            s32 dc = in[0] * (1 << P1);
            for (int y = 0; y < 8; y++)
                o[y * 8] = dc;
            continue;
        }
        IDCT_1D(in[0], in[8], in[16], in[24], in[32], in[40], in[48], in[56]);
        o[0] = DESCALE(t10 + o3, CB - P1), o[56] = DESCALE(t10 - o3, CB - P1);
        o[8] = DESCALE(t11 + o2, CB - P1), o[48] = DESCALE(t11 - o2, CB - P1);
        o[16] = DESCALE(t12 + o1, CB - P1), o[40] = DESCALE(t12 - o1, CB - P1);
        o[24] = DESCALE(t13 + o0, CB - P1), o[32] = DESCALE(t13 - o0, CB - P1);
    }
    for (int y = 0; y < 8; y++) { /* rows */
        const s32 *in = ws + y * 8;
        s16 *o = blk + y * 8;
        s32 r[8];
        if (!(in[1] | in[2] | in[3] | in[4] | in[5] | in[6] | in[7])) {
            s32 dc = DESCALE(in[0], P1 + 3);
            for (int x = 0; x < 8; x++)
                r[x] = dc;
        } else {
            IDCT_1D(in[0], in[1], in[2], in[3], in[4], in[5], in[6], in[7]);
            const int n = CB + P1 + 3;
            r[0] = DESCALE(t10 + o3, n), r[7] = DESCALE(t10 - o3, n);
            r[1] = DESCALE(t11 + o2, n), r[6] = DESCALE(t11 - o2, n);
            r[2] = DESCALE(t12 + o1, n), r[5] = DESCALE(t12 - o1, n);
            r[3] = DESCALE(t13 + o0, n), r[4] = DESCALE(t13 - o0, n);
        }
        for (int x = 0; x < 8; x++) {
            s32 v = (s32)((u32)r[x] << 23) >> 23; /* 9-bit result, as the exact passes */
            o[x] = (s16)(v < -128 ? -128 : v > 127 ? 127 : v);
        }
    }
}

/* Run-length decodes one block; 0 when the input ran out. */
static int rle(Mdec *m, s16 *blk, const u8 *qt)
{
    u16 n;
    memset(blk, 0, 64 * sizeof *blk);
    do {
        if (!halfword(m, &n))
            return 0;
    } while (n == 0xFE00);
    s32 q = (n >> 10) & 0x3F, k = 0, val = sext10(n) * qt[0];
    for (;;) {
        if (q == 0)
            val = sext10(n) * 2;
        val = val < -0x400 ? -0x400 : val > 0x3FF ? 0x3FF : val;
        if (q)
            blk[zagzig[k]] = (s16)val;
        else
            blk[k] = (s16)val;
        if (!halfword(m, &n))
            break;
        k += ((n >> 10) & 0x3F) + 1;
        if (k > 63)
            break;
        val = (sext10(n) * qt[k] * q + 4) / 8;
    }
    return 1;
}

/* Decodes one block; 0 when the input ran out. */
static int block(Mdec *m, s16 *blk, const u8 *qt)
{
    if (!rle(m, blk, qt))
        return 0;
    if (m->std_scale)
        idct_fast(blk);
    else
        idct(m, blk);
    return 1;
}

static u8 clamp8(s32 v) { return (u8)(v < -128 ? -128 : v > 127 ? 127 : v); }

/* clamp_tab[sign][512 + v]: v clamped to a signed byte, offset to unsigned when !sign. Y + the
 * chroma terms stay within +-360. */
static u8 clamp_tab[2][1024];

static void clamp_init(void)
{
    if (clamp_tab[1][0]) /* 0x80 once built */
        return;
    for (int v = -512; v < 512; v++) {
        u8 c = clamp8(v);
        clamp_tab[1][512 + v] = c, clamp_tab[0][512 + v] = c ^ 0x80;
    }
}

/* One 8x8 luma block of a colour macroblock at (xx, yy): each 2x2 shares one chroma sample.
 * Inlined twice, once per output depth. */
#ifdef _MSC_VER
#define INLINE static __forceinline
#else
#define INLINE static inline __attribute__((always_inline))
#endif
INLINE void yuv(Mdec *m, const s16 *cr, const s16 *cb, const s16 *y, int xx, int yy, int depth)
{
    const u8 *cl = clamp_tab[m->sign] + 512;
    u16 hi = m->bit15 ? 0x8000 : 0;
    for (int py = 0; py < 8; py += 2)
        for (int px = 0; px < 8; px += 2) {
            int c = ((px + xx) >> 1) + ((py + yy) >> 1) * 8;
            s32 r = cr[c], b = cb[c];
            s32 G = (-88 * b - 183 * r) >> 8, R = (359 * r) >> 8, B = (454 * b) >> 8;
            for (int k = 0; k < 4; k++) {
                int ox = xx + px + (k & 1), oy = yy + py + (k >> 1);
                s32 Y = y[(px + (k & 1)) + (py + (k >> 1)) * 8];
                u8 rr = cl[Y + R], gg = cl[Y + G], bb = cl[Y + B];
                if (depth == 2) {
                    u8 *o = (u8 *)m->out + (oy * 16 + ox) * 3;
                    o[0] = rr, o[1] = gg, o[2] = bb;
                } else
                    ((u16 *)m->out)[oy * 16 + ox] = (u16)((rr >> 3) | (gg >> 3) << 5 | (bb >> 3) << 10 | hi);
            }
        }
}

#ifdef __psp__
/*
 * The VFPU path for colour macroblocks. The IDCT out = S^T B S (S = scale / 2^16) is two 8x8
 * matrix products, each eight vmmul.q on 4x4 quadrants. lv.q loads four floats into a column, so
 * a row-major matrix arrives transposed, and the registers compute out^T = S^T B^T S, which
 * transposes back on the store. The luma blocks' x is permuted to 0 2 4 6 1 3 5 7: an output row
 * is then an even and an odd quad, each pairing with the same four chroma samples.
 *
 * The colour step works on the hardware's integers held in floats: IDCT results rounded and
 * clamped to a signed byte, chroma terms k/256 exact, + 128 for an unsigned byte. vf2iz by 2^23
 * truncates and saturates at 2^31-1, and vi2uc takes bits 30-23 with negatives at 0, which clamps
 * the sum to 0-255 for free. vi2uc on a matrix row packs one pixel's R, G, B (and a zero byte),
 * into a column that holds four pixels in order: two sv.q a row.
 *
 * Within +-1 of the exact integer passes. A result past +-256, which the hardware wraps to 9 bits,
 * saturates here; no real stream reaches it. The first colour macroblock checks the path against
 * the C one and keeps C if any byte differs by more than 1.
 */
#define A64 __attribute__((aligned(64)))
static float vf_left[2][64] A64, vf_right[64] A64; /* quadrant-blocked: S^T (chroma), S_p^T (luma); S */
static s32 vf_coef[6][64] A64; /* coefficients, row-major */
static float vf_pix[6][64] A64, vf_tmp[64] A64;
static s32 vf_qtq[2][64]; /* quant table times the block scale, scan order: luma, chroma */
static int vf_qtq_q[2] = {-1, -1};
static float vf_term[3][64] A64; /* chroma terms + 128: R, G, B */
static u32 vf_rgba[256] A64;     /* the macroblock, R G B 0 bytes per pixel */
static const float vf_k[16] A64 = {127, 127, 127, 127, -128, -128, -128, -128, 359 / 256.0f, -88 / 256.0f, -183 / 256.0f, 454 / 256.0f, 128, 128, 128, 128};
static s16 vf_from[64]; /* the scale table the factors were built from */
static int vf_state;    /* 0 unchecked, 1 in use, -1 refused by the check */
static int vf_stale = 1; /* the scale table may have changed: compare with vf_from */

/* Register element (i, j) of an 8x8 matrix: quadrant, then column, then row. */
static void vf_put(float *q, int i, int j, float v) { q[((i >> 2) * 2 + (j >> 2)) * 16 + (j & 3) * 4 + (i & 3)] = v; }

static void vf_build(const s16 *sc)
{
    static const u8 perm[8] = {0, 2, 4, 6, 1, 3, 5, 7};
    for (int i = 0; i < 8; i++)
        for (int j = 0; j < 8; j++) {
            vf_put(vf_left[0], i, j, sc[j * 8 + i] / 65536.0f);
            vf_put(vf_left[1], i, j, sc[j * 8 + perm[i]] / 65536.0f);
            vf_put(vf_right, i, j, sc[i * 8 + j] / 65536.0f);
        }
    memcpy(vf_from, sc, sizeof vf_from);
}

#define VL4(M, B, R) "lv.q C" M "00, " B "+0(%" R ")\n lv.q C" M "10, " B "+16(%" R ")\n lv.q C" M "20, " B "+32(%" R ")\n lv.q C" M "30, " B "+48(%" R ")\n"
#define VLR(M, B, R) "lv.q C" M "00, " B "+0(%" R ")\n lv.q C" M "10, " B "+32(%" R ")\n lv.q C" M "20, " B "+64(%" R ")\n lv.q C" M "30, " B "+96(%" R ")\n"
#define VS4(M, B, R) "sv.q C" M "00, " B "+0(%" R ")\n sv.q C" M "10, " B "+16(%" R ")\n sv.q C" M "20, " B "+32(%" R ")\n sv.q C" M "30, " B "+48(%" R ")\n"
#define VSR(M, B, R) "sv.q C" M "00, " B "+0(%" R ")\n sv.q C" M "10, " B "+32(%" R ")\n sv.q C" M "20, " B "+64(%" R ")\n sv.q C" M "30, " B "+96(%" R ")\n"
#define VADD4 "vadd.q C600, C600, C700\n vadd.q C610, C610, C710\n vadd.q C620, C620, C720\n vadd.q C630, C630, C730\n"
#define VI2F(M) "vi2f.q C" M "00, C" M "00, 0\n vi2f.q C" M "10, C" M "10, 0\n vi2f.q C" M "20, C" M "20, 0\n vi2f.q C" M "30, C" M "30, 0\n"
#define VZ8(R, B) "sv.q C000, " B "+0(%" R ")\n sv.q C000, " B "+16(%" R ")\n sv.q C000, " B "+32(%" R ")\n sv.q C000, " B "+48(%" R ")\n" \
    "sv.q C000, " B "+64(%" R ")\n sv.q C000, " B "+80(%" R ")\n sv.q C000, " B "+96(%" R ")\n sv.q C000, " B "+112(%" R ")\n"

/* out (row-major floats, unrounded) = IDCT of coef (row-major ints) with left factor L. */
static void vf_idct(const s32 *coef, const float *L, float *out)
{
    __asm__ volatile(
        VL4("0", "0", "1") VL4("1", "64", "1") VL4("2", "128", "1") VL4("3", "192", "1")
        /* P = L X, X = B^T, one column of quadrants at a time */
        VLR("4", "0", "0") VLR("5", "16", "0") VI2F("4") VI2F("5")
        "vmmul.q M600, M000, M400\n vmmul.q M700, M100, M500\n" VADD4 VS4("6", "0", "3")
        "vmmul.q M600, M200, M400\n vmmul.q M700, M300, M500\n" VADD4 VS4("6", "128", "3")
        VLR("4", "128", "0") VLR("5", "144", "0") VI2F("4") VI2F("5")
        "vmmul.q M600, M000, M400\n vmmul.q M700, M100, M500\n" VADD4 VS4("6", "64", "3")
        "vmmul.q M600, M200, M400\n vmmul.q M700, M300, M500\n" VADD4 VS4("6", "192", "3")
        /* Q = P S, one row of quadrants at a time; Q = out^T stores back as rows */
        VL4("0", "0", "2") VL4("1", "64", "2") VL4("2", "128", "2") VL4("3", "192", "2")
        VL4("4", "0", "3") VL4("5", "64", "3")
        "vmmul.q M600, M400, M000\n vmmul.q M700, M500, M200\n" VADD4 VSR("6", "0", "4")
        "vmmul.q M600, M400, M100\n vmmul.q M700, M500, M300\n" VADD4 VSR("6", "128", "4")
        VL4("4", "128", "3") VL4("5", "192", "3")
        "vmmul.q M600, M400, M000\n vmmul.q M700, M500, M200\n" VADD4 VSR("6", "16", "4")
        "vmmul.q M600, M400, M100\n vmmul.q M700, M500, M300\n" VADD4 VSR("6", "144", "4")
        :
        : "r"(coef), "r"(L), "r"(vf_right), "r"(vf_tmp), "r"(out)
        : "memory");
}

/* vf_idct for a block whose coefficients all sit in the top-left quadrant (rows and columns 0-3),
 * 72% of the blocks in a video: X10 = X01 = X11 = 0, so P00 = L00 X00, P10 = L10 X00, P01 = P11 = 0,
 * and Q takes four products: six vmmul instead of sixteen, P kept in registers. The terms left out
 * are exact zeros, so the results are vf_idct's bit for bit. */
static void vf_idct_tl(const s32 *coef, const float *L, float *out)
{
    __asm__ volatile(
        VL4("0", "0", "1") VL4("2", "128", "1") VLR("4", "0", "0") VI2F("4")
        "vmmul.q M600, M000, M400\n vmmul.q M700, M200, M400\n"
        VL4("0", "0", "2") VL4("1", "64", "2")
        "vmmul.q M200, M600, M000\n vmmul.q M300, M600, M100\n" VSR("2", "0", "3") VSR("3", "128", "3")
        "vmmul.q M200, M700, M000\n vmmul.q M300, M700, M100\n" VSR("2", "16", "3") VSR("3", "144", "3")
        :
        : "r"(coef), "r"(L), "r"(vf_right), "r"(out)
        : "memory");
}

/* Quadrants a raster position belongs to, as mask bits: 1 top-left, 2 top-right, 4 bottom-left,
 * 8 bottom-right. vf_mask[b] holds the quadrants block b's coefficients were written to. */
#define VF_QB_TOP 1, 1, 1, 1, 2, 2, 2, 2
#define VF_QB_BOT 4, 4, 4, 4, 8, 8, 8, 8
static const u8 vf_qbit[64] = {VF_QB_TOP, VF_QB_TOP, VF_QB_TOP, VF_QB_TOP, VF_QB_BOT, VF_QB_BOT, VF_QB_BOT, VF_QB_BOT};
static u8 vf_mask[6] = {15, 15, 15, 15, 15, 15};

static void vf_idct_any(int b)
{
    if (vf_mask[b] == 1)
        vf_idct_tl(vf_coef[b], vf_left[b >= 2], vf_pix[b]);
    else
        vf_idct(vf_coef[b], vf_left[b >= 2], vf_pix[b]);
}

/* Zeroes what block b's mask says was written: the top-left quadrant alone or the whole block. */
static void vf_zero(int b)
{
    if (vf_mask[b] == 1)
        __asm__ volatile("vzero.q C000\n sv.q C000, 0(%0)\n sv.q C000, 32(%0)\n sv.q C000, 64(%0)\n sv.q C000, 96(%0)\n"
                         : : "r"(vf_coef[b]) : "memory");
    else if (vf_mask[b])
        __asm__ volatile("vzero.q C000\n" VZ8("0", "0") VZ8("0", "128") : : "r"(vf_coef[b]) : "memory");
    vf_mask[b] = 0;
}

/* Rounds and clamps C000-C030 to signed bytes (C700 holds 127s, C710 -128s), each step on the four
 * columns before the next: four chains in flight. */
#define VROUND4 "vf2in.q C000, C000, 0\n vf2in.q C010, C010, 0\n vf2in.q C020, C020, 0\n vf2in.q C030, C030, 0\n" \
    "vi2f.q C000, C000, 0\n vi2f.q C010, C010, 0\n vi2f.q C020, C020, 0\n vi2f.q C030, C030, 0\n" \
    "vmin.q C000, C000, C700\n vmin.q C010, C010, C700\n vmin.q C020, C020, C700\n vmin.q C030, C030, C700\n" \
    "vmax.q C000, C000, C710\n vmax.q C010, C010, C710\n vmax.q C020, C020, C710\n vmax.q C030, C030, C710\n"

/* The 24-bit words of four pixels, R0 G0 B0 R1 | G1 B1 R2 G2 | B2 R3 G3 B3, are rows of three
 * matrices of the even (e: x 0 2 4 6) and odd (o: 1 3 5 7) channel columns: rows 0 and 2 of
 * [Re Ge Be Ro], of [Go Bo Re' Ge'] (Re' = Re[y,x,w,z]: element 0 is pixel 2's) and rows 1 and 3
 * of [Be Ro Go Bo]. A row of 8 pixels is six words. Row oy: M2, M3, M4; row oy + 1: M5, M1, M6. */
#define VF_CP24(A, B, C, Re, Ge, Be, Ro, Go, Bo) \
    "vmov.q C" A "30, C" Ro "\n vmov.q C" C "00, C" Be "\n vmov.q C" B "00, C" Go "\n vmov.q C" B "10, C" Bo "\n" \
    "vpfxs y, x, w, z\n vmov.q C" B "20, C" Re "\n vpfxs y, x, w, z\n vmov.q C" B "30, C" Ge "\n"

/* The six blocks in vf_coef (Cr, Cb, Y0-Y3) to vf_rgba, or as 24-bit pixels to rgb24. Two chroma
 * quads and two luma rows a step: the rows of a pair are one chroma row (oy even), so its terms
 * load once, and the two rows' operations interleave, each waiting out the other's latency. Each
 * value takes the same operations as one row at a time. */
static void vf_colour(u32 *rgb24)
{
    for (int b = 0; b < 6; b++)
        vf_idct_any(b);
    __asm__ volatile(VL4("7", "0", "0") : : "r"(vf_k) : "memory");
    for (int q = 0; q < 64; q += 8) /* chroma terms: R 359r, G -88b -183r, B 454b, all / 256, + 128 */
        __asm__ volatile("lv.q C000, 0(%0)\n lv.q C010, 0(%1)\n lv.q C020, 16(%0)\n lv.q C030, 16(%1)\n" VROUND4
                         "vscl.q C100, C000, S720\n vscl.q C200, C020, S720\n vscl.q C110, C010, S721\n vscl.q C210, C030, S721\n"
                         "vscl.q C120, C000, S722\n vscl.q C220, C020, S722\n vadd.q C100, C100, C730\n vadd.q C200, C200, C730\n"
                         "vadd.q C110, C110, C120\n vadd.q C210, C210, C220\n vscl.q C120, C010, S723\n vscl.q C220, C030, S723\n"
                         "vadd.q C110, C110, C730\n vadd.q C210, C210, C730\n vadd.q C120, C120, C730\n vadd.q C220, C220, C730\n"
                         "sv.q C100, 0(%2)\n sv.q C110, 0(%3)\n sv.q C120, 0(%4)\n sv.q C200, 16(%2)\n sv.q C210, 16(%3)\n sv.q C220, 16(%4)\n"
                         :
                         : "r"(vf_pix[0] + q), "r"(vf_pix[1] + q), "r"(vf_term[0] + q), "r"(vf_term[1] + q), "r"(vf_term[2] + q)
                         : "memory");
    if (rgb24) {
        for (int i = 0; i < 4; i++)
            for (int y = 0; y < 8; y += 2) {
                int oy = (i >> 1) * 8 + y, c = (oy >> 1) * 8 + (i & 1) * 4;
                __asm__ volatile("lv.q C000, 0(%0)\n lv.q C010, 16(%0)\n lv.q C020, 32(%0)\n lv.q C030, 48(%0)\n" VROUND4
                                 "lv.q C100, 0(%1)\n lv.q C110, 0(%2)\n lv.q C120, 0(%3)\n"
                                 /* row oy: even x in C200-C220, odd in C410-C430; row oy + 1: C500-C520, C610-C630 */
                                 "vadd.q C200, C000, C100\n vadd.q C410, C010, C100\n vadd.q C500, C020, C100\n vadd.q C610, C030, C100\n"
                                 "vadd.q C210, C000, C110\n vadd.q C420, C010, C110\n vadd.q C510, C020, C110\n vadd.q C620, C030, C110\n"
                                 "vadd.q C220, C000, C120\n vadd.q C430, C010, C120\n vadd.q C520, C020, C120\n vadd.q C630, C030, C120\n"
                                 "vf2iz.q C200, C200, 23\n vf2iz.q C410, C410, 23\n vf2iz.q C500, C500, 23\n vf2iz.q C610, C610, 23\n"
                                 "vf2iz.q C210, C210, 23\n vf2iz.q C420, C420, 23\n vf2iz.q C510, C510, 23\n vf2iz.q C620, C620, 23\n"
                                 "vf2iz.q C220, C220, 23\n vf2iz.q C430, C430, 23\n vf2iz.q C520, C520, 23\n vf2iz.q C630, C630, 23\n"
                                 VF_CP24("2", "3", "4", "200", "210", "220", "410", "420", "430")
                                 VF_CP24("5", "1", "6", "500", "510", "520", "610", "620", "630")
                                 "vi2uc.q S000, R200\n vi2uc.q S001, R300\n vi2uc.q S002, R401\n"
                                 "vi2uc.q S003, R202\n vi2uc.q S010, R302\n vi2uc.q S011, R403\n"
                                 "vi2uc.q S012, R500\n vi2uc.q S013, R100\n vi2uc.q S020, R601\n"
                                 "vi2uc.q S021, R502\n vi2uc.q S022, R102\n vi2uc.q S023, R603\n"
                                 "sv.s S000, 0(%4)\n sv.s S001, 4(%4)\n sv.s S002, 8(%4)\n sv.s S003, 12(%4)\n sv.s S010, 16(%4)\n sv.s S011, 20(%4)\n"
                                 "sv.s S012, 48(%4)\n sv.s S013, 52(%4)\n sv.s S020, 56(%4)\n sv.s S021, 60(%4)\n sv.s S022, 64(%4)\n sv.s S023, 68(%4)\n"
                                 :
                                 : "r"(vf_pix[2 + i] + y * 8), "r"(vf_term[0] + c), "r"(vf_term[1] + c), "r"(vf_term[2] + c),
                                   "r"(rgb24 + oy * 12 + (i & 1) * 6)
                                 : "memory");
            }
        return;
    }
    __asm__ volatile("vzero.q C230\n vzero.q C430\n vzero.q C530\n vzero.q C630\n");
    for (int i = 0; i < 4; i++)
        for (int y = 0; y < 8; y += 2) {
            int oy = (i >> 1) * 8 + y, c = (oy >> 1) * 8 + (i & 1) * 4;
            __asm__ volatile("lv.q C000, 0(%0)\n lv.q C010, 16(%0)\n lv.q C020, 32(%0)\n lv.q C030, 48(%0)\n" VROUND4
                             "lv.q C100, 0(%1)\n lv.q C110, 0(%2)\n lv.q C120, 0(%3)\n"
                             /* row oy: even x in M2, odd in M4; row oy + 1: even in M5, odd in M6 */
                             "vadd.q C200, C000, C100\n vadd.q C400, C010, C100\n vadd.q C500, C020, C100\n vadd.q C600, C030, C100\n"
                             "vadd.q C210, C000, C110\n vadd.q C410, C010, C110\n vadd.q C510, C020, C110\n vadd.q C610, C030, C110\n"
                             "vadd.q C220, C000, C120\n vadd.q C420, C010, C120\n vadd.q C520, C020, C120\n vadd.q C620, C030, C120\n"
                             "vf2iz.q C200, C200, 23\n vf2iz.q C400, C400, 23\n vf2iz.q C500, C500, 23\n vf2iz.q C600, C600, 23\n"
                             "vf2iz.q C210, C210, 23\n vf2iz.q C410, C410, 23\n vf2iz.q C510, C510, 23\n vf2iz.q C610, C610, 23\n"
                             "vf2iz.q C220, C220, 23\n vf2iz.q C420, C420, 23\n vf2iz.q C520, C520, 23\n vf2iz.q C620, C620, 23\n"
                             "vi2uc.q S300, R200\n vi2uc.q S301, R400\n vi2uc.q S320, R500\n vi2uc.q S321, R600\n"
                             "vi2uc.q S302, R201\n vi2uc.q S303, R401\n vi2uc.q S322, R501\n vi2uc.q S323, R601\n"
                             "vi2uc.q S310, R202\n vi2uc.q S311, R402\n vi2uc.q S330, R502\n vi2uc.q S331, R602\n"
                             "vi2uc.q S312, R203\n vi2uc.q S313, R403\n vi2uc.q S332, R503\n vi2uc.q S333, R603\n"
                             "sv.q C300, 0(%4)\n sv.q C310, 16(%4)\n sv.q C320, 64(%4)\n sv.q C330, 80(%4)\n"
                             :
                             : "r"(vf_pix[2 + i] + y * 8), "r"(vf_term[0] + c), "r"(vf_term[1] + c), "r"(vf_term[2] + c),
                               "r"(vf_rgba + oy * 16 + (i & 1) * 8)
                             : "memory");
        }
}

/* A colour macroblock to `out`: 24-bit (depth 2) straight from the VFPU, 15-bit (depth 3) from
 * vf_rgba. Signed output is the unsigned bytes with their top bit flipped. */
static void vf_output(u32 *out, int depth, int sign, int bit15)
{
    u32 x = sign ? 0x80808080u : 0;
    if (depth == 2) {
        vf_colour(out);
        for (int n = 0; x && n < 192; n++)
            out[n] ^= x;
        return;
    }
    vf_colour(NULL);
    const u32 *p = vf_rgba;
    u16 *o = (u16 *)out, hi = bit15 ? 0x8000 : 0;
    for (int n = 0; n < 256; n++) {
        u32 w = p[n] ^ x;
        o[n] = (u16)((w >> 3 & 0x1F) | (w >> 6 & 0x3E0) | (w >> 9 & 0x7C00) | hi);
    }
}

static u32 vf_check_rle(const Mdec *m);

static void yuv_c(Mdec *m, const s16 *cr, const s16 *cb, const s16 *y, int xx, int yy)
{
    if (m->depth == 2)
        yuv(m, cr, cb, y, xx, yy, 2);
    else
        yuv(m, cr, cb, y, xx, yy, 3);
}

/* Random macroblocks with the game's scale table, through the C path (exact IDCT) and the VFPU. */
static void vf_check(Psx *psx)
{
    static Mdec t;
    static s16 blk[6][64];
    static float full[64] A64;
    u32 seed = 0x2545F491u, near = 0, far = 0, bytes = 0, tl_diff = 0, tl_n = 0;
    Mdec *m = &psx->mdec;
    vf_build(m->scale);
    memcpy(t.scale, m->scale, sizeof t.scale);
    clamp_init();
    for (int it = 0; it < 64; it++) {
        /* kind 0 and 3: coefficients anywhere; 1: top-left quadrant only; 2: DC only */
        int depth = 2 + (it & 1), sign = it >> 1 & 1, kind = it >> 3 & 3;
        for (int b = 0; b < 6; b++) {
            memset(blk[b], 0, sizeof blk[b]);
            seed = seed * 1664525u + 1013904223u;
            blk[b][0] = (s16)((s32)(seed >> 16) % 1000 - 500);
            for (int k = 0; k < 10 && kind != 2; k++) {
                seed = seed * 1664525u + 1013904223u;
                u32 p = 1 + (seed >> 8) % 63;
                if (kind == 1)
                    p = (p >> 2 & 3) * 8 + (p & 3);
                blk[b][p] = (s16)((s32)(seed >> 20 & 127) - 64);
            }
            vf_mask[b] = 0;
            for (int k = 0; k < 64; k++) {
                vf_coef[b][k] = blk[b][k];
                if (blk[b][k])
                    vf_mask[b] |= vf_qbit[k];
            }
            if (vf_mask[b] == 1) { /* the top-left path against the full one, bit for bit */
                vf_idct(vf_coef[b], vf_left[b >= 2], full);
                vf_idct_tl(vf_coef[b], vf_left[b >= 2], vf_pix[b]);
                tl_diff += memcmp(full, vf_pix[b], sizeof full) != 0, tl_n++;
            }
            idct(&t, blk[b]);
        }
        t.depth = depth, t.sign = sign, t.bit15 = it >> 2 & 1;
        for (int i = 0; i < 4; i++)
            yuv_c(&t, blk[0], blk[1], blk[2 + i], (i & 1) * 8, (i >> 1) * 8);
        static u32 out[192];
        vf_output(out, depth, sign, t.bit15);
        if (depth == 2) {
            const u8 *a = (const u8 *)t.out, *b = (const u8 *)out;
            for (int k = 0; k < 768; k++) {
                int d = a[k] - b[k];
                near += d == 1 || d == -1, far += d > 1 || d < -1;
            }
            bytes += 768;
        } else {
            for (int k = 0; k < 256; k++) {
                u16 a = ((const u16 *)t.out)[k], b = ((const u16 *)out)[k];
                for (int s = 0; s < 15; s += 5) {
                    int d = (a >> s & 31) - (b >> s & 31);
                    near += d == 1 || d == -1, far += d > 1 || d < -1;
                }
                far += (a ^ b) >> 15;
            }
            bytes += 768;
        }
    }
    u32 rle_bad = vf_check_rle(m);
    vf_state = far || rle_bad || tl_diff ? -1 : 1;
    psx_log(psx, "mdec: VFPU IDCT and colour %s, %u of %u channels differ by 1, %u by more; %u of 512 RLE blocks differ; "
            "%u of %u top-left blocks differ from the full IDCT",
            vf_state > 0 ? "in use" : "refused", near, bytes, far, rle_bad, tl_diff, tl_n);
    /* What a macroblock costs, by part, on the last random blocks: the S19 device run spent
     * ~48 us a macroblock in the intro movie */
    const PsxHost *h = psx->host;
    if (vf_state > 0 && h->usec) {
        static u32 out[192];
        u32 t0 = h->usec(h->user);
        for (int i = 0; i < 64; i++)
            for (int b = 0; b < 6; b++)
                vf_idct(vf_coef[b], vf_left[b >= 2], vf_pix[b]);
        u32 t1 = h->usec(h->user);
        for (int i = 0; i < 64; i++)
            vf_output(out, 2, 0, 0);
        u32 t2 = h->usec(h->user);
        for (int i = 0; i < 64; i++)
            vf_output(out, 3, 0, 0);
        u32 t3 = h->usec(h->user);
        for (int i = 0; i < 64; i++)
            for (int b = 0; b < 6; b++)
                vf_idct_tl(vf_coef[b], vf_left[b >= 2], vf_pix[b]);
        u32 t4 = h->usec(h->user);
        psx_log(psx, "mdec: a macroblock costs %u ns of IDCT (%u ns with top-left blocks), %u ns of IDCT and 24-bit colour, %u ns of IDCT and 15-bit colour",
                (t1 - t0) * 1000 / 64, (t4 - t3) * 1000 / 64, (t2 - t1) * 1000 / 64, (t3 - t2) * 1000 / 64);
    }
    memset(vf_mask, 15, sizeof vf_mask); /* the checks left coefficients everywhere */
}

/* The stream read a halfword at a time through a pointer: on the little-endian PSP the halfword
 * hp of in[] is in[hp >> 1] >> ((hp & 1) << 4) & 0xFFFF, as halfword() reads it. */
typedef u16 __attribute__((may_alias)) vf_u16;
#define VF_CLAMP(v) (v < -0x400 ? -0x400 : v > 0x3FF ? 0x3FF : v)

/* rle() for a zeroed block of ints, with the read position in a register and one multiply per
 * coefficient: the quant table times the block's scale is kept per scale, which a video seldom
 * changes. *mask gets the quadrants written to (vf_qbit). */
static int vf_rle(Mdec *m, s32 *f, int t, u8 *mask)
{
    const vf_u16 *in = (const vf_u16 *)m->in, *h = in + m->hpos, *end = in + m->nin * 2;
    u32 n, mk = 0;
    do {
        if (h >= end) {
            m->hpos = (int)(h - in);
            return 0;
        }
        n = *h++;
    } while (n == 0xFE00);
    s32 q = (s32)(n >> 10), k = 0, val;
    if (!q)
        for (;;) { /* raster order, each value doubled */
            val = sext10(n) * 2;
            f[k] = VF_CLAMP(val);
            mk |= vf_qbit[k];
            if (h >= end)
                break;
            n = *h++;
            if ((k += (s32)(n >> 10) + 1) > 63)
                break;
        }
    else {
        const u8 *qt = t ? m->iq_uv : m->iq_y;
        s32 *qq = vf_qtq[t];
        if (vf_qtq_q[t] != q) {
            for (int i = 0; i < 64; i++)
                qq[i] = qt[i] * q;
            vf_qtq_q[t] = q;
        }
        val = sext10(n) * qt[0];
        for (;;) {
            u32 p = zagzig[k];
            f[p] = VF_CLAMP(val);
            mk |= vf_qbit[p];
            if (h >= end)
                break;
            n = *h++;
            if ((k += (s32)(n >> 10) + 1) > 63)
                break;
            val = (sext10(n) * qq[k] + 4) / 8;
        }
    }
    m->hpos = (int)(h - in);
    *mask = (u8)mk;
    return 1;
}

/* Random run-length streams, some cut short, through rle() and vf_rle(): the blocks, results and
 * read positions must match exactly. */
static u32 vf_rnd(u32 *s) { return (*s = *s * 1664525u + 1013904223u) >> 8; }

static u32 vf_check_rle(const Mdec *m)
{
    static Mdec r;
    static u32 buf[40];
    static s16 a[64];
    u32 seed = 0x9E3779B9u, bad = 0;
    memcpy(r.iq_y, m->iq_y, 64), memcpy(r.iq_uv, m->iq_uv, 64);
    r.in = buf;
    for (int it = 0; it < 512; it++) {
        u16 *h = (u16 *)buf;
        int nh = 0;
        if (vf_rnd(&seed) % 4 == 0)
            h[nh++] = 0xFE00;
        u32 q = vf_rnd(&seed) % 5 == 0 ? 0 : 1 + vf_rnd(&seed) % 63;
        h[nh++] = (u16)(q << 10 | (vf_rnd(&seed) & 0x3FF));
        for (int n = (int)(vf_rnd(&seed) % 20); n > 0; n--) {
            u32 run = vf_rnd(&seed) % 8;
            h[nh++] = (u16)(run << 10 | (vf_rnd(&seed) & 0x3FF));
        }
        h[nh++] = 0xFE00;
        if (nh & 1)
            h[nh++] = 0xFE00;
        r.nin = nh / 2;
        if (vf_rnd(&seed) % 8 == 0)
            r.nin = (int)(vf_rnd(&seed) % (u32)(nh / 2 + 1));
        int t = it & 1;
        r.hpos = 0;
        int ra = rle(&r, a, t ? r.iq_uv : r.iq_y), pa = r.hpos;
        memset(vf_coef[0], 0, sizeof vf_coef[0]);
        r.hpos = 0;
        u8 mk = 0;
        int rb = vf_rle(&r, vf_coef[0], t, &mk), diff = ra != rb || pa != r.hpos;
        for (int k = 0; k < 64; k++) /* the mask must hold every coefficient written */
            diff |= a[k] != vf_coef[0][k] || (a[k] && !(mk & vf_qbit[k]));
        bad += (u32)diff;
    }
    vf_qtq_q[0] = vf_qtq_q[1] = -1;
    return bad;
}

/* A colour macroblock through the VFPU; 0 when the input ran out. */
static int vf_decode(Mdec *m)
{
    for (int b = 0; b < 6; b++)
        vf_zero(b);
    for (int b = 0; b < 6; b++) {
        m->block = b < 2 ? 4 + b : b - 2;
        if (!vf_rle(m, vf_coef[b], b < 2, &vf_mask[b]))
            return 0;
    }
    if (vf_stale && memcmp(vf_from, m->scale, sizeof vf_from))
        vf_build(m->scale);
    vf_stale = 0;
    vf_output(m->out, m->depth, m->sign, m->bit15);
    return 1;
}
#endif

static void decode(Mdec *m);

/* Decodes the next macroblock into `out`, timed when the host has a clock. */
static void macroblock(Psx *psx)
{
    const PsxHost *h = psx->host;
    u32 t = h->usec ? h->usec(h->user) : 0;
#ifdef __psp__
    if (!vf_state && psx->mdec.depth >= 2)
        vf_check(psx);
#endif
    decode(&psx->mdec);
    if (h->usec)
        psx->mdec.us_decode += h->usec(h->user) - t;
}

static void decode(Mdec *m)
{
    m->nout = m->rout = 0;
    if (m->depth >= 2) {
#ifdef __psp__
        if (vf_state > 0) {
            if (vf_decode(m))
                m->nout = m->depth == 2 ? 192 : 128;
            return;
        }
#endif
        s16 cr[64], cb[64], y[64];
        clamp_init();
        m->block = 4;
        if (!block(m, cr, m->iq_uv))
            return;
        m->block = 5;
        if (!block(m, cb, m->iq_uv))
            return;
        for (int i = 0; i < 4; i++) {
            m->block = i;
            if (!block(m, y, m->iq_y))
                return;
            int xx = (i & 1) * 8, yy = (i >> 1) * 8;
            if (m->depth == 2)
                yuv(m, cr, cb, y, xx, yy, 2);
            else
                yuv(m, cr, cb, y, xx, yy, 3);
        }
        m->nout = m->depth == 2 ? 192 : 128;
    } else {
        s16 y[64];
        m->block = 4;
        if (!block(m, y, m->iq_y))
            return;
        u8 *o = (u8 *)m->out;
        for (int i = 0; i < 64; i++) {
            u8 v = clamp8(y[i]);
            if (!m->sign)
                v ^= 0x80;
            if (m->depth == 1)
                o[i] = v;
            else if (i & 1)
                o[i >> 1] |= (u8)(v >> 4 << 4);
            else
                o[i >> 1] = v >> 4;
        }
        m->nout = m->depth == 1 ? 16 : 8;
    }
}

static int more_input(const Mdec *m) { return m->state == 1 && m->hpos < m->nin * 2; }

u32 mdec_read(Psx *psx, int reg)
{
    Mdec *m = &psx->mdec;
    if (!reg)
        return mdec_dma_out(psx);
    if (m->rout >= m->nout && more_input(m) && !m->words_left)
        macroblock(psx);
    int out = m->rout < m->nout;
    u32 s = 0;
    if (!out)
        s |= 1u << 31;
    if (m->words_left || out || more_input(m))
        s |= 1u << 29;
    if ((m->ctl & (1u << 30)) && m->words_left)
        s |= 1u << 28;
    if ((m->ctl & (1u << 29)) && out)
        s |= 1u << 27;
    s |= (u32)m->depth << 25 | (u32)m->sign << 24 | (u32)m->bit15 << 23 | (u32)(m->block & 7) << 16;
    s |= (u32)(m->words_left ? m->words_left - 1 : 0xFFFF) & 0xFFFF;
    return s;
}

int mdec_out_ready(Psx *psx)
{
    Mdec *m = &psx->mdec;
    return m->rout < m->nout || (more_input(m) && !m->words_left);
}

u32 mdec_dma_out(Psx *psx)
{
    Mdec *m = &psx->mdec;
    if (m->rout >= m->nout && more_input(m) && !m->words_left)
        macroblock(psx);
    return m->rout < m->nout ? m->out[m->rout++] : 0;
}

u32 mdec_dma_block(Psx *psx, u32 *dst, u32 n)
{
    Mdec *m = &psx->mdec;
    if (m->rout >= m->nout && more_input(m) && !m->words_left)
        macroblock(psx);
    if (m->rout >= m->nout) {
        *dst = 0;
        return 1;
    }
    u32 k = (u32)(m->nout - m->rout);
    if (k > n)
        k = n;
#ifdef __psp__
    /* The words cover whole cache lines of RAM: those are made valid and dirty without the fetch a
     * store miss would do (Allegrex cache 0x18, create dirty exclusive), then overwritten; ch2
     * reads them back from the cache. */
    for (u32 a = ((u32)dst + 63) & ~63u, e = ((u32)dst + k * 4) & ~63u; a < e; a += 64)
        __asm__ volatile("cache 0x18, 0(%0)" : : "r"(a) : "memory");
#endif
    memcpy(dst, &m->out[m->rout], k * 4);
    m->rout += (int)k;
    return k;
}

u32 mdec_dma_in(Psx *psx, const u32 *src, u32 n)
{
    Mdec *m = &psx->mdec;
    if (m->state != 1 || m->words_left <= 0)
        return 0;
    u32 k = (u32)m->words_left < n ? (u32)m->words_left : n, fit = (u32)(m->in_cap - m->nin);
    if (fit > k)
        fit = k;
    memcpy(m->in + m->nin, src, fit * 4); /* past the buffer the words are dropped, as in param() */
    m->nin += (int)fit, m->words_left -= (int)k;
    return k;
}

static void param(Mdec *m, u32 v)
{
    switch (m->state) {
    case 1:
        if (m->nin < m->in_cap)
            m->in[m->nin++] = v;
        break;
    case 2: {
        int i = (m->color ? 32 : 16) - m->words_left;
        u8 *t = i < 16 ? m->iq_y + i * 4 : m->iq_uv + (i - 16) * 4;
        t[0] = (u8)v, t[1] = (u8)(v >> 8), t[2] = (u8)(v >> 16), t[3] = (u8)(v >> 24);
        break;
    }
    case 3: {
        int i = 32 - m->words_left;
        m->scale[i * 2] = (s16)v, m->scale[i * 2 + 1] = (s16)(v >> 16);
        if (m->words_left == 1)
            m->std_scale = standard_scale(m->scale);
        break;
    }
    }
    m->words_left--;
}

void mdec_write(Psx *psx, int reg, u32 v)
{
    Mdec *m = &psx->mdec;
    if (reg) {
        m->ctl = v;
        if (v & (1u << 31)) {
            m->state = 0, m->words_left = 0, m->nin = 0, m->hpos = 0, m->nout = m->rout = 0, m->block = 4;
            m->depth = m->sign = m->bit15 = 0;
        }
        return;
    }
    if (m->words_left) {
        param(m, v);
        if (m->state == 3 && !m->words_left) {
            static int logged;
            if (!logged++)
                psx_log(psx, "mdec: scale table %s", m->std_scale ? "standard, fast IDCT" : "not standard, exact IDCT");
        }
        return;
    }
    m->cmd = v;
    switch (v >> 29) {
    case 1:
        m->state = 1;
#ifdef __psp__
        vf_stale = 1;
#endif
        m->depth = (v >> 27) & 3, m->sign = (v >> 26) & 1, m->bit15 = (v >> 25) & 1;
        m->words_left = (int)(v & 0xFFFF);
        m->nin = 0, m->hpos = 0, m->nout = m->rout = 0;
        if (m->words_left > m->in_cap)
            psx_log(psx, "mdec: %d words exceed the input buffer (%d)", m->words_left, m->in_cap);
        break;
    case 2:
        m->state = 2, m->color = v & 1;
#ifdef __psp__
        vf_qtq_q[0] = vf_qtq_q[1] = -1;
#endif
        m->words_left = m->color ? 32 : 16;
        break;
    case 3:
        m->state = 3, m->words_left = 32, m->std_scale = 0;
        break;
    default:
        m->words_left = 0;
        break;
    }
}
