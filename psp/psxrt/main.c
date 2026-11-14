/*
 * psxrt: the native runtime of PSX2PSP (recomp backend), slice S4.
 *
 * Boots the PS1 disc stored in DATA.PSAR of its own EBOOT (argv[0]) on the psxrt core. With
 * translated executables in DATA.PSAR the guest runs as native code (native.c), the interpreter
 * filling in; a frame is then shown at every emulated vblank. Without, the interpreter runs
 * everything and a frame is shown per vblank from the main loop. The GE draws what it can of the
 * PS1 GPU's work (ge.c); the PS1 display area is shown as a scaled texture of the GE's copy of PS1
 * VRAM; the guest's TTY and the core's diagnostics scroll at
 * the bottom of the screen and go to psxrt.log next to the EBOOT. L+R+SELECT hides the text;
 * HOME exits.
 *
 * Memory: PS1 RAM, scratchpad, translation context and dispatch table live at the fixed
 * addresses of docs/recomp/S0.md (0x09800000-0x09C00FFF), reserved before anything else is
 * allocated. The translated code goes to the region DATA.PSAR names, below the PS1 RAM. VRAM,
 * SPU RAM and the MDEC input are ordinary partition blocks.
 */
#include <pspctrl.h>
#include <pspdebug.h>
#include <pspdisplay.h>
#include <pspgu.h>
#include <pspiofilemgr.h>
#include <pspkernel.h>
#include <pspsysmem.h>
#include <psputils.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "disc.h"
#include "ge.h"
#include "native.h"
#include "psx.h"

PSP_MODULE_INFO("PSX2PSP_psxrt", PSP_MODULE_USER, 0, 3);
PSP_MAIN_THREAD_ATTR(PSP_THREAD_ATTR_USER | PSP_THREAD_ATTR_VFPU);
/* A small heap, taken from the bottom of the user partition: the default one could grow over
 * 0x09800000. Large buffers are partition blocks. */
PSP_HEAP_SIZE_KB(256);

#define FIXED_BASE 0x09800000u /* PS1 RAM (2 MB), then scratchpad 0x09A00000 */
#define FIXED_SIZE 0x00401000u /* ... up to the end of the dispatch table, 0x09C01000 */
#define SCRATCH_BASE 0x09A00000u
#define USER_PARTITION 2
#define MDEC_WORDS 0x10000 /* a whole compressed frame */
#define PS1_HZ 33868800u

/* ---- text: a ring of lines, shown at the bottom and appended to psxrt.log ---- */
#define LINES 64
#define COLS 60
#define SHOWN 12
static char lines[LINES][COLS + 1];
static int line_head; /* next line to write */
static char tty_line[COLS + 1];
static int tty_len;
static SceUID log_fd = -1;
static int trace_bits; /* from psxrt.trace beside the EBOOT */
static char log_buf[8192];
static int log_len;

static void log_flush(void)
{
    if (log_fd >= 0 && log_len)
        sceIoWrite(log_fd, log_buf, log_len);
    log_len = 0;
}

static void push_line(const char *s)
{
    int n = (int)strlen(s);
    if (log_len + n + 1 > (int)sizeof log_buf)
        log_flush();
    if (n + 1 <= (int)sizeof log_buf) {
        memcpy(log_buf + log_len, s, n);
        log_len += n;
        log_buf[log_len++] = '\n';
    }
    for (int at = 0; at == 0 || at < n; at += COLS) { /* long lines wrap on screen */
        snprintf(lines[line_head], COLS + 1, "%s", s + at);
        line_head = (line_head + 1) % LINES;
    }
}

static void say(const char *fmt, ...)
{
    char line[256];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(line, sizeof line, fmt, ap);
    va_end(ap);
    push_line(line);
}

void disc_log(const char *line) { push_line(line); }

static void host_log(void *user, const char *line)
{
    (void)user;
    push_line(line);
}

