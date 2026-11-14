/*
 * psxrt: the native runtime of PSX2PSP (recomp backend), slice S1.
 *
 * S1 proves, in user mode on a real PSP, what docs/recomp/S0.md chose for the memory layout:
 *   1. PSP RAM 0x09800000-0x09C00000 (4 MB: 2 MB PS1 RAM, 2 MB dispatch table) can be
 *      reserved, first with sceKernelAllocPartitionMemory(PSP_SMEM_Addr) on the user
 *      partition, then, if that is refused, by filling the free memory below it;
 *   2. the Allegrex scratchpad 0x00010000-0x00013FFF can be read and written;
 *   3. the converter's DATA.PSAR header reaches the runtime (the EBOOT path is argv[0]).
 * No game code runs yet. Everything is printed on screen; HOME exits.
 *
 * The scratchpad test runs last: if user mode may not touch it, the access faults and the
 * PSP stops with the "testing" line as the last one on screen.
 */
#include <pspkernel.h>
#include <pspdebug.h>
#include <pspdisplay.h>
#include <pspsysmem.h>
#include <psputils.h>
#include <string.h>

PSP_MODULE_INFO("PSX2PSP_psxrt", PSP_MODULE_USER, 0, 1);
PSP_MAIN_THREAD_ATTR(PSP_THREAD_ATTR_USER | PSP_THREAD_ATTR_VFPU);
/* A small heap, taken from the bottom of the user partition: the default one could grow
 * over 0x09800000. */
PSP_HEAP_SIZE_KB(256);

#define printf pspDebugScreenPrintf

#define PS1_BASE 0x09800000u
#define PS1_SIZE 0x00400000u /* PS1 RAM (2 MB) + dispatch table (2 MB) */
#define SPAD_BASE 0x00010000u
#define SPAD_SIZE 0x00004000u
#define USER_PARTITION 2

/* DATA.PSAR of a native EBOOT, written by psx2psp-engine (convert_native). */
#define PSAR_MAGIC "PSXRTDAT"
#define PSAR_VERSION 1
typedef struct {
    char magic[8];
    u32 version;
    u32 header_len;
    char disc_id[16];
    char title[128];
} PsarHeader;

#define GREEN 0xFF40FF40
#define RED 0xFF4040FF
#define WHITE 0xFFFFFFFF
#define GREY 0xFFB0B0B0

static int failures;

/* Ends a result line with `ok_text` in green or `bad_text` in red. */
static void mark(int ok, const char *ok_text, const char *bad_text)
{
    pspDebugScreenSetTextColor(ok ? GREEN : RED);
    printf("%s", ok ? ok_text : bad_text);
    pspDebugScreenSetTextColor(WHITE);
    printf("\n");
}

/* As mark(), and a red result counts as a failure of the slice. */
static void verdict(int ok, const char *ok_text, const char *bad_text)
{
    mark(ok, ok_text, bad_text);
    if (!ok)
        failures++;
}

