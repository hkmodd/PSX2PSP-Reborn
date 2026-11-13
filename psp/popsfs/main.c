/*
 * popsfs: full-screen PS1 picture for POPS on ARK-4 / ARK-5.
 *
 * POPS (fw 6.61) samples a fixed source height from VRAM: 240 lines on NTSC, 288 on PAL.
 * It ignores the vertical display range the game programs with GP1(07h). A game that shows
 * fewer lines (a 256-line PAL game, for example) is therefore scaled as if it had 288 lines,
 * and the screen keeps an empty band at the bottom.
 *
 * This kernel plugin rewrites one block of the display routine of the "pops" module as it
 * starts. The source height becomes Y2 - Y1 of GP1(07h), clamped to [64, stock], and the
 * vertical scale becomes ceil(H * 256 / h), where H is the height of the current POPS screen
 * mode. The visible lines then fill the screen mode's box exactly in every mode.
 *
 * The block is matched word for word (two code shapes cover pops_01g..09g). If no shape
 * matches, nothing is written and POPS runs as stock.
 */
#include <pspsdk.h>
#include <pspkernel.h>

PSP_MODULE_INFO("PSX2PSP_PopsFullscreen", 0x1007, 1, 0);

/* Only the fields read here; offsets as in ARK's common/include/module2.h. */
typedef struct SceModule2 {
    struct SceModule2 *next; /* 0x00 */
    unsigned short attribute; /* 0x04 */
    unsigned char version[2]; /* 0x06 */
    char modname[27]; /* 0x08 */
    char terminal; /* 0x23 */
    unsigned int pad[18]; /* 0x24 */
    unsigned int text_addr; /* 0x6C */
    unsigned int text_size; /* 0x70 */
} SceModule2;

typedef int (*STMOD_HANDLER)(SceModule2 *);
STMOD_HANDLER sctrlHENSetStartModuleHandler(STMOD_HANDLER handler);
void sctrlFlushCache(void);

#define WORDS 21

/* From "addu t5,s5,v0" through the dead GE store that follows the always-taken branch. */
static const u32 shape_a[WORDS] = { /* pops_01g, pops_02g, pops_03g */
    0x02A26821, 0x81B90004, 0x82B20003, 0x24130010, 0x32380800, 0xAFB30018, 0x2734012C,
    0x241300F0, 0x13000004, 0xAFB20014, 0x240C0019, 0xAFAC0018, 0x24130120, 0x02134804,
    0x02C99821, 0x2A6F0200, 0x15E0013B, 0x3C18C700, 0x37050001, 0xAEE50000, 0x26F70004,
};
static const u32 shape_b[WORDS] = { /* pops_04g, pops_07g, pops_09g */
    0x02A26821, 0x81B90004, 0x82B30003, 0x24180010, 0x32510800, 0xAFB30014, 0x2734012C,
    0x241300F0, 0x12200004, 0xAFB80018, 0x240C0019, 0xAFAC0018, 0x24130120, 0x02134804,
    0x02C99821, 0x2A6F0200, 0x15E0013B, 0x3C11C700, 0x36250001, 0xAEE50000, 0x26F70004,
};

/*
 * Inputs: s5 = mode table row (x0, ytop, ybot-300, xscale, ...), v0 = PAL bit,
 * v1 = Y1, gp+0x361A = Y2. s0 = s6 = 0 here, so the stock tail of the block is a no-op.
 * Outputs: sp+20 = xscale, sp+24 = reference line, s3 = source height, s4 = scale (x256).
 * Falls through to "lw t5,24(sp)". Uses t1, t7, t8, t9 and HI/LO only.
 */
static const u32 patch[WORDS] = {
    0x82B80003, /* lb    t8,3(s5)        xscale                      */
    0x82B90002, /* lb    t9,2(s5)        ybot - 300                  */
    0x82AF0001, /* lb    t7,1(s5)        ytop                        */
    0xAFB80014, /* sw    t8,20(sp)                                   */
    0x2739012C, /* addiu t9,t9,300       ybot                        */
    0x032FC823, /* subu  t9,t9,t7        H = ybot - ytop             */
    0x306FFFFF, /* andi  t7,v1,0xffff    Y1                          */
    0x9798361A, /* lhu   t8,0x361a(gp)   Y2                          */
    0xAFAF0018, /* sw    t7,24(sp)       reference line = Y1         */
    0x030FC023, /* subu  t8,t8,t7        h = Y2 - Y1                 */
    0x240F00F0, /* addiu t7,zero,240                                 */
    0x24090120, /* addiu t1,zero,288                                 */
    0x0122780B, /* movn  t7,t1,v0        stock height                */
    0x030FC02D, /* min   t8,t8,t7                                    */
    0x24090040, /* addiu t1,zero,64                                  */
    0x0309982C, /* max   s3,t8,t1        s3 = clamp(h, 64, stock)    */
    0x0019CA00, /* sll   t9,t9,8                                     */
    0x0333C821, /* addu  t9,t9,s3                                    */
    0x2739FFFF, /* addiu t9,t9,-1                                    */
    0x0333001A, /* div   t9,s3                                       */
    0x0000A012, /* mflo  s4              s4 = ceil(H * 256 / h)      */
};

static STMOD_HANDLER previous;

static int same(const u32 *a, const u32 *b)
{
    int i;
    for (i = 0; i < WORDS; i++)
        if (a[i] != b[i])
            return 0;
    return 1;
}

static int is_pops(const char *name)
{
    return name[0] == 'p' && name[1] == 'o' && name[2] == 'p' && name[3] == 's' && name[4] == 0;
}

static void patch_pops(SceModule2 *mod)
{
    u32 *text = (u32 *)mod->text_addr;
    u32 n = mod->text_size / 4, i, k;

    for (i = 0; i + WORDS <= n; i++) {
        if (text[i] != shape_a[0] || !(same(text + i, shape_a) || same(text + i, shape_b)))
            continue;
        for (k = 0; k < WORDS; k++)
            text[i + k] = patch[k];
        sctrlFlushCache();
        return;
    }
}

static int on_module_start(SceModule2 *mod)
{
    if (is_pops(mod->modname))
        patch_pops(mod);
    return previous ? previous(mod) : 0;
}

int module_start(SceSize args, void *argp)
{
    (void)args;
    (void)argp;
    previous = sctrlHENSetStartModuleHandler(on_module_start);
    return 0;
}
