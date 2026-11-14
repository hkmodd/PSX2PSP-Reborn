/*
 * psxrt core: the SPU (psx-spx "Sound Processing Unit"). 24 ADPCM voices with 4-point gaussian
 * interpolation, ADSR envelopes, volume sweeps, pitch modulation and noise; the CD input (XA from
 * cdrom.c), the reverb unit, the capture buffers and the IRQ address; mixed at 44100 Hz into
 * PsxHost.audio.
 *
 * The SPU runs lazily: up to the CPU's time at every vblank and when the guest keys voices on or
 * off or reads what the voices did (ENDX, envelope levels), in chunks of up to SPU_CHUNK samples:
 * voice by voice over the chunk (each voice's output is the next one's pitch modulation), then the
 * mix sample by sample. Other register writes take effect at the next sync.
 *
 * Voice decode, ADSR and the XA decoder in cdrom.c follow RecompOne's Spu.cs/XaAudio.cs:
 * https://github.com/BlackLabelHQ/RecompOne, under this licence:
 *
 *   Copyright (c) 2026 flaffy
 *
 *   Permission is hereby granted, free of charge, to any person obtaining a copy
 *   of this software and associated documentation files (the "Software"), to deal
 *   in the Software without restriction, including without limitation the rights
 *   to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 *   copies of the Software, and to permit persons to whom the Software is
 *   furnished to do so, subject to the following conditions:
 *
 *   The above copyright notice and this permission notice shall be included in all
 *   copies or substantial portions of the Software.
 *
 *   THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 *   IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 *   FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 *   AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 *   LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 *   OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 *   SOFTWARE.
 */
#include "psx.h"

#include <string.h>

#define CYCLES_PER_SAMPLE 768u /* 33.8688 MHz / 44100 */
#define R(off) psx->spu.regs[((off) - 0x1F801C00u) >> 1]
#define G(off) s->regs[((off) - 0x1F801C00u) >> 1]
#define VR(i, r) s->regs[(i) * 8 + (r)] /* voice i: 0 vol L, 1 vol R, 2 pitch, 3 start, 4-5 ADSR, 6 level, 7 repeat */

enum { SPU_OFF, SPU_ATTACK, SPU_DECAY, SPU_SUSTAIN, SPU_RELEASE };

