/*
 * psxrt: the SPU's ADPCM voices played by sceSasCore, the PSP's own voice mixer, which runs on
 * the Media Engine.
 *
 * The core (core/spu.c) offers each voice at its key on (spu_hw_mode's voice) and describes it as
 * a VAG would hold it (SpuHwVoice). Taken voices keep running in the SPU without their samples:
 * blocks, loop and IRQ addresses, ENDX, envelope levels and volume sweeps stay the SPU's, so
 * everything the guest reads is as before. Only decoding, interpolation and mixing move here.
 *
 * The VAG player knows other loop flags than the SPU: it stops at 7, loops at 3, and takes 6 as
 * the loop start. The voices therefore play a mirror of SPU RAM whose flag bytes are rewritten
 * (spu_hw_flag), each voice with its exact length; the core only offers loops the player can
 * follow, and takes a voice back when the guest moves its loop, rewrites its flags or turns on
 * something only the SPU has (pitch modulation, noise, reverb).
 *
 * Time: the emulation thread queues timed commands (sample `t` of the audio stream); the audio
 * thread applies them at the grain nearest to `t` and mixes each 256-sample grain into the
 * stream with __sceSasCoreWithMix, before the resampler reads it. Pages of SPU RAM that the
 * guest writes go the same way, copied at the time of the write, so a voice reading a buffer
 * the guest refills hears what the SPU would have.
 */
#include <pspkernel.h>
#include <pspsascore.h>
#include <psputility.h>
#include <stdio.h>
#include <string.h>

#include "sas.h"

#define GRAIN 256
#define CMDS 4096 /* a power of two */
#define SLOTS 64  /* 1 KB page copies in flight, a power of two */
#define UNCACHED 0x40000000u
#define MODULE_LOADED 0x80111102u

enum { C_SET, C_ADSR, C_PITCH, C_VOL, C_KON, C_KOFF, C_STOP, C_PAGE };

typedef struct {
    u32 t;
    u8 op, voice;
    u16 a;
    s32 b, c;
} Cmd;

static SceSasCore core __attribute__((aligned(64)));
static s16 scratch[GRAIN * 4] __attribute__((aligned(64))); /* mode 2: the grain, not heard */
static Psx *machine;
static int mode;
static u8 *mirror; /* SPU RAM with VAG flag bytes */
static u8 *slots;  /* page copies */
static Cmd cmds[CMDS];
static volatile u32 cmd_wr, cmd_rd, slot_wr, slot_rd;

/* the emulation thread: what each voice was last given */
static struct {
    u16 pitch, a1, a2;
    s16 l, r;
    u8 env; /* taken mid-play: the envelope is in the volume */
} sent[24];

/* the audio thread */
static u32 pos;      /* samples mixed */
static u32 playing;  /* keyed on and not ended */
static u32 kon_wait; /* key ons the mixer refused (a voice still busy): tried again each grain */
static s32 last_err;
static int last_op;

/* counters, running totals: the log line takes the second's differences */
typedef struct {
    u32 on, adopted, refused[SPU_UNFIT_N], busy, back, cmds, lost, pages, live, errors, retries, grains, us;
    u64 e[2];
    u32 n;
} Stats;
static Stats st, st0;

static u16 vag_pitch(u16 p) { return p > 0x3FFF ? 0x4000 : p ? p : 1; }

static void push(u32 t, int op, int v, u32 a, s32 b, s32 c)
{
    u32 w = cmd_wr;
    if (w - cmd_rd >= CMDS) {
        st.lost++;
        return;
    }
    Cmd *k = &cmds[w & (CMDS - 1)];
    k->t = t, k->op = (u8)op, k->voice = (u8)v, k->a = (u16)a, k->b = b, k->c = c;
    __asm__ volatile("" ::: "memory"); /* the command before the index that publishes it */
    cmd_wr = w + 1;
    st.cmds++;
}

/* The pages the guest wrote since the last call, copied now and mirrored at `t`. */
static void pages(u32 t)
{
    u32 dirty[16];
    if (!spu_hw_dirty(dirty))
        return;
    for (int w = 0; w < 16; w++)
        for (u32 m = dirty[w]; m; m &= m - 1) {
            u32 p = (u32)w * 32 + (u32)__builtin_ctz(m);
            if (cmd_wr - cmd_rd >= CMDS) {
                st.lost++;
                continue;
            }
            s32 slot = -1;
            if (slot_wr - slot_rd < SLOTS) {
                slot = (s32)(slot_wr & (SLOTS - 1));
                memcpy(slots + slot * 1024, machine->spu.ram + p * 1024, 1024);
                slot_wr++;
            } else {
                st.live++; /* too many at once: the audio thread copies what is there then */
            }
            push(t, C_PAGE, 0, p, slot, 0);
            st.pages++;
        }
}

