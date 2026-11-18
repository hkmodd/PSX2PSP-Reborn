/*
 * psxrt: runs the translated code of DATA.PSAR, with the core's interpreter for what the
 * translation lacks (docs/recomp/PLAN.md, S3).
 *
 * Executables are translated ahead of time; one is loaded (its code to its host address, its
 * slice of the dispatch table into TABLE) when the guest first reaches it and the text in RAM is
 * the one translated (FNV-1a hash). Disc data reaching RAM (CD DMA, BIOS loads) marks the table
 * entries it covers DIRTY: the next jump there checks again, so an overlay loaded over another
 * gets its own code. Executables translated for the same address share a host region; loading
 * one evicts the others.
 *
 * Control moves between three places:
 *   - translated code, which reaches the runtime through the stubs (trampolines.S);
 *   - the interpreter (run_guest), which runs until the guest reaches an address with a
 *     translation and returns that host address;
 *   - guest calls made by the BIOS (interrupt handlers, callbacks): native_call() runs them
 *     to CALL_RET, natively when they have a translation.
 * A guest `ra` that has no translation becomes GRET when translated code takes over: its `jr ra`
 * comes back to the interpreter at the address saved on a small stack, the entry pushed at the
 * same guest sp (gret_pop).
 *
 * Time: the PS1 cycle count advances with interpreted instructions and loops: each back-edge
 * charges its estimated PS1 cycles to CTX.BUDGET and enters the runtime when that goes negative,
 * which adds what was spent to the count, so a counted wait loop lasts as long as on the PS1.
 * PS1 time runs at one PS1 frame per PSP display frame (59.94 Hz), and each PS1 vblank waits for
 * the PSP's (native_vsync), so that every game frame is shown once: PS1 time is the PSP's,
 * scaled by the frame lengths (+0.2% for NTSC, +0.9% for PAL60). A PAL frame (314 lines) gets
 * 6 PSP frames in 5, one of them shown twice: PS1 time runs at its own 50 Hz (+0.4%), not 120%.
 * When the count runs more than a frame ahead of that clock (checked at every host call and every
 * 256 interpreted instructions) the runtime sleeps; when it falls behind by more than BEHIND_MAX,
 * the clock is moved back instead: PS1 time slows down with the host, it never jumps, or devices
 * (the CD's sectors) would run ahead of the guest code that serves them. A 500 us alarm makes the budget negative, so
 * that loops enter the runtime and interrupts are taken there.
 *
 * Code written by CPU stores (decompressed or copied by the game) is checked at each I-cache
 * flush, which the R3000A needs before running it too: FlushCache, or the end of a cache
 * isolation. In a 1 KB block whose code words (translated, or delay slots) no longer sum as when
 * its table entries were set, the patch table, which holds every code word as translated, says
 * which words changed: those changed only in immediates and shift amounts (Sheep Dog sets those
 * of its 3D code at each level load) are written in the host code where it says (GOAL.MD S20d);
 * the others go DIRTY for the interpreter, with the translated words of the block and the one
 * below that reach them without the runtime (coderun.h). DBGT zeroes boot code it ran.
 * While the cache is isolated (SR bit 16) the guest runs in the interpreter, whose stores do not
 * reach RAM; translated stores would.
 *
 * Limits: code stores change with no flush after them run their old translation, as do direct
 * host jumps into a block gone DIRTY and hooked library functions (emit.rs hooks) whose words
 * the game rewrites; EPC holds host addresses while translated code is interrupted, and GRET
 * returns are LIFO.
 */
#include "native.h"
#include "coderun.h"
#include "floating.h"
#include "vlc.h"

#include <pspdisplay.h>
#include <pspkernel.h>
#include <stdarg.h>
#include <pspiofilemgr.h>
#include <pspthreadman.h>
#include <psputils.h>
#include <stdio.h>
#include <string.h>

#define RAM_HOST 0x09800000u
#define CTX_ADDR 0x09A00400u
#define GTE_ADDR 0x09A00500u /* psx->gte: translated code moves to and from it directly */
#define STUB_RT_CALL 0x09A00F00u
#define STUB_IO_READ 0x09A00F08u
#define STUB_IO_WRITE 0x09A00F10u
#define STUB_MISS 0x09A00F40u
#define STUB_DIRTY 0x09A00F60u
#define STUB_CALL_RET 0x09A00F80u
#define STUB_GRET 0x09A00FC0u
#define STUB_GTE_CMD 0x09A00FA0u
#define STUB_GTE_NCLIP 0x09A00FD0u /* 8 bytes each */
#define STUB_GTE_AVSZ3 0x09A00FD8u
#define STUB_GTE_AVSZ4 0x09A00FE0u
#define STUB_GTE_RTPS 0x09A00FE8u
#define STUB_GTE_RTPT 0x09A00FF0u
#define STUB_GTE_MVMVA 0x09A00FC8u /* these two take their word in k1 */
#define STUB_GTE_GPF 0x09A00FF8u
#define TABLE_ADDR 0x09A01000u
#define TABLE_WORDS (RAM_SIZE / 4)
/* CTX words */
#define C_HI (0x80 / 4)
#define C_LO (0x84 / 4)
#define C_TARGET (0x88 / 4)
#define C_BUDGET (0x90 / 4)
#define C_SITE (0x94 / 4) /* CTX.GTE_RET: IO_WRITE parks its site there */
#define BUDGET NATIVE_BUDGET
#define FORCED NATIVE_FORCED
#define RESUME 1u
#define POLL 0u /* host-call word of an interrupt poll */
#define ALARM_US 500
/* Lead allowed inside a frame before the runtime sleeps: a frame, past the 2 ms beyond it.
 * native_vsync paces every PS1 vblank, so the lead never gets that far; a sleep in mid-frame
 * would only delay the frame's later work. The FMV slept 130-190 ms/s there (its CD waits) while
 * its decode missed 16 PSP vblanks a second (S21). */
#define AHEAD_MAX (P->frame_cycles + PSX_CLOCK / 500)
/* Lateness kept before PS1 time slows down: two PAL frames, so that a slow frame's lateness is
 * paid back by the next vsync wait instead of being slept through. */
#define BEHIND_MAX (PSX_CLOCK / 25)
/* A clock read is a syscall (~165-250 cycles): the clock is compared once per 0.5 ms of PS1
 * time, and the profile times one call in PROF_EVERY, scaled. */
#define SYNC_STEP (PSX_CLOCK / 2000)
#define PROF_EVERY 16u
#define HEAD_LEN 2048u              /* bytes of text under CodeEntry.head_hash */

/* trampolines.S */
void psxrt_rt_call(void);
void psxrt_miss(void);
void psxrt_gret(void);
void psxrt_call_ret(void);
void psxrt_gte_cmd(void);
void psxrt_gte_prof(void);
void psxrt_gte_nclip(void), psxrt_gte_nclip_door(void);
void psxrt_gte_avsz3(void), psxrt_gte_avsz3_door(void);
void psxrt_gte_avsz4(void), psxrt_gte_avsz4_door(void);
void psxrt_gte_rtps(void), psxrt_gte_rtps_door(void);
void psxrt_gte_rtpt(void), psxrt_gte_rtpt_door(void);
void psxrt_gte_mvmva(void), psxrt_gte_gpf(void);
u32 psxrt_zero3[3]; /* MVMVA's translation when it has none */
void native_enter(u32 host_pc);
/* gte_fast.c */
void native_gte_fast(u32 cmd);
/* io_fast.c */
void psxrt_io_read(void);
void psxrt_io_write(void);
extern Psx *io_fast_psx;
extern u32 io_fast_shift;
int io_fast_dev(Psx *psx, u32 p, u32 *v);
u32 native_host_sp, native_host_gp;

NativeStats native_stats;

static volatile u32 *const ctx = (volatile u32 *)CTX_ADDR;
static u32 *const table = (u32 *)TABLE_ADDR;

typedef struct Image {
    const CodeEntry *e;
    u32 text; /* text address & 0x1FFFFC */
    int resident;  /* its code is at host_base */
    int installed; /* its table slice is in TABLE (resident, and no other slice loaded over it) */
    int patched;   /* its code holds immediates the game rewrote: read again before reinstalling */
    u32 gen, checked; /* disc loads over its text; gen at the last check */
} Image;

#define MAX_IMAGES 256
static Image img[MAX_IMAGES];
static int nimg;
static u32 code_lo, code_hi;
static u32 cpu_shift; /* DiscInfo.cpu_shift, at most 8 */
static Psx *P;
static u64 t_base, c_base;
static u64 stall_to; /* an owed vblank's CPU stall: the cycle count update() brings the devices to */
static u64 t_lock;     /* when the PSP vblank v_lock started, on native_vsync's grid */
static u32 v_lock, vc; /* vc: the PSP vblank count at the last PS1 vblank */

/* A PAL frame (314 lines, 50 Hz) is paced to 6/5 of a PSP frame; NTSC and PAL60 to one. */
static int pal_pace(void) { return P->frame_cycles > PSX_CLOCK / 55; }
/* Three PS1 frames in host us: 3 * 1001/60 ms, or 6/5 of that for PAL */
static u32 frame_us3(void) { return pal_pace() ? 60060u : 50050u; }
/* PS1 cycles in `us` of host time */
static u64 us_cycles(u64 us) { return us * P->frame_cycles * 3u / frame_us3(); }
/* PSP vblanks the PS1 frame just presented stays: 1, or 1 1 1 1 2 for PAL */
static u32 vs_step(void)
{
    static u32 phase;
    if (!pal_pace())
        return phase = 0, 1;
    phase += 6;
    u32 s = phase / 5;
    phase %= 5;
    return s;
}

#define GRET_DEPTH 64
static u32 gret[GRET_DEPTH], gret_sp[GRET_DEPTH]; /* gret_sp: the guest sp at the push */
static u32 gret_top;

static void gret_push(u32 ra, u32 sp)
{
    gret[gret_top % GRET_DEPTH] = ra, gret_sp[gret_top % GRET_DEPTH] = sp;
    gret_top++;
}

/* The entry a GRET return at guest `sp` stands for, as the stack top just past it: the newest
 * one pushed at that sp, the top when there is none. A call returns at the sp it was entered with
 * (a push is at the callee's entry, or in a leaf), so the entries above it are dead: fix_ra also
 * pushes an ra that is no longer live. Silent Hill's 8002E454 runs its switch cases in the
 * interpreter (the table is outside the image) and leaves one with `j 8002E934`, its translated
 * epilogue, with ra = 8002E6FC from the case's last `jal strcmp`: that push sat on top of the one
 * 8002E454 was entered with (8002E334, in 8002E260), its `jr ra` took 8002E6FC and ran the case
 * again in its caller's frame, and the epilogue returned to 801FFFF8. */
static u32 gret_at(u32 sp)
{
    u32 n = gret_top < GRET_DEPTH ? gret_top : GRET_DEPTH;
    for (u32 k = 0; k < n; k++)
        if (gret_sp[(gret_top - 1 - k) % GRET_DEPTH] == sp)
            return gret_top - k;
    return gret_top;
}

static u32 gret_pop(u32 sp)
{
    gret_top = gret_at(sp) - 1;
    return gret[gret_top % GRET_DEPTH];
}

