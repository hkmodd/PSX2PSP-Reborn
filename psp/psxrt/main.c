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
#include <pspaudio.h>
#include <pspctrl.h>
#include <pspdebug.h>
#include <pspdisplay.h>
#include <pspgu.h>
#include <pspiofilemgr.h>
#include <pspkernel.h>
#include <psppower.h>
#include <pspsysmem.h>
#include <psputils.h>
#include <ctype.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "disc.h"
#include "ge.h"
#include "native.h"
#include "psx.h"
#include "sas.h"
#include "snap.h"

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
static char log_path[256]; /* reopened to append: only a close updates the size on the stick */
static int trace_bits; /* from psxrt.trace beside the EBOOT */
static int cpu_override = -1; /* from psxrt.cpu beside the EBOOT */
/* psxrt.slow beside the EBOOT: us slept at each game frame, to test pacing when the PSP is late */
static u32 slow_us, slow_flips;
/* psxrt.prof beside the EBOOT: the Allegrex profiler's counters once a second (pspsdk's
 * PspDebugProfilerRegs, 20 words at 0xBC400000: 0 enable, then the counts), through KUBridge
 * (kubridge.S), which needs ARK-4 */
unsigned int kuKernelPeekw(void *addr);
void kuKernelPokew(void *addr, unsigned int value);
#define PROF_REG(k) ((void *)(0xBC400000u + 4 * (k)))
static int prof_on;

static void prof_clear(void)
{
    for (int k = 1; k < 20; k++)
        kuKernelPokew(PROF_REG(k), 0);
}

/* The counters' share of the GPU DMAs (the PS1 display lists walked into GE commands): cpu ck,
 * memory stall, i_miss, d_miss. A DMA runs on the CPU thread, never across the log's clear. */
static const u8 gd_reg[4] = {2, 4, 13, 14};
static u32 gd_at[4], gd_sum[4], gd_runs;
static void host_gpu_dma(void *user, int end)
{
    (void)user;
    if (!prof_on)
        return;
    for (int k = 0; k < 4; k++) {
        u32 r = kuKernelPeekw(PROF_REG(gd_reg[k]));
        if (end)
            gd_sum[k] += r - gd_at[k];
        else
            gd_at[k] = r;
    }
    gd_runs += end;
}

/* The same counters around the runtime's own work (native.h): sums of the kept samples times
 * their weight, the weights' sum, the samples kept and dropped. A sample across the log's clear
 * (the counters went back) is dropped too. */
static u32 pb_at[4], pb_w, pb_gd, pb_sum[PB_PARTS][4], pb_wsum[PB_PARTS], pb_kept[PB_PARTS], pb_drop[PB_PARTS];
static int pb_open = -1;

int prof_begin(int part, u32 weight)
{
    if (!prof_on || pb_open >= 0)
        return 0;
    pb_open = part, pb_w = weight, pb_gd = gd_runs;
    for (int k = 0; k < 4; k++)
        pb_at[k] = kuKernelPeekw(PROF_REG(gd_reg[k]));
    return 1;
}

void prof_end(int part, int drop)
{
    u32 r[4];
    for (int k = 0; k < 4; k++)
        r[k] = kuKernelPeekw(PROF_REG(gd_reg[k]));
    pb_open = -1;
    if (drop || pb_gd != gd_runs || r[0] < pb_at[0]) {
        pb_drop[part]++;
        return;
    }
    pb_kept[part]++, pb_wsum[part] += pb_w;
    for (int k = 0; k < 4; k++)
        pb_sum[part][k] += (r[k] - pb_at[k]) * pb_w;
}

/* Host calls by site (guest pc and flag bits), the most frequent logged each second. */
#define SITES 512
static struct { u32 pc, n; } sites[SITES];
static u32 sites_lost;

void prof_site(u32 pc)
{
    if (!prof_on)
        return;
    for (u32 i = pc * 2654435761u >> 23, k = 0; k < 8; k++, i = (i + 1) % SITES)
        if (!sites[i].n || sites[i].pc == pc) {
            sites[i].pc = pc, sites[i].n++;
            return;
        }
    sites_lost++;
}

static int sites_log(char *buf, int len)
{
    int n = snprintf(buf, (size_t)len, "-- sites (pc/n, %u lost):", (unsigned)sites_lost);
    for (int top = 0; top < 24 && n < len - 24; top++) {
        int best = -1;
        for (int i = 0; i < SITES; i++)
            if (sites[i].n && (best < 0 || sites[i].n > sites[best].n))
                best = i;
        if (best < 0)
            break;
        n += snprintf(buf + n, (size_t)(len - n), " %08X/%u", (unsigned)sites[best].pc, (unsigned)sites[best].n);
        sites[best].n = 0;
    }
    memset(sites, 0, sizeof sites);
    sites_lost = 0;
    return n < len ? n + snprintf(buf + n, (size_t)(len - n), "\n") : len;
}
/* psxrt.spu beside the EBOOT: "cpu" (0) the SPU mixes every voice; "both" (2) sceSasCore plays
 * its voices beside the SPU, for the level comparison; otherwise (1) sceSasCore plays them */
static int spu_mode = 1;
static volatile int sas_on; /* spu_mode, once sceSasCore runs */
/* psxrt.watch beside the EBOOT: PS1 RAM words logged once a second, last value and the sum of
 * the values seen at each display flip (a per-frame variable summed over the game's frames) */
#define WATCH_MAX 8
static u32 watch_addr[WATCH_MAX], watch_sum[WATCH_MAX], watch_n, watch_flips;
#define PAD_MAX 256
typedef struct {
    u32 frame, buttons, frames;
} PadPress;
static PadPress pad_script[PAD_MAX]; /* from psxrt.pad beside the EBOOT */
static int pad_n;
static char base_dir[256]; /* the EBOOT's directory, with the slash */
static u32 shot_at[32];    /* from psxrt.shots beside the EBOOT: frames to save as BMP */
static int shot_n;
#define POKE_MAX 16
typedef struct {
    u32 from, to, addr, n;
    u8 bytes[32];
} Poke;
static Poke pokes[POKE_MAX]; /* from psxrt.poke beside the EBOOT: RAM writes, like cheat codes */
static int poke_n;
static char log_buf[8192];
static int log_len;
/* psxrt.bench beside the EBOOT: seconds to measure (after BENCH_SKIP), then one summary line
 * and a stop */
#define BENCH_SKIP 2
static u32 bench_secs, bench_n;
static struct {
    u32 speed, vbl, fps, interp, io, draw, spu, timer, dma2, update, present, sleep, sas;
} bench;

static void halt(const char *why) __attribute__((noreturn));

/* A second's microseconds as ms; a sampled sum less the clock's cost can come out just below 0. */
static unsigned ms_of(u32 us) { return (s32)us < 0 ? 0 : us / 1000; }

static void log_flush(void)
{
    if (log_fd >= 0 && log_len)
        sceIoWrite(log_fd, log_buf, log_len);
    log_len = 0;
}

/* The stick's directory gets the file's size only when it is closed: a game switched off or hung
 * would leave an empty log. Closed and reopened every second for the first 20, then every 5th, and
 * in any second that logged an event (a load, an Exec, a flush): a crash then loses at most the
 * second it happens in, not the 5 before it (the S20a device crash left no line after CD_init). */
static int log_event;
static void log_keep(void)
{
    static u32 n;
    if (log_fd < 0 || (n++ >= 20 && n % 5 && !log_event))
        return;
    log_event = 0;
    sceIoClose(log_fd);
    log_fd = sceIoOpen(log_path, PSP_O_WRONLY | PSP_O_APPEND, 0777);
}

