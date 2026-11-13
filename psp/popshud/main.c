/*
 * popshud: measurement overlay for POPS on ARK-4 / ARK-5 (diagnostic, not for release).
 *
 * Draws one line in the bottom left corner of the PS1 picture, refreshed every second:
 *   FPS  frames the game showed: changes of its GP1(05h) display start, sampled each vblank
 *   EMU  emulated time over real time: POPS's emulated cycle count at the last emulated
 *        vblank, over the real microseconds, at 33.8688 MHz (100% = full speed)
 *   CPU  share of the PSP CPU outside the idle thread (100% = POPS has no headroom)
 * EMU under 100 with CPU at 100 means the PSP cannot keep up; FPS under 60 with EMU at 100
 * means the game itself, in emulated time, draws at that rate.
 *
 * Nothing in POPS is written. The two counters are POPS globals at fixed offsets from its gp,
 * the same in pops_01g..09g of firmware 6.61 (written at 0x11920 in 01g, 0x11930 in 02g/03g,
 * 0x119c0 in 04g/07g/09g). A kernel thread reads them after each vblank and, once POPS has
 * drawn its picture of that frame, draws into the frame buffer on screen.
 *
 * The module imports systemctrl only, like popsfs. The kernel NIDs of firmware 6.61 are not
 * those of the pspsdk stubs, so every Sony function is looked up at run time: in the kernel
 * library first, then in the user library of the same module, whose NIDs did not change.
 * The first steps are written to ms0:/SEPLUGINS/popshud.log, so a PSP that shows no line
 * still says why.
 */
#include <pspsdk.h>
#include <pspkernel.h>
#include "hud.h"

PSP_MODULE_INFO("PSX2PSP_PopsHud", 0x1007, 1, 2);

/* Only the fields read here; offsets as in ARK's common/include/module2.h. */
typedef struct SceModule2 {
    struct SceModule2 *next; /* 0x00 */
    unsigned short attribute; /* 0x04 */
    unsigned char version[2]; /* 0x06 */
    char modname[27]; /* 0x08 */
    char terminal; /* 0x23 */
    unsigned int pad[17]; /* 0x24 */
    unsigned int gp_value; /* 0x68 */
    unsigned int text_addr; /* 0x6C */
    unsigned int text_size; /* 0x70 */
    unsigned int data_size; /* 0x74 */
    unsigned int bss_size; /* 0x78 */
} SceModule2;

typedef int (*STMOD_HANDLER)(SceModule2 *);
STMOD_HANDLER sctrlHENSetStartModuleHandler(STMOD_HANDLER handler);
u32 sctrlHENFindFunction(const char *mod, const char *lib, u32 nid);

#define GP_VBLANK_CYCLES 0x360C /* emulated cycle count at the last emulated vblank */
#define GP_DISPLAY_START 0x3610 /* GP1(05h) display start: x (low half), y (high half) */

/*
 * POPS redraws the whole picture with the GE on every frame, right after the vblank: its
 * emulation thread (priority 0x11) waits in sceDisplayWaitVblankStartCB and then sends the
 * display list (0x118e0 in pops_01g). Drawn at the vblank, the box is erased before the
 * screen shows it. It is drawn DRAW_DELAY_US later instead, in the bottom left corner: the
 * PSP scans line n about 0.8 + n * 0.0583 ms after the vblank, so the box (lines 244-263)
 * goes out at 15.0-16.2 ms. When POPS runs behind real time its picture can land at any
 * moment, and the box then misses the frames where it lands between the two.
 */
#define DRAW_DELAY_US 12000
#define FRAME_US 16683
#define HUD_X 8

/* POPS's own frame buffer (SetFrameBuf at 0x34fc8 in pops_01g), when GetFrameBuf is missing. */
#define POPS_FB 0x04000000
#define POPS_FB_STRIDE 512
#define POPS_FB_FORMAT 0 /* 565 */

#define LOG_PATH "ms0:/SEPLUGINS/popshud.log"

typedef SceUID (*CreateThreadFn)(const char *, SceKernelThreadEntry, int, int, SceUInt,
                                 SceKernelThreadOptParam *);