static void host_tty(void *user, const char *s, int n)
{
    (void)user;
    for (int i = 0; i < n; i++) {
        char ch = s[i];
        if (ch == '\n' || tty_len == COLS) {
            tty_line[tty_len] = 0;
            push_line(tty_line);
            tty_len = 0;
        }
        if (ch != '\n' && ch != '\r')
            tty_line[tty_len++] = ch >= 32 && ch < 127 ? ch : '?';
    }
}

static void open_log(const char *eboot)
{
    char path[256];
    snprintf(path, sizeof path, "%s", eboot);
    char *slash = strrchr(path, '/');
    if (!slash)
        return;
    snprintf(slash + 1, sizeof path - (size_t)(slash + 1 - path), "psxrt.log");
    log_fd = sceIoOpen(path, PSP_O_WRONLY | PSP_O_CREAT | PSP_O_TRUNC, 0777);
    snprintf(slash + 1, sizeof path - (size_t)(slash + 1 - path), "psxrt.trace");
    SceUID fd = sceIoOpen(path, PSP_O_RDONLY, 0);
    if (fd >= 0) { /* debugging: the core's trace bits (psx.h), as a decimal number */
        char num[16] = {0};
        sceIoRead(fd, num, sizeof num - 1);
        sceIoClose(fd);
        trace_bits = (int)strtol(num, NULL, 10);
    }
}

/* ---- memory ---- */
static void *palloc(const char *name, u32 size)
{
    SceUID uid = sceKernelAllocPartitionMemory(USER_PARTITION, name, PSP_SMEM_Low, size, NULL);
    return uid < 0 ? NULL : sceKernelGetBlockHeadAddr(uid);
}

/* Reserves FIXED_BASE..+FIXED_SIZE: PSP_SMEM_Addr, or a filler block below it and then Low. */
static int reserve_fixed(void)
{
    SceUID uid = sceKernelAllocPartitionMemory(USER_PARTITION, "psxrt fixed", PSP_SMEM_Addr, FIXED_SIZE, (void *)FIXED_BASE);
    if (uid >= 0 && (u32)sceKernelGetBlockHeadAddr(uid) == FIXED_BASE)
        return 1;
    if (uid >= 0)
        sceKernelFreePartitionMemory(uid);
    SceUID probe = sceKernelAllocPartitionMemory(USER_PARTITION, "psxrt probe", PSP_SMEM_Low, 0x100, NULL);
    u32 low = probe >= 0 ? (u32)sceKernelGetBlockHeadAddr(probe) : 0;
    if (probe >= 0)
        sceKernelFreePartitionMemory(probe);
    SceUID filler = low && low < FIXED_BASE ? sceKernelAllocPartitionMemory(USER_PARTITION, "psxrt filler", PSP_SMEM_Low, FIXED_BASE - low, NULL) : -1;
    uid = sceKernelAllocPartitionMemory(USER_PARTITION, "psxrt fixed", PSP_SMEM_Low, FIXED_SIZE, NULL);
    if (filler >= 0)
        sceKernelFreePartitionMemory(filler);
    return uid >= 0 && (u32)sceKernelGetBlockHeadAddr(uid) == FIXED_BASE;
}

/* ---- screen: PSP VRAM holds the GE's PS1 VRAM and two 565 display buffers (ge.h) ---- */
static void *draw_buf; /* relative to the start of the PSP's VRAM */
static int show_text; /* the log lines over the picture: drawn by the CPU, so off at start */

typedef struct {
    u16 u, v;
    s16 x, y, z;
} Vertex;

static void screen_init(void)
{
    sceGuInit();
    ge_open();
    sceGuDrawBuffer(GU_PSM_5650, (void *)GE_DISP_B, 512);
    sceGuDispBuffer(480, 272, (void *)GE_DISP_A, 512);
    sceGuOffset(2048 - 240, 2048 - 136);
    sceGuViewport(2048, 2048, 480, 272);
    sceGuEnable(GU_SCISSOR_TEST);
    sceGuScissor(0, 0, 480, 272);
    sceGuDisable(GU_DEPTH_TEST);
    ge_sync();
    sceDisplayWaitVblankStart();
    sceGuDisplay(GU_TRUE);
    draw_buf = (void *)GE_DISP_B;
}