static void push_line(const char *s)
{
    int n = (int)strlen(s);
    log_event = 1;
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
    snprintf(log_path, sizeof log_path, "%s", path);
    snprintf(slash + 1, sizeof path - (size_t)(slash + 1 - path), "psxrt.trace");
    SceUID fd = sceIoOpen(path, PSP_O_RDONLY, 0);
    if (fd >= 0) { /* debugging: the core's trace bits (psx.h), as a decimal number */
        char num[16] = {0};
        sceIoRead(fd, num, sizeof num - 1);
        sceIoClose(fd);
        trace_bits = (int)strtol(num, NULL, 10);
    }
    snprintf(slash + 1, sizeof path - (size_t)(slash + 1 - path), "psxrt.cpu");
    fd = sceIoOpen(path, PSP_O_RDONLY, 0);
    if (fd >= 0) { /* debugging: the CPU cycle shift, as a decimal number */
        char num[16] = {0};
        sceIoRead(fd, num, sizeof num - 1);
        sceIoClose(fd);
        cpu_override = (int)strtol(num, NULL, 10);
    }
    snprintf(slash + 1, sizeof path - (size_t)(slash + 1 - path), "psxrt.slow");
    fd = sceIoOpen(path, PSP_O_RDONLY, 0);
    if (fd >= 0) {
        char num[16] = {0};
        sceIoRead(fd, num, sizeof num - 1);
        sceIoClose(fd);
        slow_us = (u32)strtoul(num, NULL, 10);
    }
    snprintf(slash + 1, sizeof path - (size_t)(slash + 1 - path), "psxrt.prof");
    fd = sceIoOpen(path, PSP_O_RDONLY, 0);
    if (fd >= 0) {
        sceIoClose(fd);
        prof_on = 1;
        kuKernelPokew(PROF_REG(0), 1);
        prof_clear();
    }
    snprintf(slash + 1, sizeof path - (size_t)(slash + 1 - path), "psxrt.spu");
    fd = sceIoOpen(path, PSP_O_RDONLY, 0);
    if (fd >= 0) {
        char word[16] = {0};
        sceIoRead(fd, word, sizeof word - 1);
        sceIoClose(fd);
        spu_mode = !strncmp(word, "cpu", 3) ? 0 : !strncmp(word, "both", 4) ? 2 : 1;
    }
    snprintf(slash + 1, sizeof path - (size_t)(slash + 1 - path), "psxrt.bench");
    fd = sceIoOpen(path, PSP_O_RDONLY, 0);
    if (fd >= 0) { /* measurements: seconds, decimal */
        char num[16] = {0};
        sceIoRead(fd, num, sizeof num - 1);
        sceIoClose(fd);
        bench_secs = (u32)strtoul(num, NULL, 10);
    }
    snprintf(slash + 1, sizeof path - (size_t)(slash + 1 - path), "psxrt.watch");
    fd = sceIoOpen(path, PSP_O_RDONLY, 0);
    if (fd >= 0) { /* debugging: hex addresses of PS1 RAM words */
        char text[256] = {0};
        sceIoRead(fd, text, sizeof text - 1);
        sceIoClose(fd);
        char *s = text, *e;
        while (watch_n < WATCH_MAX) {
            u32 a = (u32)strtoul(s, &e, 16);
            if (e == s)
                break;
            watch_addr[watch_n++] = a, s = e;
        }
    }
    snprintf(slash + 1, sizeof path - (size_t)(slash + 1 - path), "psxrt.pad");
    fd = sceIoOpen(path, PSP_O_RDONLY, 0);
    if (fd >= 0) { /* debugging: scripted presses, "frame buttons(hex, PSP bits) frames" triples */
        static char text[4096];
        sceIoRead(fd, text, sizeof text - 1);
        sceIoClose(fd);
        char *s = text, *e;
        while (pad_n < PAD_MAX) {
            u32 f = (u32)strtoul(s, &e, 10);
            if (e == s)
                break;
            u32 b = (u32)strtoul(e, &s, 16);
            u32 n = (u32)strtoul(s, &e, 10);
            if (e == s)
                break;
            pad_script[pad_n++] = (PadPress){f, b, n};
            s = e;
        }
    }
    snprintf(slash + 1, sizeof path - (size_t)(slash + 1 - path), "psxrt.shots");
    fd = sceIoOpen(path, PSP_O_RDONLY, 0);
    if (fd >= 0) {
        char text[512] = {0};
        sceIoRead(fd, text, sizeof text - 1);
        sceIoClose(fd);
        for (char *s = text, *e; shot_n < (int)(sizeof shot_at / sizeof *shot_at); s = e) {
            while (*s == ' ' || *s == '\n' || *s == '\r')
                s++;
            u32 ram = *s == 'r'; /* "r123": the PS1 RAM at frame 123 */
            u32 f = (u32)strtoul(s + ram, &e, 10);
            if (e == s + ram)
                break;
            shot_at[shot_n++] = f | ram << 31;
        }
    }
    snprintf(slash + 1, sizeof path - (size_t)(slash + 1 - path), "psxrt.poke");
    fd = sceIoOpen(path, PSP_O_RDONLY, 0);
    if (fd >= 0) { /* debugging: "from to addr(hex) bytes(hex)" lines, written at every vblank in [from, to] */
        static char text[2048];
        sceIoRead(fd, text, sizeof text - 1);
        sceIoClose(fd);
        char *s = text, *e;
        while (poke_n < POKE_MAX) {
            Poke *p = &pokes[poke_n];
            p->from = (u32)strtoul(s, &e, 10);
            if (e == s)
                break;
            p->to = (u32)strtoul(e, &s, 10);
            p->addr = (u32)strtoul(s, &e, 16) & (RAM_SIZE - 1);
            while (*e == ' ' || *e == '\t')
                e++;
            for (p->n = 0; p->n < sizeof p->bytes && isxdigit((unsigned char)e[0]) && isxdigit((unsigned char)e[1]); e += 2) {
                char hex[3] = {e[0], e[1], 0};
                p->bytes[p->n++] = (u8)strtoul(hex, NULL, 16);
            }
            if (!p->n || p->addr + p->n > RAM_SIZE)
                break;
            poke_n++;
            s = e;
        }
    }
    slash[1] = 0;
    snprintf(base_dir, sizeof base_dir, "%s", path);
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
/* present's parts, for the log: waiting for the GE to finish the frame's drawing, then its own
 * list (the PS1 picture to the screen), then the text */
static u32 us_pwait, us_pdraw, us_ptext;

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
    *h = gpu_display_height(psx);
    if (*w > 512)
        *w = 512; /* GE texture limit */
    return !(st & 0x800000) && g->disp_y + *h <= VRAM_H;
}

static int display_24bit(const Psx *psx) { return (psx->gpu.stat & 0x200000) && rgb24_tex; }

/* convert24 for present: redone only when the display area moved or VRAM was written since, and
 * at least once a second (a movie at 15 fps is shown at every vblank). force: always. */
static const void *convert24_cached(const Gpu *g, int w, int h, int force)
{
    static u32 key = ~0u, pos, writes, age;
    u32 k = (u32)w | (u32)h << 10, p = (u32)g->disp_x | (u32)g->disp_y << 16;
    if (!force && k == key && p == pos && gpu_writes == writes && ++age < 60)
        return rgb24_tex;
    key = k, pos = p, writes = gpu_writes, age = 0;
    return convert24(g, w, h);
}

/* 24-bit display on the GE, when the renderer holds PS1 VRAM: no readback, no CPU conversion.
 * Pixel x's channel c (R, G, B) is byte b = d + 3x + c of the line, d the line's start past a
 * 16-byte boundary. PS1 VRAM read as a 16-bit index texture (T16) gives halfword b >> 1, and CLUT
 * shift 8 * (b & 1) that byte; a grey CLUT makes it all three channels and the pixel mask keeps
 * channel c. So for each (c, s) one draw of 1-pixel column sprites, every x with
 * (d + 3x + c) & 1 == s: 6 draws, 3w columns, into a 5650 buffer in the VRAM left after the two
 * display buffers. The scaled sprite then reads that buffer.
 * A 16-bit framebuffer read as T16 is what PPSSPP emulates (as T32 it reads black, and a GE copy
 * to RAM first crashes it). Lines past 512 halfwords (the GE's widest texture) or past VRAM's right
 * edge stay on the CPU path. */