typedef int (*StartThreadFn)(SceUID, SceSize, void *);
typedef int (*DelayThreadFn)(SceUInt);
typedef u32 (*TimeLowFn)(void);
typedef int (*ReferStatusFn)(SceKernelSystemStatus *);
typedef int (*WaitVblankFn)(void);
typedef int (*GetFrameBufFn)(void **, int *, int *, int);
typedef SceUID (*IoOpenFn)(const char *, int, SceMode);
typedef int (*IoWriteFn)(SceUID, const void *, SceSize);
typedef int (*IoCloseFn)(SceUID);

static CreateThreadFn create_thread;
static StartThreadFn start_thread;
static DelayThreadFn delay_thread;
static TimeLowFn time_low;
static ReferStatusFn refer_status;
static WaitVblankFn wait_vblank;
static GetFrameBufFn get_frame_buf;
static IoOpenFn io_open;
static IoWriteFn io_write;
static IoCloseFn io_close;

/* One letter per function: K kernel library, U user library, - not found. */
static char found[11];

static STMOD_HANDLER previous;
static volatile int pops_seen;
static volatile u32 pops_gp, seen_gp, seen_text, seen_end;

static u32 find(int i, const char *mod, const char *klib, const char *ulib, u32 nid)
{
    u32 f = sctrlHENFindFunction(mod, klib, nid);

    found[i] = f ? 'K' : 'U';
    if (!f)
        f = sctrlHENFindFunction(mod, ulib, nid);
    if (!f)
        found[i] = '-';
    return f;
}

static void resolve(void)
{
    static const char tm[] = "sceThreadManager", tk[] = "ThreadManForKernel",
                      tu[] = "ThreadManForUser", dm[] = "sceDisplay_Service",
                      dk[] = "sceDisplay_driver", du[] = "sceDisplay", im[] = "sceIOFileManager",
                      ik[] = "IoFileMgrForKernel", iu[] = "IoFileMgrForUser";

    create_thread = (CreateThreadFn)find(0, tm, tk, tu, 0x446D8DE6);
    start_thread = (StartThreadFn)find(1, tm, tk, tu, 0xF475845D);
    delay_thread = (DelayThreadFn)find(2, tm, tk, tu, 0xCEADEB47);
    time_low = (TimeLowFn)find(3, tm, tk, tu, 0x369ED59D);
    refer_status = (ReferStatusFn)find(4, tm, tk, tu, 0x627E6F3A);
    wait_vblank = (WaitVblankFn)find(5, dm, dk, du, 0x984C27E7);
    get_frame_buf = (GetFrameBufFn)find(6, dm, dk, du, 0xEEDA2E54);
    io_open = (IoOpenFn)find(7, im, ik, iu, 0x109F50BC);
    io_write = (IoWriteFn)find(8, im, ik, iu, 0x42EC03AC);
    io_close = (IoCloseFn)find(9, im, ik, iu, 0x810C4BD1);
    found[10] = 0;
}

/* Appends "tag v0 v1 v2 v3" (values in hex) to the log. */
static void log_line(const char *tag, u32 v0, u32 v1, u32 v2, u32 v3)
{
    char line[96];
    u32 v[4] = {v0, v1, v2, v3};
    int n = 0, i, k;
    SceUID fd;

    if (!io_open || !io_write || !io_close)
        return;
    while (*tag && n < 40)
        line[n++] = *tag++;
    for (i = 0; i < 4; i++) {
        line[n++] = ' ';
        for (k = 28; k >= 0; k -= 4)
            line[n++] = "0123456789abcdef"[(v[i] >> k) & 15];
    }
    line[n++] = '\n';
    fd = io_open(LOG_PATH, PSP_O_WRONLY | PSP_O_CREAT | PSP_O_APPEND, 0777);
    if (fd >= 0) {
        io_write(fd, line, n);
        io_close(fd);
    }
}

static int is_pops(const char *name)
{
    return name[0] == 'p' && name[1] == 'o' && name[2] == 'p' && name[3] == 's' && name[4] == 0;
}