/* psx-spx "SPU ADPCM Pitch": the hardware's interpolation table. */
static const s16 gauss[512] = {
    -1, -1, -1, -1, -1, -1, -1, -1,
    -1, -1, -1, -1, -1, -1, -1, -1,
    0, 0, 0, 0, 0, 0, 0, 1,
    1, 1, 1, 2, 2, 2, 3, 3,
    3, 4, 4, 5, 5, 6, 7, 7,
    8, 9, 9, 10, 11, 12, 13, 14,
    15, 16, 17, 18, 19, 21, 22, 24,
    25, 27, 28, 30, 32, 33, 35, 37,
    39, 41, 44, 46, 48, 51, 53, 56,
    58, 61, 64, 67, 70, 73, 77, 80,
    84, 87, 91, 95, 99, 103, 107, 111,
    116, 120, 125, 130, 135, 140, 145, 150,
    156, 161, 167, 173, 179, 186, 192, 199,
    205, 212, 219, 227, 234, 242, 250, 257,
    266, 274, 283, 291, 300, 309, 319, 328,
    338, 348, 358, 369, 379, 390, 401, 412,
    424, 436, 448, 460, 473, 485, 498, 512,
    525, 539, 553, 567, 582, 597, 612, 627,
    643, 659, 675, 692, 708, 726, 743, 761,
    779, 797, 816, 835, 854, 874, 894, 914,
    935, 956, 977, 999, 1020, 1043, 1066, 1089,
    1112, 1136, 1160, 1184, 1209, 1234, 1260, 1286,
    1312, 1339, 1366, 1394, 1422, 1450, 1479, 1508,
    1537, 1567, 1598, 1628, 1660, 1691, 1723, 1756,
    1789, 1822, 1856, 1890, 1924, 1959, 1995, 2031,
    2067, 2104, 2141, 2179, 2217, 2256, 2295, 2334,
    2374, 2415, 2456, 2497, 2539, 2582, 2624, 2668,
    2712, 2756, 2801, 2846, 2892, 2938, 2985, 3032,
    3079, 3128, 3176, 3225, 3275, 3325, 3376, 3427,
    3479, 3531, 3584, 3637, 3691, 3745, 3799, 3855,
    3910, 3967, 4023, 4081, 4138, 4197, 4255, 4315,
    4374, 4435, 4495, 4557, 4619, 4681, 4744, 4807,
    4871, 4935, 5000, 5065, 5131, 5197, 5264, 5332,
    5399, 5468, 5536, 5606, 5676, 5746, 5817, 5888,
    5959, 6032, 6104, 6177, 6251, 6325, 6400, 6475,
    6550, 6626, 6702, 6779, 6856, 6934, 7012, 7091,
    7170, 7249, 7329, 7409, 7490, 7571, 7653, 7735,
    7817, 7900, 7983, 8066, 8150, 8234, 8319, 8404,
    8489, 8575, 8661, 8748, 8834, 8922, 9009, 9097,
    9185, 9273, 9362, 9451, 9541, 9630, 9720, 9811,
    9901, 9992, 10083, 10174, 10266, 10358, 10450, 10542,
    10635, 10727, 10820, 10913, 11007, 11100, 11194, 11288,
    11382, 11476, 11571, 11665, 11760, 11855, 11950, 12045,
    12140, 12236, 12331, 12427, 12522, 12618, 12714, 12809,
    12905, 13001, 13097, 13193, 13289, 13385, 13481, 13577,
    13673, 13769, 13865, 13961, 14056, 14152, 14248, 14343,
    14439, 14534, 14630, 14725, 14820, 14915, 15010, 15104,
    15199, 15293, 15387, 15481, 15575, 15669, 15762, 15855,
    15948, 16041, 16133, 16226, 16317, 16409, 16500, 16592,
    16682, 16773, 16863, 16953, 17042, 17131, 17220, 17308,
    17396, 17484, 17571, 17658, 17744, 17830, 17916, 18001,
    18086, 18170, 18254, 18337, 18420, 18502, 18584, 18665,
    18746, 18826, 18905, 18985, 19063, 19141, 19219, 19295,
    19372, 19447, 19522, 19597, 19671, 19744, 19816, 19888,
    19959, 20030, 20100, 20169, 20238, 20306, 20373, 20439,
    20505, 20570, 20634, 20698, 20760, 20822, 20884, 20944,
    21004, 21063, 21121, 21178, 21235, 21290, 21345, 21399,
    21452, 21505, 21556, 21607, 21657, 21706, 21754, 21801,
    21848, 21893, 21938, 21982, 22025, 22066, 22107, 22148,
    22187, 22225, 22262, 22299, 22334, 22369, 22402, 22435,
    22467, 22498, 22527, 22556, 22584, 22611, 22637, 22662,
    22686, 22709, 22731, 22752, 22772, 22791, 22809, 22826,
    22842, 22857, 22872, 22885, 22897, 22908, 22918, 22927,
    22935, 22942, 22948, 22953, 22957, 22960, 22962, 22963,
};

static const s32 K0[5] = {0, 60, 115, 98, 122}, K1[5] = {0, 0, -52, -55, -60};

static s32 clamp16(s32 x) { return x < -0x8000 ? -0x8000 : x > 0x7FFF ? 0x7FFF : x; }

static s32 rd16(const u8 *ram, u32 a) { return (s16)(ram[a] | ram[a + 1] << 8); }
static void wr16(u8 *ram, u32 a, s32 v) { ram[a] = (u8)v, ram[a + 1] = (u8)(v >> 8); }

static void irq(Psx *psx)
{
    Spu *s = &psx->spu;
    if ((s->ctrl & 0x40) && !(s->stat & 0x40)) {
        s->stat |= 0x40;
        psx_raise(psx, IRQ_SPU);
    }
}

/* One step of an envelope (psx-spx "SPU Volume and ADSR Generator"): the level after the step;
 * *wait gets the samples to the next one. */
static s32 env_step(s32 level, int exp, int dec, int shift, int step, s32 *wait)
{
    s32 cycles = 1 << (shift > 11 ? shift - 11 : 0);
    s32 st = shift < 11 ? step * (1 << (11 - shift)) : step;
    if (exp && !dec && level > 0x6000)
        cycles *= 4;
    if (exp && dec)
        st = st * level >> 15;
    *wait = cycles;
    level += st;
    return level < 0 ? 0 : level > 0x7FFF ? 0x7FFF : level;
}

