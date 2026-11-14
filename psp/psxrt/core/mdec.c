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

/* Decodes one block; 0 when the input ran out. */
static int block(Mdec *m, s16 *blk, const u8 *qt)
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
    idct(m, blk);
    return 1;
}

static u8 clamp8(s32 v) { return (u8)(v < -128 ? -128 : v > 127 ? 127 : v); }

static void put_rgb(Mdec *m, int x, int y, s32 r, s32 g, s32 b)
{
    u8 rr = clamp8(r), gg = clamp8(g), bb = clamp8(b);
    if (!m->sign)
        rr ^= 0x80, gg ^= 0x80, bb ^= 0x80;
    if (m->depth == 2) {
        u8 *o = (u8 *)m->out + (y * 16 + x) * 3;
        o[0] = rr, o[1] = gg, o[2] = bb;
    } else {
        u16 *o = (u16 *)m->out + y * 16 + x;
        *o = (u16)((rr >> 3) | (gg >> 3) << 5 | (bb >> 3) << 10 | (m->bit15 ? 0x8000 : 0));
    }
}

static void decode(Mdec *m);

/* Decodes the next macroblock into `out`, timed when the host has a clock. */
static void macroblock(Psx *psx)
{
    const PsxHost *h = psx->host;
    u32 t = h->usec ? h->usec(h->user) : 0;
    decode(&psx->mdec);
    if (h->usec)
        psx->mdec.us_decode += h->usec(h->user) - t;
}

static void decode(Mdec *m)
{
    m->nout = m->rout = 0;
    if (m->depth >= 2) {
        s16 cr[64], cb[64], y[64];
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
            for (int py = 0; py < 8; py++)
                for (int px = 0; px < 8; px++) {
                    int c = ((px + xx) >> 1) + ((py + yy) >> 1) * 8;
                    s32 R = cr[c], B = cb[c];
                    s32 G = (-88 * B - 183 * R) >> 8;
                    R = (359 * R) >> 8, B = (454 * B) >> 8;
                    s32 Y = y[px + py * 8];
                    put_rgb(m, px + xx, py + yy, Y + R, Y + G, Y + B);
                }
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
    memcpy(dst, &m->out[m->rout], k * 4);
    m->rout += (int)k;
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
        return;
    }
    m->cmd = v;
    switch (v >> 29) {
    case 1:
        m->state = 1;
        m->depth = (v >> 27) & 3, m->sign = (v >> 26) & 1, m->bit15 = (v >> 25) & 1;
        m->words_left = (int)(v & 0xFFFF);
        m->nin = 0, m->hpos = 0, m->nout = m->rout = 0;
        if (m->words_left > m->in_cap)
            psx_log(psx, "mdec: %d words exceed the input buffer (%d)", m->words_left, m->in_cap);
        break;
    case 2:
        m->state = 2, m->color = v & 1;
        m->words_left = m->color ? 32 : 16;
        break;
    case 3:
        m->state = 3, m->words_left = 32;
        break;
    default:
        m->words_left = 0;
        break;
    }
}