/* 24-bit display modes (MDEC movies) are converted to this 8888 texture, 512x512. */
static u32 *rgb24_tex;
static u32 us_rgb24; /* time spent converting, for the log */

static const void *convert24(const Gpu *g, int w, int h)
{
    u32 t = sceKernelGetSystemTimeLow();
    const u32 wrap = VRAM_W * 2 - 1, b0 = (u32)g->disp_x * 2;
    /* whole words when the line starts on one and does not wrap: 4 pixels in 3 words */
    u32 fast = (b0 & 3) == 0 && b0 + 3u * (u32)w <= VRAM_W * 2 ? (u32)w & ~3u : 0;
    for (int y = 0; y < h; y++) {
        const u8 *row = (const u8 *)g->vram + (u32)((g->disp_y + y) & (VRAM_H - 1)) * VRAM_W * 2;
        u32 *o = rgb24_tex + y * 512;
        const u32 *s = (const u32 *)(row + b0);
        for (u32 x = 0; x < fast; x += 4, s += 3) {
            u32 a = s[0], b = s[1], c = s[2];
            o[x] = 0xFF000000u | a;
            o[x + 1] = 0xFF000000u | a >> 24 | b << 8;
            o[x + 2] = 0xFF000000u | b >> 16 | c << 16;
            o[x + 3] = 0xFF000000u | c >> 8;
        }
        for (u32 x = fast, b = b0 + 3 * fast; x < (u32)w; x++, b += 3)
            o[x] = 0xFF000000u | (u32)row[(b + 2) & wrap] << 16 | (u32)row[(b + 1) & wrap] << 8 | row[b & wrap];
    }
    sceKernelDcacheWritebackRange(rgb24_tex, (u32)h * 512 * 4);
    us_rgb24 += sceKernelGetSystemTimeLow() - t;
    return rgb24_tex;
}

/* The PS1 display area: w x h pixels, 0 when the display is off. */
static int display_size(const Psx *psx, int *w, int *h)
{
    const Gpu *g = &psx->gpu;
    static const int widths[4] = {256, 320, 512, 640};
    u32 st = g->stat;
    *w = (st & 0x10000) ? 368 : widths[(st >> 17) & 3];
    *h = g->vrange2 - g->vrange1;
    if ((st & 0x80000) && (st & 0x400000))
        *h *= 2;
    if (*h <= 0 || *h > 512)
        *h = psx->pal ? 256 : 240;
    if (*w > 512)
        *w = 512; /* GE texture limit */
    return !(st & 0x800000) && g->disp_y + *h <= VRAM_H;
}

static int display_24bit(const Psx *psx) { return (psx->gpu.stat & 0x200000) && rgb24_tex; }

/* The PS1 display area: a 5551 texture of the GE's PS1 VRAM, or a converted copy in 24-bit modes
 * (present() has brought the GE's pixels into RAM). */
static void draw_ps1(Psx *psx)
{
    const Gpu *g = &psx->gpu;
    int w, h;
    if (!display_size(psx, &w, &h))
        return;
    if (display_24bit(psx)) {
        sceGuTexMode(GU_PSM_8888, 0, 0, 0);
        sceGuTexImage(0, 512, 512, 512, convert24(g, w, h));
    } else if (!g->renderer) {
        const u16 *src = g->vram + g->disp_y * VRAM_W + (g->disp_x & ~7);
        sceKernelDcacheWritebackRange(src, (u32)h * VRAM_W * 2);
        sceGuTexMode(GU_PSM_5551, 0, 0, 0);
        sceGuTexImage(0, 512, 512, VRAM_W, src);
    } else {
        sceGuTexMode(GU_PSM_5551, 0, 0, 0);
        sceGuTexImage(0, 512, 512, VRAM_W, (void *)(0x04000000u + GE_PS1_VRAM + ((u32)g->disp_y * VRAM_W + (g->disp_x & ~7u)) * 2));
    }
    sceGuTexFunc(GU_TFX_REPLACE, GU_TCC_RGB);
    sceGuTexFilter(GU_LINEAR, GU_LINEAR);
    sceGuTexFlush();
    sceGuEnable(GU_TEXTURE_2D);
    Vertex *v = (Vertex *)sceGuGetMemory(2 * sizeof(Vertex));
    v[0] = (Vertex){0, 0, 0, 0, 0};
    v[1] = (Vertex){(u16)w, (u16)h, 480, 272, 0};
    sceGuDrawArray(GU_SPRITES, GU_TEXTURE_16BIT | GU_VERTEX_16BIT | GU_TRANSFORM_2D, 2, 0, v);
    sceGuDisable(GU_TEXTURE_2D);
}