/* The volume a register gives now: fixed (bit 15 clear) or sweeping. */
static inline s32 volume(u16 reg, SpuSweep *w)
{
    if (!(reg & 0x8000))
        return (s16)(reg << 1);
    if (--w->wait <= 0) {
        int dec = reg >> 13 & 1, st = reg & 3;
        w->level = env_step(w->level, reg >> 14 & 1, dec, reg >> 2 & 31, dec ? st - 8 : 7 - st, &w->wait);
    }
    return (reg & 0x1000) ? -w->level : w->level;
}

static void set_volume(SpuSweep *w, u16 reg)
{
    if (!(reg & 0x8000)) {
        s32 v = (s16)(reg << 1);
        w->level = v < 0 ? (v == -0x8000 ? 0x7FFF : -v) : v;
    }
    w->wait = 0;
}

static void adsr(Spu *s, SpuVoice *v, int i)
{
    u16 a1 = VR(i, 4), a2 = VR(i, 5);
    switch (v->phase) {
    case SPU_ATTACK:
        v->env = env_step(v->env, a1 >> 15, 0, a1 >> 10 & 31, 7 - (a1 >> 8 & 3), &v->env_wait);
        if (v->env == 0x7FFF)
            v->phase = SPU_DECAY, v->env_wait = 0;
        break;
    case SPU_DECAY:
        v->env = env_step(v->env, 1, 1, a1 >> 4 & 15, -8, &v->env_wait);
        if (v->env <= ((a1 & 15) + 1) * 0x800)
            v->phase = SPU_SUSTAIN, v->env_wait = 0;
        break;
    case SPU_SUSTAIN: {
        int dec = a2 >> 14 & 1, st = a2 >> 6 & 3;
        v->env = env_step(v->env, a2 >> 15, dec, a2 >> 8 & 31, dec ? st - 8 : 7 - st, &v->env_wait);
        break;
    }
    case SPU_RELEASE:
        v->env = env_step(v->env, a2 >> 5 & 1, 1, a2 & 31, -8, &v->env_wait);
        if (v->env == 0)
            v->phase = SPU_OFF;
        break;
    }
}

/* Entering the block at v->addr: its loop-start flag, the IRQ address, and (for a voice that
 * sounds) its 28 samples. */
static void block(Psx *psx, SpuVoice *v, int decode)
{
    Spu *s = &psx->spu;
    const u8 *b = s->ram + v->addr;
    if ((b[1] & 4) && !v->ignore_loop)
        v->repeat = v->addr;
    if ((s->ctrl & 0x40) && s->irq_addr - v->addr < 16)
        irq(psx);
    if (!decode)
        return;
    memcpy(v->buf, v->buf + 28, 3 * sizeof v->buf[0]);
    int shift = b[0] & 15, f = b[0] >> 4 & 7;
    if (shift > 12)
        shift = 9;
    if (f > 4)
        f = 4;
    s32 k0 = K0[f], k1 = K1[f], h0 = v->hist[0], h1 = v->hist[1];
    for (int j = 0; j < 28; j++) {
        s32 x = ((s32)(s16)((b[2 + (j >> 1)] >> ((j & 1) * 4) & 15) << 12) >> shift) + ((h0 * k0 + h1 * k1 + 32) >> 6);
        x = clamp16(x);
        h1 = h0, h0 = x;
        v->buf[3 + j] = (s16)x;
    }
    v->hist[0] = (s16)h0, v->hist[1] = (s16)h1;
}

/* Leaving the block at v->addr: its loop-end flag (ENDX, jump to the repeat address, and without
 * the repeat flag the voice goes silent), then the next block. */
static void block_end(Psx *psx, SpuVoice *v, int i)
{
    Spu *s = &psx->spu;
    u8 f = s->ram[v->addr + 1];
    if (f & 1) {
        s->endx |= 1u << i;
        v->addr = v->repeat & 0x7FFF0;
        u32 non = G(0x1F801D94) | (u32)G(0x1F801D96) << 16;
        if (!(f & 2) && !(non >> i & 1))
            v->phase = SPU_OFF, v->env = 0;
    } else {
        v->addr = (v->addr + 16) & 0x7FFF0;
    }
    block(psx, v, v->phase != SPU_OFF);
}