const char *sas_init(Psx *psx, int m, void *(*alloc)(const char *name, u32 size))
{
    int r = sceUtilityLoadModule(PSP_MODULE_AV_AVCODEC);
    if (r < 0 && (u32)r != MODULE_LOADED)
        return "the avcodec module does not load";
    r = sceUtilityLoadModule(PSP_MODULE_AV_SASCORE);
    if (r < 0 && (u32)r != MODULE_LOADED)
        return "the sascore module does not load";
    mirror = alloc("psxrt sas", 512 * 1024 + SLOTS * 1024);
    if (!mirror)
        return "no memory for the SPU RAM mirror";
    slots = mirror + 512 * 1024;
    if (__sceSasInit(&core, GRAIN, 32, PSP_SAS_OUTPUTMODE_STEREO, PSP_SAS_SAMPLE_RATE) < 0)
        return "__sceSasInit failed";
    __sceSasRevType(&core, PSP_SAS_EFFECT_TYPE_OFF);
    __sceSasRevVON(&core, 1, 0); /* dry only */
    machine = psx, mode = m;
    return NULL;
}

int sas_voice(int i, int ev, u32 t)
{
    if (ev == SPU_HW_OFF) {
        push(t, C_KOFF, i, 0, 0, 0);
        return 0;
    }
    if (ev == SPU_HW_BACK) {
        push(t, C_STOP, i, 0, 0, 0);
        st.back++;
        return 0;
    }
    const SpuHwVoice *h = spu_hw_voice(i);
    if (h->unfit) {
        st.refused[h->unfit]++;
        return 0;
    }
    pages(t); /* what the voice will read */
    if (cmd_wr - cmd_rd > CMDS - 8) {
        st.busy++;
        return 0;
    }
    const Spu *s = &machine->spu;
    const u16 *reg = s->regs + i * 8;
    sent[i].env = (u8)h->adopted;
    /* taken mid-play: the mixer's envelope at full level within three samples and held, the
     * SPU's in the volume. The decay steps once even towards a sustain level at the top, and at
     * rate 0 that step halves the level (as the SPU's would): rate 15 takes 1/65536 off. */
    sent[i].a1 = h->adopted ? 0x00FF : reg[4];
    sent[i].a2 = h->adopted ? 0x1FC0 : reg[5];
    sent[i].pitch = vag_pitch(reg[2]);
    sent[i].l = (s16)(spu_hw_volume(s, i, 0) >> 3), sent[i].r = (s16)(spu_hw_volume(s, i, 1) >> 3);
    push(t, C_SET, i, (u32)h->loop, (s32)h->start, (s32)h->size);
    push(t, C_ADSR, i, 0, sent[i].a1, sent[i].a2 & ~0x2000); /* bit 13 is unused and must be 0 */
    push(t, C_PITCH, i, sent[i].pitch, 0, 0);
    push(t, C_VOL, i, 0, sent[i].l, sent[i].r);
    push(t, C_KON, i, 0, 0, 0);
    if (h->adopted)
        st.adopted++;
    else
        st.on++;
    return 1;
}

void sas_tick(u32 t)
{
    pages(t);
    const Spu *s = &machine->spu;
    for (u32 m = spu_hw_voices(); m; m &= m - 1) {
        int i = __builtin_ctz(m);
        const u16 *reg = s->regs + i * 8;
        u16 p = vag_pitch(reg[2]);
        if (p != sent[i].pitch) {
            sent[i].pitch = p;
            push(t, C_PITCH, i, p, 0, 0);
        }
        s16 l = (s16)(spu_hw_volume(s, i, 0) >> 3), r = (s16)(spu_hw_volume(s, i, 1) >> 3);
        if (l != sent[i].l || r != sent[i].r) {
            sent[i].l = l, sent[i].r = r;
            push(t, C_VOL, i, 0, l, r);
        }
        if (!sent[i].env && (reg[4] != sent[i].a1 || reg[5] != sent[i].a2)) {
            sent[i].a1 = reg[4], sent[i].a2 = reg[5];
            push(t, C_ADSR, i, 0, reg[4], reg[5] & ~0x2000);
        }
    }
}

/* ---- the audio thread ---- */

static void page(u32 p, const u8 *src)
{
    u8 *d = mirror + p * 1024;
    memcpy(d, src, 1024);
    for (int b = 1; b < 1024; b += 16)
        d[b] = spu_hw_flag(d[b]);
    sceKernelDcacheWritebackRange(d, 1024);
}