static int exit_callback(int a, int b, void *c)
{
    (void)a, (void)b, (void)c;
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

static void setup_callbacks(void)
{
    int th = sceKernelCreateThread("psxrt callbacks", callback_thread, 0x11, 0x1000, PSP_THREAD_ATTR_USER, NULL);
    if (th >= 0)
        sceKernelStartThread(th, 0, NULL);
}

static void show_build(int argc, char **argv)
{
    unsigned fw = (unsigned)sceKernelDevkitVersion();
    u32 here = (u32)&show_build;
    printf("PSX2PSP native runtime (psxrt) - slice S1\n");
    printf("build %s %s, gcc %s\n", __DATE__, __TIME__, __VERSION__);
    printf("firmware %x.%02x, code at 0x%08X (%s mode)\n", fw >> 24, (fw >> 16) & 0xFF, (unsigned)here,
           here < 0x80000000u ? "user" : "kernel");
    printf("EBOOT %s\n", argc > 0 ? argv[0] : "(no argv[0])");
}


/* Reads the converter's header at the start of DATA.PSAR of our own EBOOT. */
static void show_psar(int argc, char **argv)
{
    printf("DATA.PSAR: ");
    if (argc < 1) {
        verdict(0, "", "no EBOOT path");
        return;
    }
    SceUID fd = sceIoOpen(argv[0], PSP_O_RDONLY, 0);
    if (fd < 0) {
        printf("open error 0x%08X ", (unsigned)fd);
        verdict(0, "", "FAIL");
        return;
    }
    u32 pbp[10];
    PsarHeader h;
    int ok = sceIoRead(fd, pbp, sizeof pbp) == (int)sizeof pbp && pbp[0] == 0x50425000u &&
             sceIoLseek32(fd, (int)pbp[9], PSP_SEEK_SET) == (int)pbp[9] &&
             sceIoRead(fd, &h, sizeof h) == (int)sizeof h && memcmp(h.magic, PSAR_MAGIC, 8) == 0 &&
             h.version == PSAR_VERSION;
    sceIoClose(fd);
    if (ok) {
        h.disc_id[sizeof h.disc_id - 1] = 0;
        h.title[sizeof h.title - 1] = 0;
        printf("%s \"%s\" ", h.disc_id, h.title);
    }
    verdict(ok, "OK", "no psxrt header");
}

static int block_at(SceUID uid)
{
    return uid >= 0 && (u32)sceKernelGetBlockHeadAddr(uid) == PS1_BASE;
}

static void show_block(SceUID uid)
{
    if (uid < 0)
        printf("error 0x%08X ", (unsigned)uid);
    else
        printf("block at 0x%08X ", (unsigned)(u32)sceKernelGetBlockHeadAddr(uid));
}

/*
 * Reserves PS1_BASE..+PS1_SIZE. First as the S0 plan says (PSP_SMEM_Addr); if the firmware
 * refuses it or places the block elsewhere, by taking the free memory below PS1_BASE with a
 * filler block and then allocating from the bottom. The block stays allocated until exit.
 */
static void reserve_ps1_ram(void)
{
    printf("\nPS1 RAM + dispatch 0x%08X-0x%08X (4 MB):\n", PS1_BASE, PS1_BASE + PS1_SIZE - 1);
    printf("  free user memory: total %u KB, largest %u KB\n", (unsigned)(sceKernelTotalFreeMemSize() >> 10),
           (unsigned)(sceKernelMaxFreeMemSize() >> 10));

    SceUID uid = sceKernelAllocPartitionMemory(USER_PARTITION, "psxrt ps1", PSP_SMEM_Addr, PS1_SIZE, (void *)PS1_BASE);
    printf("  Alloc(2, Addr): ");
    show_block(uid);
    if (block_at(uid)) {
        verdict(1, "OK", "");
        return;
    }
    mark(0, "", "refused");
    if (uid >= 0)
        sceKernelFreePartitionMemory(uid);

    /* Fallback: fill the free memory below PS1_BASE, then allocate from the bottom. */
    SceUID probe = sceKernelAllocPartitionMemory(USER_PARTITION, "psxrt probe", PSP_SMEM_Low, 0x100, NULL);
    u32 low = probe >= 0 ? (u32)sceKernelGetBlockHeadAddr(probe) : 0;
    if (probe >= 0)
        sceKernelFreePartitionMemory(probe);
    SceUID filler = -1;
    if (low && low < PS1_BASE)
        filler = sceKernelAllocPartitionMemory(USER_PARTITION, "psxrt filler", PSP_SMEM_Low, PS1_BASE - low, NULL);
    uid = sceKernelAllocPartitionMemory(USER_PARTITION, "psxrt ps1", PSP_SMEM_Low, PS1_SIZE, NULL);
    if (filler >= 0)
        sceKernelFreePartitionMemory(filler);
    printf("  filler 0x%08X- + Alloc(2, Low): ", (unsigned)low);
    show_block(uid);
    verdict(block_at(uid), "OK", "FAIL");
}

static int spad_word(u32 addr)
{
    volatile u32 *p = (volatile u32 *)addr;
    u32 want = 0xA5C3F00Fu ^ addr;
    u32 saved = *p;
    *p = want;
    sceKernelDcacheWritebackInvalidateRange((const void *)addr, 64);
    u32 back = *p;
    *p = saved;
    sceKernelDcacheWritebackInvalidateRange((const void *)addr, 64);
    printf("  0x%08X: wrote 0x%08X, read 0x%08X ", (unsigned)addr, (unsigned)want, (unsigned)back);
    return back == want;
}

static void test_scratchpad(void)
{
    printf("\nScratchpad 0x%08X-0x%08X (16 KB):\n", SPAD_BASE, SPAD_BASE + SPAD_SIZE - 1);
    pspDebugScreenSetTextColor(GREY);
    printf("  testing (if the screen stops here: no user-mode access)\n");
    pspDebugScreenSetTextColor(WHITE);
    int ok = spad_word(SPAD_BASE);
    verdict(ok, "OK", "FAIL");
    ok = spad_word(SPAD_BASE + SPAD_SIZE - 4);
    verdict(ok, "OK", "FAIL");
}

int main(int argc, char **argv)
{
    pspDebugScreenInit();
    setup_callbacks();
    show_build(argc, argv);
    show_psar(argc, argv);
    reserve_ps1_ram();
    test_scratchpad();
    printf("\n");
    verdict(failures == 0, "S1 PASS", "S1 FAIL");
    printf("Press HOME to exit.\n");
    for (;;)
        sceDisplayWaitVblankStart();
    return 0;
}