static void present(Psx *psx, const char *status)
{
    int w, h;
    if (psx && psx->gpu.renderer && display_24bit(psx) && display_size(psx, &w, &h))
        ge_read(psx->gpu.disp_x, psx->gpu.disp_y, (w * 3 + 1) / 2 + 1, h);
    if (psx && psx->gpu.renderer)
        ge_touch();
    ge_open();
    sceGuDrawBufferList(GU_PSM_5650, draw_buf, 512);
    sceGuScissor(0, 0, 480, 272);
    sceGuDisable(GU_STENCIL_TEST);
    sceGuDisable(GU_BLEND);
    sceGuDisable(GU_DITHER);
    sceGuClearColor(0xFF000000);
    sceGuClear(GU_COLOR_BUFFER_BIT);
    if (psx)
        draw_ps1(psx);
    ge_sync();
    pspDebugScreenSetBase((u32 *)(0x44000000u + (u32)draw_buf));
    pspDebugScreenSetXY(0, 0);
    pspDebugScreenSetTextColor(0xFF40FFFF);
    pspDebugScreenPrintf("%-60s", status);
    if (show_text || !psx) {
        int n = psx ? SHOWN : 32;
        pspDebugScreenSetTextColor(0xFFFFFFFF);
        for (int i = 0; i < n; i++) {
            pspDebugScreenSetXY(0, 34 - n + i);
            pspDebugScreenPrintf("%-60s", lines[(line_head - n + i + LINES) % LINES]);
        }
    }
    draw_buf = sceGuSwapBuffers();
}

/* ---- pad: PSP bits equal PS1 bits except L/R (PSP 8/9, PS1 L1/R1 10/11); active low ---- */
static u16 read_pad(void)
{
    static u32 prev;
    SceCtrlData d;
    sceCtrlPeekBufferPositive(&d, 1);
    u32 b = d.Buttons, combo = PSP_CTRL_LTRIGGER | PSP_CTRL_RTRIGGER | PSP_CTRL_SELECT;
    if ((b & combo) == combo && (prev & combo) != combo)
        show_text = !show_text;
    prev = b;
    return (u16) ~((b & 0xF0F9u) | ((b >> 8) & 3u) << 10);
}

/* ---- the machine ---- */
static Psx psx;
static PsxHost host;
static u32 frames;
static int native;

