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
 * comes back to the interpreter at the address saved on a small stack.
 *
 * Time: the PS1 cycle count advances with interpreted instructions and loops: each back-edge
 * charges its estimated PS1 cycles to CTX.BUDGET and enters the runtime when that goes negative,
 * which adds what was spent to the count, so a counted wait loop lasts as long as on the PS1.
 * When the count runs ahead of the real clock (checked at every host call and every 256
 * interpreted instructions) the runtime sleeps; when it falls behind by more than BEHIND_MAX,
 * the clock is moved back instead: PS1 time slows down with the host, it never jumps, or devices
 * (the CD's sectors) would run ahead of the guest code that serves them. A 500 us alarm makes the budget negative, so
 * that loops enter the runtime and interrupts are taken there.
 *
 * Limits: code written by CPU stores (decompressed or copied by the game) is not tracked, EPC
 * holds host addresses while translated code is interrupted, and GRET returns are LIFO.
 */
#include "native.h"

#include <pspkernel.h>
#include <stdarg.h>
#include <pspthreadman.h>
#include <psputils.h>
#include <stdio.h>
#include <string.h>

#define RAM_HOST 0x09800000u
#define CTX_ADDR 0x09A00400u
#define GTE_ADDR 0x09A00500u /* psx->gte: translated code moves to and from it directly */
#define STUB_RT_CALL 0x09A00F00u
#define STUB_MISS 0x09A00F40u
#define STUB_DIRTY 0x09A00F60u
#define STUB_CALL_RET 0x09A00F80u
#define STUB_GRET 0x09A00FC0u
#define STUB_GTE_CMD 0x09A00FA0u
#define TABLE_ADDR 0x09A01000u
#define TABLE_WORDS (RAM_SIZE / 4)
/* CTX words */
#define C_HI (0x80 / 4)
#define C_LO (0x84 / 4)
#define C_TARGET (0x88 / 4)
#define C_BUDGET (0x90 / 4)
#define BUDGET (PSX_CLOCK / 1000) /* 1 ms of loops between runtime entries */
#define FORCED 0x40000000u        /* the alarm subtracts this from the budget */
#define RESUME 1u
#define POLL 0u /* host-call word of an interrupt poll */
#define ALARM_US 500
#define AHEAD_MAX (PSX_CLOCK / 500) /* 2 ms */
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
void native_enter(u32 host_pc);
u32 native_host_sp, native_host_gp;

NativeStats native_stats;

static volatile u32 *const ctx = (volatile u32 *)CTX_ADDR;
static u32 *const table = (u32 *)TABLE_ADDR;

typedef struct Image {
    const CodeEntry *e;
    u32 text; /* text address & 0x1FFFFC */
    int resident;  /* its code is at host_base */
    int installed; /* its table slice is in TABLE (resident, and no other slice loaded over it) */
    u32 gen, checked; /* disc loads over its text; gen at the last check */
} Image;

#define MAX_IMAGES 256
static Image img[MAX_IMAGES];
static int nimg;
static u32 code_lo, code_hi;
static Psx *P;
static u64 t_base, c_base;

#define GRET_DEPTH 64
static u32 gret[GRET_DEPTH];
static u32 gret_top;

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

static void flush_code(void)
{
    sceKernelDcacheWritebackInvalidateAll();
    sceKernelIcacheInvalidateAll();
}

/* ---- guest state: CTX (translated code) <-> psx->cpu (runtime) ---- */
static void from_ctx(Cpu *c)
{
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
    m->resident = m->installed = 0;
}

static int load(Image *m)
{
    const CodeEntry *e = m->e;
    if (!m->resident) {
        for (int i = 0; i < nimg; i++)
            if (&img[i] != m && img[i].resident && img[i].e->host_base == e->host_base)
                evict(&img[i]);
        if (disc_blob(e->code_off, e->code_stored, (void *)e->host_base, e->code_len) < 0) {
            say("native: cannot read the code of %.16s", e->name);
            return -1;
        }
        m->resident = 1;
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
    if (!candidates && *t == STUB_DIRTY)
        *t = STUB_MISS;
    return 0;
}

/* PSX.ram_loaded: the translations of what was overwritten are checked again before use. */
static void on_ram_loaded(Psx *psx, u32 addr, u32 len)
{
    (void)psx;
    u32 a = addr & (RAM_SIZE - 4), end = a + ((len + 3) & ~3u);
    if (end > RAM_SIZE)
        end = RAM_SIZE;
    for (u32 x = a; x < end; x += 4)
        table[x >> 2] = STUB_DIRTY;
    for (int i = 0; i < nimg; i++)
        if (a < img[i].text + img[i].e->text_size && img[i].text < end)
            img[i].gen++;
}

/* ---- time ---- */
static void sync_time(void)
{
    static u64 last;
    if (P->cycles - last < SYNC_STEP)
        return;
    last = P->cycles;
    u64 now = sceKernelGetSystemTimeWide(), clock = c_base + (now - t_base) * 338688u / 10000u;
    if (P->cycles + BEHIND_MAX < clock) /* the host fell behind: PS1 time slows down, never skips */
        c_base = P->cycles + BEHIND_MAX, t_base = now;
    else if (P->cycles > clock + AHEAD_MAX) {
        u32 t = sceKernelGetSystemTimeLow();
        sceKernelDelayThread((SceUInt)((P->cycles - clock) * 10000u / 338688u));
        native_stats.us_sleep += sceKernelGetSystemTimeLow() - t;
    }
}

static void update(void)
{
    static u32 n;
    if (n++ % PROF_EVERY) {
        psx_update(P);
        return;
    }
    u32 t = sceKernelGetSystemTimeLow();
    psx_update(P);
    native_stats.us_update += (sceKernelGetSystemTimeLow() - t) * PROF_EVERY;
}

/* At every entry from translated code: the cycles its loops spent join the count, and the
 * budget is refilled. */
static void charge(void)
{
    u32 b = ctx[C_BUDGET];
    if ((s32)b < -(s32)(FORCED / 2))
        b += FORCED;
    P->cycles += (u32)((s32)BUDGET - (s32)b);
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

/* A guest `ra` the translated code may return to: its translation, or GRET. */
static void fix_ra(Cpu *c)
{
    u32 ra = c->gpr[31], p = ra & 0x1FFFFFFFu;
    if (!ra || host_of(ra) == ra)
        return;
    u32 h = host_of(ra);
    if (h) {
        c->gpr[31] = h;
    } else if (p < 0x800000 || (p & 0x1FF00000u) == 0x1FC00000u) {
        gret[gret_top++ % GRET_DEPTH] = ra;
        c->gpr[31] = STUB_GRET;
    }
}

/* Interprets from cpu.pc until an instruction boundary with a translation, and returns its host
 * address. `mode`: 0 checks at once, 1 also loads at a MISS first address, 2 runs one
 * instruction first (a resume point maps to its own host call). */
static u32 interp(int mode);

static u32 run_guest(int mode)
{
    static u32 n;
    if (n++ % PROF_EVERY)
        return interp(mode);
    u32 t = sceKernelGetSystemTimeLow();
    u32 h = interp(mode);
    native_stats.us_interp += (sceKernelGetSystemTimeLow() - t) * PROF_EVERY;
    return h;
}

static u32 interp(int mode)
{
    Cpu *c = &P->cpu;
    for (u32 n = 0;; n++) {
        if (P->exiting)
            runtime_halt(P->exiting == 2 ? "the interpreter hit a fatal error" : "the game exited");
        if (!c->in_delay && mode != 2) {
            u32 pc = c->pc;
            if (pc == STUB_GRET) {
                jump(c, gret[--gret_top % GRET_DEPTH]);
                continue;
            }
            u32 h = host_of(pc);
            if (!h && try_load(pc, mode == 1))
                h = host_of(pc);
            if (h) {
                finish_load(c);
                fix_ra(c);
                return h;
            }
        }
        mode = 0;
        if (!(n & 255))
            sync_time();
        if (P->cycles >= P->next_event)
            update();
        if (cpu_step(P) < 0)
            P->exiting = 2;
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
 * equality test still sees its value. */
#define POLL_PAIR 16u
#define POLL_GAP 2048u
static void poll_skip(u32 pc, u32 addr)
{
    static u32 hot, streak;
    static u64 hot_at, last_at;
    u32 tick = timer_tick(P, addr);
    u64 now = P->cycles, since = now - last_at;
    last_at = now;
    if (!tick)
        return;
    if (pc != hot) {
        if (since >= POLL_PAIR)
            hot = pc, hot_at = now, streak = 0;
        return;
    }
    u64 period = now - hot_at;
    hot_at = now;
    if (period >= POLL_GAP) {
        streak = 0;
        return;
    }
    if (++streak >= 4 && tick > period && P->next_event > now) {
        u64 d = P->next_event - now, s = tick - period;
        P->cycles += d < s ? d : s;
        hot_at = last_at = P->cycles;
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
            poll_skip(pc, p);
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
        t = (sceKernelGetSystemTimeLow() - t) * PROF_EVERY;
        native_stats.us_io += t;
        native_stats.us_io_kind[kind] += t;
    }
    native_stats.calls++;
    native_stats.n_io[kind]++;
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

/* ---- entries from trampolines.S ---- */

/* A GTE command from translated code (GTE_CMD): no guest state copy, no interrupt service. */
void native_gte_cmd(u32 cmd)
{
    gte_command(P, cmd);
    P->cycles += 10;
    native_stats.n_io[IO_GTEC]++;
}

/* Host call at `site` (the address of its .word pc, .word insn): returns where to go on. */
u32 native_rt_call(const u32 *site)
{
    Cpu *c = &P->cpu;
    u32 pc = site[0], insn = site[1], back = (u32)site + 8;
    u32 op = insn >> 26;
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
        return to_ctx(c, run_guest(2));
    }
    if (op != 0x12 && op != 0x32 && op != 0x3A) { /* the GTE does not need the clock */
        sync_time();
        if (P->cycles >= P->next_event)
            update();
    }
    if (insn == POLL) {
        if (!psx_irq_pending(P))
            return to_ctx(c, back);
        cpu_exception(P, 0, back, 0);
        return to_ctx(c, run_guest(0));
    }
    if (psx_irq_pending(P)) { /* taken before the instruction, which runs on return */
        cpu_exception(P, 0, (u32)site - 12, 0);
        return to_ctx(c, run_guest(0));
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
        poll_skip(pc, (c->gpr[(insn >> 21) & 31] + (u32)(s32)(s16)insn) & 0x1FFFFFFFu);
    u32 t = timed ? sceKernelGetSystemTimeLow() : 0;
    if (cpu_step(P) < 0)
        P->exiting = 2;
    if (timed) {
        t = (sceKernelGetSystemTimeLow() - t) * PROF_EVERY;
        native_stats.us_io += t;
        native_stats.us_io_kind[kind] += t;
    }
    native_stats.n_io[kind]++;
    finish_load(c);
    if (c->pc == pc + 4 && !c->in_delay)
        jump(c, back);
    if (psx_irq_pending(P))
        cpu_exception(P, 0, c->pc, 0);
    return to_ctx(c, run_guest(0));
}

u32 native_miss(void)
{
    Cpu *c = &P->cpu;
    from_ctx(c);
    charge();
    native_stats.misses++;
    jump(c, ctx[C_TARGET]);
    return to_ctx(c, run_guest(1));
}

u32 native_gret(void)
{
    Cpu *c = &P->cpu;
    from_ctx(c);
    charge();
    jump(c, gret[--gret_top % GRET_DEPTH]);
    return to_ctx(c, run_guest(1));
}

/* PSX.call: the guest function at `addr`, natively where translated. */
static void native_call(Psx *psx, u32 addr)
{
    Cpu *c = &psx->cpu;
    u32 pc = c->pc, next = c->next_pc, ra = c->gpr[31];
    int delay = c->in_delay;
    c->gpr[31] = STUB_CALL_RET;
    c->load_reg = 0;
    jump(c, addr);
    u32 h = run_guest(1);
    if (h != STUB_CALL_RET) {
        native_enter(to_ctx(c, h));
        from_ctx(c);
        charge();
    }
    c->gpr[31] = ra;
    c->pc = pc, c->next_pc = next, c->in_delay = delay;
}

static void stub(u32 at, void (*fn)(void))
{
    u32 *w = (u32 *)at;
    w[0] = 0x08000000u | (((u32)fn >> 2) & 0x03FFFFFFu); /* j fn */
    w[1] = 0;                                           /* nop */
}

const char *native_init(Psx *psx, const DiscInfo *info)
{
    if (info->images > MAX_IMAGES)
        return "too many translated executables";
    code_lo = info->code_base;
    code_hi = info->code_base + info->code_size;
    for (u32 i = 0; i < info->images; i++) {
        const CodeEntry *e = &info->dir[i];
        u32 text = e->text_addr & 0x1FFFFFFFu;
        if (text >= RAM_SIZE * 4 || (text & 3) || (text & (RAM_SIZE - 1)) + e->text_size > RAM_SIZE || e->host_base < code_lo ||
            e->host_base + e->code_len > code_hi || (e->host_base & 3))
            return "DATA.PSAR code directory is out of range";
        img[i] = (Image){e, text & (RAM_SIZE - 4), 0, 0, 0, ~0u};
    }
    nimg = (int)info->images;
    P = psx;
    for (u32 i = 0; i < TABLE_WORDS; i++)
        table[i] = STUB_MISS;
    stub(STUB_RT_CALL, psxrt_rt_call);
    stub(STUB_MISS, psxrt_miss);
    stub(STUB_DIRTY, psxrt_miss);
    stub(STUB_CALL_RET, psxrt_call_ret);
    stub(STUB_GRET, psxrt_gret);
    stub(STUB_GTE_CMD, psxrt_gte_cmd);
    memset((void *)ctx, 0, 0x100);
    ctx[C_BUDGET] = BUDGET;
    memcpy((void *)GTE_ADDR, psx->gte, sizeof(Gte));
    psx->gte = (Gte *)GTE_ADDR;
    flush_code();
    __asm__ volatile("move %0, $gp" : "=r"(native_host_gp));
    psx->call = native_call;
    psx->ram_loaded = on_ram_loaded;
    return NULL;
}

void native_run(void)
{
    Cpu *c = &P->cpu;
    t_base = sceKernelGetSystemTimeWide();
    c_base = P->cycles;
    if (sceKernelSetAlarm(ALARM_US, alarm, NULL) < 0)
        say("native: no alarm, interrupts wait for host calls");
    c->load_reg = 0;
    jump(c, c->pc);
    u32 h = run_guest(1);
    native_enter(to_ctx(c, h));
    runtime_halt("translated code returned to the top level");
}
