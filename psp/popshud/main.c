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
 */
#include <pspsdk.h>
#include <pspkernel.h>
#include <pspdisplay.h>
#include "hud.h"

PSP_MODULE_INFO("PSX2PSP_PopsHud", 0x1007, 1, 0);

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
#define HUD_X 8

static STMOD_HANDLER previous;
static volatile u32 pops_gp;

static int is_pops(const char *name)
{
    return name[0] == 'p' && name[1] == 'o' && name[2] == 'p' && name[3] == 's' && name[4] == 0;
}

static int on_module_start(SceModule2 *mod)
{
    if (is_pops(mod->modname)) {
        u32 lo = mod->text_addr, hi = lo + mod->text_size + mod->data_size + mod->bss_size;
        if (mod->gp_value >= lo && mod->gp_value + GP_DISPLAY_START + 4 <= hi)
            pops_gp = mod->gp_value;
    }
    return previous ? previous(mod) : 0;
}

static u32 pops_word(u32 offset)
{
    return *(volatile u32 *)(pops_gp + offset);
}

static u32 idle_us(void)
{
    SceKernelSystemStatus st;

    st.size = sizeof st;
    return sceKernelReferSystemStatus(&st) < 0 ? 0 : st.idleClocks.low;
}

static int hud_thread(SceSize args, void *argp)
{
    HudStats stats = {HUD_NONE, HUD_NONE, HUD_NONE};
    char text[HUD_TEXT_MAX];
    u32 t0, cycles0, idle0, start, flips = 0;

    (void)args;
    (void)argp;
    while (!pops_gp)
        sceKernelDelayThread(100000);
    t0 = sceKernelGetSystemTimeLow();
    cycles0 = pops_word(GP_VBLANK_CYCLES);
    idle0 = idle_us();
    start = pops_word(GP_DISPLAY_START);
    hud_format(text, &stats);
    for (;;) {
        void *fb;
        int stride, format;
        u32 now, v;

        sceDisplayWaitVblankStart();
        v = pops_word(GP_DISPLAY_START);
        if (v != start) {
            start = v;
            flips++;
        }
        now = sceKernelGetSystemTimeLow();
        if (now - t0 >= 1000000) {
            u32 cycles = pops_word(GP_VBLANK_CYCLES), idle = idle_us();
            hud_compute(&stats, now - t0, flips, cycles - cycles0, idle - idle0);
            hud_format(text, &stats);
            t0 = now;
            cycles0 = cycles;
            idle0 = idle;
            flips = 0;
        }
        /* After POPS's picture of this frame, before the scan reaches the box. */
        sceKernelDelayThread(DRAW_DELAY_US);
        if (sceDisplayGetFrameBuf(&fb, &stride, &format, PSP_DISPLAY_SETBUF_IMMEDIATE) >= 0 && fb &&
            stride >= 480 && format >= 0 && format <= 3)
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
    previous = sctrlHENSetStartModuleHandler(on_module_start);
    thid = sceKernelCreateThread("popshud", hud_thread, 0x10, 0x1000, 0, NULL);
    if (thid >= 0)
        sceKernelStartThread(thid, 0, NULL);
    return 0;
}
