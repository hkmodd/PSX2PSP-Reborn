/* The simulator's side of the psxrt core: owns the machine and its memories, runs the
 * interpreter by frames, and exposes VRAM and the display mode to the Rust driver. */
#include "psx.h"

#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define MDEC_CAP 0x10000 /* words: a whole compressed frame */

typedef int (*ReadFn)(void *user, u32 lba, u8 *raw);
typedef void (*TextFn)(void *user, const char *s, int n);

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
    psx_init(&s->psx, &s->host, s->ram, s->scratch, s->vram, s->spu_ram, s->mdec_in, MDEC_CAP);
    return s;
}

void sim_free(Sim *s) { free(s); }

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

void sim_pad(Sim *s, u16 buttons) { s->psx.sio.buttons[0] = buttons; }

void sim_trace(Sim *s, int flags) { s->psx.trace = flags; }

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