static void apply(const Cmd *k)
{
    int v = k->voice, r = 0;
    u32 bit = 1u << v;
    switch (k->op) {
    case C_PAGE:
        if (k->b >= 0) {
            page(k->a, slots + k->b * 1024);
            slot_rd++;
        } else {
            page(k->a, machine->spu.ram + (u32)k->a * 1024);
        }
        return;
    case C_SET:
        kon_wait &= ~bit;
        r = __sceSasSetVoice(&core, v, (void *)((u32)(mirror + k->b) | UNCACHED), k->c, k->a);
        break;
    case C_ADSR: r = __sceSasSetSimpleADSR(&core, v, (u32)k->b, (u32)k->c); break;
    case C_PITCH: r = __sceSasSetPitch(&core, v, k->a); break;
    case C_VOL: r = __sceSasSetVolume(&core, v, k->b, k->c, 0, 0); break;
    case C_KON:
        if (__sceSasSetKeyOn(&core, v) < 0)
            kon_wait |= bit; /* still sounding its last note: the next grain */
        else
            playing |= bit;
        return;
    case C_KOFF:
        kon_wait &= ~bit;
        __sceSasSetKeyOff(&core, v); /* fails when it has ended already */
        return;
    case C_STOP:
        kon_wait &= ~bit;
        __sceSasSetVolume(&core, v, 0, 0, 0, 0);
        __sceSasSetKeyOff(&core, v);
        return;
    }
    if (r < 0)
        st.errors++, last_err = r, last_op = k->op;
}

u32 sas_mix(s16 *ring, u32 size, u32 wr)
{
    while (wr - pos >= GRAIN) {
        u32 r = cmd_rd, w = cmd_wr;
        __asm__ volatile("" ::: "memory"); /* the index before the commands it publishes */
        for (; r != w && (s32)(cmds[r & (CMDS - 1)].t - (pos + GRAIN / 2)) < 0; r++)
            apply(&cmds[r & (CMDS - 1)]);
        cmd_rd = r;
        for (u32 m = kon_wait; m; m &= m - 1) {
            int v = __builtin_ctz(m);
            if (__sceSasSetKeyOn(&core, v) >= 0)
                kon_wait &= ~(1u << v), playing |= 1u << v;
            st.retries++;
        }
        if (playing) {
            u32 t0 = sceKernelGetSystemTimeLow();
            if (mode == 1) {
                s16 *seg = ring + (pos & (size - 1)) * 2;
                sceKernelDcacheWritebackInvalidateRange(seg, GRAIN * 4);
                __sceSasCoreWithMix(&core, seg, PSP_SAS_VOLUME_MAX, PSP_SAS_VOLUME_MAX);
            } else {
                sceKernelDcacheWritebackInvalidateRange(scratch, sizeof scratch);
                __sceSasCore(&core, scratch);
                for (int k = 0; k < GRAIN; k++)
                    st.e[0] += (u64)((s32)scratch[2 * k] * scratch[2 * k]), st.e[1] += (u64)((s32)scratch[2 * k + 1] * scratch[2 * k + 1]);
            }
            playing &= ~__sceSasGetEndFlag(&core);
            st.us += sceKernelGetSystemTimeLow() - t0;
            st.grains++;
        }
        st.n += GRAIN;
        pos += GRAIN;
    }
    return pos;
}

static u32 isqrt(u32 x)
{
    u32 r = 0;
    for (u32 b = 1u << 30; b; b >>= 2)
        if (x >= r + b)
            x -= r + b, r = (r >> 1) + b;
        else
            r >>= 1;
    return r;
}

static u32 rms(u64 e, u32 n) { return n ? isqrt((u32)(e / n)) : 0; }

int sas_log(char *line, int cap, u32 *us)
{
    Stats d = st; /* the audio thread's counters may move meanwhile: a second's skew at most */
    u64 e[2];
    u32 n;
    spu_hw_energy(e, &n);
#define D(f) (unsigned)(d.f - st0.f)
    int len = snprintf(line, (size_t)cap,
                       "-- sas: voices %06X on %u adopted %u back %u refused pmon %u noise %u reverb %u loop %u busy %u;"
                       " cmds %u lost %u pages %u (live %u) errors %u (last %08X op %d) retries %u; grains %u %u ms",
                       (unsigned)spu_hw_voices(), D(on), D(adopted), D(back), D(refused[SPU_UNFIT_PMON]), D(refused[SPU_UNFIT_NOISE]),
                       D(refused[SPU_UNFIT_REVERB]), D(refused[SPU_UNFIT_LOOP]), D(busy), D(cmds), D(lost), D(pages), D(live), D(errors),
                       (unsigned)last_err, last_op, D(retries), D(grains), (unsigned)(d.us - st0.us) / 1000);
    if (mode == 2 && len < cap)
        len += snprintf(line + len, (size_t)(cap - len), "; rms spu %u/%u sas %u/%u", (unsigned)rms(e[0], n), (unsigned)rms(e[1], n),
                        (unsigned)rms(d.e[0] - st0.e[0], d.n - st0.n), (unsigned)rms(d.e[1] - st0.e[1], d.n - st0.n));
    if (len < cap)
        len += snprintf(line + len, (size_t)(cap - len), "\n");
#undef D
    *us = d.us - st0.us;
    st0 = d;
    return len < cap ? len : cap - 1;
}