/* Pad, status line, picture; once per second the speed (and native counters) to the log. */
static void frame(void)
{
    static u64 t0;
    static u64 c0;
    static u32 f0, speed, fps;
    static NativeStats n0;
    static u32 draw0, mdec0, rgb0;
    static char status[64];
    psx.sio.buttons[0] = read_pad();
    u64 t = sceKernelGetSystemTimeWide();
    if (!t0)
        t0 = t, c0 = psx.cycles;
    if (t - t0 >= 1000000) {
        u64 us = t - t0;
        speed = (u32)((psx.cycles - c0) * 100000000ull / ((u64)PS1_HZ * us)); /* % of real time */
        fps = (u32)((u64)(frames - f0) * 1000000 / us);
        t0 = t, c0 = psx.cycles, f0 = frames;
        snprintf(status, sizeof status, "f %u pc %08X %u%% %u fps", (unsigned)frames, (unsigned)psx.cpu.pc, (unsigned)speed, (unsigned)fps);
        if (log_fd >= 0) { /* the speed goes to the file only: the screen has its status line */
            char line[2048];
            int n = snprintf(line, sizeof line, "-- %s\n", status);
            if (native) {
                const NativeStats *s = &native_stats;
                n += snprintf(line + n, sizeof line - (size_t)n, "-- native: %u calls, %u misses, %u steps, %u hashes, %u loads\n",(unsigned)(s->calls - n0.calls),
                              (unsigned)(s->misses - n0.misses), (unsigned)(s->steps - n0.steps), (unsigned)(s->hashes - n0.hashes),
                              (unsigned)s->loads);
                n += snprintf(line + n, sizeof line - (size_t)n,
                              "-- ms: interp %u io %u (draw %u mdec %u) update %u present %u (rgb24 %u) sleep %u\n",
                              (unsigned)((s->us_interp - n0.us_interp) / 1000), (unsigned)((s->us_io - n0.us_io) / 1000),
                              (unsigned)((psx.gpu.us_draw - draw0) / 1000), (unsigned)((psx.mdec.us_decode - mdec0) / 1000),
                              (unsigned)((s->us_update - n0.us_update) / 1000), (unsigned)((s->us_present - n0.us_present) / 1000),
                              (unsigned)((us_rgb24 - rgb0) / 1000), (unsigned)((s->us_sleep - n0.us_sleep) / 1000));
                static const char *const io_names[IO_KINDS] = IO_KIND_NAMES;
                n += snprintf(line + n, sizeof line - (size_t)n, "-- io (n/ms):");
                for (int k = 0; k < IO_KINDS; k++)
                    if (s->n_io[k] != n0.n_io[k])
                        n += snprintf(line + n, sizeof line - (size_t)n, " %s %u/%u", io_names[k], (unsigned)(s->n_io[k] - n0.n_io[k]),
                                      (unsigned)((s->us_io_kind[k] - n0.us_io_kind[k]) / 1000));
                n += snprintf(line + n, sizeof line - (size_t)n, "\n");
                n0 = *s, draw0 = psx.gpu.us_draw, mdec0 = psx.mdec.us_decode, rgb0 = us_rgb24;
                Gpu *g = &psx.gpu;
                n += snprintf(line + n, sizeof line - (size_t)n,
                              "-- gpu: poly %u/%u line %u/%u rect %u/%u copy %u/%u fill %u/%u other %u/%u (n/ms), frags %u tested %u\n",
                              (unsigned)g->n_kind[0], (unsigned)(g->us_kind[0] / 1000), (unsigned)g->n_kind[1], (unsigned)(g->us_kind[1] / 1000),
                              (unsigned)g->n_kind[2], (unsigned)(g->us_kind[2] / 1000), (unsigned)g->n_kind[3], (unsigned)(g->us_kind[3] / 1000),
                              (unsigned)g->n_kind[4], (unsigned)(g->us_kind[4] / 1000), (unsigned)g->n_kind[5], (unsigned)(g->us_kind[5] / 1000),
                              (unsigned)g->frags, (unsigned)g->tested);
                memset(g->n_kind, 0, sizeof g->n_kind), memset(g->us_kind, 0, sizeof g->us_kind), g->frags = g->tested = 0;
            }
            n += ge_log(line + n, (int)sizeof line - n);
            log_flush();
            sceIoWrite(log_fd, line, n);
        }
    }
    if (!status[0])
        snprintf(status, sizeof status, "booting");
    u32 tp = sceKernelGetSystemTimeLow();
    present(&psx, status);
    native_stats.us_present += sceKernelGetSystemTimeLow() - tp;
}

static u32 host_usec(void *user)
{
    (void)user;
    return sceKernelGetSystemTimeLow();
}

static void host_vblank(void *user)
{
    (void)user;
    frames++;
    if (native)
        frame();
}

static int exit_callback(int a, int b, void *c)
{
    (void)a, (void)b, (void)c;
    log_flush();
    sceKernelExitGame();
    return 0;
}

static int callback_thread(SceSize args, void *argp)
{
    (void)args, (void)argp;
    int cb = sceKernelCreateCallback("psxrt exit", exit_callback, NULL);
    sceKernelRegisterExitCallback(cb);
    sceKernelSleepThreadCB();
    return 0;
}