static int on_module_start(SceModule2 *mod)
{
    if (is_pops(mod->modname)) {
        u32 lo = mod->text_addr, hi = lo + mod->text_size + mod->data_size + mod->bss_size;
        seen_gp = mod->gp_value;
        seen_text = lo;
        seen_end = hi;
        if (mod->gp_value >= lo && mod->gp_value + GP_DISPLAY_START + 4 <= hi)
            pops_gp = mod->gp_value;
        pops_seen = 1;
    }
    return previous ? previous(mod) : 0;
}

static u32 pops_word(u32 offset)
{
    return pops_gp ? *(volatile u32 *)(pops_gp + offset) : 0;
}

static u32 now_us(void)
{
    return time_low ? time_low() : 0;
}

static u32 idle_us(void)
{
    SceKernelSystemStatus st;

    st.size = sizeof st;
    return !refer_status || refer_status(&st) < 0 ? 0 : st.idleClocks.low;
}

static void wait(SceUInt us)
{
    if (delay_thread)
        delay_thread(us);
}

static int hud_thread(SceSize args, void *argp)
{
    HudStats stats = {HUD_NONE, HUD_NONE, HUD_NONE};
    char text[HUD_TEXT_MAX];
    u32 t0, cycles0, idle0, start, flips = 0, windows = 0;
    int logged_fb = 0;

    (void)args;
    (void)argp;
    log_line("start", found[0] << 24 | found[1] << 16 | found[2] << 8 | found[3],
             found[4] << 24 | found[5] << 16 | found[6] << 8 | found[7],
             found[8] << 24 | found[9] << 16, 0);
    while (!pops_seen)
        wait(100000);
    log_line("pops gp text end", seen_gp, seen_text, seen_end, pops_gp);
    t0 = now_us();
    cycles0 = pops_word(GP_VBLANK_CYCLES);
    idle0 = idle_us();
    start = pops_word(GP_DISPLAY_START);
    hud_format(text, &stats);
    for (;;) {
        void *fb = 0;
        int stride = 0, format = -1, ret = -1;
        u32 now, v;

        if (wait_vblank)
            wait_vblank();
        else
            wait(FRAME_US);
        v = pops_word(GP_DISPLAY_START);
        if (v != start) {
            start = v;
            flips++;
        }
        now = now_us();
        if (now - t0 >= 1000000) {
            u32 cycles = pops_word(GP_VBLANK_CYCLES), idle = idle_us();
            hud_compute(&stats, now - t0, flips, cycles - cycles0, idle - idle0);
            if (!pops_gp)
                stats.fps = stats.emu = HUD_NONE;
            hud_format(text, &stats);
            if (windows < 3)
                log_line("stats fps emu cpu", stats.fps, stats.emu, stats.cpu, ++windows);
            t0 = now;
            cycles0 = cycles;
            idle0 = idle;
            flips = 0;
        }
        /* After POPS's picture of this frame, before the scan reaches the box. */
        wait(DRAW_DELAY_US);
        if (get_frame_buf)
            ret = get_frame_buf(&fb, &stride, &format, 0 /* PSP_DISPLAY_SETBUF_IMMEDIATE */);
        else {
            fb = (void *)POPS_FB;
            stride = POPS_FB_STRIDE;
            format = POPS_FB_FORMAT;
        }
        if (!logged_fb) {
            log_line("fb ret addr stride format", ret, (u32)fb, stride, format);
            logged_fb = 1;
        }
        if ((ret >= 0 || !get_frame_buf) && fb && stride >= 480 && format >= 0 && format <= 3)
            /* Uncached alias: the display reads memory, not the CPU cache. */
            hud_draw((void *)(((u32)fb & 0x1FFFFFFF) | 0x40000000), stride, format, HUD_X,
                     272 - HUD_X - hud_height(), text);
    }
    return 0;
}

int module_start(SceSize args, void *argp)
{
    SceUID thid;

    (void)args;
    (void)argp;
    resolve();
    previous = sctrlHENSetStartModuleHandler(on_module_start);
    if (create_thread && start_thread) {
        thid = create_thread("popshud", hud_thread, 0x10, 0x1000, 0, NULL);
        if (thid >= 0)
            start_thread(thid, 0, NULL);
    }
    return 0;
}