static void say(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
static void say(const char *fmt, ...)
{
    char line[160];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(line, sizeof line, fmt, ap);
    va_end(ap);
    disc_log(line);
}

static u32 fnv1a(const u8 *p, u32 n)
{
    u32 h = 0x811C9DC5u;
    for (u32 i = 0; i < n; i++)
        h = (h ^ p[i]) * 0x01000193u;
    return h;
}

/* Code CPU stores change, seen at a cache flush: RAM in blocks of 1 KB, each with two sums of
 * its code words (those the table translates, and the delay slots after them), as RAM held them
 * when the block's entries were last set (every change of the table sums its blocks again): the
 * words, and their form (the words without the immediates and shift amounts a patch table can
 * rewrite). Data and undiscovered words are left out: an executable's text_size covers its data
 * section too, which the game writes all the time. */
#define BLK_SHIFT 10
#define BLKS (RAM_SIZE >> BLK_SHIFT)
static u32 blk_sum[BLKS], blk_form[BLKS];
static u8 blk_code[BLKS]; /* the block has translated words: a flush checks it */
static u8 blk_slot0[BLKS];

/* A rewrite of `w` a patch table can take: an I-type ALU immediate or a constant shift (emit.rs
 * patchable). */
static int patchable(u32 w)
{
    u32 op = w >> 26, f = w & 0x3F;
    return (op >= 8 && op <= 0xF) || (op == 0 && (f == 0 || f == 2 || f == 3));
}

/* `w` without what a patch rewrites: its immediate (addi as addiu) or shift amount. */
static u32 form(u32 w)
{
    if (!patchable(w))
        return w;
    if (!(w >> 26))
        return w & ~(0x1Fu << 6);
    return ((w >> 26) == 8 ? w | 1u << 26 : w) & 0xFFFF0000u;
}

/* An instruction with a delay slot: branches, jumps, COP branches. */
static int has_slot(u32 w)
{
    u32 op = w >> 26, f = w & 0x3F;
    return (op >= 1 && op <= 7) || (op == 0 && (f == 8 || f == 9)) || (op >= 0x10 && op <= 0x13 && ((w >> 21) & 0x1F) == 8);
}

/* The new host word for `guest` where the simple instruction translated to `host` is, if it is
 * the same instruction with another immediate or shift amount: 1 and *out (emit.rs repatch). */
static int repatch(u32 host, u32 guest, u32 *out)
{
    u32 nw = guest >> 26 == 8 ? guest | 1u << 26 : guest;
    if (!patchable(nw) || form(nw) != form(host))
        return 0;
    *out = nw;
    return 1;
}

/* `slot`: the block's first word is the delay slot of the word before, as when the sums were
 * taken (blk_slot0): a block sent to the interpreter must not change the next one's sums. */
static u32 blk_code_sum(u32 b, u8 *code, u32 *fsum, int slot)
{
    const u32 *w = (const u32 *)(P->ram + (b << BLK_SHIFT)), *t = &table[b << (BLK_SHIFT - 2)];
    u32 h = 0x811C9DC5u, f = h, len = code_hi - code_lo;
    *code = 0;
    for (u32 i = 0; i < 1u << (BLK_SHIFT - 2); i++) {
        int in = t[i] - code_lo < len;
        if (in || slot)
            h = (h ^ w[i]) * 0x01000193u, f = (f ^ form(w[i])) * 0x01000193u, *code = 1;
        slot = in && has_slot(w[i]);
    }
    *fsum = f;
    return h;
}

/* The table entries of [a, a + len) of RAM changed: their blocks are summed again. */
static void blk_resum(u32 a, u32 len)
{
    for (u32 b = a >> BLK_SHIFT, e = (a + len + (1u << BLK_SHIFT) - 1) >> BLK_SHIFT; b < e && b < BLKS; b++) {
        const u32 *w = (const u32 *)(P->ram + (b << BLK_SHIFT));
        blk_slot0[b] = b && table[(b << (BLK_SHIFT - 2)) - 1] - code_lo < code_hi - code_lo && has_slot(w[-1]);
        blk_sum[b] = blk_code_sum(b, &blk_code[b], &blk_form[b], blk_slot0[b]);
    }
}

/* ---- floating code (floating.h): units translated once, placed where RAM holds their words ----
 * The table entries of a placement's translated words point into its slot. */
static Floating fl;
static FlArena ar; /* from code_lo; the executables' code is above it */
static u32 fl_bodies; /* DATA.PSAR offset of the units' bodies */

static u32 pl_words(const FlPlace *q) { return fl_unit(&fl, q->unit)->g_len; }

/* q's entries in the table, except where an executable's translation is installed; an entry of
 * a word it does not translate that was DIRTY is MISS (the words are the unit's). */
static void fl_install(const FlPlace *q)
{
    const FlUnit *x = fl_unit(&fl, q->unit);
    const u32 *t = (const u32 *)q->host + x->h_len; /* the body's table, after the code */
    u32 *e = &table[q->at >> 2];
    for (u32 j = 0; j < x->g_len; j++) {
        if (e[j] - ar.hi < code_hi - ar.hi)
            continue;
        if (t[j] != FL_NONE)
            e[j] = q->host + 4 * t[j];
        else if (e[j] == STUB_DIRTY)
            e[j] = STUB_MISS;
    }
    blk_resum(q->at, 4 * x->g_len);
}

/* q gives its slot back: the entries pointing into it are DIRTY. */
static void fl_gone(const FlPlace *q)
{
    u32 *e = &table[q->at >> 2];
    for (u32 j = 0, n = pl_words(q); j < n; j++)
        if (e[j] - q->host < q->len)
            e[j] = STUB_DIRTY;
    blk_resum(q->at, 4 * pl_words(q));
    native_stats.fl_drops++;
}

/* q's unit read from DATA.PSAR into its slot and placed there: 0, or -1 when it cannot be. */
static int fl_load(const FlPlace *q)
{
    const FlUnit *x = fl_unit(&fl, q->unit);
    if (disc_blob(fl_bodies + x->b_off, x->b_len, (void *)q->host, 4 * fl_body_words(x)) < 0 ||
        fl_place(&fl, q->unit, (u32 *)q->host, q->guest, q->host, (const u32 *)(P->ram + q->at)) < 0)
        return -1;
    sceKernelDcacheWritebackRange((void *)q->host, q->len);
    sceKernelIcacheInvalidateRange((void *)q->host, q->len);
    return 0;
}

/* The guest runs RAM offset `p` (address `pc`) with no translation: a placement there whose words
 * RAM holds goes back in, or the longest unit RAM holds there is placed. 1 when either. */
static int fl_try(u32 pc, u32 p)
{
    static u32 full;
    if (!fl.w)
        return 0;
    u32 base = pc - p, at;
    int i = fl_placed(&fl, &ar, P->ram, p, base);
    if (i >= 0) {
        fl_install(&ar.p[i]);
        native_stats.fl_reused++;
        return 1;
    }
    int u = fl_find(&fl, P->ram, p, &at);
    if (u < 0)
        return 0;
    if ((i = fl_alloc(&fl, &ar, P->ram, (u32)u, at, at + base, fl_gone)) < 0) {
        if (full++ < 4)
            say("native: floating arena full (%u placements, %u B wanted at %08X): to the interpreter", (unsigned)ar.n,
                (unsigned)(4 * fl_unit(&fl, (u32)u)->h_len), (unsigned)pc);
        native_stats.fl_full++;
        return 0;
    }
    const FlPlace *q = &ar.p[i];
    if (fl_load(q) < 0) {
        say("native: cannot read floating unit %u: to the interpreter", (unsigned)q->unit);
        memmove(ar.p + i, ar.p + i + 1, (ar.n - i - 1) * sizeof *ar.p);
        ar.n--;
        return 0;
    }
    fl_install(q);
    if (native_stats.fl_places++ < 8)
        say("native: floating unit %u (%u words) placed at %08X for guest %08X, host %08X (%u B), from %08X", (unsigned)q->unit,
            (unsigned)pl_words(q), (unsigned)q->at, (unsigned)q->guest, (unsigned)q->host, (unsigned)q->len, (unsigned)pc);
    return 1;
}

/* Patch tables (engine native_code::patch_groups): per executable, one group per PATCH_BLKS
 * blocks from its text's first, read when a block of it changes. A group: PATCH_BLKS + 1 word
 * offsets of each block's entries, then entries: `index | n << 8` (the word in its block), then
 * n host addresses of its copies, or for n 0 the word as translated (any rewrite is DIRTY). */
#define PATCH_BLKS 4u
#define PATCH_MAX (32u * 1024u)
static u32 pbuf[PATCH_MAX / 4];
static const CodeEntry *pbuf_e;
static u32 pbuf_g;

static const u32 *patch_group(const CodeEntry *e, u32 g)
{
    if (pbuf_e == e && pbuf_g == g)
        return pbuf;
    u32 d[3]; /* offset from patch_off, stored length, raw length */
    pbuf_e = NULL;
    if (!e->patch_off || g >= e->patch_groups || disc_blob(e->patch_off + 12 * g, 12, d, 12) < 0 || d[2] > PATCH_MAX ||
        d[2] < 4 * (PATCH_BLKS + 1) || (d[2] & 3) || disc_blob(e->patch_off + d[0], d[1], pbuf, d[2]) < 0)
        return NULL;
    for (u32 k = 0; k < PATCH_BLKS; k++)
        if (pbuf[k] < PATCH_BLKS + 1 || pbuf[k] > pbuf[k + 1] || pbuf[k + 1] > d[2] / 4)
            return NULL;
    pbuf_e = e, pbuf_g = g;
    return pbuf;
}

/* Whether `m`'s translation runs RAM word `o`: its table entry is in m's code, or it is the
 * delay slot of a word whose entry is. */
static int runs(const Image *m, u32 o)
{
    u32 lo = m->e->host_base, len = m->e->code_len, t = table[o >> 2];
    if (t - lo < len)
        return 1;
    return o >= 4 && t - code_lo >= code_hi - code_lo && table[(o >> 2) - 1] - lo < len && has_slot(*(const u32 *)(P->ram + o - 4));
}

/* Block `b` of RAM in `m`'s translation, from its patch table, which has an entry for each word
 * of code. Without `write`: marks in `bad` (the block's words) each word `m` runs there that RAM
 * no longer holds as translated beyond what a patch takes (its form changed, or it is not one
 * plain copy and changed at all). With `write`: the host words of the immediates and shift
 * amounts RAM holds, for the words not marked. The host words written, or -1 when the table
 * cannot say (no group, a bad entry or host address). `same`: the block's code words keep their
 * form sum. */
static u32 why[4]; /* the last refusal: kind (1 no group, 2 bad entry, 5 host address) | word << 8, RAM's, expected, host address */
static const CodeEntry *why_e;
#define REFUSE(k, at, now, was) (why[0] = (k) | (at) << 8, why[1] = (now), why[2] = (was), -1)
static int patch_block(Image *m, u32 b, u8 *bad, int write, int same)
{
    const CodeEntry *e = m->e;
    u32 k = b - (m->text >> BLK_SHIFT), o = b << BLK_SHIFT;
    const u32 *p = patch_group(e, k / PATCH_BLKS), *w = (const u32 *)(P->ram + o);
    if (!p)
        return REFUSE(1, 0, k, e->patch_groups);
    int n = 0;
    for (u32 i = p[k % PATCH_BLKS], end = p[k % PATCH_BLKS + 1]; i < end;) {
        u32 at = p[i] & 0xFF, hosts = p[i] >> 8 & 0xFF, now = w[at], i0 = ++i;
        if (i + (hosts ? hosts : 1) > end)
            return REFUSE(2, at, i, end);
        i += hosts ? hosts : 1;
        if (bad[at] || !runs(m, o + 4 * at))
            continue;
        if (!hosts) {
            if (p[i0] != now)
                bad[at] = 1;
            continue;
        }
        for (u32 j = i0; j < i; j++) {
            u32 a = p[j], nw;
            if (a - e->host_base >= e->code_len || (a & 3))
                return why[3] = a, REFUSE(5, at, now, 0);
            /* PPSSPP's JIT marks its blocks' first words in memory with op 0x1A (never Allegrex's):
             * that word's form is the block's when the block's form sum vouches for it. */
            u32 hw = *(u32 *)a;
            if (hw >> 26 == 0x1A && same)
                hw = form(now >> 26 == 8 ? now | 1u << 26 : now);
            if (!repatch(hw, now, &nw)) {
                bad[at] = 1;
                break;
            }
            if (write && nw != *(u32 *)a)
                *(u32 *)a = nw, n++;
        }
    }
    return n;
}

/* Block `b` changed: as patch_block, for every translation running it. Placed floating code
 * that no longer holds its words marks the whole block (-1). */
static int block_patch(u32 b, u8 *bad, int write, int same)
{
    u32 lo = b << BLK_SHIFT, hi = lo + (1u << BLK_SHIFT);
    int n = 0;
    for (u32 i = 0; i < ar.n && !write; i++) /* a placement runs its unit's words only */
        if (ar.p[i].at < hi && lo < ar.p[i].at + 4 * pl_words(&ar.p[i]) && !fl_holds(&fl, P->ram, &ar.p[i]))
            return why[0] = 6, why_e = NULL, -1;
    for (int i = 0; i < nimg; i++) {
        Image *m = &img[i];
        if (!m->resident || m->text >= hi || m->text + m->e->text_size <= lo)
            continue;
        int r = patch_block(m, b, bad, write, same);
        if (r < 0)
            return why_e = m->e, -1;
        if (r)
            n += r, m->patched = 1;
    }
    return n;
}

/* RAM word `j` has a translation that runs: its table entry is code, or it is the delay slot of
 * one whose entry is (coderun.h). */
static int held_word(void *ctx, u32 j)
{
    (void)ctx;
    u32 len = code_hi - code_lo;
    return table[j] - code_lo < len || (j && table[j - 1] - code_lo < len && has_slot(((const u32 *)P->ram)[j - 1]));
}

static void flush_code(void)
{
    sceKernelDcacheWritebackInvalidateAll();
    sceKernelIcacheInvalidateAll();
}

/* ---- guest state: CTX (translated code) <-> psx->cpu (runtime) ---- */
/* The return site a jump the runtime ran (not translated code) last linked into ra, or 1 (never
 * one): only that ra goes to translated code as a return address (fix_ra). */
static u32 ra_link = 1;

static void from_ctx(Cpu *c)
{
    ra_link = 1;
    for (int i = 1; i < 32; i++)
        c->gpr[i] = ctx[i];
    c->gpr[0] = 0;
    c->hi = ctx[C_HI];
    c->lo = ctx[C_LO];
    c->in_delay = 0;
    c->load_reg = 0;
}

static u32 to_ctx(const Cpu *c, u32 host)
{
    for (int i = 0; i < 32; i++)
        ctx[i] = c->gpr[i];
    ctx[C_HI] = c->hi;
    ctx[C_LO] = c->lo;
    return host;
}

static void jump(Cpu *c, u32 pc)
{
    c->pc = pc;
    c->next_pc = pc + 4;
    c->in_delay = 0;
}

static void finish_load(Cpu *c)
{
    if (c->load_reg)
        c->gpr[c->load_reg] = c->load_val;
    c->load_reg = 0;
    c->gpr[0] = 0;
}

/* ---- translations ---- */

/* The host address for guest or host `pc`: itself when it is host code, its translation, or 0. */
static u32 host_of(u32 pc)
{
    if ((pc >= code_lo && pc < code_hi) || pc == STUB_CALL_RET || pc == STUB_GRET)
        return pc;
    u32 p = pc & 0x1FFFFFFFu;
    if (p >= 0x800000 || (pc & 3))
        return 0;
    u32 h = table[(p & (RAM_SIZE - 4)) >> 2];
    return h == STUB_MISS || h == STUB_DIRTY ? 0 : h;
}

static void evict(Image *m)
{
    const CodeEntry *e = m->e;
    u32 lo = e->host_base, hi = e->host_base + e->code_len;
    for (u32 i = 0; i < (e->text_size + 3) / 4; i++) {
        u32 *t = &table[(m->text >> 2) + i];
        if (*t >= lo && *t < hi)
            *t = STUB_MISS;
    }
    blk_resum(m->text, e->text_size);
    m->resident = m->installed = 0;
}

static int load(Image *m)
{
    const CodeEntry *e = m->e;
    if (!m->resident || m->patched) {
        for (int i = 0; i < nimg; i++)
            if (&img[i] != m && img[i].resident && img[i].e->host_base == e->host_base)
                evict(&img[i]);
        if (disc_blob(e->code_off, e->code_stored, (void *)e->host_base, e->code_len) < 0) {
            say("native: cannot read the code of %.16s", e->name);
            m->resident = 0;
            return -1;
        }
        m->resident = 1;
        m->patched = 0;
    }
    if (disc_blob(e->table_off, e->table_stored, &table[m->text >> 2], (e->text_size + 3) & ~3u) < 0) {
        say("native: cannot read the table of %.16s", e->name);
        evict(m);
        return -1;
    }
    for (int i = 0; i < nimg; i++)
        if (img[i].text < m->text + e->text_size && m->text < img[i].text + img[i].e->text_size)
            img[i].installed = 0;
    m->installed = 1;
    m->checked = m->gen;
    blk_resum(m->text, e->text_size);
    flush_code();
    native_stats.loads++;
    say("native: %.16s at %08X (%u B of code)", e->name, (unsigned)e->text_addr, (unsigned)e->code_len);
    return 0;
}

/* Loads the executable containing `pc` when its text is in RAM. Only for a DIRTY entry, or a
 * MISS one when `miss` (the first address of a dispatch miss): 1 when loaded. */
static int try_load(u32 pc, int miss)
{
    u32 p = pc & 0x1FFFFFFFu;
    if (p >= 0x800000 || (pc & 3))
        return 0;
    p &= RAM_SIZE - 4;
    u32 *t = &table[p >> 2];
    if (*t != STUB_DIRTY && !(miss && *t == STUB_MISS))
        return 0;
    int candidates = 0;
    for (int i = 0; i < nimg; i++) {
        Image *m = &img[i];
        if (p < m->text || p - m->text >= m->e->text_size || m->gen == m->checked)
            continue;
        /* A MISS entry in an installed slice was not loaded over (it would be DIRTY): the code
         * there is still that executable's, only not discovered by the translator. */
        if (*t == STUB_MISS && m->installed)
            continue;
        candidates++;
        native_stats.hashes++;
        u32 head = m->e->text_size < HEAD_LEN ? m->e->text_size : HEAD_LEN;
        if (fnv1a(P->ram + m->text, head) == m->e->head_hash && fnv1a(P->ram + m->text, m->e->text_size) == m->e->hash && load(m) == 0)
            return 1;
        m->checked = m->gen;
    }
    if (fl_try(pc, p))
        return 1;
    if (!candidates && *t == STUB_DIRTY)
        *t = STUB_MISS;
    return 0;
}

static void on_ram_loaded(Psx *psx, u32 addr, u32 len);

/* The words of block `b` marked in `mark` lose their translation, as if loaded over (DIRTY: the
 * interpreter, and their executables are checked again before use); the rest of the block keeps
 * it. Their count. */
static u32 dirty_words(u32 b, const u8 *mark)
{
    u32 a = b << BLK_SHIFT, n = 0;
    for (u32 i = 0; i < CR_WORDS; i++)
        if (mark[i])
            table[(a >> 2) + i] = STUB_DIRTY, n++;
    for (int i = 0; i < nimg && n; i++)
        if (a < img[i].text + img[i].e->text_size && img[i].text < a + (1u << BLK_SHIFT))
            img[i].gen++;
    blk_resum(a, 1u << BLK_SHIFT);
    return n;
}

/* Code words changed under their translation: in each block with code (`all`), or each whose code
 * words no longer sum as when its table entries were set, the words changed only in immediates and
 * shift amounts keep their translation, patched; the others lose it, and with them the translated
 * words that reach them without the runtime (coderun.h); the rest of the block keeps it. Blocks go
 * from the top, each with what the one above marked in it. `st`: KB of code, KB changed, host words
 * patched, words lost; the first four blocks losing words are logged. */
static u8 cbad[2 * CR_WORDS]; /* cr_spread's window: the block below, then the block */
static void recheck(int all, u32 st[4])
{
    u32 logged = 0;
    int carried = 0; /* cbad's lower half holds marks for the next block down */
    u8 any, *up = cbad + CR_WORDS;
    for (u32 b = BLKS; b-- > 0;) {
        if (carried)
            memcpy(up, cbad, CR_WORDS), memset(cbad, 0, CR_WORDS);
        int has = carried;
        carried = 0;
        u32 f = 0, s = 0;
        if (blk_code[b])
            st[0]++, s = blk_code_sum(b, &any, &f, blk_slot0[b]);
        int diff = blk_code[b] && (all || s != blk_sum[b]), same = f == blk_form[b];
        if (!diff && !has)
            continue;
        int whole = 0;
        if (diff) {
            st[1]++;
            if (block_patch(b, up, 0, same) < 0) {
                whole = 1;
                for (u32 i = 0; i < CR_WORDS; i++)
                    up[i] = (u8)held_word(NULL, (b << (BLK_SHIFT - 2)) + i);
            }
        }
        cr_spread((const u32 *)P->ram, b << (BLK_SHIFT - 2), cbad, held_word, NULL);
        if (diff && !whole)
            st[2] += (u32)block_patch(b, up, 1, same);
        u32 d = dirty_words(b, up);
        if (d && logged++ < 4) {
            if (whole)
                say("native: %08X: code rewritten where the patch table cannot say (%.16s: %u at word %u: %08X, %08X at %08X): the block to the interpreter",
                    (unsigned)(0x80000000u | b << BLK_SHIFT), why_e ? why_e->name : "?", (unsigned)(why[0] & 0xFF), (unsigned)(why[0] >> 8),
                    (unsigned)why[1], (unsigned)why[2], (unsigned)why[3]);
            else
                say("native: %08X: %u words of code rewritten, or reaching rewritten ones: to the interpreter", (unsigned)(0x80000000u | b << BLK_SHIFT),
                    (unsigned)d);
        }
        st[3] += d;
        memset(up, 0, CR_WORDS);
        carried = memchr(cbad, 1, CR_WORDS) != NULL;
    }
    if (st[2])
        flush_code();
}

/* The I-cache was flushed: code CPU stores changed is checked (recheck). The first flushes, and
 * those that find a change, are logged with their time. */
static void cache_flushed(void)
{
    static u32 n;
    u32 t = sceKernelGetSystemTimeLow(), st[4] = {0};
    recheck(0, st);
    if (n++ < 8 || st[1])
        say("native: cache flush %u: %u of %u KB of code changed, %u words patched, %u words to the interpreter (%u us)", (unsigned)n,
            (unsigned)st[1], (unsigned)st[0], (unsigned)st[2], (unsigned)st[3], (unsigned)(sceKernelGetSystemTimeLow() - t));
}

/* PSX.ram_loaded: the translations of what was overwritten are checked again before use. `len` 0:
 * the I-cache was flushed. */
static void on_ram_loaded(Psx *psx, u32 addr, u32 len)
{
    (void)psx;
    if (!len) {
        cache_flushed();
        return;
    }
    u32 t = sceKernelGetSystemTimeLow(), a = addr & (RAM_SIZE - 4), end = a + ((len + 3) & ~3u);
    if (end > RAM_SIZE)
        end = RAM_SIZE;
    for (u32 x = a; x < end; x += 4)
        table[x >> 2] = STUB_DIRTY;
    blk_resum(a, end - a);
    for (int i = 0; i < nimg; i++)
        if (a < img[i].text + img[i].e->text_size && img[i].text < end)
            img[i].gen++;
    native_stats.us_loaded += sceKernelGetSystemTimeLow() - t;
}

/* ---- time ---- */
static void sync_time(void)
{
    static u64 last;
    if (P->cycles - last < SYNC_STEP)
        return;
    last = P->cycles;
    u64 now = sceKernelGetSystemTimeWide(), clock = c_base + us_cycles(now - t_base);
    if (P->cycles + BEHIND_MAX < clock) /* the host fell behind: PS1 time slows down, never skips */
        c_base = P->cycles + BEHIND_MAX, t_base = now;
    else if (P->cycles > clock + AHEAD_MAX) {
        u32 t = sceKernelGetSystemTimeLow();
        sceKernelDelayThread((SceUInt)((P->cycles - clock) * frame_us3() / (3u * P->frame_cycles)));
        native_stats.us_sleep += sceKernelGetSystemTimeLow() - t;
    }
}

/* At each PS1 vblank, its frame presented, the next PS1 frame is given the PSP frame up to
 * VS_MARGIN before the vblank after this one's: its swap lands there however its work jitters.
 * When this frame made its vblank (or came before it: the runtime does not sleep inside a
 * frame), the next one starts at its slot, slept to if still ahead; when
 * it did not, the next one's slot is still the one after this one's: up to OWE_MAX vblanks are
 * owed, beyond that PS1 time slows down. For an owed vblank the CPU stalls to VS_SHORT PS1 cycles
 * before it: PS1 time keeps up with the PSP's, and a game that counts vblanks moves as far as on
 * time, at fewer frames. The stall moves the cycle count, so the timers, the CD and the SPU see the
 * whole frame pass too (vblanks VS_SHORT apart alone made the CD's vblank-counted timeouts expire
 * 8 times early: with a late PSP the level fell into disc errors). (Before, the late frames ran on at one vblank each
 * and went unshown: on the PSP the level ran at 55% speed showing 11 pictures a second, against
 * 90% and 40 with the old over-charged loops, which made frames span vblanks.)
 * The FMV lost 13 vblanks a second when a late frame's slot
 * moved to the vblank that passed, while its light frames slept 110 ms (S21). The vblanks' times come from a grid, a
 * frame of 1001/60 ms from one seen at boot, kept inside what the count says at each call: vblank
 * v has come, v + 1 has not. sceDisplayWaitVblankStart is not used: called close to a vblank,
 * PPSSPP's waits for the next. */
#define VS_US(n) ((u64)(n) * 50050u / 3u) /* n frames of 16683.3 us */
#define VS_MARGIN 6000u
/* Vblanks owed at most: with the margin, 39.4 ms behind, inside BEHIND_MAX */
#define OWE_MAX 2u
/* PS1 cycles run to an owed vblank after the stall: 1/8 of a frame */
#define VS_SHORT (P->frame_cycles / 8)
void native_vsync(void)
{
    u32 v = sceDisplayGetVcount();
    u64 now = sceKernelGetSystemTimeWide(), t_v = t_lock + VS_US(v - v_lock);
    if (t_v > now)
        t_lock -= t_v - now, t_v = now;
    else if (now - t_v > VS_US(1))
        t_lock += now - t_v - VS_US(1), t_v = now - VS_US(1);
    u32 s = vs_step();
    if ((s32)(vc - v) > 0) /* early (no sleep inside a frame): made vblank vc, still to come */
        t_v += VS_US(vc - v), v = vc;
    if (v == vc) {
        t_base = t_v + VS_US(s) - VS_MARGIN, vc = v + s;
        if (t_base > now) {
            u32 dt = (u32)(t_base - now);
            sceKernelDelayThread(dt);
            native_stats.us_sleep += dt, native_stats.us_vs_wait += dt, native_stats.vs_wait++;
        }
    } else {
        u32 late = (u32)(now - t_v);
        native_stats.vs_late++, native_stats.us_late += late;
        if (late > native_stats.us_late_max)
            native_stats.us_late_max = late;
        if (v - vc > OWE_MAX) /* forgiven: PS1 time slows down */
            native_stats.vs_adv += v - vc - OWE_MAX, vc = v - OWE_MAX;
        vc += s;
        t_base = t_lock + VS_US(vc - v_lock) - VS_MARGIN;
        /* the next slot has passed too: the CPU stalls to VS_SHORT before its vblank (psx_update
         * has moved next_vblank a frame on already; update() runs the stall, event by event) */
        if ((s32)(v - vc) >= 0 && P->cycles + VS_SHORT < P->next_vblank)
            stall_to = P->next_vblank - VS_SHORT, native_stats.vs_short++;
    }
    c_base = P->cycles;
}

/* Before the frame is presented: one that came before its PSP vblank (vc) waits for it, or its
 * swap would replace the last frame's before that one was shown. */
void native_vsync_early(void)
{
    u32 v = sceDisplayGetVcount();
    if ((s32)(vc - v) <= 0)
        return;
    u64 now = sceKernelGetSystemTimeWide(), t = t_lock + VS_US(vc - v_lock);
    if (t > now) {
        u32 dt = (u32)(t - now);
        sceKernelDelayThread(dt);
        native_stats.us_sleep += dt, native_stats.us_vs_wait += dt;
    }
}

/* Times one call in PROF_EVERY, and every vblank (rare, and the longest: the frame is
 * presented and paced inside), with psx_update's parts (Psx.us_upd); the clock reads they add
 * are taken off. */
static void update_once(void);
/* psx_update, then the stall native_vsync asked for: the guest runs no code while the devices
 * go through their events in order up to stall_to (one jump made the CD lose interrupts: the
 * boot hung in CD_init). */
static void update_stall(void);
/* Brackets one non-vblank update in PB_EVERY (a vblank's presents and sleeps: other threads run
 * inside it, on the same counters). */
static void update(void)
{
    static u32 k;
    u32 calls = native_stats.calls;
    int b = P->cycles < P->next_vblank && !(k++ % PB_EVERY) && prof_begin(PB_UPDATE, PB_EVERY);
    native_stats.updates++;
    update_stall();
    if (b)
        prof_end(PB_UPDATE, native_stats.calls != calls);
}

static void update_stall(void)
{
    update_once();
    if (stall_to > P->cycles) /* native_vsync's c_base is after the stall, as for a frame on time */
        c_base += stall_to - P->cycles;
    for (int k = 0; stall_to > P->cycles; k++) {
        u64 to = stall_to < P->next_event && k < 1024 ? stall_to : P->next_event;
        if (k >= 1024 || to <= P->cycles)
            to = stall_to;
        P->cycles = to;
        if (P->cycles >= P->next_event)
            psx_update(P);
    }
    stall_to = 0;
}

static void update_once(void)
{
    static u32 n;
    u32 s = P->cycles >= P->next_vblank ? 1 : n++ % PROF_EVERY ? 0 : PROF_EVERY;
    if (!s || P->prof_scale) { /* inside a timed one (a guest callback of the vblank): its part */
        u32 s0 = P->prof_scale;
        P->prof_scale = 0;
        psx_update(P);
        P->prof_scale = s0;
        return;
    }
    P->prof_scale = s, P->prof_bias = prof_bias * s / PROF_EVERY, P->prof_reads = 0;
    u32 t = sceKernelGetSystemTimeLow();
    psx_update(P);
    native_stats.us_update += (sceKernelGetSystemTimeLow() - t) * s - (P->prof_reads + 1) * P->prof_bias;
    P->prof_scale = 0;
}

/* At every entry from translated code: the cycles its loops spent join the count, and the
 * budget is refilled. */
static void charge(void)
{
    u32 b = ctx[C_BUDGET];
    if ((s32)b < -(s32)(FORCED / 2))
        b += FORCED;
    P->cycles += (u32)((s32)BUDGET - (s32)b) >> cpu_shift;
    ctx[C_BUDGET] = BUDGET;
}

static SceUInt alarm(void *common)
{
    (void)common;
    if ((s32)ctx[C_BUDGET] >= -(s32)(FORCED / 2))
        ctx[C_BUDGET] -= FORCED;
    return ALARM_US;
}

/* ---- the interpreter, between translations ---- */

/* A guest `ra` the translated code may return to: its translation, or GRET. Only a return site, a
 * KUSEG/KSEG0/KSEG1 word after a jal/jalr/bltzal/bgezal and its delay slot, or a KSEG0/KSEG1 BIOS
 * address: hand-written code keeps data in ra (Crash 2's rasterizer at 8003C360, `move ra,v1`
 * with a jump offset in the top byte, 0x08..0xC8, read by `srl at,ra,24`), and GRET there sent
 * 8003C448's `jr at` to 8003C459. KUSEG too: Lilo & Stitch runs at 00018000, and its code loaded
 * at run time (interpreted) calls `jal 73178` at 000D9A1C; the translated callee saved the guest
 * ra and its `jr ra` jumped to 0x000D9A24 as host code. Only the ra a link the runtime ran wrote
 * (ra_link), never one a move or a load did: a word after a call is no proof. The same
 * rasterizer's commands are an offset byte and a halfword (`lui v1,0x800; or v1,v1,at` at
 * 8003C5A4), 00 and 80..B8 KUSEG/KSEG0/KSEG1 words of the first 64 KB, and it pushes them with
 * `move sp,ra` (8003C55C); run by the interpreter, a command whose word 8 bytes below was a call
 * became GRET, and after `move ra,sp` 8003C448's `jr at` went to 8003C459 again (PPSSPP, attract
 * mode, frame 5016: guest sp 0x09A00FC0, GRET). */
static void fix_ra(Cpu *c)
{
    u32 ra = c->gpr[31], p = ra & 0x1FFFFFFFu;
    if (ra != ra_link || (ra >> 29 == 0 ? ra >= 0x800000 : ra >> 29 != 4 && ra >> 29 != 5))
        return;
    u32 h = host_of(ra);
    if (h) {
        c->gpr[31] = h;
    } else if (p < 0x800000 || (p & 0x1FF00000u) == 0x1FC00000u) {
        gret_push(ra, c->gpr[29]);
        c->gpr[31] = STUB_GRET;
    }
}

/* After a call the interpreter ran at `at`: the link register gets the translation of the return
 * site when there is one, as a translated call writes it. A callee whose prologue is interpreted
 * (a function pointer the discovery missed) then saves an ra its translated epilogue can return
 * to: Crash 2's 80041FAC keeps it in the scratchpad for 800420E4's `lw ra; jr ra`. */
static void link_host(Cpu *c, u32 at)
{
    if (c->in_delay && c->gpr[31] == at + 8)
        ra_link = at + 8;
    if (!c->in_delay || (at & 0x1FFFFFFFu) >= 0x800000)
        return;
    u32 w = ram_r32(P, at), op = w >> 26, rd, h;
    if (op == 3 || (op == 1 && (w >> 17 & 15) == 8))
        rd = 31;
    else if (op == 0 && (w & 63) == 9)
        rd = (w >> 11) & 31;
    else
        return;
    if (rd && c->gpr[rd] == at + 8 && (h = host_of(at + 8)) != 0)
        c->gpr[rd] = h;
}

/* Interprets from cpu.pc until an instruction boundary with a translation, and returns its host
 * address. `mode`: 0 checks at once, 1 also loads at a MISS first address, 2 runs one
 * instruction first (a resume point maps to its own host call). */
static u32 interp(int mode);

/* Where the interpreter is entered (psxrt.trace bit 8): per guest pc, entries and instructions
 * run, for the log's "-- interp" line. A full table drops new pcs. */
#define ENTRY_SLOTS 256
static struct {
    u32 pc, n, steps;
} entries[ENTRY_SLOTS];

static void entry_note(u32 pc, u32 steps)
{
    for (u32 i = (pc >> 2) * 2654435761u >> 24, k = 0; k < ENTRY_SLOTS; k++, i = (i + 1) % ENTRY_SLOTS)
        if (!entries[i].n || entries[i].pc == pc) {
            entries[i].pc = pc, entries[i].n++, entries[i].steps += steps;
            return;
        }
}

int native_entries_log(char *buf, int len)
{
    int n = snprintf(buf, (size_t)len, "-- interp (pc entries/steps):");
    for (int top = 0; top < 10 && n < len; top++) {
        int best = -1;
        for (int i = 0; i < ENTRY_SLOTS; i++)
            if (entries[i].n && (best < 0 || entries[i].steps > entries[best].steps))
                best = i;
        if (best < 0)
            break;
        n += snprintf(buf + n, (size_t)(len - n), " %08X %u/%u", (unsigned)entries[best].pc, (unsigned)entries[best].n,
                      (unsigned)entries[best].steps);
        entries[best].n = 0;
    }
    memset(entries, 0, sizeof entries);
    return n < len ? n + snprintf(buf + n, (size_t)(len - n), "\n") : len;
}

/* us_interp is the interpreter's own time: inside a timed run_guest, the work other counters time
 * (device updates, sync_time's sleeps, guest calls run natively) is taken off. In a filmed second
 * of Sheep Dog its update() calls were 30-50 ms of "interp" against ~2 ms of interpreting (the
 * 473 ReturnFromException a second), and one long update in a sample counted 16 times made the
 * 74-425 ms spikes. The outermost bracket open sets excl_us to its start value plus its length,
 * whatever timed samples nested inside it took off. */
static int timing;   /* timed run_guests in progress */
static int excl_open; /* a bracket is open (inside the innermost timed run_guest) */
static u32 excl_us;
typedef struct {
    u32 t, e;
    int on;
} Excl;

static void excl_begin(Excl *x)
{
    x->on = timing && !excl_open;
    if (x->on)
        excl_open = 1, x->e = excl_us, x->t = sceKernelGetSystemTimeLow();
}

static void excl_end(const Excl *x)
{
    if (x->on)
        excl_us = x->e + (sceKernelGetSystemTimeLow() - x->t), excl_open = 0;
}

static u32 run_guest(int mode)
{
    static u32 n;
    u32 pc = P->cpu.pc, s0 = native_stats.steps, h;
    if (n++ % PROF_EVERY) {
        h = interp(mode);
    } else {
        int open = excl_open;
        u32 e0 = excl_us, t = sceKernelGetSystemTimeLow();
        excl_open = 0, timing++;
        h = interp(mode);
        timing--, excl_open = open;
        native_stats.us_interp += (sceKernelGetSystemTimeLow() - t - (excl_us - e0)) * PROF_EVERY - prof_bias;
    }
    if (P->trace & 256)
        entry_note(pc, native_stats.steps - s0);
    return h;
}

/* The interpreter's last steps (pc, ra, at after it) and where it was entered: the log of a fatal
 * error shows how the guest got there. */
#define HIST 16
static u32 hist[HIST][3], hist_n, hist_entry, hist_mode;

static void hist_log(const Cpu *c)
{
    psx_log(P, "interp entered at %08X (mode %u); last steps (pc ra at):", (unsigned)hist_entry, (unsigned)hist_mode);
    for (u32 i = hist_n < HIST ? 0 : hist_n - HIST; i < hist_n; i++)
        psx_log(P, "  %08X %08X %08X", (unsigned)hist[i % HIST][0], (unsigned)hist[i % HIST][1],
                (unsigned)hist[i % HIST][2]);
    for (int i = 0; i < 32; i += 8)
        psx_log(P, "  r%02d %08X %08X %08X %08X %08X %08X %08X %08X", i, (unsigned)c->gpr[i], (unsigned)c->gpr[i + 1],
                (unsigned)c->gpr[i + 2], (unsigned)c->gpr[i + 3], (unsigned)c->gpr[i + 4], (unsigned)c->gpr[i + 5],
                (unsigned)c->gpr[i + 6], (unsigned)c->gpr[i + 7]);
}

static u32 interp(int mode)
{
    static int isolated; /* the cache was isolated since the last return to translated code */
    Cpu *c = &P->cpu;
    hist_entry = c->pc, hist_mode = (u32)mode;
    for (u32 n = 0;; n++) {
        if (P->exiting) {
            if (P->exiting == 2)
                hist_log(c);
            runtime_halt(P->exiting == 2 ? "the interpreter hit a fatal error" : "the game exited");
        }
        if (!c->in_delay && mode != 2) {
            u32 pc = c->pc;
            if (pc == STUB_GRET) {
                jump(c, gret_pop(c->gpr[29]));
                continue;
            }
            /* Cache isolated: only the interpreter keeps stores out of RAM. Its end is a flush. */
            if (c->sr & 0x10000) {
                isolated = 1;
            } else {
                if (isolated)
                    isolated = 0, cache_flushed();
                /* the BIOS vectors are cpu_step's HLE whatever RAM holds there: no translation to
                 * look for (try_load's floating-code search ran on every ReturnFromException) */
                u32 h = host_of(pc), v = pc & 0x1FFFFFFFu;
                if (!h && v != 0xA0 && v != 0xB0 && v != 0xC0 && try_load(pc, mode == 1))
                    h = host_of(pc);
                if (h) {
                    finish_load(c);
                    fix_ra(c);
                    return h;
                }
            }
        }
        mode = 0;
        if (!(n & 255) || P->cycles >= P->next_event) {
            Excl x;
            excl_begin(&x);
            if (!(n & 255))
                sync_time();
            if (P->cycles >= P->next_event)
                update();
            excl_end(&x);
        }
        u32 at = c->pc, p = at & 0x1FFFFFFFu;
        if (cpu_step(P) < 0)
            P->exiting = 2;
        link_host(c, at);
        u32 *hs = hist[hist_n++ % HIST];
        hs[0] = at, hs[1] = c->gpr[31], hs[2] = c->gpr[1];
        if (p == 0xA0 || p == 0xB0 || p == 0xC0)
            native_stats.bios++;
        else
            native_stats.steps++;
    }
}

/* The device at a physical address, for the per-device times in the log. */
static int io_kind_at(u32 a)
{
    if (a >= 0x1F801810 && a < 0x1F801818)
        return IO_GPU;
    if (a >= 0x1F801080 && a < 0x1F801100)
        return IO_DMA;
    if (a >= 0x1F801800 && a < 0x1F801804)
        return IO_CD;
    if (a >= 0x1F801C00 && a < 0x1F802000)
        return IO_SPU;
    if (a >= 0x1F801100 && a < 0x1F801130)
        return IO_TIMER;
    if (a >= 0x1F801070 && a < 0x1F801078)
        return IO_IRQ;
    if (a >= 0x1F801820 && a < 0x1F801828)
        return IO_MDEC;
    if (a >= 0x1F801040 && a < 0x1F801060)
        return IO_PAD;
    return IO_OTHER;
}

/* What a host-call instruction touches. */
static int io_kind(const Cpu *c, u32 insn)
{
    u32 op = insn >> 26;
    if (op == 0x12 || op == 0x32 || op == 0x3A)
        return IO_GTE;
    if (op == 0x10)
        return IO_COP0;
    if (op < 0x20)
        return IO_OTHER;
    return io_kind_at((c->gpr[(insn >> 21) & 31] + (u32)(s32)(s16)insn) & 0x1FFFFFFFu);
}

/* GTE instructions run straight on the guest registers in CTX: they touch no device, need no
 * clock and take no interrupt, so a host call for one skips the CPU state copy and the
 * interpreter. LWC2/SWC2 here only reach RAM and the scratchpad, aligned; 0 if the full path
 * must run the instruction. */
static int gte_fast(u32 insn)
{
    u32 op = insn >> 26, rs = (insn >> 21) & 31, rt = (insn >> 16) & 31, rd = (insn >> 11) & 31;
    u32 vs = rs ? ctx[rs] : 0, vt = rt ? ctx[rt] : 0;
    if (op == 0x12) {
        if (rs & 0x10) {
            gte_command(P, insn & 0x1FFFFFF);
            P->cycles += 10;
            return 1;
        }
        switch (rs) {
        case 0x00: if (rt) ctx[rt] = gte_read_data(P, (int)rd); break;
        case 0x02: if (rt) ctx[rt] = gte_read_ctrl(P, (int)rd); break;
        case 0x04: gte_write_data(P, (int)rd, vt); break;
        case 0x06: gte_write_ctrl(P, (int)rd, vt); break;
        default: return 0;
        }
        P->cycles += 2;
        return 1;
    }
    u32 a = vs + (u32)(s32)(s16)insn, p = a & 0x1FFFFFFFu;
    u32 *m;
    if ((a & 3) || (P->cpu.sr & 0x10000))
        return 0;
    if (p < 0x800000)
        m = (u32 *)(P->ram + (p & (RAM_SIZE - 1)));
    else if (p >= 0x1F800000 && p < 0x1F800400)
        m = (u32 *)(P->scratch + (p & 0x3FF));
    else
        return 0;
    if (op == 0x32)
        gte_write_data(P, (int)rt, *m);
    else
        *m = gte_read_data(P, (int)rt);
    P->cycles += 2;
    return 1;
}

/* Before a timer read: a loop that polls a counter (a game spinning on VSync(-1), whose stable
 * read of the hblank counter repeats until two reads agree) lets PS1 time pass instead of
 * spinning a host call per few dozen cycles. The loop's site is the first read after a pause of
 * POLL_PAIR cycles or more (a read right after another is the second half of a stable read, and
 * time must not move between the two). From its fourth return less than POLL_GAP cycles after
 * the last, the clock moves on to one tick of the counter after that previous read, never past
 * the next device event: counts derive from the clock and no value is stepped over, so an
 * equality test still sees its value. When the code only compares the value's magnitude with a
 * limit (pollscan.c, the streamed-frame wait of S21), the clock moves on to where that comparison
 * flips, the counter's target or wrap, or the next device event, whichever comes first. */
#define POLL_PAIR NATIVE_POLL_PAIR
#define POLL_GAP NATIVE_POLL_GAP

/* The guest return address that `ra` stands for. Translated calls write a host ra (the start of
 * the translation of the word after the call's delay slot), calls from the interpreter into
 * translated code STUB_GRET: found back in the table, or in gret. The search reads the whole text
 * (about a millisecond for a level), so the last RA_SEEN answers are remembered, any ra in any
 * entry (polling loops return to a few sites, which may alternate), a found one checked against
 * the table again. 1 (no word can be read there) when it is host code that is not a return site. */
#define RA_SEEN 16
static u32 guest_ra(u32 ra, u32 sp)
{
    static u32 seen_h[RA_SEEN], seen_g[RA_SEEN], next;
    if (ra == STUB_GRET)
        return gret_top ? gret[(gret_at(sp) - 1) % GRET_DEPTH] : 1;
    if (ra - code_lo >= code_hi - code_lo)
        return ra;
    int k = -1;
    for (int i = 0; i < RA_SEEN && k < 0; i++)
        if (seen_h[i] == ra)
            k = i;
    if (k >= 0 && (seen_g[k] == 1 || table[(seen_g[k] & (RAM_SIZE - 4)) >> 2] == ra))
        return seen_g[k];
    if (k < 0)
        k = next++ % RA_SEEN;
    u32 last_g = 0;
    for (int i = 0; i < nimg && !last_g; i++) {
        const Image *m = &img[i];
        if (!m->installed || ra - m->e->host_base >= m->e->code_len)
            continue;
        const u32 *t = &table[m->text >> 2];
        for (u32 j = 2; j < (m->e->text_size + 3) / 4; j++) {
            u32 w = t[j] == ra ? ram_r32(P, 0x80000000u | (m->text + 4 * (j - 2))) : 0;
            if (w >> 26 == 3 || (w >> 26 == 0 && (w & 63) == 9) || (w >> 26 == 1 && (w >> 20 & 1))) {
                last_g = 0x80000000u | (m->text + 4 * j);
                break;
            }
        }
    }
    for (u32 i = 0; i < ar.n && !last_g; i++) {
        const FlPlace *q = &ar.p[i];
        if (ra - q->host >= q->len)
            continue;
        const u32 *t = &table[q->at >> 2];
        for (u32 j = 2, n = pl_words(q); j < n; j++) {
            u32 w = t[j] == ra ? ram_r32(P, q->guest + 4 * (j - 2)) : 0;
            if (w >> 26 == 3 || (w >> 26 == 0 && (w & 63) == 9) || (w >> 26 == 1 && (w >> 20 & 1))) {
                last_g = q->guest + 4 * j;
                break;
            }
        }
    }
    native_stats.ra_scans++;
    seen_h[k] = ra, seen_g[k] = last_g ? last_g : 1;
    return seen_g[k];
}

NativePoll native_poll;

static void poll_skip(u32 pc, u32 addr, const u32 *gpr)
{
    NativePoll *q = &native_poll;
    u32 tick = timer_tick(P, addr);
    u64 now = P->cycles;
    if (native_poll_note(q, pc, tick, now, P->next_event)) {
        u32 g[32], room, raw = timer_poll_count(P, addr, &room);
        for (int i = 0; i < 32; i++)
            g[i] = gpr[i];
        g[31] = guest_ra(g[31], g[29]);
        u32 t = poll_bound(P, pc, g, raw, room);
        if (t) {
            u64 to = timer_tick_at(P, addr, t);
            P->cycles = to < P->next_event ? to : P->next_event;
            q->hot_at = q->last_at = P->cycles;
            native_stats.poll_skips++;
            native_stats.poll_far++;
            return;
        }
        u64 d = P->next_event - now, s = tick - q->period;
        P->cycles += d < s ? d : s;
        q->hot_at = q->last_at = P->cycles;
        native_stats.poll_skips++;
    }
}

/* Loads and stores on device registers run straight on the guest registers in CTX, like the GTE
 * instructions: no CPU state copy and no interpreter. Not when a device event is due or an
 * interrupt pending (the full path serves those first); an interrupt the access raises is taken
 * after it, as on the full path. 0 if the full path must run the instruction. */
static int io_fast(u32 pc, u32 insn, u32 back, u32 *next)
{
    u32 op = insn >> 26, rs = (insn >> 21) & 31, rt = (insn >> 16) & 31;
    int size;
    switch (op) {
    case 0x20: case 0x24: case 0x28: size = 1; break;
    case 0x21: case 0x25: case 0x29: size = 2; break;
    case 0x23: case 0x2B: size = 4; break;
    default: return 0;
    }
    u32 a = (rs ? ctx[rs] : 0) + (u32)(s32)(s16)insn, p = a & 0x1FFFFFFFu;
    if ((a & (u32)(size - 1)) || p < 0x1F801000 || p >= 0x1F803000)
        return 0;
    charge();
    sync_time();
    if (P->cycles >= P->next_event || psx_irq_pending(P))
        return 0;
    static u32 n;
    int kind = io_kind_at(p), timed = !(n++ % PROF_EVERY);
    u32 t = timed ? sceKernelGetSystemTimeLow() : 0;
    if (op < 0x28) {
        if (kind == IO_TIMER)
            poll_skip(pc, p, (const u32 *)ctx);
        u32 v = psx_read(P, a, size);
        if (op == 0x20)
            v = (u32)(s32)(s8)v;
        else if (op == 0x21)
            v = (u32)(s32)(s16)v;
        if (rt)
            ctx[rt] = v;
    } else
        psx_write(P, a, rt ? ctx[rt] : 0, size);
    P->cycles += 2;
    if (timed) {
        t = (sceKernelGetSystemTimeLow() - t) * PROF_EVERY - prof_bias;
        native_stats.us_io += t;
        native_stats.us_io_kind[kind] += t;
    }
    native_stats.calls++;
    native_stats.n_io[kind]++;
    native_stats.io_host += kind == IO_TIMER || kind == IO_IRQ || kind == IO_PAD;
    *next = back;
    if (psx_irq_pending(P)) {
        Cpu *c = &P->cpu;
        from_ctx(c);
        jump(c, back);
        cpu_exception(P, 0, c->pc, 0);
        *next = to_ctx(c, run_guest(0));
    }
    return 1;
}

/* IO_WRITE (psxrt_io_write) with the guest address `a`: an aligned store to the pad port, I_STAT
 * or I_MASK runs here as io_fast() runs it, the value from CTX, without the host-call path (no
 * CPU state copy, no clock read). None of these raises an interrupt but an I_MASK write that
 * unmasks a pending one: that write is undone and, like any write when a device event is due or
 * an interrupt pending, goes on as a host call (nonzero). */
u32 native_io_write(u32 a)
{
    const u32 *site = (const u32 *)ctx[C_SITE];
    u32 insn = site[1], op = insn >> 26, rt = (insn >> 16) & 31, p = a & 0x1FFFFFFFu;
    u32 size = op == 0x2B ? 4 : op == 0x29 ? 2 : 1;
    int sio = p - 0x1F801040 < 0x20;
    if ((a & (size - 1)) || (!sio && p - 0x1F801070 >= 8) || (s32)ctx[C_BUDGET] < -(s32)(FORCED / 2))
        return 1;
    charge();
    if (P->cycles >= P->next_event || psx_irq_pending(P)) {
        native_stats.io_fall[P->cycles >= P->next_event ? 0 : 1]++;
        return 1;
    }
    u32 mask = P->i_mask;
    psx_write(P, a, rt ? ctx[rt] : 0, (int)size);
    if (psx_irq_pending(P)) {
        P->i_mask = mask;
        native_stats.io_fall[1]++;
        return 1;
    }
    P->cycles += 2;
    native_stats.calls++;
    native_stats.n_io[sio ? IO_PAD : IO_IRQ]++;
    native_stats.io_lean++;
    return 0;
}

/* ---- entries from trampolines.S ---- */

volatile int native_snap_req;
static int guest_calls; /* native_call() depth */

/* Back to translated code from an entry: the guest state into the context, and a requested
 * snapshot when no guest call of the BIOS is in progress. */
static u32 leave(Cpu *c, u32 host)
{
    to_ctx(c, host);
    if (native_snap_req && !guest_calls) {
        native_snap_req = 0;
        runtime_snapshot(host);
    }
    return host;
}

/* ---- recipes (crates/psx2psp-recomp/src/hooks.rs) ---- */

#define HOOK 2u
enum { HOOK_PAD_CHK_RC2_WAIT = 1, HOOK_PAD_SIO_ACK_WAIT = 2, HOOK_DEC_DCT_VLC = 3, HOOK_VSYNC_WAIT = 4 };
#define T2_COUNT 0x1F801120u
#define T2_MODE 0x1F801124u
#define T2_TARGET 0x1F801128u

/* A guest device or memory read, as the recipe's code makes it (2 cycles, like io_fast). */
static u32 rd(u32 a, int size)
{
    native_stats.n_io[io_kind_at(a & 0x1FFFFFFFu)]++;
    P->cycles += 2;
    return psx_read(P, a, size);
}

/* PS1 time on to `due`, never past the next device event, which then runs. */
static void wait_until(u64 due)
{
    u64 to = due < P->next_event ? due : P->next_event;
    if (to > P->cycles)
        P->cycles = to;
    if (P->cycles >= P->next_event)
        update();
}

/* libpad's waits on root counter 2: `elapsed` is the count since `start` (a wrap adds the target),
 * in counter ticks when the mode has bit 9 (sysclock/8), else in eighths of them; the wait expires
 * when it reaches `limit`. The cycles from `cnt` until it does (one tick at least), or 0 when the
 * counter is stopped. */
static u32 rc2_left(u32 cnt_addr, u32 cnt, u32 start, u32 limit, u32 mode)
{
    u32 tick = timer_tick(P, cnt_addr & 0x1FFFFFFFu);
    if (!tick)
        return 0;
    s32 d = (s32)(cnt - start), left = (mode & 0x200) ? (s32)limit - d : 8 * (s32)limit - d;
    return (left > 0 ? (u32)left : 1u) * tick;
}

/* `_padChkRC2wait()` (hooks.rs PAD_CHK_RC2_WAIT): its reads and its results (v0 expired, v1 the
 * limit, a0 the count). Its callers poll it until it expires, waiting for nothing but the counter
 * and the device events (the /ACK), so when it has not, PS1 time moves on to the expiry or to the
 * next event: the next call returns what the loop would have. Returns to `ra`. */
static u32 hook_pad_chk(Cpu *c, const u32 *arg)
{
    u32 start = ram_r32(P, arg[0]), limit = ram_r32(P, arg[1]);
    u32 cnt = rd(T2_COUNT, 2) & 0xFFFF;
    if ((s32)cnt < (s32)start) {
        u32 t = rd(T2_TARGET, 2);
        cnt += t ? rd(T2_TARGET, 2) : 0x10000u;
    }
    u32 mode = rd(T2_MODE, 2);
    s32 e = (s32)(cnt - start);
    if (!(mode & 0x200))
        e >>= 3;
    c->gpr[2] = !(e < (s32)limit);
    c->gpr[3] = limit;
    c->gpr[4] = cnt;
    if (!c->gpr[2]) {
        u32 left = rc2_left(T2_COUNT, cnt, start, limit, mode);
        wait_until(left ? P->cycles + left : ~0ull);
    }
    return c->gpr[31];
}

/* The /ACK wait loop of `_padSioRW2` (hooks.rs PAD_SIO_ACK_WAIT), on its registers: a pass makes
 * the loop's reads; between passes PS1 time moves on to the timeout or to the next device event
 * (the /ACK is one). Returns the /ACK exit (v0 0x80), the timeout exit (v0 -2), or the loop's head
 * `pc` when an interrupt is to be taken there. */
#define SIO_PASS 24u /* PS1 cycles of one pass besides its reads */
static u32 hook_pad_ack(Cpu *c, const u32 *arg, u32 pc)
{
    u32 *g = c->gpr;
    for (;;) {
        rd(g[7], 2);
        u32 cnt = rd(g[9], 2) & 0xFFFF;
        if ((s32)cnt < (s32)g[4]) {
            u32 t = rd(g[6], 2);
            cnt += t ? rd(g[6], 2) : g[10];
        }
        u32 mode = rd(g[7], 2);
        s32 e = (s32)(cnt - g[4]);
        if (!(mode & 0x200))
            e >>= 3;
        g[3] = cnt;
        g[2] = (u32)-2;
        if (!(e < (s32)g[5]))
            return arg[1];
        if (rd(g[8], 4) & 0x80) {
            g[2] = 0x80;
            return arg[0];
        }
        g[2] = 0;
        P->cycles += SIO_PASS;
        u32 left = rc2_left(g[9], cnt, g[4], g[5], mode);
        wait_until(left ? P->cycles + left : ~0ull);
        if (psx_irq_pending(P))
            return pc;
    }
}

/* libpress's `DecDCTvlc(bs, buf)` (hooks.rs DEC_DCT_VLC) in C (vlc.c): the same output and the
 * same PS1 time as its code. It runs up to the next device event, a block at a time; when an
 * interrupt is then due, the guest takes it at the routine's entry `pc` with the call's registers
 * as they were, and the call coming back there goes on where it stopped. Returns to `ra`. */
#define IN_RAM(a) (((a) & 0x1FFFFFFFu) < 0x800000u)
static u32 hook_dct_vlc(Cpu *c, const u32 *arg, u32 pc)
{
    static Vlc r;
    static u32 key[3], spent;
    static int busy;
    u32 *g = c->gpr;
    const VlcVars v = { arg[0], arg[0] + 4, arg[0] + 0x28, arg[1], arg[1] + 0x10000 };
    u32 t = sceKernelGetSystemTimeLow(), next;
    if (!busy || key[0] != g[4] || key[1] != g[5] || key[2] != g[31]) {
        vlc_start(P->ram, &v, &r, g[4], g[5]);
        key[0] = g[4], key[1] = g[5], key[2] = g[31], spent = 0, busy = 1;
        native_stats.n_io[IO_VLC]++;
    }
    for (;;) {
        u64 left = P->next_event > P->cycles ? (P->next_event - P->cycles) << cpu_shift : 1;
        int ret = vlc_run(P->ram, &v, &r, left < 0x40000000u ? (u32)left : 0x40000000u);
        u32 now = r.ins + 2 * r.mem;
        P->cycles += (now >> cpu_shift) - (spent >> cpu_shift);
        spent = now;
        if (ret >= 0) {
            busy = 0;
            g[2] = (u32)ret, g[4] = r.a0, g[5] = r.a1, g[6] = v.tab1, g[7] = v.tab2;
            next = g[31];
            break;
        }
        if (P->cycles >= P->next_event) { /* the devices' time is update's, not the VLC's */
            u32 u = sceKernelGetSystemTimeLow();
            update();
            t += sceKernelGetSystemTimeLow() - u;
        }
        if (psx_irq_pending(P)) {
            next = pc;
            break;
        }
    }
    t = sceKernelGetSystemTimeLow() - t;
    native_stats.us_io += t;
    native_stats.us_io_kind[IO_VLC] += t;
    return next;
}

/* The loop of libetc's `v_wait(count, timeout)` (hooks.rs VSYNC_WAIT), from its head `pc`: a pass
 * counts the timeout down at 16(sp) and stops on -1 (v1) or when Vcount (arg 0) reaches a0, the
 * PS1 cycles of the translated pass (arg 1) charged. Vcount moves only in the vblank interrupt, so
 * between device events the passes that fit are counted down at once, never the one that times
 * out: the same writes, v0 and PS1 time as the loop's, without the host spinning through them.
 * Returns the exit (head + 0x60), the timeout path (head + 0x20), or `pc` to take an interrupt. */
static u32 hook_vsync_wait(Cpu *c, const u32 *arg, u32 pc)
{
    u32 *g = c->gpr, at = g[29] + 16, pass = arg[1];
    for (;;) {
        u32 n = psx_read(P, at, 4) - 1;
        psx_write(P, at, n, 4);
        g[2] = n;
        if (n == g[3])
            return pc + 0x20;
        g[2] = (s32)psx_read(P, arg[0], 4) < (s32)g[4];
        P->cycles += pass;
        if (!g[2])
            return pc + 0x60;
        u64 room = P->next_event > P->cycles ? (P->next_event - P->cycles) / pass : 0;
        u32 left = n - g[3] - 1; /* passes on before the one that times out */
        if (room > left)
            room = left;
        if (room) {
            psx_write(P, at, n - (u32)room, 4);
            P->cycles += room * pass;
            native_stats.vsync_passes += (u32)room;
        }
        if (P->cycles >= P->next_event)
            update();
        if (psx_irq_pending(P))
            return pc;
    }
}

/* A hook's host call: the recipe runs, then the guest goes on where it says (a guest address or a
 * host one), through the table. An unknown id runs the guest code instead. */
static u32 hook(const u32 *site, u32 pc)
{
    Cpu *c = &P->cpu;
    from_ctx(c);
    charge();
    sync_time();
    if (P->cycles >= P->next_event)
        update();
    native_stats.calls++;
    native_stats.hooks++;
    u32 next = pc;
    if (!psx_irq_pending(P)) {
        static u32 n;
        int timed = !(n++ % PROF_EVERY);
        u32 t = timed ? sceKernelGetSystemTimeLow() : 0;
        switch (site[1]) {
        case HOOK_PAD_CHK_RC2_WAIT: next = hook_pad_chk(c, site + 2); break;
        case HOOK_PAD_SIO_ACK_WAIT: next = hook_pad_ack(c, site + 2, pc); break;
        case HOOK_VSYNC_WAIT: next = hook_vsync_wait(c, site + 2, pc); break;
        case HOOK_DEC_DCT_VLC:
            if (IN_RAM(c->gpr[5]) && (!c->gpr[4] || IN_RAM(c->gpr[4]))) {
                next = hook_dct_vlc(c, site + 2, pc);
                break;
            }
            /* fall through */
        default:
            jump(c, pc);
            return leave(c, run_guest(2));
        }
        if (timed && site[1] <= HOOK_PAD_SIO_ACK_WAIT) { /* the libpad recipes wait on the timer:
                                                           * their time is the timer's (the VLC times
                                                           * itself, v_wait's is update's) */
            t = (sceKernelGetSystemTimeLow() - t) * PROF_EVERY - prof_bias;
            native_stats.us_io += t;
            native_stats.us_io_kind[IO_TIMER] += t;
        }
    }
    if (psx_irq_pending(P)) {
        cpu_exception(P, 0, next, 0);
        return leave(c, run_guest(0));
    }
    jump(c, next);
    return leave(c, run_guest(0));
}

/* GTE commands with their time (psxrt.trace bit 7): one in PROF_EVERY timed and scaled. */
void native_gte_prof(u32 cmd)
{
    static u32 n;
    if (n++ % PROF_EVERY) {
        native_gte_fast(cmd);
        return;
    }
    u32 t = sceKernelGetSystemTimeLow();
    native_gte_fast(cmd);
    native_stats.us_io_kind[IO_GTEC] += (sceKernelGetSystemTimeLow() - t) * PROF_EVERY - prof_bias;
}

/* What an empty timed interval costs, times PROF_EVERY: every sampled time (here, the core's
 * draw) is less this, so the clock's own syscalls are not counted. On the PSP one costs about as
 * much as a GPU state command. Sums of times wrap, but per-second differences come out right. */
static void prof_init(void)
{
    u32 sum = 0;
    for (int i = 0; i < 4096; i++) {
        u32 t = sceKernelGetSystemTimeLow();
        sum += sceKernelGetSystemTimeLow() - t;
    }
    prof_bias = sum * PROF_EVERY / 4096;
    psx_log(P, "profile: an empty timed interval costs %u ns", (unsigned)(sum * 1000 / 4096));
}

/* Where the guest's PS1 cycles go: a poll runs where the budget ran out, so the cycles since the
 * last poll are put on its loop (the guest pc of the back-edge). Ranked by native_hot_log. */
#define HOT_SLOTS 2048
static struct { u32 pc; u64 cycles; } hots[HOT_SLOTS];
static u64 hot_last, hot_lost;

static void hot(u32 pc)
{
    u64 w = hot_last ? P->cycles - hot_last : 0;
    hot_last = P->cycles;
    for (u32 i = (pc >> 2) * 2654435761u >> 21, k = 0; k < 16; k++, i = (i + 1) % HOT_SLOTS)
        if (!hots[i].pc || hots[i].pc == pc) {
            hots[i].pc = pc, hots[i].cycles += w;
            return;
        }
    hot_lost += w;
}

int native_hot_log(char *buf, int len)
{
    u64 all = hot_lost;
    for (int i = 0; i < HOT_SLOTS; i++)
        all += hots[i].cycles;
    int n = snprintf(buf, (size_t)len, "-- hot loops (pc, 1/10000 of %u Mcycles, lost %u):", (unsigned)(all >> 20),
                     (unsigned)(hot_lost >> 20));
    for (int top = 0; top < 48 && n < len - 24 && all; top++) {
        int best = -1;
        for (int i = 0; i < HOT_SLOTS; i++)
            if (hots[i].cycles && (best < 0 || hots[i].cycles > hots[best].cycles))
                best = i;
        if (best < 0)
            break;
        n += snprintf(buf + n, (size_t)(len - n), " %08X %u", (unsigned)hots[best].pc, (unsigned)(hots[best].cycles * 10000 / all));
        hots[best].cycles = 0;
    }
    memset(hots, 0, sizeof hots);
    hot_lost = 0;
    return n < len ? n + snprintf(buf + n, (size_t)(len - n), "\n") : len;
}

static u32 rt_call(const u32 *site);

/* Host call at `site` (the address of its .word pc, .word insn): returns where to go on. One in
 * PB_EVERY is bracketed (dropped when it ran an update or translated code), and one in PB_EVERY,
 * halfway between, brackets nothing before it runs. */
u32 native_rt_call(const u32 *site)
{
    static u32 k;
    u32 s = k++ % PB_EVERY;
    prof_site(site[0]);
    if (s == PB_EVERY / 2 && prof_begin(PB_EMPTY, PB_EVERY))
        prof_end(PB_EMPTY, 0);
    if (s || !prof_begin(PB_CALL, PB_EVERY))
        return rt_call(site);
    u32 calls = native_stats.calls, updates = native_stats.updates;
    u32 next = rt_call(site);
    prof_end(PB_CALL, native_stats.calls - calls > 1 || native_stats.updates != updates);
    return next;
}

static u32 rt_call(const u32 *site)
{
    Cpu *c = &P->cpu;
    u32 pc = site[0], insn = site[1], back = (u32)site + 8;
    u32 op = insn >> 26;
    if (pc & HOOK)
        return hook(site, pc & ~HOOK);
    /* not timed: two clock reads would cost more than the instruction */
    if (!(pc & RESUME) && (op == 0x12 || op == 0x32 || op == 0x3A) && gte_fast(insn)) {
        native_stats.calls++;
        native_stats.n_io[IO_GTE]++;
        return back;
    }
    u32 next;
    if (!(pc & RESUME) && op >= 0x20 && io_fast(pc, insn, back, &next))
        return next;
    from_ctx(c);
    charge();
    native_stats.calls++;
    if (pc & RESUME) {
        jump(c, pc & ~RESUME);
        return leave(c, run_guest(2));
    }
    if (op != 0x12 && op != 0x32 && op != 0x3A) { /* the GTE does not need the clock */
        sync_time();
        if (P->cycles >= P->next_event)
            update();
    }
    if (insn == POLL) {
        hot(pc);
        if (!psx_irq_pending(P))
            return leave(c, back);
        cpu_exception(P, 0, back, 0);
        return leave(c, run_guest(0));
    }
    if (psx_irq_pending(P)) { /* taken before the instruction, which runs on return */
        cpu_exception(P, 0, (u32)site - 12, 0);
        return leave(c, run_guest(0));
    }
    jump(c, pc);
    if (ram_r32(P, pc) != insn) {
        static int told;
        if (!told++)
            say("native: host call at %08X: RAM %08X, translated %08X", (unsigned)pc, (unsigned)ram_r32(P, pc), (unsigned)insn);
    }
    static u32 n;
    int kind = io_kind(c, insn), timed = !(n++ % PROF_EVERY);
    if (kind == IO_TIMER && insn >> 29 == 4) /* loads */
        poll_skip(pc, (c->gpr[(insn >> 21) & 31] + (u32)(s32)(s16)insn) & 0x1FFFFFFFu, c->gpr);
    u32 t = timed ? sceKernelGetSystemTimeLow() : 0;
    if (cpu_step(P) < 0)
        P->exiting = 2;
    if (timed) {
        t = (sceKernelGetSystemTimeLow() - t) * PROF_EVERY - prof_bias;
        native_stats.us_io += t;
        native_stats.us_io_kind[kind] += t;
    }
    native_stats.n_io[kind]++;
    native_stats.io_host += kind == IO_TIMER || kind == IO_IRQ || kind == IO_PAD;
    link_host(c, pc);
    finish_load(c);
    if (c->pc == pc + 4 && !c->in_delay)
        jump(c, back);
    if (psx_irq_pending(P))
        cpu_exception(P, 0, c->pc, 0);
    return leave(c, run_guest(0));
}

u32 native_miss(void)
{
    Cpu *c = &P->cpu;
    from_ctx(c);
    charge();
    native_stats.misses++;
    jump(c, ctx[C_TARGET]);
    return leave(c, run_guest(1));
}

u32 native_gret(void)
{
    Cpu *c = &P->cpu;
    from_ctx(c);
    charge();
    jump(c, gret_pop(c->gpr[29]));
    return leave(c, run_guest(1));
}

/* PSX.call: the guest function at `addr`, natively where translated. */
static void native_call(Psx *psx, u32 addr)
{
    Cpu *c = &psx->cpu;
    u32 pc = c->pc, next = c->next_pc, ra = c->gpr[31], link = ra_link;
    int delay = c->in_delay;
    c->gpr[31] = STUB_CALL_RET;
    ra_link = 1;
    c->load_reg = 0;
    jump(c, addr);
    Excl x;
    excl_begin(&x);
    u32 h = run_guest(1);
    if (h != STUB_CALL_RET) {
        guest_calls++;
        native_enter(to_ctx(c, h));
        guest_calls--;
        from_ctx(c);
        charge();
    }
    excl_end(&x);
    c->gpr[31] = ra, ra_link = link;
    c->pc = pc, c->next_pc = next, c->in_delay = delay;
}

static void stub(u32 at, void (*fn)(void))
{
    u32 *w = (u32 *)at;
    w[0] = 0x08000000u | (((u32)fn >> 2) & 0x03FFFFFFu); /* j fn */
    w[1] = 0;                                           /* nop */
}

/* The GTE commands translated code enters directly (layout.rs GTE_OWN), with their routines in
 * trampolines.S and a door to STUB_GTE_CMD each; `cmd` is a word the translator sends there. */
static const struct {
    u32 at, cmd;
    void (*fn)(void), (*door)(void);
} gte_ops[] = {
    {STUB_GTE_NCLIP, 0x01400006u, psxrt_gte_nclip, psxrt_gte_nclip_door},
    {STUB_GTE_AVSZ3, 0x0158002Du, psxrt_gte_avsz3, psxrt_gte_avsz3_door},
    {STUB_GTE_AVSZ4, 0x0168002Eu, psxrt_gte_avsz4, psxrt_gte_avsz4_door},
    {STUB_GTE_RTPS, 0x00180001u, psxrt_gte_rtps, psxrt_gte_rtps_door},
    {STUB_GTE_RTPT, 0x00280030u, psxrt_gte_rtpt, psxrt_gte_rtpt_door},
    /* MVMVA and GPF as Sheep Dog's executables word them, and the other matrix, BK and lm */
    {STUB_GTE_MVMVA, 0x0041E012u, psxrt_gte_mvmva, psxrt_gte_cmd},
    {STUB_GTE_MVMVA, 0x0049E012u, psxrt_gte_mvmva, psxrt_gte_cmd},
    {STUB_GTE_MVMVA, 0x00486012u, psxrt_gte_mvmva, psxrt_gte_cmd},
    {STUB_GTE_MVMVA, 0x00480012u, psxrt_gte_mvmva, psxrt_gte_cmd},
    {STUB_GTE_MVMVA, 0x00426012u, psxrt_gte_mvmva, psxrt_gte_cmd},
    {STUB_GTE_MVMVA, 0x00488012u, psxrt_gte_mvmva, psxrt_gte_cmd},
    {STUB_GTE_MVMVA, 0x00490012u, psxrt_gte_mvmva, psxrt_gte_cmd},
    {STUB_GTE_MVMVA, 0x00406012u, psxrt_gte_mvmva, psxrt_gte_cmd},
    {STUB_GTE_MVMVA, 0x000CA412u, psxrt_gte_mvmva, psxrt_gte_cmd},
    {STUB_GTE_GPF, 0x0190003Du, psxrt_gte_gpf, psxrt_gte_cmd},
    {STUB_GTE_GPF, 0x0198003Du, psxrt_gte_gpf, psxrt_gte_cmd},
    {STUB_GTE_GPF, 0x0008043Du, psxrt_gte_gpf, psxrt_gte_cmd},
};
#define GTE_OPS (int)(sizeof gte_ops / sizeof gte_ops[0])

/* The divide's reciprocal table for psxrt_gte_rtps/rtpt, as core/gte.c builds its own. */
u8 psxrt_unr[0x104];

static void gte_call_asm(void (*fn)(void), u32 cmd)
{
    __asm__ volatile("lui $26, 0x09A0\n\tori $26, $26, 0x004C\n\tmove $27, %1\n\tjalr %0\n\tnop" : : "r"(fn), "r"(cmd) : "$26", "$27", "$31", "memory");
}

static u32 rnd_state = 0x2545F491u;
static u32 rnd(void)
{
    rnd_state ^= rnd_state << 13, rnd_state ^= rnd_state >> 17, rnd_state ^= rnd_state << 5;
    return rnd_state;
}

/* Random values of random magnitude: the products then fall both in range and out of it. */
static s32 rnd_mag(int bits) { return (s32)rnd() >> (32 - bits + (rnd() % bits)); }
static u32 rnd16(void) { return (u32)rnd_mag(16) & 0xFFFF; }
static s32 rnd_pair(void) { return (s32)(rnd16() | rnd16() << 16); }

/* Each routine against gte_command on random registers (TR within 31 bits, as the routines take
 * them): the number of differing results. The GTE and the CTX words the routines park guest
 * state in are kept. */
static int gte_asm_check(Psx *psx, int n)
{
    Gte *g = psx->gte, keep = *g, in, want;
    u32 park[10];
    for (int r = 0; r < 8; r++)
        park[r] = ctx[8 + r];
    park[8] = ctx[C_HI], park[9] = ctx[C_LO];
    int bad = 0, per[GTE_OPS] = {0};
    for (int i = 0; i < n; i++) {
        for (int r = 0; r < 32; r++)
            in.d[r] = (s32)rnd(), in.c[r] = rnd_pair();
        for (int r = 0; r < 6; r++)
            in.d[r] = rnd_pair();
        for (int r = 12; r < 15; r++)
            in.d[r] = rnd_pair();
        for (int r = 16; r < 20; r++)
            in.d[r] = (s32)rnd16();
        for (int r = 8; r < 12; r++) /* IR0-IR3: 16-bit values, as every write leaves them */
            in.d[r] = rnd_mag(16);
        for (int r = 5; r < 8; r++)
            in.c[r] = rnd_mag(31), in.c[r + 8] = rnd_mag(31); /* TR, BK */
        in.c[24] = rnd_mag(32), in.c[25] = rnd_mag(32), in.c[28] = rnd_mag(32);
        int k = i % GTE_OPS;
        *g = in;
        gte_command(psx, gte_ops[k].cmd);
        want = *g;
        *g = in;
        gte_call_asm(gte_ops[k].fn, gte_ops[k].cmd);
        if (memcmp(g, &want, sizeof want) && !per[k]++)
            for (int r = 0; r < 64; r++)
                if ((r < 32 ? g->d[r] : g->c[r - 32]) != (r < 32 ? want.d[r] : want.c[r - 32]))
                    say("native: GTE %08X in asm: %s%d %08X, want %08X", (unsigned)gte_ops[k].cmd, r < 32 ? "d" : "c",
                        r & 31, (unsigned)(r < 32 ? g->d[r] : g->c[r - 32]), (unsigned)(r < 32 ? want.d[r] : want.c[r - 32]));
    }
    for (int k = 0; k < GTE_OPS; k++)
        bad += per[k];
    *g = keep;
    for (int r = 0; r < 8; r++)
        ctx[8 + r] = park[r];
    ctx[C_HI] = park[8], ctx[C_LO] = park[9];
    return bad;
}

/* The direct GTE entries: their routines when `asm_ok`, else their doors. */
static void gte_stubs(int asm_ok)
{
    for (int i = 0; i < GTE_OPS; i++)
        stub(gte_ops[i].at, asm_ok ? gte_ops[i].fn : gte_ops[i].door);
}

/* io_fast_dev against psx_read on random counters, clock, interrupt, pad, DMA and GPU registers:
 * the number of reads it serves (`*served`) with another value or leaving another state. The
 * device state it randomises is kept. */
static int io_fast_check(Psx *psx, int n, int *served)
{
    static const u32 regs[] = {0x1F801070, 0x1F801074, 0x1F801100, 0x1F801104, 0x1F801108, 0x1F80110C,
                               0x1F801110, 0x1F801114, 0x1F801118, 0x1F801120, 0x1F801124, 0x1F801128,
                               0x1F801040, 0x1F801044, 0x1F801048, 0x1F80104A, 0x1F80104E, 0x1F801054, 0x1F801042,
                               0x1F8010A0, 0x1F8010A4, 0x1F8010A8, 0x1F8010AA, 0x1F8010E8, 0x1F8010EB, 0x1F8010AC,
                               0x1F8010F0, 0x1F8010F4, 0x1F8010F6, 0x1F8010F8, 0x1F801810, 0x1F801814, 0x1F801816};
    Timers keep = psx->tmr;
    Sio sio = psx->sio;
    Dma dma = psx->dma;
    int xfer = psx->gpu.xfer;
    u32 gpuread = psx->gpu.gpuread;
    u64 cycles = psx->cycles;
    u32 stat = psx->i_stat, mask = psx->i_mask, gpustat = psx->gpu.stat;
    int bad = 0;
    *served = 0;
    for (int k = 0; k < n; k++) {
        Timers in;
        u64 now = (u64)rnd() << 6 | (rnd() & 63);
        for (int i = 0; i < 3; i++) {
            Timer *t = &in.t[i];
            static const u32 spans[4] = {0xFF, 0xFFFFF, 0x7FFFFFFF, 0xFFFFFFFF};
            t->mode = rnd() & 0x1FFF;
            if (rnd() & 1)
                t->mode &= ~0x330u; /* system clock, no interrupt: the reads it serves */
            u32 r = rnd() & 3;
            t->target = r == 0 ? 0 : r == 1 ? rnd() & 0xFF : rnd16();
            t->count = rnd() & 1 ? rnd() % (t->target + 1) : rnd16();
            t->base = now - (rnd() & spans[rnd() & 3]);
        }
        u32 is = rnd() & 0x7FF, im = rnd() & 0x7FF, a = regs[rnd() % (sizeof regs / 4)], v, want;
        psx->tmr = in, psx->cycles = now, psx->i_stat = is, psx->i_mask = im;
        psx->gpu.stat = rnd(); /* PAL or NTSC hblanks, any DMA direction */
        psx->gpu.xfer = (int)(rnd() % 3), psx->gpu.gpuread = rnd();
        for (int i = 0; i < 7; i++)
            psx->dma.ch[i] = (DmaChan){rnd(), rnd(), rnd()};
        psx->dma.dpcr = rnd(), psx->dma.dicr = rnd();
        Sio *o = &psx->sio;
        o->rx = (u8)rnd(), o->rx_full = (int)(rnd() & 1), o->stat = (u16)rnd16(), o->mode = (u16)rnd16(), o->ctrl = (u16)rnd16(), o->baud = (u16)rnd16();
        Sio sin = *o;
        if (io_fast_dev(psx, a, &v))
            continue;
        ++*served;
        Timers got = psx->tmr;
        u32 got_stat = psx->i_stat;
        int got_full = o->rx_full;
        psx->tmr = in, psx->i_stat = is, *o = sin;
        want = psx_read(psx, a, 4);
        int differ = v != want || got_stat != psx->i_stat || got_full != o->rx_full;
        for (int i = 0; i < 3; i++) {
            const Timer *x = &got.t[i], *y = &psx->tmr.t[i];
            differ |= x->count != y->count || x->mode != y->mode || x->target != y->target || x->base != y->base;
        }
        if (differ && !bad++)
            say("native: device read %08X in io_fast: %08X, want %08X", (unsigned)a, (unsigned)v, (unsigned)want);
    }
    psx->tmr = keep, psx->cycles = cycles, psx->i_stat = stat, psx->i_mask = mask, psx->gpu.stat = gpustat;
    psx->sio = sio, psx->dma = dma, psx->gpu.xfer = xfer, psx->gpu.gpuread = gpuread;
    return bad;
}

const char *native_init(Psx *psx, const DiscInfo *info)
{
    if (info->images > MAX_IMAGES)
        return "too many translated executables";
    code_lo = info->code_base;
    code_hi = info->code_base + info->code_size;
    ar.lo = ar.hi = code_lo, ar.n = 0;
    if (info->fl_len) {
        if (info->fl_arena > info->code_size || (info->fl_arena & 63))
            return "DATA.PSAR floating code is out of range";
        ar.hi = code_lo + info->fl_arena;
    }
    for (u32 i = 0; i < info->images; i++) {
        const CodeEntry *e = &info->dir[i];
        u32 text = e->text_addr & 0x1FFFFFFFu;
        if (text >= RAM_SIZE * 4 || (text & 3) || (text & (RAM_SIZE - 1)) + e->text_size > RAM_SIZE || e->host_base < ar.hi ||
            e->host_base + e->code_len > code_hi || (e->host_base & 3))
            return "DATA.PSAR code directory is out of range";
        img[i] = (Image){e, text & (RAM_SIZE - 4), 0, 0, 0, 0, ~0u};
    }
    nimg = (int)info->images;
    if (info->fl_len) {
        if (disc_blob(info->fl_offset, info->fl_stored, (void *)info->fl_host, info->fl_len) < 0 ||
            fl_open(&fl, (const u32 *)info->fl_host, info->fl_len / 4) < 0)
            return "DATA.PSAR floating code is damaged";
        fl_bodies = info->fl_bodies;
        say("native: floating code: %u units, %u entries, %u KB arena at %08X", (unsigned)fl.units, (unsigned)fl.index,
            (unsigned)(info->fl_arena >> 10), (unsigned)ar.lo);
    }
    cpu_shift = info->cpu_shift > 8 ? 8 : info->cpu_shift;
    P = psx;
    extern Psx *gte_fast_psx;
    gte_fast_psx = psx;
    for (u32 i = 0; i < TABLE_WORDS; i++)
        table[i] = STUB_MISS;
    blk_resum(0, RAM_SIZE);
    stub(STUB_RT_CALL, psxrt_rt_call);
    stub(STUB_MISS, psxrt_miss);
    stub(STUB_DIRTY, psxrt_miss);
    stub(STUB_CALL_RET, psxrt_call_ret);
    stub(STUB_GRET, psxrt_gret);
    stub(STUB_GTE_CMD, psxrt_gte_cmd);
    prof_init();
    if (psx->trace & 128) {
        stub(STUB_GTE_CMD, psxrt_gte_prof);
    }
    memset((void *)ctx, 0, 0x100);
    ctx[C_BUDGET] = BUDGET;
    memcpy((void *)GTE_ADDR, psx->gte, sizeof(Gte));
    psx->gte = (Gte *)GTE_ADDR;
    for (int i = 0; i < 0x101; i++) {
        int v = (0x40000 / (i + 0x100) + 1) / 2 - 0x101;
        psxrt_unr[i] = (u8)(v < 0 ? 0 : v);
    }
    int bad = gte_asm_check(psx, GTE_OPS * 4096);
    gte_stubs(!bad);
    say("native: GTE NCLIP/AVSZ/RTPS/RTPT/MVMVA/GPF in asm, %d of %d checks differ%s", bad, GTE_OPS * 4096,
        bad ? ": through the runtime" : "");
    io_fast_psx = psx, io_fast_shift = cpu_shift;
    int served;
    bad = io_fast_check(psx, 65536, &served);
    stub(STUB_IO_READ, bad ? psxrt_rt_call : psxrt_io_read);
    stub(STUB_IO_WRITE, psxrt_io_write);
    say("native: timer/IRQ/pad/DMA/GPU reads in io_fast, %d of %d served checks differ%s", bad, served, bad ? ": through the runtime" : "");
    flush_code();
    __asm__ volatile("move %0, $gp" : "=r"(native_host_gp));
    psx->call = native_call;
    psx->ram_loaded = on_ram_loaded;
    return NULL;
}

void native_run(u32 host_pc)
{
    Cpu *c = &P->cpu;
    sceDisplayWaitVblankStart();
    t_base = t_lock = sceKernelGetSystemTimeWide();
    c_base = P->cycles;
    v_lock = vc = sceDisplayGetVcount();
    if (sceKernelSetAlarm(ALARM_US, alarm, NULL) < 0)
        say("native: no alarm, interrupts wait for host calls");
    if (!host_pc) {
        c->load_reg = 0;
        jump(c, c->pc);
        host_pc = to_ctx(c, run_guest(1));
    }
    native_enter(host_pc);
    runtime_halt("translated code returned to the top level");
}

/* ---- snapshots: the context (with the GTE), the images, the GRET stack, the table, the
 * placements (their code is placed again) ---- */
#define CTX_BYTES (GTE_ADDR + sizeof(Gte) - CTX_ADDR)

int native_save(int fd)
{
    u32 st[MAX_IMAGES][4], n = (u32)nimg;
    for (u32 i = 0; i < n; i++)
        st[i][0] = (u32)img[i].resident, st[i][1] = (u32)img[i].installed, st[i][2] = img[i].gen, st[i][3] = img[i].checked;
    u32 top = gret_top;
    int w = sceIoWrite(fd, (const void *)CTX_ADDR, CTX_BYTES) != (int)CTX_BYTES || sceIoWrite(fd, &n, 4) != 4 ||
            sceIoWrite(fd, st, n * 16) != (int)(n * 16) || sceIoWrite(fd, &top, 4) != 4 || sceIoWrite(fd, gret, sizeof gret) != (int)sizeof gret ||
            sceIoWrite(fd, table, TABLE_WORDS * 4) != (int)(TABLE_WORDS * 4) || sceIoWrite(fd, &ar.n, 4) != 4 ||
            sceIoWrite(fd, ar.p, ar.n * sizeof *ar.p) != (int)(ar.n * sizeof *ar.p);
    return w ? -1 : 0;
}

int native_load(int fd)
{
    u32 st[MAX_IMAGES][4], n;
    if (sceIoRead(fd, (void *)CTX_ADDR, CTX_BYTES) != (int)CTX_BYTES || sceIoRead(fd, &n, 4) != 4 || n != (u32)nimg ||
        sceIoRead(fd, st, n * 16) != (int)(n * 16) || sceIoRead(fd, &gret_top, 4) != 4 || sceIoRead(fd, gret, sizeof gret) != (int)sizeof gret ||
        sceIoRead(fd, table, TABLE_WORDS * 4) != (int)(TABLE_WORDS * 4) || sceIoRead(fd, &ar.n, 4) != 4 || ar.n > (fl.w ? FL_PLACES : 0) ||
        sceIoRead(fd, ar.p, ar.n * sizeof *ar.p) != (int)(ar.n * sizeof *ar.p))
        return ar.n = 0, -1;
    for (u32 i = 0; i < ar.n; i++) {
        const FlPlace *q = &ar.p[i];
        if (q->unit >= fl.units || q->len < 4 * fl_body_words(fl_unit(&fl, q->unit)) || q->host < (i ? ar.p[i - 1].host + ar.p[i - 1].len : ar.lo) ||
            q->host > ar.hi || ar.hi - q->host < q->len || q->at > RAM_SIZE - 4 * pl_words(q) || (q->at & 3) || fl_load(q) < 0)
            return ar.n = 0, -1;
    }
    memset(gret_sp, 0xFF, sizeof gret_sp); /* not in the snapshot: no entry matches, pops take the top */
    sceKernelDcacheWritebackInvalidateAll();
    blk_resum(0, RAM_SIZE); /* RAM and the table are the snapshot's already */
    for (u32 i = 0; i < n; i++) {
        Image *m = &img[i];
        m->resident = (int)st[i][0], m->installed = (int)st[i][1], m->gen = st[i][2], m->checked = st[i][3];
        m->patched = 0;
        if (m->resident && disc_blob(m->e->code_off, m->e->code_stored, (void *)m->e->host_base, m->e->code_len) < 0) {
            say("native: cannot read the code of %.16s", m->e->name);
            return -1;
        }
        if (m->resident)
            say("native: %.16s at %08X (snapshot)", m->e->name, (unsigned)m->e->text_addr);
    }
    /* The code read is the executable's: RAM's code goes back in where it differs (recheck). */
    u32 rs[4] = {0};
    recheck(1, rs);
    if (rs[2] || rs[3])
        say("native: snapshot: %u words patched, %u words to the interpreter", (unsigned)rs[2], (unsigned)rs[3]);
    flush_code();
    return 0;
}
