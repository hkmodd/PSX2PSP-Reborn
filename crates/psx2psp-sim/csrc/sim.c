/* The simulator's side of the psxrt core: owns the machine and its memories, runs the
 * interpreter by frames, and exposes VRAM and the display mode to the Rust driver. */
#include "psx.h"
#include "floating.h"
#include "coderun.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define MDEC_CAP 0x10000 /* words: a whole compressed frame */

void ge_shadow_init(Psx *psx);             /* psp/psxrt/ge_host_pc.c */
void ge_shadow_frame(Psx *psx, u32 frame); /* reads psx, writes only the log */
static int sim_ge;                         /* PSX2PSP_SIM_GE, read once in sim_new */

typedef int (*ReadFn)(void *user, u32 lba, u8 *raw);
typedef void (*TextFn)(void *user, const char *s, int n);
typedef u32 (*SelectFn)(void *user, u32 n); /* sectors of disc n, now in the drive; 0: none */

typedef struct Sim {
    Psx psx;
    PsxHost host;
    TextFn log_fn;
    void *user;
    u8 ram[RAM_SIZE];
    u8 scratch[SCRATCH_SIZE];
    u16 vram[VRAM_W * VRAM_H];
    u8 spu_ram[512 * 1024];
    u32 mdec_in[MDEC_CAP];
    u32 frames;
    u8 card[CARD_SIZE];
} Sim;

static void log_line(void *user, const char *line)
{
    Sim *s = (Sim *)user;
    s->log_fn(s->user, line, (int)strlen(line));
}

static void vblank(void *user) { ((Sim *)user)->frames++; }

/* PSX2PSP_SIM_AUDIO=<file>: the SPU's output as raw 44100 Hz stereo s16le, for offline checks. */
static FILE *audio_file;

static void audio(void *user, const s16 *samples, int frames)
{
    (void)user;
    fwrite(samples, 4, (size_t)frames, audio_file);
}

static void *rust_user;
static ReadFn rust_read;
static TextFn rust_tty;
static SelectFn rust_select;

static int read_fwd(void *user, u32 lba, u8 *raw)
{
    (void)user;
    return rust_read(rust_user, lba, raw);
}

static void tty_fwd(void *user, const char *s, int n)
{
    (void)user;
    rust_tty(rust_user, s, n);
}

/* The RAM psxrt_pc_trace reads the instructions that run in (ran_word). */
static const u8 *ran_ram;

/* PSX2PSP_SIM_WATCH: the words watched, their last value, the instruction that ran last, the frame. */
static u32 watchw_at[8], watchw_val[8], watchw_pc, watchw_frame;
static int nwatchw;

/* PSX2PSP_SIM_SPUME=1|r<seed>: the SPU through psxrt's front and back (SpuMe, core/psx.h) in this
 * thread, as the PSP's ME runs it. 1: the back runs up to every beacon; r: it runs at random
 * beacons, a random number of samples ahead, so the mix is cut where inline never cuts it. The
 * audio (PSX2PSP_SIM_AUDIO) must be inline's bit for bit. */
static SpuMe sme;
static Psx sme_bk;
static PsxHost sme_host;
static SpuEv sme_ring[1024];
static volatile u32 sme_wr, sme_rd;
static int sme_random;
static u32 sme_seed, sme_runs;

static u32 sme_rand(void) { return sme_seed = sme_seed * 1664525u + 1013904223u; }

static void sme_kick(SpuMe *m, u64 cycle, int why)
{
    if (why == SPU_ME_OFF)
        return;
    if (why == SPU_ME_BEACON) {
        if (sme_random && (sme_rand() >> 28) > 3) /* 1 in 4 */
            return;
        sme_runs++;
        spu_me_run(m, cycle, sme_random ? 1 + (sme_rand() >> 23) : 0);
        return;
    }
    spu_me_run(m, cycle, 0);
}