#define GE_RGB24 0x188000u /* 512 x 256 x 2 bytes, to 0x1C8000 */
static u32 g24_clut[256] __attribute__((aligned(64)));
static Vertex g24_v[3 * 512 * 2] __attribute__((aligned(64)));
static int g24_at[3][3], g24_key = -1, g24_bad, g24_frames;
/* The 60th frame's check also times both conversions: on the real GE the 960 one-pixel columns
 * cost far more than on PPSSPP's (the S19 device run waited 126-149 ms a second for the GE in the
 * intro movie, where the CPU converts a frame in ~3.5 ms). The faster one is kept. */
static int g24_cpu;

static int display_ge24(const Psx *psx, int *w, int *h)
{
    const Gpu *g = &psx->gpu;
    return !g24_bad && !g24_cpu && g->renderer && display_24bit(psx) && display_size(psx, w, h) && *h <= 256 &&
           (g->disp_x & 7) * 2 + 3 * *w <= 1024 && g->disp_x * 2 + 3 * *w <= VRAM_W * 2;
}

/* GE_RGB24 converted from what is shown; the conversion is redone only when the display area
 * moved or VRAM was written since (a movie at 15 fps was converted at every vblank: on the real
 * GE its 960 one-pixel columns cost several ms), and at least once a second. */
static void *ge24(const Gpu *g, int w, int h, int force)
{
    static u32 g24_pos, g24_writes, g24_age;
    int d = (g->disp_x & 7) * 2, key = w | h << 10 | d << 20;
    u32 pos = (u32)g->disp_x | (u32)g->disp_y << 16;
    if (!force && key == g24_key && pos == g24_pos && gpu_writes == g24_writes && ++g24_age < 60)
        return (void *)(0x04000000u + GE_RGB24);
    g24_pos = pos, g24_writes = gpu_writes, g24_age = 0;
    if (key != g24_key) {
        int k = 0;
        for (int c = 0; c < 3; c++)
            for (int s = 0; s < 2; s++) {
                g24_at[c][s] = k;
                for (int x = 0; x < w; x++) {
                    u32 b = (u32)(d + 3 * x + c);
                    if ((int)(b & 1) != s)
                        continue;
                    g24_v[k++] = (Vertex){(u16)(b >> 1), 0, (s16)x, 0, 0};
                    g24_v[k++] = (Vertex){(u16)((b >> 1) + 1), (u16)h, (s16)(x + 1), (s16)h, 0};
                }
                g24_at[c][s + 1] = k;
            }
        for (int i = 0; i < 256; i++)
            g24_clut[i] = 0xFF000000u | (u32)i * 0x010101u;
        sceKernelDcacheWritebackRange(g24_v, (u32)k * sizeof(Vertex));
        sceKernelDcacheWritebackRange(g24_clut, sizeof g24_clut);
        g24_key = key;
    }
    sceGuTexSync();
    sceGuDrawBufferList(GU_PSM_5650, (void *)GE_RGB24, 512);
    sceGuScissor(0, 0, w, h);
    sceGuDisable(GU_ALPHA_TEST);
    sceGuDisable(GU_DEPTH_TEST);
    sceGuTexMode(GU_PSM_T16, 0, 0, 0);
    sceGuTexImage(0, 512, 256, VRAM_W, (void *)(0x04000000u + GE_PS1_VRAM + ((u32)g->disp_y * VRAM_W + (g->disp_x & ~7u)) * 2));
    sceGuTexFunc(GU_TFX_REPLACE, GU_TCC_RGB);
    sceGuTexFilter(GU_NEAREST, GU_NEAREST);
    sceGuClutMode(GU_PSM_8888, 0, 0xFF, 0);
    sceGuClutLoad(32, g24_clut);
    sceGuTexFlush();
    sceGuEnable(GU_TEXTURE_2D);
    for (int c = 0; c < 3; c++) {
        sceGuPixelMask(0xFFFFFFu & ~(0xFFu << 8 * c));
        for (int s = 0; s < 2; s++) {
            int n = g24_at[c][s + 1] - g24_at[c][s];
            if (!n)
                continue;
            sceGuClutMode(GU_PSM_8888, 8 * s, 0xFF, 0);
            sceGuDrawArray(GU_SPRITES, GU_TEXTURE_16BIT | GU_VERTEX_16BIT | GU_TRANSFORM_2D, n, 0, g24_v + g24_at[c][s]);
        }
    }
    sceGuPixelMask(0);
    sceGuDrawBufferList(GU_PSM_5650, draw_buf, 512);
    sceGuScissor(0, 0, 480, 272);
    sceGuTexSync();
    return (void *)(0x04000000u + GE_RGB24);
}

/* The PS1 display area scaled to the screen: a 5551 texture of the GE's PS1 VRAM, or in 24-bit
 * modes the GE's own conversion (ge24) or a CPU one (present() has brought the GE's pixels into
 * RAM). It is drawn in 32-pixel columns: one 480x272 sprite walks whole texture lines, wider
 * than the GE's 8 KB texture cache. 0: the display is off, nothing drawn. */
typedef struct {
    float u, v, x, y, z;
} SliceVtx;
#define SLICE_W 32