static void key_on(Psx *psx, int i)
{
    Spu *s = &psx->spu;
    SpuVoice *v = &s->v[i];
    v->addr = ((u32)VR(i, 3) << 3) & 0x7FFF0;
    v->counter = 0;
    v->env = 0, v->env_wait = 0;
    v->phase = SPU_ATTACK;
    v->ignore_loop = 0;
    v->hist[0] = v->hist[1] = 0;
    memset(v->buf, 0, sizeof v->buf);
    s->endx &= ~(1u << i);
    block(psx, v, 1);
}

static void key_off(Spu *s, int i)
{
    SpuVoice *v = &s->v[i];
    if (v->phase != SPU_OFF)
        v->phase = SPU_RELEASE, v->env_wait = 0;
}

/* A silent voice still walks its blocks: ENDX, the repeat address and the IRQ address stay as
 * the hardware would have them. */
static void skip(Psx *psx, SpuVoice *v, int i, int n, u32 pitch)
{
    u32 c = v->counter + (pitch > 0x3FFF ? 0x4000 : pitch) * (u32)n;
    while (c >= 28u << 12) {
        c -= 28u << 12;
        block_end(psx, v, i);
    }
    v->counter = c;
}

/* Voice i over n samples: out[] gets its level (the next voice's modulation), the mixes its
 * share. */
static void voice(Psx *psx, int i, int n, const s16 *mod, s16 *out, const s16 *noise, s32 *dl, s32 *dr, s32 *rl, s32 *rr)
{
    Spu *s = &psx->spu;
    SpuVoice *v = &s->v[i];
    u32 bit = 1u << i;
    u32 pitch = VR(i, 2);
    int pmon = i && ((G(0x1F801D90) | (u32)G(0x1F801D92) << 16) & bit);
    int is_noise = ((G(0x1F801D94) | (u32)G(0x1F801D96) << 16) & bit) != 0;
    int rev = ((G(0x1F801D98) | (u32)G(0x1F801D9A) << 16) & bit) != 0;
    u16 vl = VR(i, 0), vr = VR(i, 1);
    int k = 0;
    while (v->phase != SPU_OFF && k < n) {
        u32 c = v->counter;
        s32 x;
        if (is_noise) {
            x = noise[k];
        } else {
            u32 g = c >> 4 & 0xFF;
            const s16 *b = v->buf + (c >> 12);
            x = (gauss[0xFF - g] * b[0] >> 15) + (gauss[0x1FF - g] * b[1] >> 15) + (gauss[0x100 + g] * b[2] >> 15) + (gauss[g] * b[3] >> 15);
        }
        if (--v->env_wait <= 0)
            adsr(s, v, i);
        x = clamp16(x * v->env >> 15);
        out[k] = (s16)x;
        s32 l = x * volume(vl, &v->vol[0]) >> 15, r = x * volume(vr, &v->vol[1]) >> 15;
        dl[k] += l, dr[k] += r;
        if (rev)
            rl[k] += l, rr[k] += r;
        u32 step = pitch;
        if (pmon)
            step = (step * (u32)(mod[k] + 0x8000) >> 15) & 0xFFFF;
        c += step > 0x3FFF ? 0x4000 : step;
        k++;
        if (c >= 28u << 12) {
            v->counter = c - (28u << 12);
            block_end(psx, v, i);
        } else {
            v->counter = c;
        }
    }
    if (k < n) {
        memset(out + k, 0, (size_t)(n - k) * sizeof *out);
        skip(psx, v, i, n - k, pitch);
    }
}

/* Reverb (psx-spx "SPU Reverb Formula"), one step at 22050 Hz over the work area mBASE..7FFFF. */
enum { dAPF1, dAPF2, vIIR, vCOMB1, vCOMB2, vCOMB3, vCOMB4, vWALL, vAPF1, vAPF2, mLSAME, mRSAME, mLCOMB1, mRCOMB1, mLCOMB2, mRCOMB2,
       dLSAME, dRSAME, mLDIFF, mRDIFF, mLCOMB3, mRCOMB3, mLCOMB4, mRCOMB4, dLDIFF, dRDIFF, mLAPF1, mRAPF1, mLAPF2, mRAPF2, vLIN, vRIN };
#define RV(k) ((s32)(s16)s->regs[(0x1DC0 - 0x1C00) / 2 + (k)])
#define RA(k) ((s32)s->regs[(0x1DC0 - 0x1C00) / 2 + (k)] * 4) /* halfwords */