static void sme_on(Sim *s)
{
    const char *e = getenv("PSX2PSP_SIM_SPUME");
    if (!e)
        return;
    sme_random = *e == 'r';
    sme_seed = sme_random ? (u32)strtoul(e + 1, NULL, 10) : 0;
    sme.ring = sme_ring, sme.size = sizeof sme_ring / sizeof sme_ring[0];
    sme.wr = &sme_wr, sme.rd = &sme_rd;
    sme.bk = &sme_bk, sme.bk_view = &sme_bk;
    sme.ram_view = s->spu_ram;
    sme.xa_view = s->psx.cd.xa_fifo;
    sme.kick = sme_kick;
    if (!spu_me_on(&s->psx, &sme)) {
        log_line(s, "spume: refused (host mixer or IRQ on)");
        return;
    }
    sme_host = s->host;
    sme_host.log = NULL; /* the ME has no log */
    sme_bk.host = &sme_host;
    log_line(s, sme_random ? "spume: on, the back runs at random beacons" : "spume: on, the back runs at every beacon");
}

static void sme_report(Sim *s)
{
    if (!sme.kick)
        return;
    char line[160];
    snprintf(line, sizeof line, "spume: %s; waits %u, ring full %u, IRQ fallbacks %u, beacon runs %u", spu_me ? "on" : "off",
             (unsigned)sme.waits, (unsigned)sme.fulls, (unsigned)sme.offs, (unsigned)sme_runs);
    log_line(s, line);
}

/* PSX2PSP_SIM_MEWALK=1: the GPU's lists as psxrt's ME walks them (psp/psxrt/me.c, me_walk and
 * walk_end): on a copy of the Psx with its own Gpu, and the CPU goes on until an entry of the GPU
 * waits for it (gpu_sync), with GPUSTAT read from gpu_list_scan meanwhile and the walk's GPU IRQ
 * raised at the wait. Here the walk runs whole at the kick and its Gpu comes back at the wait.
 * Pictures and audio must be the walk on the CPU's; the waits by cause are the PSP's (2:1 gp1 to
 * vblank on Sheep Dog, as its L2c log). */
static Psx mw_psx, *mw_game;
static PsxHost mw_host;
static u32 mw_writes, mw_walks, mw_cpu_walks, mw_waits[GPU_SYNC_N], mw_stat_bad, mw_words_bad;
static int mw_init;

static void mw_sync(int why)
{
    if (!gpu_me_busy)
        return;
    Psx *psx = mw_game;
    psx->gpu = mw_psx.gpu;
    gpu_writes += mw_writes;
    if (mw_psx.i_stat & (1u << IRQ_GPU))
        psx_raise(psx, IRQ_GPU);
    mw_stat_bad += gpu_me_stat_ok && (psx->gpu.stat != gpu_me_stat || psx->gpu.xfer);
    mw_waits[why]++;
    gpu_me_busy = 0;
}

static u32 mw_walk(Psx *psx, u32 addr)
{
    mw_sync(GPU_SYNC_DMA);
    if (psx->trace) {
        mw_cpu_walks++;
        return dma_list_walk(psx, addr);
    }
    u32 words = gpu_list_scan(psx, addr, &gpu_me_stat, &gpu_me_stat_ok);
    if (!mw_init)
        mw_psx = *psx, mw_init = 1;
    mw_psx.gpu = psx->gpu;
    mw_psx.ram = psx->ram;
    mw_psx.trace = 0;
    mw_psx.i_stat = 0;
    mw_host = *psx->host;
    mw_host.log = NULL;
    mw_host.usec = NULL;
    mw_psx.host = &mw_host;
    mw_writes = 0;
    mw_words_bad += dma_list_walk(&mw_psx, addr) != words;
    mw_game = psx;
    gpu_me_head = addr;
    gpu_me_busy = 1;
    mw_walks++;
    return words;
}

static void mw_on(void)
{
    const char *e = getenv("PSX2PSP_SIM_MEWALK");
    if (!e)
        return;
    gpu_me_psx = &mw_psx;
    gpu_me_writes = &mw_writes;
    gpu_sync_hook = mw_sync;
    dma_list_hook = mw_walk;
}

