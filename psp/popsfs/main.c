/*
 * popsfs: full-screen PS1 picture for POPS on ARK-4 / ARK-5.
 *
 * POPS (fw 6.61) samples a fixed source height from VRAM: 240 lines on NTSC, 288 on PAL.
 * It ignores the vertical display range the game programs with GP1(07h). A game that shows
 * fewer lines (a 256-line PAL game, for example) is therefore scaled as if it had 288 lines,
 * and the screen keeps an empty band at the bottom. POPS also places the picture by
 * (Y1 - 16) or (Y1 - 25) lines, so a game whose range starts lower is shifted down.
 *
 * This kernel plugin rewrites part of the display routine of the "pops" module as it starts:
 *   - the source height h becomes Y2 - Y1 of GP1(07h), clamped to [64, stock];
 *   - the reference line becomes Y1, so the first displayed line lands on the top of the box;
 *   - the vertical scale becomes floor(H * 256 / h) + 1, where H is the height of the current
 *     POPS screen mode, so the scissor never cuts the last line.
 * The stock code between the two rewritten parts is kept. It adds the display start row to
 * the texture base (or sets the texture to repeat when the picture crosses VRAM row 512) and
 * doubles h for interlaced games. Version 1 replaced it, which broke every game that shows
 * its picture from a row other than 0.
 *
 * The 31-word block is matched word for word (two code shapes cover pops_01g..09g). If no
 * shape matches, nothing is written and POPS runs as stock.
 */
#include <pspsdk.h>
#include <pspkernel.h>

PSP_MODULE_INFO("PSX2PSP_PopsFullscreen", 0x1007, 2, 0);

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

#define WORDS 31

/*
 * From "addu t5,s5,v0" (geometry setup) through the delay slot of "beq a0,zero" that picks
 * the LCD path. Words 0-12: stock height, reference line, scale. Words 13-20: stock tail
 * (texture rebase or wrap). Words 21-30: destination top and bottom.
 */
static const u32 shape_a[WORDS] = { /* pops_01g, pops_02g, pops_03g */
    0x02A26821, 0x81B90004, 0x82B20003, 0x24130010, 0x32380800, 0xAFB30018, 0x2734012C,
    0x241300F0, 0x13000004, 0xAFB20014, 0x240C0019, 0xAFAC0018, 0x24130120, 0x02134804,
    0x02C99821, 0x2A6F0200, 0x15E0013B, 0x3C18C700, 0x37050001, 0xAEE50000, 0x26F70004,
    0x8FAD0018, 0x3072FFFF, 0x83843664, 0x024D8823, 0x02340018, 0x82A30001, 0x0000C812,
    0x00199203, 0x1080012A, 0x00728821,
};
static const u32 shape_b[WORDS] = { /* pops_04g, pops_07g, pops_09g */
    0x02A26821, 0x81B90004, 0x82B30003, 0x24180010, 0x32510800, 0xAFB30014, 0x2734012C,
    0x241300F0, 0x12200004, 0xAFB80018, 0x240C0019, 0xAFAC0018, 0x24130120, 0x02134804,
    0x02C99821, 0x2A6F0200, 0x15E0013B, 0x3C11C700, 0x36250001, 0xAEE50000, 0x26F70004,
    0x8FAD0018, 0x3072FFFF, 0x83843664, 0x024DC823, 0x03340018, 0x82A30001, 0x0000C012,
    0x00189203, 0x1080012A, 0x00728821,
};

/*
 * 0 = keep the stock word (no word written here is a nop).
 * Inputs: s5 = mode table row (x0, ytop, ybot - 300, xscale, ...), v0 = PAL bit,
 * v1 = Y1 (zero-extended), gp+0x361A = Y2. Kept live across the stock tail: t4 = h and
 * t9 = ybot - 300 (the tail and its rebase branch use t1, t7, t8/s1, a1, a2, a3, s3, s6).
 * Outputs: sp+20 = xscale, sp+24 = Y1, s3 = h, s1 = ytop, s2 = 0, s4 = scale (x256).
 */
static const u32 patch[WORDS] = {
    0x82B90003, /* lb    t9,3(s5)        xscale                        */
    0xAFB90014, /* sw    t9,20(sp)                                     */
    0xAFA30018, /* sw    v1,24(sp)       reference line = Y1           */
    0x9798361A, /* lhu   t8,0x361a(gp)   Y2                            */
    0x0303C023, /* subu  t8,t8,v1        Y2 - Y1                       */
    0x241300F0, /* addiu s3,zero,240                                   */
    0x24090120, /* addiu t1,zero,288                                   */
    0x0122980B, /* movn  s3,t1,v0        stock height                  */
    0x0313C02D, /* min   t8,t8,s3                                      */
    0x24090040, /* addiu t1,zero,64                                    */
    0x0309982C, /* max   s3,t8,t1        h = clamp(Y2 - Y1, 64, stock) */
    0x02606021, /* addu  t4,s3,zero      keep h                        */
    0x82B90002, /* lb    t9,2(s5)        ybot - 300                    */
    0, 0, 0, 0, 0, 0, 0, 0, /* stock tail: s3 = s6 + (h << s0), rebase or wrap */
    0x83843664, /* lb    a0,0x3664(gp)   TV output flag                */
    0x82B10001, /* lb    s1,1(s5)        top = ytop, no offset         */
    0x0331C823, /* subu  t9,t9,s1                                      */
    0x2739012C, /* addiu t9,t9,300       H                             */
    0x0019CA00, /* sll   t9,t9,8                                       */
    0x032CC821, /* addu  t9,t9,t4                                      */
    0x032C001A, /* div   t9,t4                                         */
    0x00009021, /* addu  s2,zero,zero    bottom offset 0               */
    0,          /* stock: beq a0,zero,<LCD path>                       */
    0x0000A012, /* mflo  s4              s4 = H * 256 / h + 1          */
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
            if (patch[k])
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