static u32 rv_at(Spu *s, s32 off)
{
    s32 base = (s32)G(0x1F801DA2) * 4, size = 0x40000 - base;
    s32 a = (s32)s->rev_cur - base + off;
    if (a < 0 || a >= size) {
        a %= size;
        if (a < 0)
            a += size;
    }
    return (u32)(base + a) * 2;
}

static s32 rv_rd(Spu *s, s32 off) { return rd16(s->ram, rv_at(s, off)); }

static void rv_wr(Spu *s, s32 off, s32 x)
{
    if (s->ctrl & 0x80)
        wr16(s->ram, rv_at(s, off), clamp16(x));
}

static s32 rv_side(Spu *s, s32 in, int m, int d)
{
    s32 p = rv_rd(s, RA(m) - 1);
    s32 x = clamp16(in + (rv_rd(s, RA(d)) * RV(vWALL) >> 15) - p);
    x = (x * RV(vIIR) >> 15) + p;
    rv_wr(s, RA(m), x);
    return x;
}

static s32 rv_apf(Spu *s, s32 x, int m, int d, int vol)
{
    s32 t = rv_rd(s, RA(m) - RA(d)), g = RV(vol);
    x = clamp16(x - (g * t >> 15));
    rv_wr(s, RA(m), x);
    return clamp16((x * g >> 15) + t);
}

static void reverb(Spu *s, s32 inl, s32 inr)
{
    s32 lin = inl * RV(vLIN) >> 15, rin = inr * RV(vRIN) >> 15;
    rv_side(s, lin, mLSAME, dLSAME);
    rv_side(s, rin, mRSAME, dRSAME);
    rv_side(s, lin, mLDIFF, dRDIFF);
    rv_side(s, rin, mRDIFF, dLDIFF);
    s32 l = clamp16((RV(vCOMB1) * rv_rd(s, RA(mLCOMB1)) >> 15) + (RV(vCOMB2) * rv_rd(s, RA(mLCOMB2)) >> 15) +
                    (RV(vCOMB3) * rv_rd(s, RA(mLCOMB3)) >> 15) + (RV(vCOMB4) * rv_rd(s, RA(mLCOMB4)) >> 15));
    s32 r = clamp16((RV(vCOMB1) * rv_rd(s, RA(mRCOMB1)) >> 15) + (RV(vCOMB2) * rv_rd(s, RA(mRCOMB2)) >> 15) +
                    (RV(vCOMB3) * rv_rd(s, RA(mRCOMB3)) >> 15) + (RV(vCOMB4) * rv_rd(s, RA(mRCOMB4)) >> 15));
    l = rv_apf(s, rv_apf(s, l, mLAPF1, dAPF1, vAPF1), mLAPF2, dAPF2, vAPF2);
    r = rv_apf(s, rv_apf(s, r, mRAPF1, dAPF1, vAPF1), mRAPF2, dAPF2, vAPF2);
    s->rev_out[0] = l * (s16)G(0x1F801D84) >> 15;
    s->rev_out[1] = r * (s16)G(0x1F801D86) >> 15;
    u32 base = (u32)G(0x1F801DA2) * 4;
    s->rev_cur = (s->rev_cur + 1) & 0x3FFFF;
    if (s->rev_cur < base)
        s->rev_cur = base;
}

static void flush(Psx *psx)
{
    Spu *s = &psx->spu;
    if (s->nout && psx->host->audio)
        psx->host->audio(psx->host->user, s->out, s->nout);
    s->nout = 0;
}