static void mw_report(Sim *s)
{
    if (!dma_list_hook)
        return;
    char line[256];
    int n = snprintf(line, sizeof line, "mewalk: walks %u (%u on the CPU), words bad %u, stat bad %u, reads kept %u; waits",
                     (unsigned)mw_walks, (unsigned)mw_cpu_walks, (unsigned)mw_words_bad, (unsigned)mw_stat_bad,
                     (unsigned)gpu_me_reads_kept);
    static const char *const why[GPU_SYNC_N] = {"read", "gp0", "gp1", "vblank", "dma", "ge", "other"};
    for (int k = 0; k < GPU_SYNC_N && n < (int)sizeof line; k++)
        n += snprintf(line + n, sizeof line - (size_t)n, " %s %u", why[k], (unsigned)mw_waits[k]);
    log_line(s, line);
    dma_list_hook = NULL, gpu_sync_hook = NULL, gpu_me_psx = NULL, gpu_me_writes = NULL;
}

Sim *sim_new(void *user, ReadFn rd, u32 disc_end, TextFn tty, TextFn log)
{
    Sim *s = (Sim *)calloc(1, sizeof(Sim));
    if (!s)
        return NULL;
    rust_user = user, rust_read = rd, rust_tty = tty;
    s->user = user;
    s->log_fn = log;
    s->host.user = s;
    s->host.read_sector = read_fwd;
    s->host.disc_end = disc_end;
    s->host.tty = tty_fwd;
    s->host.log = log_line;
    s->host.vblank = vblank;
    const char *audio_path = getenv("PSX2PSP_SIM_AUDIO");
    if (audio_path && !audio_file && (audio_file = fopen(audio_path, "wb")) != NULL)
        s->host.audio = audio;
    /* PSX2PSP_SIM_CARD=<file>: a card in slot 1, read from the file (formatted when it is not a
     * 128 KB image); the simulator never writes it back. */
    const char *card_path = getenv("PSX2PSP_SIM_CARD");
    if (card_path) {
        FILE *cf = fopen(card_path, "rb");
        size_t n = cf ? fread(s->card, 1, CARD_SIZE, cf) : 0;
        if (cf)
            fclose(cf);
        if (n != CARD_SIZE)
            card_format(s->card);
        s->host.card[0] = s->card;
    }
    psx_init(&s->psx, &s->host, s->ram, s->scratch, s->vram, s->spu_ram, s->mdec_in, MDEC_CAP);
    ran_ram = s->ram;
    /* PSX2PSP_SIM_WATCH=ADDR,...: RAM words (hex, KSEG0/1 or physical, up to 8) whose every
     * change is printed with the instruction before it (a DMA's shows the last one that ran). */
    nwatchw = 0;
    for (const char *p = getenv("PSX2PSP_SIM_WATCH"); p && *p && nwatchw < 8;) {
        char *end;
        u32 a = (u32)strtoul(p, &end, 16) & (RAM_SIZE - 4);
        if (end == p)
            break;
        watchw_at[nwatchw] = a;
        memcpy(&watchw_val[nwatchw++], s->ram + a, 4);
        p = *end == ',' ? end + 1 : end;
    }
    /* PSX2PSP_SIM_GE=1: the PSP's GE translator (ge.c) runs beside the software rasterizer and
     * logs a hash of its display lists each frame (psp/psxrt/ge_host_pc.c); pictures unchanged. */
    if (getenv("PSX2PSP_SIM_GE")) {
        sim_ge = 1;
        ge_shadow_init(&s->psx);
    }
    sme_on(s);
    mw_on();
    return s;
}

static void watch_reset(void);

void sim_free(Sim *s)
{
    sme_report(s);
    mw_report(s);
    if (spu_me) /* what the back mixed after the last vblank stays unplayed, as inline */
        spu_me_off(&s->psx);
    watch_reset();
    free(s);
}

/* --trace: the straight-line runs the interpreter executes, as (first pc, last pc) pairs of u32;
 * a run equal to the one before it is not written again (a loop of one run stays in the I-cache
 * after its first pass), but its instructions count. */
static FILE *trace_file;
static u32 tr_first, tr_last, tr_prev[2], tr_buf[8192];
static int tr_n;
static unsigned long long tr_insns, tr_runs;

