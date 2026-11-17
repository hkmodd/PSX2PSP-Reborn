/* The simulator's side of the psxrt core: owns the machine and its memories, runs the
 * interpreter by frames, and exposes VRAM and the display mode to the Rust driver. */
#include "psx.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define MDEC_CAP 0x10000 /* words: a whole compressed frame */

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
    return s;
}

void sim_free(Sim *s) { free(s); }

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
 * capture takes code that ran. */
static u8 ran[RAM_SIZE / 4];

const u8 *sim_ran(void) { return ran; }
void sim_ran_clear(void) { memset(ran, 0, sizeof ran); }

void psxrt_pc_trace(u32 pc)
{
    if ((pc & 0x1FFFFFFFu) < RAM_SIZE)
        ran[(pc & (RAM_SIZE - 1)) >> 2] = 1;
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
    u32 f = s->frames;
    while (s->frames == f && !s->psx.exiting)
        cpu_run(&s->psx, s->psx.cycles + 4096);
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
}