static int draw_ps1(Psx *psx)
{
    const Gpu *g = &psx->gpu;
    int w, h;
    if (!display_size(psx, &w, &h))
        return 0;
    if (display_ge24(psx, &w, &h)) {
        void *t = ge24(g, w, h, 0);
        sceGuTexMode(GU_PSM_5650, 0, 0, 0);
        sceGuTexImage(0, 512, 256, 512, t);
    } else if (display_24bit(psx)) {
        sceGuTexMode(GU_PSM_8888, 0, 0, 0);
        sceGuTexImage(0, 512, 512, 512, convert24_cached(g, w, h, 0));
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
    const int n = (480 + SLICE_W - 1) / SLICE_W;
    SliceVtx *v = (SliceVtx *)sceGuGetMemory(2 * n * sizeof(SliceVtx));
    int px0, py0, px1, py1; /* the part of the display area that holds the picture */
    gpu_picture(psx, w, h, &px0, &py0, &px1, &py1);
    const float su = (float)(px1 - px0) / 480.0f;
    for (int i = 0; i < n; i++) {
        int x0 = i * SLICE_W, x1 = x0 + SLICE_W > 480 ? 480 : x0 + SLICE_W;
        v[2 * i] = (SliceVtx){(float)px0 + (float)x0 * su, (float)py0, (float)x0, 0.0f, 0.0f};
        v[2 * i + 1] = (SliceVtx){(float)px0 + (float)x1 * su, (float)py1, (float)x1, 272.0f, 0.0f};
    }
    sceGuDrawArray(GU_SPRITES, GU_TEXTURE_32BITF | GU_VERTEX_32BITF | GU_TRANSFORM_2D, 2 * n, 0, v);
    sceGuDisable(GU_TEXTURE_2D);
    return 1;
}

/* Debugging: the PS1 display area as a 24-bit BMP, shotNNNNN.bmp beside the EBOOT. */
static void shot(Psx *psx, u32 frame_no)
{
    const Gpu *g = &psx->gpu;
    int w, h, deep = display_24bit(psx);
    if (!display_size(psx, &w, &h))
        return;
    if (g->renderer)
        ge_read(g->disp_x, g->disp_y, deep ? (w * 3 + 1) / 2 + 1 : w, h);
    if (deep)
        convert24(g, w, h);
    char path[300];
    snprintf(path, sizeof path, "%sshot%05u.bmp", base_dir, (unsigned)frame_no);
    SceUID fd = sceIoOpen(path, PSP_O_WRONLY | PSP_O_CREAT | PSP_O_TRUNC, 0777);
    if (fd < 0)
        return;
    u32 stride = ((u32)w * 3 + 3) & ~3u, size = 54 + stride * (u32)h;
    u8 hdr[54] = {'B', 'M'};
    u32 fields[] = {2, size, 10, 54, 14, 40, 18, (u32)w, 22, (u32)h, 28, 24, 34, stride * (u32)h};
    for (u32 i = 0; i < sizeof fields / sizeof *fields; i += 2)
        memcpy(hdr + fields[i], &fields[i + 1], 4);
    hdr[26] = 1; /* planes */
    sceIoWrite(fd, hdr, sizeof hdr);
    static u8 row[512 * 3 + 4];
    for (int y = h - 1; y >= 0; y--) {
        memset(row, 0, stride);
        for (int x = 0; x < w; x++) {
            u8 *o = row + x * 3;
            if (deep) {
                u32 c = rgb24_tex[y * 512 + x];
                o[0] = (u8)(c >> 16), o[1] = (u8)(c >> 8), o[2] = (u8)c;
            } else {
                u16 c = g->vram[((g->disp_y + y) & (VRAM_H - 1)) * VRAM_W + ((g->disp_x + x) & (VRAM_W - 1))];
                o[0] = (u8)((c >> 10 & 31) << 3), o[1] = (u8)((c >> 5 & 31) << 3), o[2] = (u8)((c & 31) << 3);
            }
        }
        sceIoWrite(fd, row, stride);
    }
    sceIoClose(fd);
    say("shot %s (%dx%d%s)", path, w, h, deep ? ", 24-bit" : "");
}

/* On the 60th and 600th frames the GE converts: the CPU's conversion of the same pixels must match it
 * to the bit, or the CPU path takes over for good. */
static void g24_check(const Gpu *g, int w, int h)
{
    const u32 *ref = convert24(g, w, h);
    u16 *got = (u16 *)(rgb24_tex + 512 * 256); /* the GE copies its buffer here, past ref */
    sceKernelDcacheWritebackInvalidateRange(got, 512u * (u32)h * 2);
    ge_open();
    sceGuCopyImage(GU_PSM_5650, 0, 0, w, h, 512, (void *)(0x04000000u + GE_RGB24), 0, 0, 512, got);
    ge_sync();
    u32 bad = 0, lit = 0;
    for (int y = 0; y < h; y++)
        for (int x = 0; x < w; x++) {
            u32 c = ref[y * 512 + x];
            lit += (c & 0xFFFFFF) != 0;
            u16 e = (u16)((c >> 3 & 0x1F) | (c >> 10 & 0x3F) << 5 | (c >> 19 & 0x1F) << 11);
            if (got[y * 512 + x] != e && bad++ < 4)
                say("rgb24 on the GE: (%d, %d) %04x, expected %04x", x, y, got[y * 512 + x], e);
        }
    g24_bad = bad != 0;
    say("rgb24 on the GE: %u of %u pixels (%u not black) differ%s", (unsigned)bad, (unsigned)(w * h), (unsigned)lit,
        bad ? ", the CPU converts from now on" : "");
}

/* Debugging: the 2 MB of PS1 RAM, ramNNNNN.bin beside the EBOOT. */
static void ram_dump(Psx *psx, u32 frame_no)
{
    char path[300];
    snprintf(path, sizeof path, "%sram%05u.bin", base_dir, (unsigned)frame_no);
    SceUID fd = sceIoOpen(path, PSP_O_WRONLY | PSP_O_CREAT | PSP_O_TRUNC, 0777);
    if (fd < 0)
        return;
    sceIoWrite(fd, psx->ram, RAM_SIZE);
    sceIoClose(fd);
    say("ram %s", path);
}

/* The status line and the log lines over a finished picture, then that picture to the display. */
static void show(const char *status, int all)
{
    u32 t = sceKernelGetSystemTimeLow();
    pspDebugScreenSetBase((u32 *)(0x44000000u + (u32)draw_buf));
    pspDebugScreenSetXY(0, 0);
    pspDebugScreenSetTextColor(0xFF40FFFF);
    pspDebugScreenPrintf("%-60s", status);
    if (show_text || all) {
        int n = all ? 32 : SHOWN;
        pspDebugScreenSetTextColor(0xFFFFFFFF);
        for (int i = 0; i < n; i++) {
            pspDebugScreenSetXY(0, 34 - n + i);
            pspDebugScreenPrintf("%-60s", lines[(line_head - n + i + LINES) % LINES]);
        }
    }
    us_ptext += sceKernelGetSystemTimeLow() - t;
    draw_buf = sceGuSwapBuffers();
}

/* The picture of frame N is drawn by the GE while the CPU runs frame N+1, and shown by the
 * present of frame N+1 once its ge_sync has found the GE done: the CPU never waits for the blit
 * (on the PSP 4-5 ms a frame), at the cost of one frame of latency. */
static void present(Psx *psx, const char *status)
{
    static char pending[64]; /* the status of the picture the GE is drawing; "": none */
    int w, h, chk = 0, now = !psx || !psx->gpu.renderer;
    u32 t0 = sceKernelGetSystemTimeLow();
    ge_sync(); /* what the GE has still to draw: the frame, and the last present's picture */
    u32 t1 = sceKernelGetSystemTimeLow();
    us_pwait += t1 - t0;
    if (pending[0])
        show(pending, 0), pending[0] = 0;
    if (psx && psx->gpu.renderer && display_24bit(psx) && display_size(psx, &w, &h)) {
        int ge = display_ge24(psx, &w, &h);
        ++g24_frames;
        chk = ge && (g24_frames == 60 || g24_frames == 600);
        if (!ge || chk)
            ge_read(psx->gpu.disp_x, psx->gpu.disp_y, (w * 3 + 1) / 2 + 1, h);
    }
    if (psx && psx->gpu.renderer)
        ge_touch();
    ge_open();
    sceGuDrawBufferList(GU_PSM_5650, draw_buf, 512);
    sceGuScissor(0, 0, 480, 272);
    sceGuDisable(GU_STENCIL_TEST);
    sceGuDisable(GU_BLEND);
    sceGuDisable(GU_DITHER);
    if (!psx || !draw_ps1(psx)) {
        sceGuClearColor(0xFF000000);
        sceGuClear(GU_COLOR_BUFFER_BIT);
    }
    if (now || chk) {
        ge_sync();
        if (chk)
            g24_check(&psx->gpu, w, h);
        if (chk && !g24_bad) { /* the GE's conversion on its own, then the CPU's */
            u32 a = sceKernelGetSystemTimeLow();
            ge_open();
            ge24(&psx->gpu, w, h, 1);
            ge_sync();
            u32 b = sceKernelGetSystemTimeLow();
            convert24(&psx->gpu, w, h);
            u32 c = sceKernelGetSystemTimeLow();
            g24_cpu = c - b < b - a;
            say("rgb24: a frame converts in %u us on the GE, %u us on the CPU: the %s converts from now on", (unsigned)(b - a),
                (unsigned)(c - b), g24_cpu ? "CPU" : "GE");
        }
        us_pdraw += sceKernelGetSystemTimeLow() - t1;
        show(status, !psx);
        return;
    }
    ge_run();
    snprintf(pending, sizeof pending, "%s", status[0] ? status : " ");
    us_pdraw += sceKernelGetSystemTimeLow() - t1;
}

/* ---- pad: PSP bits equal PS1 bits except L/R (PSP 8/9, PS1 L1/R1 10/11); active low ---- */
static u16 read_pad(u32 frame_no)
{
    static u32 prev;
    SceCtrlData d;
    sceCtrlPeekBufferPositive(&d, 1);
    u32 b = d.Buttons;
    for (int i = 0; i < pad_n; i++)
        if (frame_no - pad_script[i].frame < pad_script[i].frames)
            b |= pad_script[i].buttons;
    u32 combo = PSP_CTRL_LTRIGGER | PSP_CTRL_RTRIGGER | PSP_CTRL_SELECT;
    if ((b & combo) == combo && (prev & combo) != combo)
        show_text = !show_text;
    /* L+R+START: a snapshot (native.c takes it at its next host call); the game does not see
     * the three buttons */
    u32 snap = PSP_CTRL_LTRIGGER | PSP_CTRL_RTRIGGER | PSP_CTRL_START;
    if ((b & snap) == snap && (prev & snap) != snap)
        native_snap_req = 1;
    prev = b;
    if ((b & snap) == snap)
        b &= ~snap;
    return (u16) ~((b & 0xF0F9u) | ((b >> 8) & 3u) << 10);
}

/* ---- the machine ---- */
static Psx psx;
static PsxHost host;
static u32 frames;
static int native;
static u32 audio_drops, audio_gaps, audio_trims; /* per second; the audio ring below */
static u32 audio_peak_in, audio_peak_out; /* per second: the largest sample from the SPU, to the channel */

static u32 watch_word(u32 a)
{
    u32 v;
    memcpy(&v, psx.ram + (a & (RAM_SIZE - 4)), 4);
    return v;
}

/* Pad, status line, picture; once per second the speed (and native counters) to the log. */
static void frame(void)
{
    static u64 t0;
    static u64 c0;
    static u32 f0, flips0, speed, fps, game_fps;
    static NativeStats n0;
    static u32 draw0, mdec0, spu0, rgb0;
    static char status[64];
    psx.sio.buttons[0] = read_pad(frames);
    if (watch_n && psx.gpu.flips != watch_flips) {
        watch_flips = psx.gpu.flips;
        for (u32 k = 0; k < watch_n; k++)
            watch_sum[k] += watch_word(watch_addr[k]);
    }
    u64 t = sceKernelGetSystemTimeWide();
    if (!t0)
        t0 = t, c0 = psx.cycles;
    if (t - t0 >= 1000000) {
        u64 us = t - t0;
        speed = (u32)((psx.cycles - c0) * 100000000ull / ((u64)PS1_HZ * us)); /* % of real time */
        fps = (u32)((u64)(frames - f0) * 1000000 / us);
        game_fps = (u32)((u64)(psx.gpu.flips - flips0) * 1000000 / us);
        t0 = t, c0 = psx.cycles, f0 = frames, flips0 = psx.gpu.flips;
        /* "vbl": emulated vblanks a second; "fps": the game's own frames (display flips) */
        snprintf(status, sizeof status, "f %u pc %08X %u%% %u vbl %u fps", (unsigned)frames, (unsigned)psx.cpu.pc, (unsigned)speed,
                 (unsigned)fps, (unsigned)game_fps);
        if (log_fd >= 0) { /* the speed goes to the file only: the screen has its status line */
            char line[4096]; /* a busy second writes ~2.2 KB */
            int n = snprintf(line, sizeof line, "-- %s\n", status);
            if (watch_n) {
                n += snprintf(line + n, sizeof line - (size_t)n, "-- watch (last/flip sum):");
                for (u32 k = 0; k < watch_n; k++) {
                    n += snprintf(line + n, sizeof line - (size_t)n, " %08X %d/%d", (unsigned)watch_addr[k],
                                  (int)watch_word(watch_addr[k]), (int)watch_sum[k]);
                    watch_sum[k] = 0;
                }
                n += snprintf(line + n, sizeof line - (size_t)n, "\n");
            }
            if (native) {
                const NativeStats *s = &native_stats;
                n += snprintf(line + n, sizeof line - (size_t)n, "-- native: %u calls (%u lean, %u timer/irq/pad host; not: event %u irq %u poll %u source %u hit %u once %u), %u misses, %u steps, %u bios, %u hashes, %u loads, %u poll skips (%u far, %u ra scans), %u hooks (%u v_wait passes), %u updates\n",(unsigned)(s->calls - n0.calls), (unsigned)(s->io_lean - n0.io_lean), (unsigned)(s->io_host - n0.io_host),
                              (unsigned)(s->io_fall[0] - n0.io_fall[0]), (unsigned)(s->io_fall[1] - n0.io_fall[1]), (unsigned)(s->io_fall[2] - n0.io_fall[2]), (unsigned)(s->io_fall[3] - n0.io_fall[3]), (unsigned)(s->io_fall[4] - n0.io_fall[4]), (unsigned)(s->io_fall[5] - n0.io_fall[5]),
                              (unsigned)(s->misses - n0.misses), (unsigned)(s->steps - n0.steps), (unsigned)(s->bios - n0.bios), (unsigned)(s->hashes - n0.hashes),
                              (unsigned)s->loads, (unsigned)(s->poll_skips - n0.poll_skips), (unsigned)(s->poll_far - n0.poll_far),
                              (unsigned)(s->ra_scans - n0.ra_scans), (unsigned)(s->hooks - n0.hooks), (unsigned)(s->vsync_passes - n0.vsync_passes), (unsigned)(s->updates - n0.updates));
                n += snprintf(line + n, sizeof line - (size_t)n,
                              "-- ms: interp %u io %u (draw %u mdec %u spu %u) update %u present %u (wait %u draw %u text %u rgb24 %u) sleep %u;"
                              " audio drops %u gaps %u trims %u peak %u/%u; vsync wait %u (%u ms) late %u (avg %u max %u us) skipped %u short %u\n",
                              ms_of(s->us_interp - n0.us_interp), ms_of(s->us_io - n0.us_io),
                              ms_of(psx.gpu.us_draw - draw0), ms_of(psx.mdec.us_decode - mdec0),
                              ms_of(psx.spu.us_mix - spu0),
                              ms_of(s->us_update - n0.us_update), ms_of(s->us_present - n0.us_present),
                              ms_of(us_pwait), ms_of(us_pdraw), ms_of(us_ptext),
                              ms_of(us_rgb24 - rgb0), ms_of(s->us_sleep - n0.us_sleep),
                              (unsigned)audio_drops, (unsigned)audio_gaps, (unsigned)audio_trims, (unsigned)audio_peak_in, (unsigned)audio_peak_out, (unsigned)(s->vs_wait - n0.vs_wait),
                              ms_of(s->us_vs_wait - n0.us_vs_wait), (unsigned)(s->vs_late - n0.vs_late),
                              (unsigned)((s->us_late - n0.us_late) / ((s->vs_late - n0.vs_late) | 1)), (unsigned)s->us_late_max,
                              (unsigned)(s->vs_adv - n0.vs_adv), (unsigned)(s->vs_short - n0.vs_short));
                static u32 upd0[UPD_PARTS];
                static const char *const upd_names[UPD_PARTS] = UPD_PART_NAMES;
                n += snprintf(line + n, sizeof line - (size_t)n, "-- update (ms):");
                for (int k = 0; k < UPD_PARTS; k++)
                    n += snprintf(line + n, sizeof line - (size_t)n, " %s %u", upd_names[k], ms_of(psx.us_upd[k] - upd0[k])),
                        upd0[k] = psx.us_upd[k];
                static DiscStats d0;
                n += snprintf(line + n, sizeof line - (size_t)n, "\n-- disc blocks: inline %u ahead %u (waited %u); ms read %u+%u inflate %u+%u\n",
                              (unsigned)(disc_stats.loads - d0.loads), (unsigned)(disc_stats.prefetched - d0.prefetched),
                              (unsigned)(disc_stats.waited - d0.waited),
                              (unsigned)((disc_stats.us_read[0] - d0.us_read[0]) / 1000),
                              (unsigned)((disc_stats.us_read[1] - d0.us_read[1]) / 1000),
                              (unsigned)((disc_stats.us_inflate[0] - d0.us_inflate[0]) / 1000),
                              (unsigned)((disc_stats.us_inflate[1] - d0.us_inflate[1]) / 1000));
                d0 = disc_stats;
                static u32 cs0[5];
                const u32 cs[5] = {psx.cd.n_sect, psx.cd.n_xa, psx.cd.n_replaced, psx.cd.n_unread, psx.cd.n_loads};
                n += snprintf(line + n, sizeof line - (size_t)n, "-- cd sectors: data %u xa %u replaced %u unread %u loaded %u\n",
                              (unsigned)(cs[0] - cs0[0]), (unsigned)(cs[1] - cs0[1]), (unsigned)(cs[2] - cs0[2]),
                              (unsigned)(cs[3] - cs0[3]), (unsigned)(cs[4] - cs0[4]));
                memcpy(cs0, cs, sizeof cs0);
                native_stats.us_late_max = 0;
                us_pwait = us_pdraw = us_ptext = 0;
                audio_drops = audio_gaps = audio_trims = audio_peak_in = audio_peak_out = 0;
                static const char *const io_names[IO_KINDS] = IO_KIND_NAMES;
                n += snprintf(line + n, sizeof line - (size_t)n, "-- io (n/ms):");
                for (int k = 0; k < IO_KINDS; k++)
                    if (s->n_io[k] != n0.n_io[k])
                        n += snprintf(line + n, sizeof line - (size_t)n, " %s %u/%u", io_names[k], (unsigned)(s->n_io[k] - n0.n_io[k]),
                                      ms_of(s->us_io_kind[k] - n0.us_io_kind[k]));
                n += snprintf(line + n, sizeof line - (size_t)n, "\n-- gte ops (hex/n):");
                for (int k = 0; k < 64; k++)
                    if (s->n_gte[k] != n0.n_gte[k])
                        n += snprintf(line + n, sizeof line - (size_t)n, " %02X/%u", k, (unsigned)(s->n_gte[k] - n0.n_gte[k]));
                n += snprintf(line + n, sizeof line - (size_t)n, "\n");
                if (trace_bits & 256)
                    n += native_entries_log(line + n, (int)sizeof line - n);
                if (bench_secs && ++bench_n > BENCH_SKIP) {
                    bench.speed += speed, bench.vbl += fps, bench.fps += game_fps;
                    bench.interp += ms_of(s->us_interp - n0.us_interp), bench.io += ms_of(s->us_io - n0.us_io);
                    bench.draw += ms_of(psx.gpu.us_draw - draw0), bench.spu += ms_of(psx.spu.us_mix - spu0);
                    bench.timer += ms_of(s->us_io_kind[IO_TIMER] - n0.us_io_kind[IO_TIMER]), bench.dma2 += ms_of(psx.dma.us_run[2]);
                    bench.update += ms_of(s->us_update - n0.us_update), bench.present += ms_of(s->us_present - n0.us_present);
                    bench.sleep += ms_of(s->us_sleep - n0.us_sleep);
                }
                n0 = *s, draw0 = psx.gpu.us_draw, mdec0 = psx.mdec.us_decode, spu0 = psx.spu.us_mix, rgb0 = us_rgb24;
                Gpu *g = &psx.gpu;
                n += snprintf(line + n, sizeof line - (size_t)n,
                              "-- gpu: poly %u/%u (slow %u ms) line %u/%u rect %u/%u copy %u/%u fill %u/%u other %u/%u (n/ms), frags %u tested %u\n",
                              (unsigned)g->n_kind[0], ms_of(g->us_kind[0]), ms_of(gpu_slow_us), (unsigned)g->n_kind[1], ms_of(g->us_kind[1]),
                              (unsigned)g->n_kind[2], ms_of(g->us_kind[2]), (unsigned)g->n_kind[3], ms_of(g->us_kind[3]),
                              (unsigned)g->n_kind[4], ms_of(g->us_kind[4]), (unsigned)g->n_kind[5], ms_of(g->us_kind[5]),
                              (unsigned)g->frags, (unsigned)g->tested);
                memset(g->n_kind, 0, sizeof g->n_kind), memset(g->us_kind, 0, sizeof g->us_kind), g->frags = g->tested = 0;
                gpu_slow_us = 0;
                Dma *d = &psx.dma;
                n += snprintf(line + n, sizeof line - (size_t)n, "-- dma (n/ms/kwords):");
                for (int ch = 0; ch < 7; ch++)
                    if (d->n_run[ch])
                        n += snprintf(line + n, sizeof line - (size_t)n, " ch%d %u/%u/%u", ch, (unsigned)d->n_run[ch],
                                      ms_of(d->us_run[ch]), ms_of(d->words[ch]));
                n += snprintf(line + n, sizeof line - (size_t)n, "\n");
                memset(d->n_run, 0, sizeof d->n_run), memset(d->us_run, 0, sizeof d->us_run), memset(d->words, 0, sizeof d->words);
                if (sas_on) {
                    u32 us;
                    n += sas_log(line + n, (int)sizeof line - n, &us);
                    if (bench_secs && bench_n > BENCH_SKIP)
                        bench.sas += us / 1000;
                }
            }
            if (prof_on) { /* counts since the last second, in thousands */
                u32 r[20];
                for (int j = 0; j < 20; j++)
                    r[j] = kuKernelPeekw(PROF_REG(j));
                prof_clear();
                n += snprintf(line + n, sizeof line - (size_t)n,
                              "-- cpu (k): on %u ck %u stall %u memory %u copz %u vfpu %u sleep %u bus %u uncached %u/%u"
                              " cached %u/%u i_miss %u d_miss %u wb %u; gpu dma ck %u memory %u i_miss %u d_miss %u\n",
                              (unsigned)r[0], (unsigned)(r[2] / 1000), (unsigned)(r[3] / 1000), (unsigned)(r[4] / 1000),
                              (unsigned)(r[5] / 1000), (unsigned)(r[6] / 1000), (unsigned)(r[7] / 1000), (unsigned)(r[8] / 1000),
                              (unsigned)(r[9] / 1000), (unsigned)(r[10] / 1000), (unsigned)(r[11] / 1000), (unsigned)(r[12] / 1000),
                              (unsigned)(r[13] / 1000), (unsigned)(r[14] / 1000), (unsigned)(r[15] / 1000), (unsigned)(gd_sum[0] / 1000),
                              (unsigned)(gd_sum[1] / 1000), (unsigned)(gd_sum[2] / 1000), (unsigned)(gd_sum[3] / 1000));
                memset(gd_sum, 0, sizeof gd_sum);
                static const char *const pb_names[PB_PARTS] = {"calls", "update", "empty"};
                n += snprintf(line + n, sizeof line - (size_t)n, "-- rt (k, sampled 1/%u):", (unsigned)PB_EVERY);
                for (int p = 0; p < PB_PARTS; p++)
                    n += snprintf(line + n, sizeof line - (size_t)n, " %s ck %u memory %u i_miss %u d_miss %u (w %u kept %u dropped %u)%s",
                                  pb_names[p], (unsigned)(pb_sum[p][0] / 1000), (unsigned)(pb_sum[p][1] / 1000),
                                  (unsigned)(pb_sum[p][2] / 1000), (unsigned)(pb_sum[p][3] / 1000), (unsigned)pb_wsum[p],
                                  (unsigned)pb_kept[p], (unsigned)pb_drop[p], p + 1 < PB_PARTS ? ";" : "\n");
                memset(pb_sum, 0, sizeof pb_sum), memset(pb_wsum, 0, sizeof pb_wsum);
                memset(pb_kept, 0, sizeof pb_kept), memset(pb_drop, 0, sizeof pb_drop);
                n += sites_log(line + n, (int)sizeof line - n);
            }
            n += ge_log(line + n, (int)sizeof line - n);
            u32 k = bench_n - BENCH_SKIP;
            if (bench_secs && bench_n > BENCH_SKIP && k >= bench_secs)
                n += snprintf(line + n, sizeof line - (size_t)n,
                              "-- bench %u s at frame %u: speed %u%% vbl %u fps %u.%u | ms/s interp %u io %u (draw %u spu %u timer %u)"
                              " dma2 %u update %u present %u sleep %u | sas thread %u\n",
                              (unsigned)k, (unsigned)frames, (unsigned)(bench.speed / k), (unsigned)(bench.vbl / k), (unsigned)(bench.fps / k),
                              (unsigned)(bench.fps * 10 / k % 10), (unsigned)(bench.interp / k), (unsigned)(bench.io / k), (unsigned)(bench.draw / k),
                              (unsigned)(bench.spu / k), (unsigned)(bench.timer / k), (unsigned)(bench.dma2 / k), (unsigned)(bench.update / k),
                              (unsigned)(bench.present / k), (unsigned)(bench.sleep / k), (unsigned)(bench.sas / k));
            log_flush();
            sceIoWrite(log_fd, line, n);
            log_keep();
            if (bench_secs && bench_n > BENCH_SKIP && k >= bench_secs) {
                char hot[1024];
                sceIoWrite(log_fd, hot, native_hot_log(hot, sizeof hot));
                halt("bench done");
            }
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

/* ---- audio: the SPU's 44100 Hz stereo through a ring to a thread that feeds one hardware
 * channel. PS1 time follows the PSP display (native_vsync), so the SPU makes up to ~0.9% more
 * samples than the channel's 44100 Hz takes: the consumer resamples, its step following the
 * ring's smoothed fill (each sample above the target is 1/65536 faster), which settles where the
 * two rates meet. The ring absorbs the SPU's per-frame bursts; past ~190 ms of backlog the
 * consumer trims it, when it runs dry the channel gets silence. ---- */
#define AUDIO_RING 16384 /* stereo samples, a power of two */
#define AUDIO_BLOCK 1024 /* samples per hardware output, a multiple of 64 */
static s16 audio_ring[AUDIO_RING * 2] __attribute__((aligned(64))); /* sceSasCore mixes into its grains */
static volatile u32 audio_rd, audio_wr;

/* sceSasCore (sas.c): the SPU's events at the stream's sample they belong to */
static int host_spu_voice(int voice, int ev, int pending) { return sas_voice(voice, ev, audio_wr + (u32)pending); }

static void host_spu_tick(int pending) { sas_tick(audio_wr + (u32)pending); }

static void host_audio(void *user, const s16 *samples, int frames)
{
    (void)user;
    u32 wr = audio_wr, room = AUDIO_RING - (wr - audio_rd);
    if ((u32)frames > room)
        audio_drops += (u32)frames - room, frames = (int)room;
    for (int i = 0; i < frames; i++, wr++) {
        u32 at = (wr & (AUDIO_RING - 1)) * 2;
        audio_ring[at] = samples[2 * i], audio_ring[at + 1] = samples[2 * i + 1];
        u32 l = (u32)abs(samples[2 * i]), r = (u32)abs(samples[2 * i + 1]), m = l > r ? l : r;
        if (m > audio_peak_in)
            audio_peak_in = m;
    }
    __asm__ volatile("" ::: "memory"); /* the samples before the index that publishes them */
    audio_wr = wr;
}

static int audio_thread(SceSize args, void *argp)
{
    (void)args, (void)argp;
    static s16 buf[AUDIO_BLOCK * 2] __attribute__((aligned(64)));
    int ch = sceAudioChReserve(PSP_AUDIO_NEXT_CHANNEL, AUDIO_BLOCK, PSP_AUDIO_FORMAT_STEREO);
    if (ch < 0) {
        say("audio: no channel (%08X)", ch);
        return 0;
    }
    const s32 target = 3 * AUDIO_BLOCK;
    s32 avg = target; /* the fill, smoothed over ~16 blocks */
    u32 pos = 0;      /* 16.16: the fraction of a sample past rd */
    for (;;) {
        u32 top = audio_wr; /* with sceSasCore playing voices: up to where it has mixed them in */
        if (sas_on) {
            u32 mixed = sas_mix(audio_ring, AUDIO_RING, top);
            if (sas_on == 1)
                top = mixed;
        }
        u32 rd = audio_rd, fill = top - rd;
        if (fill > 8 * AUDIO_BLOCK) /* more than ~190 ms behind: catch up to ~70 ms */
            rd += fill - (u32)target, fill = (u32)target, avg = target, audio_trims++;
        avg += ((s32)fill - avg) / 16;
        s32 d = avg - target;
        u32 step = 65536u + (u32)(d < -1311 ? -1311 : d > 1311 ? 1311 : d); /* within 2% */
        if (fill > ((pos + step * AUDIO_BLOCK) >> 16) + 1) {
            for (u32 i = 0; i < AUDIO_BLOCK; i++, pos += step) {
                u32 a = ((rd + (pos >> 16)) & (AUDIO_RING - 1)) * 2, b = (a + 2) & (AUDIO_RING * 2 - 1);
                s32 f = (s32)(pos & 0xFFFF) >> 1; /* 15 bits: the product fits */
                buf[2 * i] = (s16)(audio_ring[a] + ((audio_ring[b] - audio_ring[a]) * f >> 15));
                buf[2 * i + 1] = (s16)(audio_ring[a + 1] + ((audio_ring[b + 1] - audio_ring[a + 1]) * f >> 15));
            }
            rd += pos >> 16, pos &= 0xFFFF;
        } else { /* ran dry: what there is, then silence */
            u32 n = fill < AUDIO_BLOCK ? fill : AUDIO_BLOCK;
            for (u32 i = 0; i < n; i++, rd++) {
                u32 at = (rd & (AUDIO_RING - 1)) * 2;
                buf[2 * i] = audio_ring[at], buf[2 * i + 1] = audio_ring[at + 1];
            }
            memset(buf + 2 * n, 0, (AUDIO_BLOCK - n) * 4);
            audio_gaps += n > 0;
            pos = 0;
        }
        for (u32 i = 0; i < AUDIO_BLOCK * 2; i++)
            if ((u32)abs(buf[i]) > audio_peak_out)
                audio_peak_out = (u32)abs(buf[i]);
        audio_rd = rd;
        sceKernelDcacheWritebackRange(buf, sizeof buf);
        sceAudioOutputBlocking(ch, PSP_AUDIO_VOLUME_MAX, buf);
    }
    return 0;
}

/* ---- memory card: slot 1's image in card0.mcr beside the EBOOT (Sony's 128 KB layout) ---- */
static char card_file[256];
static u8 *card_image;
static u32 card_changed; /* frame of the last change, 0 = saved */

static void card_load(const char *eboot)
{
    snprintf(card_file, sizeof card_file, "%s", eboot);
    char *slash = strrchr(card_file, '/');
    if (!slash || !(card_image = palloc("psxrt card", CARD_SIZE)))
        return;
    snprintf(slash + 1, sizeof card_file - (size_t)(slash + 1 - card_file), "card0.mcr");
    SceUID fd = sceIoOpen(card_file, PSP_O_RDONLY, 0);
    int n = fd >= 0 ? sceIoRead(fd, card_image, CARD_SIZE) : -1;
    if (fd >= 0)
        sceIoClose(fd);
    if (n != (int)CARD_SIZE)
        card_format(card_image);
    say("card %s: %s", card_file, n == (int)CARD_SIZE ? "loaded" : "new, formatted");
}

static void card_save(void)
{
    card_changed = 0;
    SceUID fd = sceIoOpen(card_file, PSP_O_WRONLY | PSP_O_CREAT | PSP_O_TRUNC, 0777);
    int n = fd >= 0 ? sceIoWrite(fd, card_image, CARD_SIZE) : -1;
    if (fd >= 0)
        sceIoClose(fd);
    say("card saved (%d)", n);
}

static void host_card_dirty(void *user, int slot)
{
    (void)user, (void)slot;
    card_changed = frames | 1;
}

static void host_vblank(void *user)
{
    (void)user;
    frames++;
    if (card_changed && frames - card_changed >= 30) /* a save writes many sectors in a row */
        card_save();
    for (int i = 0; i < poke_n; i++)
        if (frames >= pokes[i].from && frames <= pokes[i].to) {
            memcpy(psx.ram + pokes[i].addr, pokes[i].bytes, pokes[i].n);
            if (frames == pokes[i].from)
                say("poke %06X %u B, frames %u-%u", (unsigned)pokes[i].addr, (unsigned)pokes[i].n, (unsigned)pokes[i].from, (unsigned)pokes[i].to);
        }
    for (int i = 0; i < shot_n; i++)
        if ((shot_at[i] & 0x7FFFFFFFu) == frames)
            (shot_at[i] >> 31 ? ram_dump : shot)(&psx, frames);
    if (native) {
        if (slow_us && psx.gpu.flips != slow_flips) /* debugging: a slower CPU, per game frame */
            slow_flips = psx.gpu.flips, sceKernelDelayThread(slow_us);
        native_vsync_early();
        frame();
        native_vsync();
    }
}

static int exit_callback(int a, int b, void *c)
{
    (void)a, (void)b, (void)c;
    if (card_changed)
        card_save();
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

static DiscInfo disc_info;

static void snap_path(char *path, size_t cap) { snprintf(path, cap, "%spsxrt.snap", base_dir); }

/* native.c, at a host call of the top-level code: the machine to psxrt.snap beside the EBOOT,
 * which the next start resumes from. */
void runtime_snapshot(u32 host_pc)
{
    char path[300];
    snap_path(path, sizeof path);
    u32 t = sceKernelGetSystemTimeLow();
    if (psx.gpu.renderer)
        ge_read(0, 0, VRAM_W, VRAM_H);
    int r = snap_save(path, &psx, &disc_info, host_pc, frames);
    say("snapshot %s at frame %u: %s (%u ms)", path, (unsigned)frames, r ? "FAILED" : "saved", (unsigned)((sceKernelGetSystemTimeLow() - t) / 1000));
    log_flush();
}

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
    /* the PSP starts an application at 222 MHz; POPS runs at 333 */
    int clock = scePowerSetClockFrequency(333, 333, 166);
    say("clock %d MHz (%d)", scePowerGetCpuClockFrequencyInt(), clock);
    if (pad_n)
        say("pad script: %d presses", pad_n);
    say("EBOOT %s", eboot);
    if (!fixed)
        halt("0x09800000-0x09C00FFF is not free");

    const char *err = disc_open(eboot, &disc_info);
    if (err)
        halt(err);
    say("disc %s \"%s\": %u sectors, %u blocks", disc_info.disc_id, disc_info.title, (unsigned)disc_info.sectors, (unsigned)disc_info.blocks);
    native = disc_info.images > 0;
    if (native) {
        u32 size = FIXED_BASE - disc_info.code_base;
        SceUID uid = disc_info.code_base < FIXED_BASE ? sceKernelAllocPartitionMemory(USER_PARTITION, "psxrt code", PSP_SMEM_Addr, size, (void *)disc_info.code_base) : -1;
        if (uid < 0 || (u32)sceKernelGetBlockHeadAddr(uid) != disc_info.code_base)
            halt("the translated code's region is not free");
        say("native: %u executables, code at %08X-%08X", (unsigned)disc_info.images, (unsigned)disc_info.code_base, (unsigned)(disc_info.code_base + disc_info.code_size));
    }

    u16 *vram = palloc("psxrt vram", VRAM_W * VRAM_H * 2 + 0x10000); /* + slack for texture reads */
    u8 *spu_ram = palloc("psxrt spu", 512 * 1024);
    u32 *mdec_in = palloc("psxrt mdec", MDEC_WORDS * 4);
    rgb24_tex = palloc("psxrt rgb24", 512 * 512 * 4);
    if (!vram || !spu_ram || !mdec_in || !rgb24_tex)
        halt("out of memory");
    host.read_sector = disc_read_sector;
    host.disc_end = 150 + disc_info.sectors;
    host.tty = host_tty;
    host.log = host_log;
    host.vblank = host_vblank;
    host.usec = host_usec;
    host.gpu_dma = host_gpu_dma;
    host.audio = host_audio;
    int ath = sceKernelCreateThread("psxrt audio", audio_thread, 0x12, 0x2000, PSP_THREAD_ATTR_USER, NULL);
    if (ath >= 0)
        sceKernelStartThread(ath, 0, NULL);
    card_load(eboot);
    host.card[0] = card_image;
    host.card_dirty = host_card_dirty;
    psx_init(&psx, &host, (u8 *)FIXED_BASE, (u8 *)SCRATCH_BASE, vram, spu_ram, mdec_in, MDEC_WORDS);
    psx.trace = trace_bits;
    if (spu_mode) { /* before the SPU makes a sample: the stream's sample 0 is sceSasCore's */
        const char *why = sas_init(&psx, spu_mode, palloc);
        if (why) {
            say("sceSasCore: %s; the SPU mixes every voice", why);
        } else {
            spu_hw_mode(spu_mode, host_spu_voice, host_spu_tick); /* every page dirty: the first voice brings all of SPU RAM */
            sas_on = spu_mode;
            say("sceSasCore plays the SPU's voices%s", spu_mode == 2 ? " beside it (comparison)" : "");
        }
    }
    psx.pal60 = !!(disc_info.flags & DISC_PAL60);
    if (psx.pal60)
        say("PAL60: a PAL picture at 60 Hz");
    if (!(trace_bits & 16)) /* bit 4: everything in software, for A/B runs */
        ge_init(&psx, (trace_bits >> 3) & 1);
    if (psx.gpu.renderer)
        say("GE renderer%s", (trace_bits & 8) ? ", compare mode" : "");
    if (cpu_override >= 0) /* debugging: psxrt.cpu beside the EBOOT overrides the shift */
        disc_info.cpu_shift = (u32)cpu_override;
    if (native && disc_info.cpu_shift)
        say("CPU: loops charge PS1 cycles >> %u", (unsigned)disc_info.cpu_shift);
    if (native && (err = native_init(&psx, &disc_info)) != NULL)
        halt(err);
    u32 resume = 0; /* psxrt.snap beside the EBOOT: the run goes on from there */
    if (native) {
        char path[300];
        u32 f;
        snap_path(path, sizeof path);
        int r = snap_load(path, &psx, &disc_info, &resume, &f);
        if (r == -3)
            halt("psxrt.snap is unreadable: delete it to boot the disc");
        if (r == -2)
            say("%s is from another EBOOT or runtime: booting the disc", path);
        if (r == 0) {
            frames = f;
            say("resumed from %s at frame %u", path, (unsigned)f);
            if (psx.gpu.renderer) /* the GE's copy of PS1 VRAM from RAM's */
                psx.gpu.renderer->wrote(psx.gpu.renderer->self, 0, 0, VRAM_W, VRAM_H);
        }
    }
    if (!resume && psx_boot(&psx) < 0)
        halt("cannot boot the disc (SYSTEM.CNF / boot EXE)");
    present(&psx, "booting");
    log_flush();
    log_keep();
    if (native)
        native_run(resume);

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