static void trace_put(void)
{
    tr_insns += (tr_last - tr_first) / 4 + 1;
    if (tr_first == tr_prev[0] && tr_last == tr_prev[1])
        return;
    tr_prev[0] = tr_first, tr_prev[1] = tr_last, tr_runs++;
    tr_buf[tr_n++] = tr_first, tr_buf[tr_n++] = tr_last;
    if (tr_n == 8192)
        fwrite(tr_buf, 4, (size_t)tr_n, trace_file), tr_n = 0;
}

/* Instructions run at each RAM word while a trace is written, written beside it as FILE.cnt (2 MB
 * of RAM as 512k little-endian u32). */
static u32 pc_count[RAM_SIZE / 4];

/* Words of RAM the interpreter ran since sim_ran_clear, 1 byte per word: the converter's code
 * capture takes code that ran. ran_word: the instruction each last ran as, read when it ran (a
 * load over it before the RAM is read would give the words of another file: Spider-Man loads
 * a sound bank over the module it ran). */
static u8 ran[RAM_SIZE / 4];
static u32 ran_word[RAM_SIZE / 4];

/* Static images watched as the native runtime takes them (native.c try_load, on_ram_loaded,
 * cache_flushed): a word holds a translation from when the guest first runs in an image's text
 * after boot, or after a load over it, and the image's head and text hash as the translation's,
 * until a load over that word, or an I-cache flush that finds its 1 KB block changed beyond
 * immediates and shift amounts. ran_held: words that ran while they held one. */
#define WATCH_MAX 256 /* native_code::MAX_IMAGES */
static struct Watch {
    u32 at, size, head_len, head_hash, hash;
    u8 *words; /* 1 per word with a translation */
    u8 *miss;  /* the text at its last failed check, or the last whose head matched */
    u32 checks;
    u8 armed, live; /* live: matched at its last check */
    u8 miss_head;   /* the head matched at that check */
} watch[WATCH_MAX];
static int nwatch;
static const u8 *watch_ram;
static u16 watch_page[RAM_SIZE >> 12]; /* armed images over each 4 KB page */
static u8 held[RAM_SIZE / 4], ran_held[RAM_SIZE / 4];
static u32 held_form[RAM_SIZE / 4]; /* form() of each held word when it took its translation */
static const Cpu *watch_cpu;        /* for the end of a cache isolation, which is a flush */
static u8 watch_isolated;

/* native.c patchable and form: `w` without what the runtime's patch table rewrites in place. */
static int patchable(u32 w)
{
    u32 op = w >> 26, f = w & 0x3F;
    return (op >= 8 && op <= 0xF) || (op == 0 && (f == 0 || f == 2 || f == 3));
}

static u32 form(u32 w)
{
    if (!patchable(w))
        return w;
    if (!(w >> 26))
        return w & ~(0x1Fu << 6);
    return ((w >> 26) == 8 ? w | 1u << 26 : w) & 0xFFFF0000u;
}

static u32 ram_word(u32 j)
{
    u32 w;
    memcpy(&w, watch_ram + 4 * j, 4);
    return w;
}

static u32 fnv1a(const u8 *p, u32 n)
{
    u32 h = 0x811C9DC5u;
    for (u32 i = 0; i < n; i++)
        h = (h ^ p[i]) * 0x01000193u;
    return h;
}

static void watch_arm(struct Watch *w, u8 on)
{
    if (w->armed == on)
        return;
    w->armed = on;
    for (u32 pg = w->at >> 12; pg <= (w->at + w->size - 1) >> 12; pg++)
        watch_page[pg] = (u16)(watch_page[pg] + (on ? 1 : -1));
}

static void watch_flushed(void);

/* A load over [addr, addr+len): its words lose their translation and the images over it are
 * checked again when next run. len 0: the I-cache was flushed. */
static void watch_loaded(Psx *psx, u32 addr, u32 len)
{
    (void)psx;
    if (!len) {
        watch_flushed();
        return;
    }
    u32 a = addr & (RAM_SIZE - 4), end = a + ((len + 3) & ~3u);
    if (end > RAM_SIZE)
        end = RAM_SIZE;
    memset(held + a / 4, 0, (end - a) / 4);
    for (int i = 0; i < nwatch; i++)
        if (a < watch[i].at + watch[i].size && watch[i].at < end)
            watch_arm(&watch[i], 1);
}