static void render(Psx *psx, int n)
{
    static s32 dl[SPU_CHUNK], dr[SPU_CHUNK], rl[SPU_CHUNK], rr[SPU_CHUNK];
    static s16 noise[SPU_CHUNK], va[SPU_CHUNK], vb[SPU_CHUNK], cap1[SPU_CHUNK], cap3[SPU_CHUNK];
    Spu *s = &psx->spu;
    size_t bytes = (size_t)n * sizeof dl[0];
    memset(dl, 0, bytes), memset(dr, 0, bytes), memset(rl, 0, bytes), memset(rr, 0, bytes);
    if (G(0x1F801D94) | G(0x1F801D96)) {
        int step = (s->ctrl >> 8 & 3) + 4, shift = s->ctrl >> 10 & 15;
        for (int k = 0; k < n; k++) {
            u32 lv = (u16)s->noise;
            s->noise_timer -= step;
            if (s->noise_timer < 0) {
                s->noise = (s16)(lv << 1 | (((lv >> 15) ^ (lv >> 12) ^ (lv >> 11) ^ (lv >> 10) ^ 1) & 1));
                s->noise_timer += 0x20000 >> shift;
                if (s->noise_timer < 0)
                    s->noise_timer += 0x20000 >> shift;
            }
            noise[k] = (s16)s->noise;
        }
    }
    s16 *mod = va, *out = vb;
    memset(mod, 0, (size_t)n * sizeof *mod);
    for (int i = 0; i < 24; i++) {
        voice(psx, i, n, mod, out, noise, dl, dr, rl, rr);
        if (i == 1 || i == 3)
            memcpy(i == 1 ? cap1 : cap3, out, (size_t)n * sizeof *out);
        s16 *t = mod;
        mod = out, out = t;
    }
    s32 cdl = (s16)G(0x1F801DB0), cdr = (s16)G(0x1F801DB2);
    u16 ml = G(0x1F801D80), mr = G(0x1F801D82);
    for (int k = 0; k < n; k++) {
        s32 cl, cr;
        cd_audio(psx, &cl, &cr);
        u32 c = s->capture * 2;
        wr16(s->ram, c, cl), wr16(s->ram, 0x400 + c, cr), wr16(s->ram, 0x800 + c, cap1[k]), wr16(s->ram, 0xC00 + c, cap3[k]);
        if ((s->ctrl & 0x40) && s->irq_addr < 0x1000 && (s->irq_addr & 0x3FF) == c)
            irq(psx);
        s->capture = (s->capture + 1) & 0x1FF;
        s32 l = dl[k], r = dr[k], vl = rl[k], vr = rr[k];
        if (s->ctrl & 1) {
            cl = cl * cdl >> 15, cr = cr * cdr >> 15;
            l += cl, r += cr;
            if (s->ctrl & 4)
                vl += cl, vr += cr;
        }
        s->rev_in[0] += vl, s->rev_in[1] += vr;
        if (!(s->rev_odd ^= 1)) {
            reverb(s, clamp16(s->rev_in[0] >> 1), clamp16(s->rev_in[1] >> 1));
            s->rev_in[0] = s->rev_in[1] = 0;
        }
        l = clamp16(l + s->rev_out[0]) * volume(ml, &s->main[0]) >> 15;
        r = clamp16(r + s->rev_out[1]) * volume(mr, &s->main[1]) >> 15;
        if (!(s->ctrl & 0x4000)) /* not unmuted */
            l = r = 0;
        s->out[2 * s->nout] = (s16)l, s->out[2 * s->nout + 1] = (s16)r;
        if (++s->nout == (int)(sizeof s->out / sizeof s->out[0] / 2))
            flush(psx);
    }
    s->stat = (u16)((s->stat & ~0x800u) | (s->capture >= 0x100) << 11);
}

/* Runs the SPU up to the CPU's time. */
static void sync(Psx *psx)
{
    Spu *s = &psx->spu;
    if (psx->cycles < s->last + CYCLES_PER_SAMPLE)
        return;
    u64 d = psx->cycles - s->last;
    if (d > (u64)CYCLES_PER_SAMPLE * 44100) { /* more than a second: the machine jumped */
        s->last = psx->cycles - (u64)CYCLES_PER_SAMPLE * 44100;
        d = (u64)CYCLES_PER_SAMPLE * 44100;
    }
    u32 n = (u32)d / CYCLES_PER_SAMPLE;
    s->last += (u64)n * CYCLES_PER_SAMPLE;
    u32 t0 = psx->host->usec ? psx->host->usec(psx->host->user) : 0;
    while (n) {
        u32 k = n > SPU_CHUNK ? SPU_CHUNK : n;
        render(psx, (int)k);
        n -= k;
    }
    if (psx->host->usec)
        s->us_mix += psx->host->usec(psx->host->user) - t0;
}

static void status(Spu *s)
{
    u32 mode = (s->ctrl >> 4) & 3;
    if (!(s->ctrl & 0x40))
        s->stat &= ~0x40u; /* IRQ acknowledged */
    s->stat = (u16)((s->stat & 0x840) | (s->ctrl & 0x3F) | ((s->ctrl >> 5) & 1) << 7 | (mode == 2) << 8 | (mode == 3) << 9);
}