static void halt(const char *why) __attribute__((noreturn));
static void halt(const char *why)
{
    say("STOP: %s", why);
    log_flush();
    for (;;) {
        present(NULL, "psxrt stopped - HOME exits");
        sceDisplayWaitVblankStart();
    }
}

void runtime_halt(const char *why) { halt(why); }

int main(int argc, char **argv)
{
    int fixed = reserve_fixed(); /* before any other allocation */
    int th = sceKernelCreateThread("psxrt callbacks", callback_thread, 0x11, 0x1000, PSP_THREAD_ATTR_USER, NULL);
    if (th >= 0)
        sceKernelStartThread(th, 0, NULL);
    pspDebugScreenInitEx((void *)(0x44000000u + GE_DISP_A), PSP_DISPLAY_PIXEL_FORMAT_565, 0);
    screen_init();
    sceCtrlSetSamplingCycle(0);
    sceCtrlSetSamplingMode(PSP_CTRL_MODE_DIGITAL);
    const char *eboot = argc > 0 ? argv[0] : "";
    open_log(eboot);
    say("PSX2PSP native runtime (psxrt) - S4, built %s %s", __DATE__, __TIME__);
    say("EBOOT %s", eboot);
    if (!fixed)
        halt("0x09800000-0x09C00FFF is not free");

    DiscInfo info;
    const char *err = disc_open(eboot, &info);
    if (err)
        halt(err);
    say("disc %s \"%s\": %u sectors, %u blocks", info.disc_id, info.title, (unsigned)info.sectors, (unsigned)info.blocks);
    native = info.images > 0;
    if (native) {
        u32 size = FIXED_BASE - info.code_base;
        SceUID uid = info.code_base < FIXED_BASE ? sceKernelAllocPartitionMemory(USER_PARTITION, "psxrt code", PSP_SMEM_Addr, size, (void *)info.code_base) : -1;
        if (uid < 0 || (u32)sceKernelGetBlockHeadAddr(uid) != info.code_base)
            halt("the translated code's region is not free");
        say("native: %u executables, code at %08X-%08X", (unsigned)info.images, (unsigned)info.code_base, (unsigned)(info.code_base + info.code_size));
    }

    u16 *vram = palloc("psxrt vram", VRAM_W * VRAM_H * 2 + 0x10000); /* + slack for texture reads */
    u8 *spu_ram = palloc("psxrt spu", 512 * 1024);
    u32 *mdec_in = palloc("psxrt mdec", MDEC_WORDS * 4);
    rgb24_tex = palloc("psxrt rgb24", 512 * 512 * 4);
    if (!vram || !spu_ram || !mdec_in || !rgb24_tex)
        halt("out of memory");
    host.read_sector = disc_read_sector;
    host.disc_end = 150 + info.sectors;
    host.tty = host_tty;
    host.log = host_log;
    host.vblank = host_vblank;
    host.usec = host_usec;
    psx_init(&psx, &host, (u8 *)FIXED_BASE, (u8 *)SCRATCH_BASE, vram, spu_ram, mdec_in, MDEC_WORDS);
    psx.trace = trace_bits;
    if (!(trace_bits & 16)) /* bit 4: everything in software, for A/B runs */
        ge_init(&psx, (trace_bits >> 3) & 1);
    if (psx.gpu.renderer)
        say("GE renderer%s", (trace_bits & 8) ? ", compare mode" : "");
    if (native && (err = native_init(&psx, &info)) != NULL)
        halt(err);
    if (psx_boot(&psx) < 0)
        halt("cannot boot the disc (SYSTEM.CNF / boot EXE)");
    present(&psx, "booting");
    log_flush();
    if (native)
        native_run();

    for (;;) {
        u32 f = frames;
        while (frames == f && !psx.exiting)
            cpu_run(&psx, psx.cycles + 4096);
        if (psx.exiting)
            halt(psx.exiting == 2 ? "the interpreter hit a fatal error" : "the game exited");
        frame();
    }
    return 0;
}