/* As native.c cache_flushed: a held word changed beyond immediates and shift amounts (which the
 * runtime patches in place) is loaded over, and with it the held words whose translation reaches
 * its one without the runtime (coderun.h), block by block from the top. The rest keeps its
 * translation. */
static u8 bad_buf[CR_WORDS + RAM_SIZE / 4], *const bad = bad_buf + CR_WORDS;
static int held_at(void *ctx, u32 j) { return (void)ctx, held[j]; }
static void watch_flushed(void)
{
    int any = 0;
    for (u32 j = 0; j < RAM_SIZE / 4; j++)
        any |= bad[j] = (u8)(held[j] && form(ram_word(j)) != held_form[j]);
    if (!any)
        return;
    for (u32 lo = RAM_SIZE / 4; lo;) {
        lo -= CR_WORDS;
        if (memchr(bad + lo, 1, CR_WORDS))
            cr_spread((const u32 *)watch_ram, lo, bad + lo - CR_WORDS, held_at, NULL);
    }
    for (u32 j = 0; j < RAM_SIZE / 4; j++)
        if (bad[j])
            watch_loaded(NULL, j << 2, 4);
}

static void watch_run(u32 p)
{
    for (int i = 0; i < nwatch; i++) {
        struct Watch *w = &watch[i];
        if (!w->armed || p < w->at || p - w->at >= w->size)
            continue;
        watch_arm(w, 0);
        w->checks++;
        u8 head = (u8)(fnv1a(watch_ram + w->at, w->head_len) == w->head_hash);
        w->live = (u8)(head && fnv1a(watch_ram + w->at, w->size) == w->hash);
        if (w->live)
            for (u32 j = 0; j < w->size / 4; j++)
                if (w->words[j])
                    held[w->at / 4 + j] = 1, held_form[w->at / 4 + j] = form(ram_word(w->at / 4 + j));
        else if ((head || !w->miss_head) && (w->miss || (w->miss = (u8 *)malloc(w->size)) != NULL))
            memcpy(w->miss, watch_ram + w->at, w->size), w->miss_head = head;
    }
}

/* Watches the image at `at` (RAM offset) of `size` bytes; `words` (size / 4 bytes) marks the
 * words with a translation. Returns its number, -1 when full. */
int sim_watch(Sim *s, u32 at, u32 size, u32 head_len, u32 head_hash, u32 hash, const u8 *words)
{
    if (nwatch == WATCH_MAX || !size || at + size > RAM_SIZE || (at | size) & 3)
        return -1;
    struct Watch *w = &watch[nwatch];
    if (!(w->words = (u8 *)malloc(size / 4)))
        return -1;
    memcpy(w->words, words, size / 4);
    w->at = at, w->size = size, w->head_len = head_len, w->head_hash = head_hash, w->hash = hash, w->armed = 0, w->live = 0, w->miss = NULL, w->checks = 0, w->miss_head = 0;
    watch_ram = s->ram;
    watch_cpu = &s->psx.cpu;
    s->psx.ram_loaded = watch_loaded;
    watch_arm(w, 1);
    return nwatch++;
}

/* Whether watched image i matched at its last check. */
int sim_watch_live(int i) { return watch[i].live; }

/* Watched image i's text (size bytes) at its last failed check whose head matched, else at its
 * last failed check; NULL when none failed. *checks: its checks, *head: whether that one's head
 * matched. */
const u8 *sim_watch_miss(int i, u32 *checks, int *head)
{
    *checks = watch[i].checks, *head = watch[i].miss_head;
    return watch[i].miss;
}

/* Floating code (floating.h) placed as native.c places it, in an arena of the runtime's size: a
 * word the guest runs that no image holds takes the translation of the placement there whose
 * words RAM holds, or of a new placement of the unit with an entry there whose words RAM holds,
 * until a load over it. fl_stat: placements, the words they gave a translation, placements the
 * arena had no room for. */
static Floating fl;
static FlArena fl_ar;
static u32 *fl_blob, fl_stat[3];