u16 spu_read(Psx *psx, u32 addr)
{
    Spu *s = &psx->spu;
    u32 off = addr - 0x1F801C00u;
    if (off < 0x180 && (off & 0xC) == 0xC) {
        sync(psx);
        SpuVoice *v = &s->v[off >> 4];
        return (off & 2) ? (u16)(v->repeat >> 3) : (u16)v->env;
    }
    if (addr >= 0x1F801E00 && addr < 0x1F801E60) { /* current voice volumes */
        SpuSweep *w = &s->v[(addr - 0x1F801E00) >> 2].vol[(addr >> 1) & 1];
        return (u16)w->level;
    }
    switch (addr) {
    case 0x1F801D9C: sync(psx); return (u16)s->endx;
    case 0x1F801D9E: sync(psx); return (u16)(s->endx >> 16);
    case 0x1F801DA6: return (u16)(s->xfer_addr >> 3);
    case 0x1F801DAA: return s->ctrl;
    case 0x1F801DAE: return s->stat;
    case 0x1F801DB8: return (u16)s->main[0].level;
    case 0x1F801DBA: return (u16)s->main[1].level;
    }
    return R(addr);
}

static void transfer(Psx *psx, u16 v)
{
    Spu *s = &psx->spu;
    u32 a = s->xfer_addr & 0x7FFFE;
    wr16(s->ram, a, v);
    if ((s->ctrl & 0x40) && a == s->irq_addr)
        irq(psx);
    s->xfer_addr = (a + 2) & 0x7FFFF;
}

void spu_write(Psx *psx, u32 addr, u16 v)
{
    Spu *s = &psx->spu;
    u32 off = addr - 0x1F801C00u;
    R(addr) = v;
    if (off < 0x180) {
        SpuVoice *vc = &s->v[off >> 4];
        switch (off & 0xE) {
        case 0x0: set_volume(&vc->vol[0], v); break;
        case 0x2: set_volume(&vc->vol[1], v); break;
        case 0xC: vc->env = v & 0x7FFF; break;
        case 0xE:
            vc->repeat = ((u32)v << 3) & 0x7FFF0;
            vc->ignore_loop = vc->phase != SPU_OFF;
            break;
        }
        return;
    }
    switch (addr) {
    case 0x1F801D80: set_volume(&s->main[0], v); break;
    case 0x1F801D82: set_volume(&s->main[1], v); break;
    case 0x1F801D88:
    case 0x1F801D8A:
        sync(psx);
        for (int i = 0; i < 16; i++)
            if (v >> i & 1)
                key_on(psx, i + (addr == 0x1F801D8A) * 16);
        break;
    case 0x1F801D8C:
    case 0x1F801D8E:
        sync(psx);
        for (int i = 0; i < 16; i++)
            if (v >> i & 1)
                key_off(s, i + (addr == 0x1F801D8E) * 16);
        break;
    case 0x1F801DA2: s->rev_cur = (u32)v * 4; break;
    case 0x1F801DA4: s->irq_addr = (u32)v << 3; break;
    case 0x1F801DA6: s->xfer_addr = (u32)v << 3; break;
    case 0x1F801DA8: transfer(psx, v); break;
    case 0x1F801DAA:
        sync(psx);
        if ((v & 0x40) && !(s->ctrl & 0x40)) {
            static int told;
            if (!told++)
                psx_log(psx, "spu: IRQ enabled at %05X", (unsigned)s->irq_addr);
        }
        s->ctrl = v;
        status(s);
        break;
    }
}

void spu_dma_write(Psx *psx, u32 v)
{
    transfer(psx, (u16)v);
    transfer(psx, (u16)(v >> 16));
}

u32 spu_dma_read(Psx *psx)
{
    Spu *s = &psx->spu;
    u32 v = 0;
    for (int i = 0; i < 2; i++) {
        u32 a = s->xfer_addr & 0x7FFFE;
        v |= (u32)(s->ram[a] | s->ram[a + 1] << 8) << (16 * i);
        s->xfer_addr = (s->xfer_addr + 2) & 0x7FFFF;
    }
    return v;
}

void spu_run(Psx *psx)
{
    sync(psx);
    flush(psx);
}