/* A placement gives its slot back: its words are the interpreter's, as the runtime makes its
 * entries DIRTY (a watched image there takes them back when it runs them). */
static void fl_gone(const FlPlace *q) { memset(held + q->at / 4, 0, fl_unit(&fl, q->unit)->g_len); }

static void fl_run(u32 pc, u32 p)
{
    u32 at, base = pc - p;
    int i = fl_placed(&fl, &fl_ar, watch_ram, p, base);
    if (i < 0) {
        int u = fl_find(&fl, watch_ram, p, &at);
        if (u < 0)
            return;
        if ((i = fl_alloc(&fl, &fl_ar, watch_ram, (u32)u, at, at + base, fl_gone)) < 0) {
            fl_stat[2]++;
            return;
        }
        fl_stat[0]++;
    }
    const FlPlace *q = &fl_ar.p[i];
    for (u32 j = 0, n = fl_unit(&fl, q->unit)->g_len; j < n; j++) /* every word of a unit is code its translation runs */
        if (!held[q->at / 4 + j])
            held[q->at / 4 + j] = 1, held_form[q->at / 4 + j] = form(ram_word(q->at / 4 + j)), fl_stat[1]++;
}

/* Takes the floating code `blob` (`words` words, copied) for the machine `s`, with an arena of
 * `arena` bytes (0: no limit but the placements'): 0, or -1 when it is damaged. */
int sim_floating(Sim *s, const u32 *blob, u32 words, u32 arena)
{
    free(fl_blob);
    fl.w = NULL, fl_blob = NULL;
    if (!(fl_blob = (u32 *)malloc(4 * (size_t)words + 4)))
        return -1;
    memcpy(fl_blob, blob, 4 * (size_t)words);
    if (fl_open(&fl, fl_blob, words) < 0)
        return free(fl_blob), fl_blob = NULL, -1;
    fl_ar.lo = 0x08800000u, fl_ar.hi = fl_ar.lo + (arena ? arena : 0x70000000u), fl_ar.n = 0;
    watch_ram = s->ram;
    watch_cpu = &s->psx.cpu;
    s->psx.ram_loaded = watch_loaded;
    return 0;
}

/* For tests, on a head of its own: fl_place of unit `u`, its body `body` of `body_words` words,
 * at guest and host addresses into `dst` of `cap` words (its host words, or -1), its sites from
 * `ram` (the unit's words as RAM holds them, or NULL), and fl_find in `ram` (RAM_SIZE bytes). */
int sim_fl_place(const u32 *blob, u32 words, u32 u, const u32 *body, u32 body_words, u32 *dst, u32 cap, u32 guest, u32 host, const u32 *ram)
{
    Floating f;
    if (fl_open(&f, blob, words) < 0 || u >= f.units || fl_body_words(fl_unit(&f, u)) != body_words || body_words > cap)
        return -1;
    memcpy(dst, body, 4 * (size_t)body_words);
    return fl_place(&f, u, dst, guest, host, ram) < 0 ? -1 : (int)fl_unit(&f, u)->h_len;
}

int sim_fl_find(const u32 *blob, u32 words, const u8 *ram, u32 p, u32 *at)
{
    Floating f;
    return fl_open(&f, blob, words) < 0 ? -2 : fl_find(&f, ram, p, at);
}

void sim_fl_stats(u32 *out) { out[0] = fl_stat[0], out[1] = fl_stat[1], out[2] = fl_stat[2]; }

static void watch_reset(void)
{
    free(fl_blob);
    fl.w = NULL, fl_blob = NULL, fl_stat[0] = fl_stat[1] = fl_stat[2] = 0, fl_ar.n = 0;
    for (int i = 0; i < nwatch; i++)
        free(watch[i].words), free(watch[i].miss);
    nwatch = 0;
    watch_ram = NULL, watch_cpu = NULL, watch_isolated = 0;
    memset(watch_page, 0, sizeof watch_page);
    memset(held, 0, sizeof held);
}

const u8 *sim_ran(void) { return ran; }
const u8 *sim_ran_held(void) { return ran_held; }
const u32 *sim_ran_words(void) { return ran_word; }
void sim_ran_clear(void) { memset(ran, 0, sizeof ran), memset(ran_held, 0, sizeof ran_held); }

void psxrt_pc_trace(u32 pc)
{
    for (int i = 0; i < nwatchw; i++) {
        u32 v;
        memcpy(&v, ran_ram + watchw_at[i], 4);
        if (v != watchw_val[i]) {
            fprintf(stderr, "[watch] f%u %06X: %08X -> %08X after pc %08X\n", (unsigned)watchw_frame,
                    (unsigned)watchw_at[i], (unsigned)watchw_val[i], (unsigned)v, (unsigned)watchw_pc);
            watchw_val[i] = v;
        }
    }
    watchw_pc = pc;
    /* Cache isolated (SR bit 16): its end is a flush, as in native.c. */
    if (watch_cpu) {
        if (watch_cpu->sr & 0x10000)
            watch_isolated = 1;
        else if (watch_isolated)
            watch_isolated = 0, watch_flushed();
    }
    if ((pc & 0x1FFFFFFFu) < RAM_SIZE) {
        u32 p = pc & (RAM_SIZE - 4), w;
        memcpy(&w, ran_ram + p, 4);
        /* Another instruction there: whether it ran held is its own. */
        if (ran[p >> 2] && ran_word[p >> 2] != w)
            ran_held[p >> 2] = 0;
        ran[p >> 2] = 1, ran_word[p >> 2] = w;
        if (watch_page[p >> 12] && !held[p >> 2])
            watch_run(p);
        if (fl.w && !held[p >> 2])
            fl_run(pc, p);
        ran_held[p >> 2] |= held[p >> 2];
    }
    if (!trace_file)
        return;
    if ((pc & 0x1FFFFFFFu) < RAM_SIZE)
        pc_count[(pc & (RAM_SIZE - 1)) >> 2]++;
    if (pc == tr_last + 4 && tr_first) {
        tr_last = pc;
        return;
    }
    if (tr_first)
        trace_put();
    tr_first = tr_last = pc;
}

/* The device accesses while a trace is written, by instruction, address and direction (2: a
 * psx_update, at pc 0; 3: a GTE command, p = its number), written beside the trace as FILE.io,
 * most frequent first. In the trace each is a marker pair (0xFFFFFFF0 | direction, p) after the
 * run that made it, where the native runtime's code runs. */
#define IO_SLOTS 8192
static struct IoSlot { u32 pc, p, w, n; } io_slots[IO_SLOTS];

void psxrt_io_trace(u32 p, int write)
{
    if (!trace_file)
        return;
    u32 pc = write == 2 ? 0 : tr_last;
    if (tr_first)
        trace_put();
    tr_first = tr_last = 0;
    tr_prev[0] = tr_prev[1] = 0;
    tr_buf[tr_n++] = 0xFFFFFFF0u | (u32)write, tr_buf[tr_n++] = p;
    if (tr_n == 8192)
        fwrite(tr_buf, 4, (size_t)tr_n, trace_file), tr_n = 0;
    for (u32 i = (pc * 2654435761u ^ p * 40503u ^ (u32)write) % IO_SLOTS, k = 0; k < IO_SLOTS; k++, i = (i + 1) % IO_SLOTS) {
        struct IoSlot *s = &io_slots[i];
        if (!s->n)
            s->pc = pc, s->p = p, s->w = (u32)write;
        if (s->pc == pc && s->p == p && s->w == (u32)write) {
            s->n++;
            return;
        }
    }
}

static int io_cmp(const void *a, const void *b)
{
    u32 x = ((const struct IoSlot *)a)->n, y = ((const struct IoSlot *)b)->n;
    return x < y ? 1 : x > y ? -1 : 0;
}

static void io_dump(const char *trace_path)
{
    char path[1024];
    snprintf(path, sizeof path, "%s.io", trace_path);
    FILE *f = fopen(path, "w");
    qsort(io_slots, IO_SLOTS, sizeof io_slots[0], io_cmp);
    for (int i = 0; f && i < IO_SLOTS && io_slots[i].n; i++)
        fprintf(f, "%08X %08X %c %u\n", (unsigned)io_slots[i].pc, (unsigned)io_slots[i].p, "rwug"[io_slots[i].w],
                (unsigned)io_slots[i].n);
    if (f)
        fclose(f);
    memset(io_slots, 0, sizeof io_slots);
    snprintf(path, sizeof path, "%s.cnt", trace_path);
    if ((f = fopen(path, "wb")) != NULL) {
        fwrite(pc_count, 4, RAM_SIZE / 4, f);
        fclose(f);
    }
    memset(pc_count, 0, sizeof pc_count);
}

static char trace_path[1024];

/* Starts writing the trace to `path`, or ends it (NULL): returns the instructions and the runs
 * written so far. */
int sim_trace_to(const char *path, unsigned long long *out)
{
    if (trace_file) {
        if (tr_first)
            trace_put();
        fwrite(tr_buf, 4, (size_t)tr_n, trace_file);
        fclose(trace_file);
        trace_file = NULL;
        io_dump(trace_path);
    }
    if (path)
        snprintf(trace_path, sizeof trace_path, "%s", path);
    out[0] = tr_insns, out[1] = tr_runs;
    tr_n = 0, tr_first = tr_last = tr_prev[0] = tr_prev[1] = 0, tr_insns = tr_runs = 0;
    if (path && !(trace_file = fopen(path, "wb")))
        return -1;
    return 0;
}

int sim_boot(Sim *s) { return psx_boot(&s->psx); }

/* Runs until the next vblank has been handled; returns the exit state (0 running). */
int sim_frame(Sim *s)
{
    u32 f = watchw_frame = s->frames;
    while (s->frames == f && !s->psx.exiting)
        cpu_run(&s->psx, s->psx.cycles + 4096);
    if (sim_ge)
        ge_shadow_frame(&s->psx, s->frames);
    return s->psx.exiting;
}

const u16 *sim_vram(const Sim *s) { return s->vram; }
u8 *sim_ram(Sim *s) { return s->ram; }

/* pollscan.c on the guest code in RAM (tests) */
u32 sim_poll_bound(Sim *s, u32 pc, const u32 *gpr, u32 raw, u32 room) { return poll_bound(&s->psx, pc, gpr, raw, room); }

void sim_pad(Sim *s, u16 buttons) { s->psx.sio.buttons[0] = buttons; }

void sim_trace(Sim *s, int flags) { s->psx.trace = flags; }

static int select_fwd(void *user, u32 n)
{
    Sim *s = (Sim *)user;
    u32 sectors = rust_select(rust_user, n);
    if (sectors)
        s->host.disc_end = 150 + sectors;
    return sectors != 0;
}

/* The game has `n` discs, put in the drive by `sel` (manual changes and the core's automatic
 * one). */
void sim_discs(Sim *s, SelectFn sel, u32 n)
{
    rust_select = sel;
    s->host.disc_select = select_fwd;
    s->host.discs = n;
    s->host.auto_swap = getenv("PSX2PSP_SIM_MANUAL_DISC") == NULL;
}

/* The shell opens, or closes after disc `disc` (0-based; < 0: the same) goes in. */
void sim_lid(Sim *s, int open, int disc)
{
    if (!open && disc >= 0)
        cd_insert(&s->psx, (u32)disc);
    cd_lid(&s->psx, open);
}

/* Display area: x, y, width, height, 24-bit flag, enabled. */
void sim_display(const Sim *s, int *out)
{
    const Gpu *g = &s->psx.gpu;
    static const int widths[4] = {256, 320, 512, 640};
    u32 st = g->stat;
    int w = (st & 0x10000) ? 368 : widths[(st >> 17) & 3];
    int h = gpu_display_height(&s->psx);
    out[0] = g->disp_x, out[1] = g->disp_y, out[2] = w, out[3] = h;
    out[4] = (st >> 21) & 1;
    out[5] = !(st & 0x800000);
}

void sim_status(const Sim *s, u32 *out)
{
    out[0] = s->psx.cpu.pc;
    out[1] = (u32)(s->psx.cycles >> 20);
    out[2] = s->frames;
    out[3] = s->psx.i_mask;
    out[4] = (u32)s->psx.gpu.flips; /* display-area changes: the game's own frames */
}
