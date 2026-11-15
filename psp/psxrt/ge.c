/*
 * psxrt: the GE renderer of the PS1 GPU, slices S4a-S4b of docs/recomp/S4.md.
 *
 * The core (core/gpu.c) decodes the commands and offers each primitive here first. Triangles,
 * rectangles, fills and copies are drawn by the GE into its copy of PS1 VRAM (PSP VRAM 0x000000,
 * 5551, stride 1024); lines, overlapping copies, 15-bit textures and texture windows stay in
 * software on RAM's copy. The two copies are kept in step per 64x32 tile: `newer` marks tiles the GE drew since RAM
 * last saw them, `read` brings all of VRAM back (one block transfer) before the CPU reads one of
 * those, `wrote` sends the CPU's pixels up as textured sprites. Textures come from RAM's copy:
 * 4/8-bit pages are copied into a cache of T4/T8 textures, CLUTs are converted into a pool.
 * The list is built in a GU_SEND context and enqueued when it opens; the GE runs behind it up to
 * a stall address moved every STALL_STEP bytes, not after every draw as GU_DIRECT would.
 */
#include <pspge.h>
#include <pspgu.h>
#include <pspkernel.h>
#include <pspsysmem.h>
#include <psputils.h>
#include <stdio.h>
#include <string.h>

#include "ge.h"

#define LIST_BYTES (512 * 1024)
#define LIST_SLACK (32 * 1024) /* room a primitive, a readback or the host may need */
#define TILE_W 64
#define TILE_H 32
#define TILES_X (VRAM_W / TILE_W)
#define TILES_Y (VRAM_H / TILE_H)
#define VRAM_PTR ((void *)(0x04000000u + GE_PS1_VRAM))

/* through mode: colour, then position (pixel corners: PS1 coordinates + 0.5) */
typedef struct {
    u32 c;
    float x, y, z;
} GeVtx;
#define GE_VTX (GU_COLOR_8888 | GU_VERTEX_32BITF | GU_TRANSFORM_2D)

/* through mode, textured: texel, colour, position */
typedef struct {
    float u, v;
    u32 c;
    float x, y, z;
} TexVtx;
#define TEX_VTX (GU_TEXTURE_32BITF | GU_COLOR_8888 | GU_VERTEX_32BITF | GU_TRANSFORM_2D)

typedef struct {
    u32 tris, ttris, rects, trects, fills, lines, line_runs, sw_tex15, sw_texwin, sw_copy, pages, cluts, clut_tmp;
    u32 readbacks, read_tiles, read_us, uploads, upload_px, flushes;
    u32 ge_copy, null_copy, copy_px, copy_from_ge; /* copies: on the GE, onto themselves, pixels, read from the GE */
    u32 bind_tex, bind_clut, kicks, rd_checks;     /* textures and CLUTs sent, stall moves, readback checks */
    u32 fast, fast_setups, fast_hits;              /* polygons through the fast lane; its state changes, found cached */
    u32 slow_tex, slow_big, flat, clipped;        /* fast lane: turned away for texture or size; no area; cut at the origin */
    u32 ring_wb_us;                               /* writing the cached ring back */
    u32 kick_us, setup_us;                        /* stall moves, all timed; fl_setup, 1 in 16 timed */
} GeStats;

/* A 4/8-bit texture page as a 256x256 T4/T8 texture. */
#define PAGE_SLOTS 32
#define PAGE_BYTES (256 * 256)
typedef struct {
    u32 key, stamp; /* key 0: empty; stamp: the page's tile generations, summed */
    u32 used, age;  /* used: the list (ge.syncs) that last drew with it */
    u32 epoch;      /* ge.epoch when the stamp was last found current */
    u32 ver;        /* bumped whenever data is written: what was built on it is stale */
    u8 *data;
} Page;

/* Games switch CLUTs per model, and models are drawn interleaved in depth order. A converted CLUT
 * is kept across lists in a slot of the pool, two ways per set, as long as its tiles' stamp holds
 * (S19: the device rebuilt every CLUT once per list, ~600 a second). */
#define CLUT_BITS 7
#define CLUT_SLOTS (1 << CLUT_BITS)
typedef struct {
    u32 key, stamp; /* key 0: empty; stamp: the CLUT's tile generations, summed */
    u32 used, age;  /* used: the list (ge.syncs) that last loaded it */
    u32 epoch;      /* ge.epoch when the stamp was last found current */
    u32 ver;        /* bumped whenever the slot is written */
    int flags;      /* clut()'s */
} ClutSlot;
static u32 __attribute__((aligned(64))) clut_pool[CLUT_SLOTS][256];

static unsigned int __attribute__((aligned(64))) list[LIST_BYTES / 4];
static int list_open, state_valid;

static struct {
    Psx *psx;
    GpuRenderer r;
    int compare;
    u16 *cmp;                  /* compare mode: the GE's copy, read back */
    u16 newer[TILES_Y];        /* per tile row, a bit per tile column: the GE's copy is newer */
    u32 nver;                  /* bumped whenever `newer` gains a bit */
    s32 mk_x1, mk_y1, mk_x2, mk_y2; /* tiles all marked newer (inclusive pixels); empty after a readback */
    u32 rk_tex, rk_clut, rk_ver;    /* prim_begin: this page and CLUT had no newer tile at nver rk_ver */
    u32 gen[TILES_Y][TILES_X]; /* bumped whenever RAM's copy of the tile changes */
    /* GE state as last sent */
    int sc_x1, sc_y1, sc_x2, sc_y2;
    int st_check, st_set, blend, dither; /* -2: unknown */
    const void *tex_sent, *clut_sent;    /* NULL: unknown */
    int texfn;                           /* 0 modulate, 1 replace, -1 unknown */
    int tex_on;                          /* texturing and the alpha test enabled */
    int a_func, a_ref;                   /* alpha test; a_func -1: unknown */
    u32 fill_color;
    /* texture cache */
    Page pages[PAGE_SLOTS];
    int npages;
    u32 syncs, ages; /* syncs: counts the lists run */
    u32 epoch;       /* bumped whenever any tile's generation changes */
    Page *last_page; /* the page last used, current as of last_epoch */
    u32 last_epoch;
    ClutSlot cluts[CLUT_SLOTS]; /* by a hash of their key; the data in clut_pool */
    int clut_slot;              /* the slot clut() last returned; -1: a CLUT in the list */
    GeStats s;
} ge;

/* The polygon fast lane (rd_poly): while a polygon command has the key of the one before (its
 * command bits, texpage, CLUT, mask bits and drawing area, with no tile changed or newly drawn
 * under its texture), the GE is still in that polygon's state, and the polygon is its vertices in
 * a ring plus a few words of list. Anything else that writes the list clears `valid` (begin). */
#define RING_MAX (512 * 1024)
static struct {
    int valid;
    u32 k0, k1, a0, a1; /* command bits, texpage and mask bits; CLUT; drawing area */
    u32 epoch, nver;    /* ge.epoch and, for a textured key, ge.nver when it was set */
    int flags;          /* prim_begin's for the key; 0: it draws nothing */
    int two;            /* two passes, each after its words of state */
    s32 x1, y1, x2, y2; /* the drawing area within VRAM */
    u32 base;           /* BASE as the lane last sent it; ~0: unknown */
    int n1, n2;
    const u32 *pass1, *pass2; /* the key's cache entry's */
    int dirty;          /* a cached block set the GE's state: ge's copy of it is stale */
    u8 *ring;           /* where vertices are written: ring_c or ring_u; NULL: no lane */
    u8 *ring_c, *ring_u; /* the ring through the cache, and through the uncached mirror */
    u32 ring_bytes, ring_off;
    u32 ring_wb;        /* cached: bytes written back, the GE may read them */
    int cached, want;   /* the ring is written through the cache; the mode after the next sync */
} fl;
/* 1: the mode changes every second, and the "-- ge:" line says which ran. The PSP wrote 64 KB of
 * vertices in 326 us uncached, 700 us cached and written back (S16b): the ring stays uncached. */
static int ring_ab = 0;

/* The PS1 dither matrix, row = y & 3, column = x & 3; the GE adds it to 8-bit colours before
 * they are cut to 5 bits, as the PS1 does. */
static const ScePspIMatrix4 ps1_dither = {{-4, 0, -3, 1}, {2, -2, 3, -1}, {-3, 1, -4, 0}, {3, -1, 2, -2}};

#define DONE (ge.compare ? GPU_BOTH : GPU_DRAWN)

/* The list runs while it is written, but its stall address moves every STALL_STEP bytes and at
 * the end, not after every draw as GU_DIRECT does (one system call per draw): a GU_SEND list,
 * queued here with its stall at the start. pspgu writes it through the uncached mirror.
 * A frame of level 1 is ~20 KB of list: with a 16 KB step the real GE got its work at present()
 * and drew it while the CPU waited (6-10 ms a frame). 1 KB is ~60 polygons, a few hundred
 * system calls a second. The S18c device run alternated 1 KB and 8 KB every second: the moves
 * cost under 1 ms a second either way, and a polygon ~0.5 us less with 8 KB. */
#define STALL_STEP (8 * 1024)
static u32 stall_step = STALL_STEP;
#define LIST_UNCACHED ((u8 *)((u32)list | 0x40000000u))
static int list_id;
static u32 list_stall; /* bytes the GE may run */

void ge_open(void)
{
    if (!list_open) {
        sceGuStart(GU_SEND, list);
        list_id = sceGeListEnQueue(LIST_UNCACHED, LIST_UNCACHED, -1, NULL);
        list_stall = 0;
        list_open = 1;
    }
    state_valid = 0;
    fl.valid = 0;
}

/* pspgu's display list (guInternal.h): both pointers through the uncached mirror. */
typedef struct {
    unsigned int *start, *current;
    int parent_context;
} GuListHead;
extern GuListHead *gu_list;

/* Before the GE may run more of the list: the vertices written through the cache go to RAM. */
static void ring_flush(void)
{
    if (fl.cached && fl.ring_off > fl.ring_wb) {
        u32 a = fl.ring_wb & ~63u, t = sceKernelGetSystemTimeLow();
        sceKernelDcacheWritebackRange(fl.ring_c + a, fl.ring_off - a);
        ge.s.ring_wb_us += sceKernelGetSystemTimeLow() - t;
        fl.ring_wb = fl.ring_off;
    }
}

/* Moves the stall address, timed: on the device the system call's cost is not known. */
static void stall_to(u32 n)
{
    u32 t = sceKernelGetSystemTimeLow();
    list_stall = n;
    sceGeListUpdateStallAddr(list_id, LIST_UNCACHED + n);
    ge.s.kick_us += sceKernelGetSystemTimeLow() - t;
    ge.s.kicks++;
}

/* Lets the GE run what is written so far, once STALL_STEP more bytes are there. */
static void ge_kick(void)
{
    u32 n = (u32)((u8 *)gu_list->current - (u8 *)gu_list->start); /* sceGuCheckList, inline */
    if (n - list_stall >= stall_step) {
        ring_flush();
        stall_to(n);
    }
}

void ge_run(void)
{
    if (!list_open)
        return;
    u32 n = (u32)((u8 *)gu_list->current - (u8 *)gu_list->start);
    if (n != list_stall) {
        ring_flush();
        stall_to(n);
    }
}

void ge_sync(void)
{
    if (list_open) {
        u32 n = (u32)sceGuFinish();
        ring_flush();
        sceGeListUpdateStallAddr(list_id, LIST_UNCACHED + n);
        sceGuSync(0, 0);
        list_open = 0;
        ge.syncs++;
    }
    state_valid = 0;
    fl.valid = 0, fl.ring_off = fl.ring_wb = 0; /* the GE is done with the ring */
    if (fl.want != fl.cached && fl.ring_c) {
        /* no line of the ring stays in the cache: one dirty from the cached mode would be written
         * back over uncached writes, one clean but stale would be written back with them */
        sceKernelDcacheWritebackInvalidateRange(fl.ring_c, fl.ring_bytes);
        fl.cached = fl.want, fl.ring = fl.cached ? fl.ring_c : fl.ring_u;
    }
}

/* The state every renderer list starts from; the rest is sent as primitives need it. */
static void base_state(void)
{
    sceGuDrawBufferList(GU_PSM_5551, (void *)GE_PS1_VRAM, VRAM_W);
    sceGuSendCommandi(0x15, 0); /* REGION1/REGION2: the whole 1024x512, so the oracle sizes it so */
    sceGuSendCommandi(0x16, (VRAM_H - 1) << 10 | (VRAM_W - 1));
    sceGuOffset(2048 - VRAM_W / 2, 2048 - VRAM_H / 2);
    sceGuViewport(2048, 2048, VRAM_W, VRAM_H);
    sceGuDisable(GU_TEXTURE_2D);
    sceGuDisable(GU_DEPTH_TEST);
    sceGuDepthMask(GU_TRUE);
    sceGuDisable(GU_ALPHA_TEST);
    sceGuDisable(GU_CULL_FACE);
    sceGuDisable(GU_CLIP_PLANES);
    sceGuEnable(GU_SCISSOR_TEST);
    sceGuEnable(GU_STENCIL_TEST); /* bit 15 = stencil: the mask bit is written and tested by it */
    sceGuPixelMask(0);
    sceGuShadeModel(GU_SMOOTH);
    sceGuSetDither(&ps1_dither);
    sceGuTexWrap(GU_REPEAT, GU_REPEAT);
    sceGuTexFilter(GU_NEAREST, GU_NEAREST);
    ge.sc_x1 = -1;
    ge.st_check = ge.st_set = ge.blend = ge.dither = -2;
    ge.tex_sent = ge.clut_sent = NULL;
    ge.texfn = -1;
    ge.tex_on = 0, ge.a_func = -1;
    state_valid = 1;
}

/* ge's copy of the GE state, all unknown: what follows sends all of it. */
static void fl_forget(void)
{
    ge.sc_x1 = -1;
    ge.st_check = ge.st_set = ge.blend = ge.dither = -2;
    ge.tex_sent = ge.clut_sent = NULL;
    ge.texfn = -1, ge.tex_on = -1, ge.a_func = -1;
}

/* Before renderer commands: a list with room, in the renderer's state. */
static void begin(void)
{
    fl.valid = 0;
    if (fl.dirty)
        fl_forget(), fl.dirty = 0;
    if (list_open) {
        if (sceGuCheckList() > LIST_BYTES - LIST_SLACK) {
            ge_sync();
            ge.s.flushes++;
        } else
            ge_kick();
    }
    if (!list_open)
        ge_open();
    if (!state_valid)
        base_state();
}

static void scissor(s32 x1, s32 y1, s32 x2, s32 y2)
{
    if (x1 == ge.sc_x1 && y1 == ge.sc_y1 && x2 == ge.sc_x2 && y2 == ge.sc_y2)
        return;
    ge.sc_x1 = x1, ge.sc_y1 = y1, ge.sc_x2 = x2, ge.sc_y2 = y2;
    sceGuScissor(x1, y1, x2 - x1 + 1, y2 - y1 + 1);
}

/* check: draw only where bit 15 is 0 (stencil != 0xFF); set: the written bit 15. */
static void stencil(int check, int set)
{
    if (check != ge.st_check) {
        ge.st_check = check;
        sceGuStencilFunc(check ? GU_NOTEQUAL : GU_ALWAYS, 0xFF, 0xFF);
    }
    if (set != ge.st_set) {
        ge.st_set = set;
        sceGuStencilOp(GU_KEEP, GU_KEEP, set ? GU_REPLACE : GU_ZERO);
    }
}

/* PS1 semi-transparency mode, or -1 for none. The GE computes ((2c+1)(2f+1)) >> 10 per channel. */
static void blend(int mode)
{
    static const int op[4] = {GU_ADD, GU_ADD, GU_REVERSE_SUBTRACT, GU_ADD};
    static const u32 src[4] = {0x7D7D7D, 0xFFFFFF, 0xFFFFFF, 0x3A3A3A};
    static const u32 dst[4] = {0x7D7D7D, 0xFFFFFF, 0xFFFFFF, 0xFFFFFF};
    if (mode == ge.blend)
        return;
    if (mode < 0)
        sceGuDisable(GU_BLEND);
    else {
        if (ge.blend < 0)
            sceGuEnable(GU_BLEND);
        sceGuBlendFunc(op[mode], GU_FIX, GU_FIX, src[mode], dst[mode]);
    }
    ge.blend = mode;
}

static void tex_mode(int on)
{
    if (on == ge.tex_on)
        return;
    ge.tex_on = on;
    if (on) {
        sceGuEnable(GU_TEXTURE_2D);
        sceGuEnable(GU_ALPHA_TEST);
    } else {
        sceGuDisable(GU_ALPHA_TEST);
        sceGuDisable(GU_TEXTURE_2D);
    }
}

static void alpha(int func, int ref)
{
    if (func == ge.a_func && ref == ge.a_ref)
        return;
    ge.a_func = func, ge.a_ref = ref;
    sceGuAlphaFunc(func, ref, 0xFF);
}

static void dither(int on)
{
    if (on == ge.dither)
        return;
    ge.dither = on;
    if (on)
        sceGuEnable(GU_DITHER);
    else
        sceGuDisable(GU_DITHER);
}

/* ---- tiles ---- */

/* The tiles a span of n units from v covers on an axis of `size` (wrapping), as a bit mask. */
static u16 span(s32 v, s32 n, u32 size, u32 tile)
{
    if (n >= (s32)size)
        return 0xFFFF;
    u32 v0 = (u32)v & (size - 1), v1 = v0 + (u32)n - 1; /* v1 / tile <= 31: 2u << 31 wraps to 0, all ones */
    u32 m = ((2u << (v1 / tile)) - 1) & ~((1u << (v0 / tile)) - 1);
    return (u16)(m | m >> 16);
}

static void bump(s32 x, s32 y, s32 w, s32 h);

/* A GE draw inside x1..x2, y1..y2 (inclusive, not wrapping). In compare mode the software
 * rasterizer draws there too, so RAM's copy changes. */
static void mark(s32 x1, s32 y1, s32 x2, s32 y2)
{
    if (ge.compare) {
        bump(x1, y1, x2 - x1 + 1, y2 - y1 + 1);
        return;
    }
    if (x1 >= ge.mk_x1 && y1 >= ge.mk_y1 && x2 <= ge.mk_x2 && y2 <= ge.mk_y2)
        return; /* its tiles are newer already */
    ge.mk_x1 = x1 & ~(TILE_W - 1), ge.mk_y1 = y1 & ~(TILE_H - 1);
    ge.mk_x2 = x2 | (TILE_W - 1), ge.mk_y2 = y2 | (TILE_H - 1);
    u16 c = span(x1, x2 - x1 + 1, VRAM_W, TILE_W), r = span(y1, y2 - y1 + 1, VRAM_H, TILE_H);
    for (int ty = 0; ty < TILES_Y; ty++)
        if (r >> ty & 1) {
            u16 o = ge.newer[ty];
            if ((o | c) != o)
                ge.newer[ty] = o | c, ge.nver++;
        }
}

/* No tile is newer, so none is cached as marked. */
static void newer_clear(void)
{
    memset(ge.newer, 0, sizeof ge.newer);
    ge.mk_x1 = ge.mk_y1 = 1, ge.mk_x2 = ge.mk_y2 = 0;
}

static void bump(s32 x, s32 y, s32 w, s32 h)
{
    u16 c = span(x, w, VRAM_W, TILE_W), r = span(y, h, VRAM_H, TILE_H);
    for (int ty = 0; ty < TILES_Y; ty++)
        if (r >> ty & 1)
            for (int tx = 0; tx < TILES_X; tx++)
                if (c >> tx & 1)
                    ge.gen[ty][tx]++;
    ge.epoch++;
}

/* Calls fn on the up to four pieces of a rectangle that wraps around VRAM. */
static void pieces(s32 x, s32 y, s32 w, s32 h, void (*fn)(s32 x, s32 y, s32 w, s32 h))
{
    x &= VRAM_W - 1, y &= VRAM_H - 1;
    w = w > VRAM_W ? VRAM_W : w, h = h > VRAM_H ? VRAM_H : h;
    if (w <= 0 || h <= 0)
        return;
    s32 w0 = x + w > VRAM_W ? VRAM_W - x : w, h0 = y + h > VRAM_H ? VRAM_H - y : h;
    fn(x, y, w0, h0);
    if (w0 < w)
        fn(0, y, w - w0, h0);
    if (h0 < h) {
        fn(x, 0, w0, h - h0);
        if (w0 < w)
            fn(0, 0, w - w0, h - h0);
    }
}

/* through mode, textured: texel, then position */
typedef struct {
    u16 u, v;
    s16 x, y, z;
} UpVtx;

static int pow2_log(s32 n)
{
    int l = 0;
    while ((1 << l) < n)
        l++;
    return l;
}

/* A piece of a blit (the texture starts 16-byte aligned, at most 512 texels wide). The written
 * bit 15 is the stencil's: with `set` every pixel gets it; otherwise two passes, every pixel with
 * it clear, then the pixels whose source has it set again, through the alpha test. */
static void blit_part(u16 *src, s32 sx, s32 sy, s32 dx, s32 dy, s32 w, s32 h, int check, int set)
{
    s32 x8 = sx & ~7, u0 = sx - x8;
    u16 *tex = src + sy * VRAM_W + x8;
    sceGuTexMode(GU_PSM_5551, 0, 0, 0);
    /* The oracle checks a texture in RAM for changes only when its address register changes, so
     * a second upload from the same place would draw the first one's pixels; a different address
     * first makes it look. */
    sceGuTexImage(0, 1, 1, 8, tex + 8);
    sceGuTexImage(0, 1 << pow2_log(u0 + w), 1 << pow2_log(h), VRAM_W, tex);
    sceGuTexFlush();
    UpVtx *v = (UpVtx *)sceGuGetMemory(2 * sizeof(UpVtx));
    v[0] = (UpVtx){(u16)u0, 0, (s16)dx, (s16)dy, 0};
    v[1] = (UpVtx){(u16)(u0 + w), (u16)h, (s16)(dx + w), (s16)(dy + h), 0};
    scissor(dx, dy, dx + w - 1, dy + h - 1);
    stencil(check, set);
    sceGuDrawArray(GU_SPRITES, GU_TEXTURE_16BIT | GU_VERTEX_16BIT | GU_TRANSFORM_2D, 2, 0, v);
    if (set)
        return;
    stencil(check, 1);
    sceGuEnable(GU_ALPHA_TEST);
    sceGuDrawArray(GU_SPRITES, GU_TEXTURE_16BIT | GU_VERTEX_16BIT | GU_TRANSFORM_2D, 2, 0, v);
    sceGuDisable(GU_ALPHA_TEST);
}

/* A rectangle of `src` (RAM's copy, or the GE's own) drawn exactly at dx,dy as 5551 sprites. A
 * block transfer into the framebuffer would be exact on the PSP, but the oracle turns it into a
 * lossy redraw of the pixels and does not carry one out between two places of this framebuffer;
 * a textured sprite is exact on both. */
static void blit(u16 *src, s32 sx, s32 sy, s32 dx, s32 dy, s32 w, s32 h, int check, int set)
{
    tex_mode(0);
    blend(-1);
    dither(0);
    sceGuTexFunc(GU_TFX_REPLACE, GU_TCC_RGBA);
    sceGuTexFilter(GU_NEAREST, GU_NEAREST);
    sceGuAlphaFunc(GU_GREATER, 0, 0xFF);
    sceGuEnable(GU_TEXTURE_2D);
    for (s32 cx = 0; cx < w;) {
        s32 cw = w - cx, room = 512 - ((sx + cx) & 7);
        cw = cw > room ? room : cw;
        blit_part(src, sx + cx, sy, dx + cx, dy, cw, h, check, set);
        cx += cw;
    }
    sceGuDisable(GU_TEXTURE_2D);
    ge.tex_sent = NULL, ge.texfn = -1, ge.a_func = -1;
}

static void upload(s32 x, s32 y, s32 w, s32 h)
{
    u16 *ram = ge.psx->gpu.vram;
    sceKernelDcacheWritebackRange(ram + y * VRAM_W + x, (u32)((h - 1) * VRAM_W + w) * 2);
    blit(ram, x, y, x, y, w, h, 0, 0);
    ge.s.uploads++, ge.s.upload_px += (u32)(w * h);
}

/* The CPU wrote a rectangle of RAM's copy: send it to the GE. */
static void rd_wrote(void *self, s32 x, s32 y, s32 w, s32 h)
{
    (void)self;
    if (w <= 0 || h <= 0)
        return;
    begin();
    pieces(x, y, w, h, upload);
    bump(x, y, w, h);
}

/* The CPU is about to read a rectangle of RAM's copy: when the GE drew any of its tiles, all of
 * PS1 VRAM comes back. The GE's copy is never behind RAM's (every CPU write is sent up before the
 * GE runs again), so the tiles nobody asked for come back unchanged. One whole transfer, because
 * the oracle reads back pieces of a framebuffer this large wrongly. */
static void rd_read(void *self, s32 x, s32 y, s32 w, s32 h)
{
    (void)self;
    if (w <= 0 || h <= 0)
        return;
    u16 c = span(x, w, VRAM_W, TILE_W), r = span(y, h, VRAM_H, TILE_H);
    int any = 0;
    for (int ty = 0; ty < TILES_Y && !any; ty++)
        any = (r >> ty & 1) && (ge.newer[ty] & c);
    if (!any)
        return;
    u16 *ram = ge.psx->gpu.vram;
    u32 t0 = sceKernelGetSystemTimeLow();
    begin();
    sceKernelDcacheWritebackInvalidateRange(ram, VRAM_W * VRAM_H * 2);
    sceGuCopyImage(GU_PSM_5551, 0, 0, VRAM_W, VRAM_H, VRAM_W, VRAM_PTR, 0, 0, VRAM_W, ram);
    ge_sync();
    ge.s.read_us += sceKernelGetSystemTimeLow() - t0;
    for (int ty = 0; ty < TILES_Y; ty++) {
        for (int tx = 0; tx < TILES_X; tx++)
            if (ge.newer[ty] >> tx & 1)
                ge.gen[ty][tx]++, ge.s.read_tiles++;
    }
    newer_clear();
    ge.epoch++;
    ge.s.readbacks++;
}

void ge_read(int x, int y, int w, int h) { rd_read(&ge, x, y, w, h); }

/* ---- primitives ---- */

static u32 abgr(s32 r, s32 g, s32 b) { return 0xFF000000u | (u32)b << 16 | (u32)g << 8 | (u32)r; }

/* The drawing area within VRAM (inclusive); 0 when empty. */
static int area(s32 *x1, s32 *y1, s32 *x2, s32 *y2)
{
    const Gpu *g = &ge.psx->gpu;
    *x1 = g->clip_x1, *y1 = g->clip_y1;
    *x2 = g->clip_x2 < VRAM_W - 1 ? g->clip_x2 : VRAM_W - 1;
    *y2 = g->clip_y2 < VRAM_H - 1 ? g->clip_y2 : VRAM_H - 1;
    return *x1 <= *x2 && *y1 <= *y2;
}

/* After prim_begin. Textured primitives set the stencil and blending per pass (tex_draw). */
static void prim_state(const GpuPrim *p, s32 x1, s32 y1, s32 x2, s32 y2)
{
    scissor(x1, y1, x2, y2);
    stencil(p->mask_check, p->mask_set != 0);
    blend(p->semi ? (int)p->abr : -1);
    dither(p->dither && !(p->textured && p->raw)); /* a raw texel is not dithered */
}

static void sprite(s32 x1, s32 y1, s32 x2, s32 y2, u32 c)
{
    tex_mode(0);
    GeVtx *o = sceGuGetMemory(2 * sizeof(GeVtx));
    o[0] = (GeVtx){c, (float)x1, (float)y1, 0};
    o[1] = (GeVtx){c, (float)x2, (float)y2, 0};
    sceGuDrawArray(GU_SPRITES, GE_VTX, 2, NULL, o);
}

/* Once per frame, a draw into PS1 VRAM that changes nothing: the oracle drops a framebuffer it has
 * not drawn to for a few frames, and with it what the GE drew there. The scissor is the whole of
 * VRAM: the oracle sizes the framebuffer by it, and shrinks one drawn in a small part only. */
void ge_touch(void)
{
    begin();
    sceGuPixelMask(0xFFFFFFFF);
    scissor(0, 0, VRAM_W - 1, VRAM_H - 1);
    sprite(0, 0, 1, 1, 0xFF000000u);
    sceGuPixelMask(0);
}

/* ---- textures ---- */

/* The sum of the generations of the tiles under a rectangle: it changes whenever RAM's copy of
 * any of them does. */
static u32 stamp(s32 x, s32 y, s32 w, s32 h)
{
    u16 c = span(x, w, VRAM_W, TILE_W), r = span(y, h, VRAM_H, TILE_H);
    u32 s = 0;
    for (int ty = 0; ty < TILES_Y; ty++)
        if (r >> ty & 1)
            for (int tx = 0; tx < TILES_X; tx++)
                if (c >> tx & 1)
                    s += ge.gen[ty][tx];
    return s;
}

/* What the GE can sample: 4/8-bit pages without a texture window. */
static int tex_ok(const GpuPrim *p)
{
    if (p->depth >= 2 || !ge.npages) {
        ge.s.sw_tex15++;
        return 0;
    }
    if (p->tw_and_u != 0xFF || p->tw_and_v != 0xFF || p->tw_or_u || p->tw_or_v) {
        ge.s.sw_texwin++;
        return 0;
    }
    return 1;
}

/* The page as a T4/T8 texture from the cache; RAM's copy of it is current. The least recently
 * used slot is replaced, after running the list when that may still read it. */
static const u8 *page(const GpuPrim *p)
{
    s32 hw = p->depth ? 128 : 64; /* VRAM halfwords per row */
    u32 key = 1 + (p->tpx / 64 | p->tpy / 256 << 4 | p->depth << 5);
    Page *s = ge.last_page;
    if (s && s->key == key && ge.last_epoch == ge.epoch) { /* no tile changed since it was checked */
        s->used = ge.syncs, s->age = ++ge.ages;
        return s->data;
    }
    s = NULL;
    for (int i = 0; i < ge.npages && !s; i++)
        if (ge.pages[i].key == key)
            s = &ge.pages[i];
    u32 st = s && s->epoch == ge.epoch ? s->stamp : stamp((s32)p->tpx, (s32)p->tpy, hw, 256);
    if (!s || s->stamp != st) {
        if (!s) {
            s = &ge.pages[0];
            for (int i = 1; i < ge.npages; i++)
                if (ge.pages[i].age < s->age)
                    s = &ge.pages[i];
        }
        if (s->used == ge.syncs && list_open) {
            ge_sync();
            ge.s.flushes++;
            begin();
        }
        /* rows of 128 or 256 bytes: the low nibble is the even texel on both machines */
        const u16 *vram = ge.psx->gpu.vram;
        for (s32 y = 0; y < 256; y++) {
            const u16 *row = vram + (((s32)p->tpy + y) & (VRAM_H - 1)) * VRAM_W;
            u16 *o = (u16 *)(s->data + y * hw * 2);
            s32 a = VRAM_W - (s32)p->tpx < hw ? VRAM_W - (s32)p->tpx : hw;
            memcpy(o, row + p->tpx, (size_t)a * 2);
            if (a < hw)
                memcpy(o + a, row, (size_t)(hw - a) * 2);
        }
        sceKernelDcacheWritebackRange(s->data, (u32)(256 * hw * 2));
        s->key = key, s->stamp = st, s->ver++;
        if (ge.tex_sent == s->data)
            ge.tex_sent = NULL;
        ge.s.pages++;
    }
    s->used = ge.syncs, s->age = ++ge.ages, s->epoch = ge.epoch;
    ge.last_page = s, ge.last_epoch = ge.epoch;
    return s->data;
}

/* The CLUT as 8888, its alpha saying what the texel means: 0 for 0x0000 (not drawn), 0x80 with
 * STP set, 0xFF otherwise. flags: 1 some texels without STP, 2 some with. From its pool slot while
 * the stamp holds; a slot the open list loads from is never written, the CLUT goes into the list
 * instead (ge.clut_slot -1). */
static const u32 *clut(const GpuPrim *p, int *flags)
{
    int n = p->depth ? 256 : 16;
    u32 key = 1 + (p->clut_x / 16 | p->clut_y << 6 | p->depth << 15);
    ClutSlot *e = &ge.cluts[(key * 2654435761u) >> (32 - CLUT_BITS) & ~1u], *o = e + 1;
    if (e->key != key && (o->key == key || (o->used != ge.syncs && (e->used == ge.syncs || o->age < e->age))))
        e = o;
    e->age = ++ge.ages;
    u32 st = e->key == key && e->epoch == ge.epoch ? e->stamp : stamp((s32)p->clut_x, (s32)p->clut_y, n, 1);
    if (e->key != key || e->stamp != st) {
        int tmp = e->used == ge.syncs;
        u32 *c = tmp ? (u32 *)(((u32)sceGuGetMemory(n * 4 + 16) + 15) & ~15u) : clut_pool[e - ge.cluts];
        const u16 *row = ge.psx->gpu.vram + p->clut_y * VRAM_W;
        int f = 0;
        for (int i = 0; i < n; i++) {
            u32 t = row[(p->clut_x + (u32)i) & (VRAM_W - 1)];
            u32 a = !t ? 0 : t & 0x8000 ? 0x80 : 0xFF;
            f |= !t ? 0 : t & 0x8000 ? 2 : 1;
            c[i] = a << 24 | (t >> 10 & 31) << 19 | (t >> 5 & 31) << 11 | (t & 31) << 3;
        }
        if (tmp) {
            ge.s.clut_tmp++;
            ge.clut_slot = -1;
            *flags = f;
            return c;
        }
        sceKernelDcacheWritebackRange(c, (u32)n * 4);
        if (ge.clut_sent == c)
            ge.clut_sent = NULL;
        e->key = key, e->stamp = st, e->flags = f, e->ver++;
        ge.s.cluts++;
    }
    e->epoch = ge.epoch, e->used = ge.syncs;
    ge.clut_slot = (int)(e - ge.cluts);
    *flags = e->flags;
    return clut_pool[ge.clut_slot];
}

/* A page slot, or a CLUT slot, still holds RAM's copy of its key's pixels. */
static int page_current(Page *s)
{
    u32 k = s->key - 1;
    if (s->epoch != ge.epoch) {
        if (stamp((s32)(k & 15) * 64, (s32)(k >> 4 & 1) * 256, k >> 5 ? 128 : 64, 256) != s->stamp)
            return 0;
        s->epoch = ge.epoch;
    }
    return 1;
}

static int clut_current(ClutSlot *c)
{
    u32 k = c->key - 1;
    if (c->epoch != ge.epoch) {
        if (stamp((s32)(k & 63) * 16, (s32)(k >> 6 & 511), k >> 15 ? 256 : 16, 1) != c->stamp)
            return 0;
        c->epoch = ge.epoch;
    }
    return 1;
}

static void bind(const GpuPrim *p, const u8 *pg, const u32 *cl)
{
    if (cl != ge.clut_sent) {
        sceGuClutMode(GU_PSM_8888, 0, 0xFF, 0);
        sceGuClutLoad(p->depth ? 32 : 2, cl); /* blocks of 8 entries */
        ge.clut_sent = cl;
        ge.s.bind_clut++;
    }
    if (pg != ge.tex_sent) {
        sceGuTexMode(p->depth ? GU_PSM_T8 : GU_PSM_T4, 0, 0, 0);
        sceGuTexImage(0, 1, 1, 64, pg + 16); /* another address first: see upload_part */
        sceGuTexImage(0, 256, 256, 256, pg);
        sceGuTexFlush();
        ge.tex_sent = pg;
        ge.s.bind_tex++;
    }
    int fn = p->raw;
    if (fn != ge.texfn) {
        ge.texfn = fn;
        /* TEX_FUNC: modulate with colour doubling, so 0x80 is 1.0 as on the PS1; or replace */
        sceGuSendCommandi(0xC9, fn ? GU_TCC_RGBA << 8 | GU_TFX_REPLACE : 1 << 16 | GU_TCC_RGBA << 8 | GU_TFX_MODULATE);
    }
}

/* Opens the list for a primitive and binds its texture. Returns the texels it has (clut's
 * flags; 3 untextured), 0 when nothing would be drawn. No flush may follow until it is drawn. */
static int prim_begin(const GpuPrim *p)
{
    if (!p->textured) {
        begin();
        return 3;
    }
    /* While no tile turned newer, a page and CLUT checked before need no readback. */
    u32 tk = 1 + (p->tpx | p->tpy << 10 | p->depth << 20), ck = p->clut_x | p->clut_y << 10 | p->depth << 20;
    if (tk != ge.rk_tex || ck != ge.rk_clut || ge.nver != ge.rk_ver) {
        ge.s.rd_checks++;
        rd_read(&ge, (s32)p->tpx, (s32)p->tpy, p->depth ? 128 : 64, 256);
        rd_read(&ge, (s32)p->clut_x, (s32)p->clut_y, p->depth ? 256 : 16, 1);
        ge.rk_tex = tk, ge.rk_clut = ck, ge.rk_ver = ge.nver;
    }
    begin();
    const u8 *pg = page(p);
    int flags;
    const u32 *cl = clut(p, &flags);
    if (flags)
        bind(p, pg, cl);
    return flags;
}

static void tex_pass(const GpuPrim *p, int func, int ref, int mode, int set, int prim, int n, const TexVtx *v)
{
    alpha(func, ref);
    blend(mode);
    stencil(p->mask_check, set);
    sceGuDrawArray(prim, TEX_VTX, n, NULL, v);
}

/* Texels without STP are opaque and written with the mask setting; texels with STP are blended
 * when the primitive is semi-transparent and set bit 15. A CLUT with both takes two passes. */
static void tex_draw(const GpuPrim *p, int flags, int prim, int n, const TexVtx *v)
{
    int semi = p->semi ? (int)p->abr : -1, set = p->mask_set != 0;
    tex_mode(1);
    if (flags == 1)
        tex_pass(p, GU_GREATER, 0, -1, set, prim, n, v);
    else if (flags == 2)
        tex_pass(p, GU_GREATER, 0, semi, 1, prim, n, v);
    else if (semi < 0 && set)
        tex_pass(p, GU_GREATER, 0, -1, 1, prim, n, v);
    else {
        tex_pass(p, GU_EQUAL, 0xFF, -1, set, prim, n, v);
        tex_pass(p, GU_EQUAL, 0x80, semi, 1, prim, n, v);
    }
}

/* A polygon vertex while clipping. */
typedef struct {
    float x, y, r, g, b, u, v;
} ClipVtx;

/* Sutherland-Hodgman against x >= -0.25 (axis 0) or y >= -0.25 (axis 1). */
static int clip_axis(const ClipVtx *in, int n, ClipVtx *out, int axis)
{
    int m = 0;
    for (int i = 0; i < n; i++) {
        const ClipVtx *a = &in[i], *b = &in[i + 1 < n ? i + 1 : 0];
        float da = (axis ? a->y : a->x) + 0.25f, db = (axis ? b->y : b->x) + 0.25f;
        if (da >= 0)
            out[m++] = *a;
        if ((da >= 0) != (db >= 0)) {
            float t = da / (da - db);
            ClipVtx *o = &out[m++];
            o->x = axis ? a->x + (b->x - a->x) * t : -0.25f;
            o->y = axis ? -0.25f : a->y + (b->y - a->y) * t;
            o->r = a->r + (b->r - a->r) * t;
            o->g = a->g + (b->g - a->g) * t;
            o->b = a->b + (b->b - a->b) * t;
            o->u = a->u + (b->u - a->u) * t;
            o->v = a->v + (b->v - a->v) * t;
        }
    }
    return m;
}

static s32 col8(float c)
{
    s32 v = (s32)(c + 0.5f);
    return v < 0 ? 0 : v > 255 ? 255 : v;
}

/* A triangle (n 3, drawn as a fan) or a quad (n 4: the triangles 0,1,2 and 1,2,3, drawn as a
 * strip). A quad left of or above the VRAM origin is GPU_SW: the core offers its two triangles
 * instead. In compare mode the core draws a quad's two triangles too (GPU_BOTH). */
static int polygon(const GpuPrim *p, const GpuVtx *const *v, int n)
{
    s32 minx = v[0]->x, maxx = v[0]->x, miny = v[0]->y, maxy = v[0]->y;
    for (int i = 1; i < n; i++) {
        minx = v[i]->x < minx ? v[i]->x : minx, maxx = v[i]->x > maxx ? v[i]->x : maxx;
        miny = v[i]->y < miny ? v[i]->y : miny, maxy = v[i]->y > maxy ? v[i]->y : maxy;
    }
    if (n == 4 && (minx < 0 || miny < 0))
        return GPU_SW;
    if (p->textured && !tex_ok(p))
        return GPU_SW;
    s32 x1, y1, x2, y2;
    if (!area(&x1, &y1, &x2, &y2))
        return DONE;
    s32 bx1 = minx > x1 ? minx : x1, by1 = miny > y1 ? miny : y1, bx2 = maxx < x2 ? maxx : x2, by2 = maxy < y2 ? maxy : y2;
    if (bx1 > bx2 || by1 > by2)
        return DONE;
    int flags = prim_begin(p);
    if (!flags)
        return DONE;
    if (p->textured)
        ge.s.ttris += (u32)n - 2;
    else
        ge.s.tris += (u32)n - 2;
    prim_state(p, x1, y1, x2, y2);
    mark(bx1, by1, bx2, by2);
    /* +0.5: the GE samples pixel centres, the PS1 integer corners; texels too, where the PS1
     * rounds and the GE floors */
    int prim = n == 4 ? GU_TRIANGLE_STRIP : GU_TRIANGLE_FAN;
    if (minx >= 0 && miny >= 0) {
        if (!p->textured) {
            tex_mode(0);
            GeVtx *o = sceGuGetMemory((u32)n * sizeof(GeVtx));
            for (int i = 0; i < n; i++)
                o[i] = (GeVtx){abgr(v[i]->r, v[i]->g, v[i]->b), (float)v[i]->x + 0.5f, (float)v[i]->y + 0.5f, 0};
            sceGuDrawArray(prim, GE_VTX, n, NULL, o);
            return DONE;
        }
        TexVtx *o = sceGuGetMemory((u32)n * sizeof(TexVtx));
        for (int i = 0; i < n; i++)
            o[i] = (TexVtx){(float)v[i]->u + 0.5f, (float)v[i]->v + 0.5f, abgr(v[i]->r, v[i]->g, v[i]->b), (float)v[i]->x + 0.5f,
                            (float)v[i]->y + 0.5f, 0};
        tex_draw(p, flags, prim, n, o);
        return DONE;
    }
    ClipVtx a[8], b[8], *in = a, *out = b, *t;
    for (int i = 0; i < 3; i++)
        a[i] = (ClipVtx){(float)v[i]->x, (float)v[i]->y, (float)v[i]->r, (float)v[i]->g, (float)v[i]->b, (float)v[i]->u, (float)v[i]->v};
    n = 3;
    /* left of or above the VRAM origin: the GE's handling is unknown, so cut at -0.25 */
    if (minx < 0)
        n = clip_axis(in, n, out, 0), t = in, in = out, out = t;
    if (miny < 0 && n >= 3)
        n = clip_axis(in, n, out, 1), t = in, in = out, out = t;
    if (n < 3)
        return DONE;
    if (!p->textured) {
        tex_mode(0);
        GeVtx *o = sceGuGetMemory((u32)n * sizeof(GeVtx));
        for (int i = 0; i < n; i++)
            o[i] = (GeVtx){abgr(col8(in[i].r), col8(in[i].g), col8(in[i].b)), in[i].x + 0.5f, in[i].y + 0.5f, 0};
        sceGuDrawArray(GU_TRIANGLE_FAN, GE_VTX, n, NULL, o);
        return DONE;
    }
    TexVtx *o = sceGuGetMemory((u32)n * sizeof(TexVtx));
    for (int i = 0; i < n; i++)
        o[i] = (TexVtx){in[i].u + 0.5f, in[i].v + 0.5f, abgr(col8(in[i].r), col8(in[i].g), col8(in[i].b)), in[i].x + 0.5f, in[i].y + 0.5f, 0};
    tex_draw(p, flags, GU_TRIANGLE_FAN, n, o);
    return DONE;
}

static int rd_triangle(void *self, const GpuPrim *p, const GpuVtx *v0, const GpuVtx *v1, const GpuVtx *v2)
{
    (void)self;
    const GpuVtx *v[3] = {v0, v1, v2};
    return polygon(p, v, 3);
}

static int rd_quad(void *self, const GpuPrim *p, const GpuVtx *v)
{
    (void)self;
    const GpuVtx *q[4] = {&v[0], &v[1], &v[2], &v[3]};
    return polygon(p, q, 4);
}

static int rd_rect(void *self, const GpuPrim *p, s32 x, s32 y, s32 w, s32 h, const GpuVtx *c, int flip_x, int flip_y)
{
    (void)self;
    if (p->textured && !tex_ok(p))
        return GPU_SW;
    s32 x1, y1, x2, y2;
    if (!area(&x1, &y1, &x2, &y2))
        return DONE;
    s32 bx1 = x > x1 ? x : x1, by1 = y > y1 ? y : y1;
    s32 bx2 = x + w - 1 < x2 ? x + w - 1 : x2, by2 = y + h - 1 < y2 ? y + h - 1 : y2;
    if (bx1 > bx2 || by1 > by2)
        return DONE;
    int flags = prim_begin(p);
    if (!flags)
        return DONE;
    prim_state(p, x1, y1, x2, y2);
    mark(bx1, by1, bx2, by2);
    u32 col = abgr(c->r, c->g, c->b);
    if (!p->textured) {
        ge.s.rects++;
        sprite(bx1, by1, bx2 + 1, by2 + 1, col); /* a sprite covers [x1, x2) */
        return DONE;
    }
    ge.s.trects++;
    /* The texel of pixel (x + i) is u + i, or u - i flipped; the GE takes the texel under the
     * pixel's centre, so a flipped sprite starts one texel further, and 1024 later to stay
     * positive. The texture repeats as the PS1's 8-bit u, v wrap. */
    s32 dx = bx1 - x, dy = by1 - y, sw = bx2 + 1 - bx1, sh = by2 + 1 - by1;
    float u0 = (float)(flip_x ? ((c->u - dx) & 255) + 1 + 1024 : (c->u + dx) & 255);
    float v0 = (float)(flip_y ? ((c->v - dy) & 255) + 1 + 1024 : (c->v + dy) & 255);
    float u1 = flip_x ? u0 - (float)sw : u0 + (float)sw, v1 = flip_y ? v0 - (float)sh : v0 + (float)sh;
    TexVtx *o = sceGuGetMemory(2 * sizeof(TexVtx));
    o[0] = (TexVtx){u0, v0, col, (float)bx1, (float)by1, 0};
    o[1] = (TexVtx){u1, v1, col, (float)(bx2 + 1), (float)(by2 + 1), 0};
    tex_draw(p, flags, GU_SPRITES, 2, o);
    return DONE;
}

/* The core's pixels (gpu.c line()), as sprites: a run of pixels of one colour along a row or a
 * column is one sprite. The GE's own lines rasterize differently. */
static int rd_line(void *self, const GpuPrim *p, const GpuVtx *a, const GpuVtx *b)
{
    (void)self;
    s32 x1, y1, x2, y2;
    if (!area(&x1, &y1, &x2, &y2))
        return DONE;
    s32 dx = b->x - a->x, dy = b->y - a->y, adx = dx < 0 ? -dx : dx, ady = dy < 0 ? -dy : dy;
    s32 n = adx > ady ? adx : ady;
    static struct { s16 x0, y0, x1, y1; u32 c; } run[1025];
    int nr = 0;
    s32 bx1 = VRAM_W, by1 = VRAM_H, bx2 = -1, by2 = -1;
    for (s32 i = 0; i <= n; i++) {
        s32 x = a->x + (n ? (dx * i + (dx >= 0 ? n / 2 : -n / 2)) / n : 0);
        s32 y = a->y + (n ? (dy * i + (dy >= 0 ? n / 2 : -n / 2)) / n : 0);
        if (x < x1 || x > x2 || y < y1 || y > y2)
            continue;
        u32 c = abgr(a->r + (n ? (b->r - a->r) * i / n : 0), a->g + (n ? (b->g - a->g) * i / n : 0),
                     a->b + (n ? (b->b - a->b) * i / n : 0));
        bx1 = x < bx1 ? x : bx1, by1 = y < by1 ? y : by1, bx2 = x > bx2 ? x : bx2, by2 = y > by2 ? y : by2;
        if (nr) {
            typeof(run[0]) *r = &run[nr - 1];
            int row = r->y0 == y && r->y1 == y && (x == r->x1 + 1 || x == r->x0 - 1);
            int col = r->x0 == x && r->x1 == x && (y == r->y1 + 1 || y == r->y0 - 1);
            if (r->c == c && (row || col)) {
                r->x0 = x < r->x0 ? x : r->x0, r->x1 = x > r->x1 ? x : r->x1;
                r->y0 = y < r->y0 ? y : r->y0, r->y1 = y > r->y1 ? y : r->y1;
                continue;
            }
        }
        run[nr++] = (typeof(run[0])){(s16)x, (s16)y, (s16)x, (s16)y, c};
    }
    if (!nr)
        return DONE;
    prim_begin(p);
    prim_state(p, x1, y1, x2, y2);
    mark(bx1, by1, bx2, by2);
    tex_mode(0);
    GeVtx *o = sceGuGetMemory((u32)nr * 2 * sizeof(GeVtx));
    for (int i = 0; i < nr; i++) {
        o[2 * i] = (GeVtx){run[i].c, (float)run[i].x0, (float)run[i].y0, 0};
        o[2 * i + 1] = (GeVtx){run[i].c, (float)(run[i].x1 + 1), (float)(run[i].y1 + 1), 0};
    }
    sceGuDrawArray(GU_SPRITES, GE_VTX, 2 * nr, NULL, o);
    ge.s.lines++, ge.s.line_runs += (u32)nr;
    return DONE;
}

static void fill_piece(s32 x, s32 y, s32 w, s32 h)
{
    scissor(x, y, x + w - 1, y + h - 1);
    sprite(x, y, x + w, y + h, ge.fill_color);
    mark(x, y, x + w - 1, y + h - 1);
}

/* Fill ignores the drawing area and the mask, and clears bit 15. */
static int rd_fill(void *self, s32 x, s32 y, s32 w, s32 h, u16 color)
{
    (void)self;
    begin();
    stencil(0, 0);
    blend(-1);
    dither(0);
    ge.fill_color = abgr((color & 31) << 3, (color >> 5 & 31) << 3, (color >> 10 & 31) << 3);
    pieces(x, y, w, h, fill_piece);
    ge.s.fills++;
    return DONE;
}

/* Whether the GE drew any tile of a rectangle since RAM last saw it. */
static int newer(s32 x, s32 y, s32 w, s32 h)
{
    u16 c = span(x, w, VRAM_W, TILE_W), r = span(y, h, VRAM_H, TILE_H);
    for (int ty = 0; ty < TILES_Y; ty++)
        if ((r >> ty & 1) && (ge.newer[ty] & c))
            return 1;
    return 0;
}

/* A copy is done where the pixels it reads are current. When the GE drew them, it is a blit of
 * the GE's copy into itself (that copy is never behind RAM's), and nothing is read back or sent
 * up; in software it would cost a readback of all of VRAM. Otherwise it is done in RAM and its
 * destination sent up: a blit there would leave the destination newer on the GE, and the next
 * texture or copy read from it would bring all of VRAM back. Copies that wrap around VRAM, or
 * whose source and destination overlap (the GE would read pixels it is writing), stay in
 * software. Compare mode blits every copy, to check the blit. */
static int rd_copy(void *self, s32 sx, s32 sy, s32 dx, s32 dy, s32 w, s32 h, int mask_check, u16 mask_set)
{
    (void)self;
    ge.s.copy_px += (u32)(w * h);
    /* A rectangle copied onto itself without setting bit 15 changes nothing (Sheep Dog copies 2x1
     * pixels at 0,0 onto themselves every frame, as a wait for the GPU). */
    if (sx == dx && sy == dy && !mask_set) {
        ge.s.null_copy++;
        return GPU_DRAWN;
    }
    int from_ge = newer(sx, sy, w, h) || (mask_check && newer(dx, dy, w, h));
    ge.s.copy_from_ge += (u32)from_ge;
    s32 ox = sx > dx ? sx - dx : dx - sx, oy = sy > dy ? sy - dy : dy - sy;
    if ((!from_ge && !ge.compare) || sx + w > VRAM_W || dx + w > VRAM_W || sy + h > VRAM_H || dy + h > VRAM_H ||
        (ox < w && oy < h)) {
        ge.s.sw_copy++;
        return GPU_SW;
    }
    begin();
    blit((u16 *)VRAM_PTR, sx, sy, dx, dy, w, h, mask_check, mask_set != 0);
    mark(dx, dy, dx + w - 1, dy + h - 1);
    ge.s.ge_copy++;
    return DONE;
}

/* ---- the polygon fast lane ---- */

/* A key's state as a block of GE words, sent from unknown so that they alone set it, with what
 * prim_begin found for it. Each entry has its own block in an arena the GE calls into: a key seen
 * again, with nothing turned newer under its page and CLUT, is a CALL. A kept entry (untextured,
 * or its CLUT in the pool) outlives the list: its page slot and CLUT slot must still hold what it
 * was built with (their ver) and be current (their stamps). The others load a CLUT from the list
 * they were built in, and last as long as it and the epoch. The block is written only while no
 * open list calls it (blk). S19: the device built every key again in every list, ~1.5k a second. */
#define FL_BITS 8
#define FL_SLOT 64 /* words: a block is at most ~30 */
#define FL_ARENA ((1 << FL_BITS) * FL_SLOT * 4)
typedef struct {
    u32 k0, k1, a0, a1, syncs, epoch, nver;
    int flags, two, n1, n2, slot, cslot, keep;
    u32 pver, cver; /* the page slot's and the CLUT slot's ver it was built with */
    u32 blk;        /* the list that last called its block */
    u32 use;        /* fl_uses when last found */
    u32 call[2];    /* BASE and CALL of its block */
    u32 pass1[8], pass2[8];
} FlEntry;
static FlEntry fl_cache[1 << FL_BITS];
static u32 fl_uses;
static u32 __attribute__((aligned(64))) fl_arena[FL_ARENA / 4];
#define FL_ARENA_UNCACHED ((unsigned int *)((u32)fl_arena | 0x40000000u))

/* A tile under a texpage's page or a CLUT is newer on the GE (prim_begin would read back). */
static int fl_newer(u32 tp, u32 clut)
{
    int d = tp >> 7 & 3;
    return newer((s32)(tp & 0xF) * 64, (s32)(tp >> 4 & 1) * 256, d ? 128 : 64, 256) ||
           newer((s32)(clut & 0x3F) * 16, (s32)(clut >> 6 & 0x1FF), d ? 256 : 16, 1);
}

/* A kept entry's page slot and CLUT slot still hold what it was built with, and are current. */
static int fl_current(FlEntry *e)
{
    if (!(e->k0 & 1))
        return 1;
    if (ge.pages[e->slot].ver != e->pver || ge.cluts[e->cslot].ver != e->cver)
        return 0;
    if (e->epoch != ge.epoch) {
        if (!page_current(&ge.pages[e->slot]) || !clut_current(&ge.cluts[e->cslot]))
            return 0;
        e->epoch = ge.epoch;
    }
    return 1;
}

/* A pass's state, sent from unknown so that its words alone set it, and kept. */
static void fl_capture(u32 *dst, int *n, int func, int ref, int mode, int check, int set)
{
    unsigned int *s = gu_list->current;
    ge.a_func = -1, ge.blend = -2, ge.st_check = ge.st_set = -2;
    alpha(func, ref);
    blend(mode);
    stencil(check, set);
    *n = (int)(gu_list->current - s);
    for (int i = 0; i < *n; i++)
        dst[i] = s[i];
}

/* A new key: its state words, as polygon() and tex_draw() would send them, into the entry's block
 * (into the list when the open list still calls the block, and the entry is used this once). */
static void fl_miss(FlEntry *e, u32 cmd, u32 clut, s32 x1, s32 y1, s32 x2, s32 y2)
{
    const Gpu *g = &ge.psx->gpu;
    GpuPrim p;
    int gour = cmd >> 4 & 1;
    gpu_prim_from(ge.psx, &p, cmd, clut, g->texpage);
    p.gouraud = gour;
    p.dither = (g->stat & 0x200) && (gour || (p.textured && !p.raw));
    int flags = prim_begin(&p);
    e->k0 = fl.k0, e->k1 = fl.k1, e->a0 = fl.a0, e->a1 = fl.a1;
    e->flags = flags, e->two = 0, e->slot = e->cslot = -1, e->keep = !p.textured;
    if (p.textured) { /* prim_begin's page() and clut() */
        e->slot = (int)(ge.last_page - ge.pages), e->pver = ge.last_page->ver;
        e->cslot = ge.clut_slot, e->keep = ge.clut_slot >= 0;
        e->cver = e->keep ? ge.cluts[ge.clut_slot].ver : 0;
    }
    if (flags) {
        const u8 *pg = ge.tex_sent;
        const u32 *cl = ge.clut_sent;
        unsigned int *l = gu_list->current;
        int arena = e->blk != ge.syncs; /* prim_begin may have run the list */
        if (arena)
            gu_list->current = FL_ARENA_UNCACHED + (e - fl_cache) * FL_SLOT;
        unsigned int *s = gu_list->current;
        fl_forget();
        if (p.textured)
            bind(&p, pg, cl);
        prim_state(&p, x1, y1, x2, y2);
        if (!p.textured)
            tex_mode(0);
        else {
            int semi = p.semi ? (int)p.abr : -1, set = p.mask_set != 0;
            tex_mode(1);
            if (flags == 1 || flags == 2 || (semi < 0 && set)) {
                alpha(GU_GREATER, 0);
                blend(flags == 2 ? semi : -1);
                stencil(p.mask_check, flags == 1 ? set : 1);
            } else {
                unsigned int *m = gu_list->current; /* the lane sends these before each pass */
                fl_capture(e->pass1, &e->n1, GU_EQUAL, 0xFF, -1, p.mask_check, set);
                fl_capture(e->pass2, &e->n2, GU_EQUAL, 0x80, semi, p.mask_check, 1);
                gu_list->current = m;
                ge.a_func = -1, ge.blend = -2, ge.st_check = ge.st_set = -2;
                e->two = 1;
            }
        }
        sceGuSendCommandi(0x12, p.textured ? TEX_VTX : GE_VTX); /* VTYPE */
        if (arena) {
            sceGuSendCommandi(0x0B, 0); /* RET */
            e->call[0] = 0x10000000u | ((u32)s >> 8 & 0xF0000);
            e->call[1] = 0x0A000000u | ((u32)s & 0xFFFFFF);
            l[0] = e->call[0], l[1] = e->call[1];
            gu_list->current = l + 2;
            e->blk = ge.syncs;
        } else
            e->k0 = ~0u, e->call[0] = 0; /* used this once, never found */
    }
    e->syncs = ge.syncs, e->epoch = ge.epoch, e->nver = ge.nver; /* prim_begin may have run the list */
}

/* The key in fl.k0-a1 changed: the GE gets its state, from the cache when it holds. */
static void fl_setup(u32 cmd, u32 clut)
{
    s32 x1, y1, x2, y2;
    int flags = 0;
    /* two ways per set, the one used longer ago replaced */
    u32 h = (fl.k0 ^ fl.k1 * 0x9E3779B1u ^ fl.a0 * 0x85EBCA6Bu ^ fl.a1) * 0xC2B2AE35u;
    FlEntry *e = &fl_cache[(h ^ h >> 15) >> (32 - FL_BITS) & ~1u];
    if (e->k0 != fl.k0 || e->k1 != fl.k1 || e->a0 != fl.a0 || e->a1 != fl.a1) {
        FlEntry *o = e + 1;
        if ((o->k0 == fl.k0 && o->k1 == fl.k1 && o->a0 == fl.a0 && o->a1 == fl.a1) || o->use < e->use)
            e = o;
    }
    e->use = ++fl_uses;
    if (area(&x1, &y1, &x2, &y2)) {
        begin(); /* first: running the list ends its blocks */
        if (e->k0 == fl.k0 && e->k1 == fl.k1 && e->a0 == fl.a0 && e->a1 == fl.a1 &&
            (e->keep ? fl_current(e) : e->syncs == ge.syncs && e->epoch == ge.epoch) &&
            (e->nver == ge.nver || !(fl.k0 & 1) || !fl_newer(ge.psx->gpu.texpage, clut))) {
            e->nver = ge.nver, e->syncs = ge.syncs;
            if (e->slot >= 0) { /* the open list draws with them: neither is written until it has run */
                Page *s = &ge.pages[e->slot];
                s->used = ge.syncs, s->age = ++ge.ages;
                if (e->cslot >= 0)
                    ge.cluts[e->cslot].used = ge.syncs;
            }
            if (e->flags) {
                unsigned int *l = gu_list->current;
                l[0] = e->call[0], l[1] = e->call[1];
                gu_list->current = l + 2;
                fl.dirty = 1;
                e->blk = ge.syncs;
            }
            ge.s.fast_hits++;
        } else
            fl_miss(e, cmd, clut, x1, y1, x2, y2);
        flags = e->flags;
    }
    fl.flags = flags;
    fl.two = flags && e->two, fl.n1 = e->n1, fl.n2 = e->n2, fl.pass1 = e->pass1, fl.pass2 = e->pass2;
    fl.x1 = x1, fl.y1 = y1, fl.x2 = x2, fl.y2 = y2;
    fl.base = flags && e->call[0] ? e->call[0] & 0xF0000 : ~0u; /* the block's CALL set it last */
    fl.epoch = ge.epoch, fl.nver = ge.nver;
    fl.valid = 1;
    ge.s.fast_setups++;
}

/* fl_setup, one call in 16 timed for the log. */
static void fl_setup_timed(u32 cmd, u32 clut)
{
    static u32 n;
    if (n++ % 16) {
        fl_setup(cmd, clut);
        return;
    }
    u32 t = sceKernelGetSystemTimeLow();
    fl_setup(cmd, clut);
    ge.s.setup_us += (sceKernelGetSystemTimeLow() - t) * 16 - prof_bias;
}

/* A draw of n ring vertices at vp in the lane's state: BASE when it moves, VADDR and PRIM, per
 * pass. Returns the list's next word. */
static unsigned int *fl_draw(unsigned int *l, const u8 *vp, int prim, int n)
{
    u32 base = (u32)vp >> 8 & 0xF0000, va = 0x01000000u | ((u32)vp & 0xFFFFFF);
    u32 pr = 0x04000000u | (u32)prim << 16 | (u32)n;
    if (fl.two)
        for (int i = 0; i < fl.n1; i++)
            *l++ = fl.pass1[i];
    if (base != fl.base)
        *l++ = 0x10000000u | base, fl.base = base;
    *l++ = va, *l++ = pr;
    if (fl.two) {
        for (int i = 0; i < fl.n2; i++)
            *l++ = fl.pass2[i];
        *l++ = va, *l++ = pr;
    }
    return l;
}

/* A polygon command (GP0 20-3F) straight from its words, when the GE can sample its texture and
 * it is within the core's size limits; -1 otherwise. Each vertex is written once into the ring.
 * One left of or above the VRAM origin is cut there, triangle by triangle, as polygon() cuts the
 * core's triangles. One with no area draws nothing, as in the core. */
static int rd_poly(void *self, const u32 *c)
{
    (void)self;
    const Gpu *g = &ge.psx->gpu;
    u32 cmd = c[0] >> 24;
    int quad = cmd >> 3 & 1, tex = cmd >> 2 & 1, gour = cmd >> 4 & 1, n = quad ? 4 : 3;
    if (tex && ((g->texpage >> 7 & 3) >= 2 || g->tw_mask_x || g->tw_mask_y || !ge.npages))
        return ge.s.slow_tex++, -1;
    s32 x[4], y[4];
    u32 col[4], uv[4];
    u32 cl = c[0] & 0xFFFFFF;
    int k = 1;
    for (int i = 0; i < n; i++) {
        if (gour && i)
            cl = c[k++] & 0xFFFFFF;
        u32 w = c[k++];
        x[i] = ((s32)(w << 21) >> 21) + g->off_x;
        y[i] = ((s32)(w << 5) >> 21) + g->off_y;
        col[i] = cl;
        uv[i] = tex ? c[k++] : 0;
    }
    s32 minx = x[0], maxx = x[0], miny = y[0], maxy = y[0];
    for (int i = 1; i < n; i++) {
        minx = x[i] < minx ? x[i] : minx, maxx = x[i] > maxx ? x[i] : maxx;
        miny = y[i] < miny ? y[i] : miny, maxy = y[i] > maxy ? y[i] : maxy;
    }
    if (!quad && (x[1] - x[0]) * (y[2] - y[0]) == (x[2] - x[0]) * (y[1] - y[0]))
        return ge.s.flat++, DONE; /* no area: the core draws nothing either */
    if (maxx - minx > 1023 || maxy - miny > 511)
        return ge.s.slow_big++, -1; /* the core drops what is too large, triangle by triangle */
    int cut = (minx | miny) < 0; /* left of or above the VRAM origin: cut as polygon() does */
    u32 vsz = tex ? sizeof(TexVtx) : sizeof(GeVtx);
    if (fl.ring_off + (u32)(cut ? 10 : n) * vsz > fl.ring_bytes) {
        ge_sync();
        ge.s.flushes++;
    } else if (fl.valid && (u32)((u8 *)gu_list->current - (u8 *)gu_list->start) > LIST_BYTES - LIST_SLACK)
        fl.valid = 0; /* fl_setup's begin() runs the list */
    /* only what changes the GE state: flat and Gouraud polygons, triangles and quads share it,
     * and the dither bit counts only where fl_setup's prim_state would dither */
    int raw = tex && (cmd & 1), semi = cmd >> 1 & 1;
    u32 dith = (g->texpage >> 9 & 1) && (gour || tex) && !raw;
    u32 tp = (tex ? g->texpage & 0x19F : 0) | (semi ? g->texpage & 0x60 : 0);
    u32 k0 = (u32)tex | (u32)raw << 1 | (u32)semi << 2 | dith << 3 | tp << 8 | (u32)(g->mask_check != 0) << 20 |
             (u32)(g->mask_set != 0) << 21;
    u32 k1 = uv[0] >> 16;
    u32 a0 = (u32)g->clip_x1 | (u32)g->clip_y1 << 16, a1 = (u32)g->clip_x2 | (u32)g->clip_y2 << 16;
    if (!fl.valid || k0 != fl.k0 || k1 != fl.k1 || a0 != fl.a0 || a1 != fl.a1 || ge.epoch != fl.epoch) {
        fl.k0 = k0, fl.k1 = k1, fl.a0 = a0, fl.a1 = a1;
        fl_setup_timed(cmd, k1);
    } else if (tex && ge.nver != fl.nver) {
        /* tiles turned newer: only those under the page or the CLUT need the readback */
        if (fl_newer(g->texpage, k1))
            fl_setup_timed(cmd, k1);
        else
            fl.nver = ge.nver;
    }
    if (!fl.flags)
        return DONE;
    s32 bx1 = minx > fl.x1 ? minx : fl.x1, by1 = miny > fl.y1 ? miny : fl.y1;
    s32 bx2 = maxx < fl.x2 ? maxx : fl.x2, by2 = maxy < fl.y2 ? maxy : fl.y2;
    if (bx1 > bx2 || by1 > by2)
        return DONE;
    if (tex)
        ge.s.ttris += (u32)n - 2;
    else
        ge.s.tris += (u32)n - 2;
    ge.s.fast++;
    mark(bx1, by1, bx2, by2);
    if (raw)
        for (int i = 0; i < n; i++)
            col[i] = 0x808080;
    unsigned int *l = gu_list->current;
    u8 *vp = fl.ring + fl.ring_off;
    if (!cut) {
        /* +0.5 as in polygon() */
        if (tex) {
            TexVtx *o = (TexVtx *)vp;
            for (int i = 0; i < n; i++) {
                o[i].u = (float)(uv[i] & 0xFF) + 0.5f, o[i].v = (float)(uv[i] >> 8 & 0xFF) + 0.5f;
                o[i].c = 0xFF000000u | col[i];
                o[i].x = (float)x[i] + 0.5f, o[i].y = (float)y[i] + 0.5f, o[i].z = 0;
            }
        } else {
            GeVtx *o = (GeVtx *)vp;
            for (int i = 0; i < n; i++) {
                o[i].c = 0xFF000000u | col[i];
                o[i].x = (float)x[i] + 0.5f, o[i].y = (float)y[i] + 0.5f, o[i].z = 0;
            }
        }
        l = fl_draw(l, vp, quad ? GU_TRIANGLE_STRIP : GU_TRIANGLE_FAN, n);
        fl.ring_off += (u32)n * vsz;
    } else {
        /* the core's triangles (a quad's are 0,1,2 and 1,2,3), each cut to a fan */
        ge.s.clipped++;
        for (int t = 0; t + 2 < n; t++) {
            int i0 = t, i1 = t + 1, i2 = t + 2;
            if ((x[i1] - x[i0]) * (y[i2] - y[i0]) == (x[i2] - x[i0]) * (y[i1] - y[i0]))
                continue;
            ClipVtx a[8], b[8], *in = a, *out = b, *s;
            for (int j = 0; j < 3; j++) {
                int i = t + j;
                a[j] = (ClipVtx){(float)x[i], (float)y[i], (float)(col[i] & 0xFF), (float)(col[i] >> 8 & 0xFF),
                                 (float)(col[i] >> 16 & 0xFF), (float)(uv[i] & 0xFF), (float)(uv[i] >> 8 & 0xFF)};
            }
            int m = 3;
            if (minx < 0)
                m = clip_axis(in, m, out, 0), s = in, in = out, out = s;
            if (miny < 0 && m >= 3)
                m = clip_axis(in, m, out, 1), s = in, in = out, out = s;
            if (m < 3)
                continue;
            if (tex) {
                TexVtx *o = (TexVtx *)vp;
                for (int i = 0; i < m; i++)
                    o[i] = (TexVtx){in[i].u + 0.5f, in[i].v + 0.5f, abgr(col8(in[i].r), col8(in[i].g), col8(in[i].b)),
                                    in[i].x + 0.5f, in[i].y + 0.5f, 0};
            } else {
                GeVtx *o = (GeVtx *)vp;
                for (int i = 0; i < m; i++)
                    o[i] = (GeVtx){abgr(col8(in[i].r), col8(in[i].g), col8(in[i].b)), in[i].x + 0.5f, in[i].y + 0.5f, 0};
            }
            l = fl_draw(l, vp, GU_TRIANGLE_FAN, m);
            vp += (u32)m * vsz, fl.ring_off += (u32)m * vsz;
        }
    }
    gu_list->current = l;
    ge_kick();
    return DONE;
}

/* ---- setup and log ---- */

/* At boot: 64 KB of vertices written as rd_poly writes them, through the uncached mirror and
 * through the cache with the writeback the GE needs; the PSP's own figures (an emulator charges
 * neither the bus nor the cache). */
static void ring_bench(Psx *psx)
{
    u32 t[2], n = 64 * 1024 / sizeof(TexVtx);
    for (int m = 0; m < 2; m++) {
        TexVtx *o = (TexVtx *)(m ? fl.ring_c : fl.ring_u);
        u32 t0 = sceKernelGetSystemTimeLow();
        for (u32 i = 0; i < n; i++) {
            o[i].u = (float)(i & 0xFF) + 0.5f, o[i].v = (float)(i >> 8 & 0xFF) + 0.5f;
            o[i].c = 0xFF000000u | i;
            o[i].x = (float)(i & 511) + 0.5f, o[i].y = (float)(i >> 9) + 0.5f, o[i].z = 0;
        }
        if (m)
            sceKernelDcacheWritebackRange(o, n * sizeof(TexVtx));
        t[m] = sceKernelGetSystemTimeLow() - t0;
    }
    sceKernelDcacheWritebackInvalidateRange(fl.ring_c, fl.ring_bytes);
    psx_log(psx, "ge: 64 KB of vertices in %u us uncached, %u us cached and written back", (unsigned)t[0], (unsigned)t[1]);
    /* the profile's own cost (each timed call adds one clock read) and a data cache miss: 512
     * reads of PS1 RAM 4 KB apart, after the cache was emptied of it */
    u32 t0 = sceKernelGetSystemTimeLow();
    for (int i = 0; i < 256; i++)
        sceKernelGetSystemTimeLow();
    u32 clk = sceKernelGetSystemTimeLow() - t0, sum = 0;
    sceKernelDcacheWritebackInvalidateRange(psx->ram, 2 * 1024 * 1024);
    volatile const u32 *ram = (const u32 *)psx->ram;
    t0 = sceKernelGetSystemTimeLow();
    for (u32 i = 0; i < 512; i++)
        sum += ram[i * 1024 + (i & 15) * 4];
    u32 miss = sceKernelGetSystemTimeLow() - t0;
    psx_log(psx, "cpu: clock read %u ns, cache miss %u ns (%u)", (unsigned)(clk * 1000 / 256), (unsigned)(miss * 1000 / 512),
            (unsigned)(sum & 1));
}

/* At boot: the costs rd_poly pays while the GE draws, which ring_bench measures with the GE idle.
 * The GE draws 32 full-screen sprites textured from a page slot in RAM, as the game's polygons
 * are; meanwhile the CPU misses the cache on PS1 RAM and writes 64 KB of vertices uncached. Then
 * the stall move, a system call made every 1 KB of list, on its own. */
static void busy_bench(Psx *psx)
{
    typedef struct {
        u16 u, v;
        s16 x, y, z;
    } SprVtx;
    if (!ge.npages)
        return;
    ge_open();
    sceGuDrawBufferList(GU_PSM_5650, (void *)GE_DISP_B, 512);
    sceGuScissor(0, 0, 480, 272);
    sceGuDisable(GU_DEPTH_TEST), sceGuDisable(GU_STENCIL_TEST), sceGuDisable(GU_BLEND), sceGuDisable(GU_ALPHA_TEST);
    sceGuTexMode(GU_PSM_5650, 0, 0, 0);
    sceGuTexImage(0, 128, 256, 128, ge.pages[0].data);
    sceGuTexFunc(GU_TFX_REPLACE, GU_TCC_RGB);
    sceGuTexFilter(GU_NEAREST, GU_NEAREST);
    sceGuTexFlush();
    sceGuEnable(GU_TEXTURE_2D);
    SprVtx *v = (SprVtx *)sceGuGetMemory(64 * sizeof(SprVtx));
    for (int i = 0; i < 32; i++)
        v[2 * i] = (SprVtx){0, 0, 0, 0, 0}, v[2 * i + 1] = (SprVtx){128, 256, 480, 272, 0};
    sceGuDrawArray(GU_SPRITES, GU_TEXTURE_16BIT | GU_VERTEX_16BIT | GU_TRANSFORM_2D, 64, 0, v);
    sceGuDisable(GU_TEXTURE_2D);
    sceKernelDcacheWritebackInvalidateRange(psx->ram, 2 * 1024 * 1024);
    volatile const u32 *ram = (const u32 *)psx->ram;
    u32 sum = 0, t0 = sceKernelGetSystemTimeLow();
    ge_run();
    u32 t1 = sceKernelGetSystemTimeLow();
    for (u32 i = 0; i < 512; i++)
        sum += ram[i * 1024 + (i & 15) * 4];
    u32 t2 = sceKernelGetSystemTimeLow(), n = 64 * 1024 / sizeof(TexVtx);
    TexVtx *o = (TexVtx *)fl.ring_u;
    for (u32 i = 0; i < n; i++) {
        o[i].u = (float)(i & 0xFF) + 0.5f, o[i].v = (float)(i >> 8 & 0xFF) + 0.5f;
        o[i].c = 0xFF000000u | i;
        o[i].x = (float)(i & 511) + 0.5f, o[i].y = (float)(i >> 9) + 0.5f, o[i].z = 0;
    }
    u32 t3 = sceKernelGetSystemTimeLow();
    ge_sync();
    u32 t4 = sceKernelGetSystemTimeLow();
    ge_open();
    u32 t5 = sceKernelGetSystemTimeLow();
    for (int i = 0; i < 256; i++)
        sceGeListUpdateStallAddr(list_id, LIST_UNCACHED + list_stall);
    u32 t6 = sceKernelGetSystemTimeLow();
    ge_sync();
    psx_log(psx, "ge busy %u us (from RAM): cache miss %u ns, 64 KB of vertices uncached %u us%s; stall move %u ns (%u)",
            (unsigned)(t4 - t0), (unsigned)((t2 - t1) * 1000 / 512), (unsigned)(t3 - t2),
            t4 - t3 < 20 ? " (the GE finished first)" : "", (unsigned)((t6 - t5) * 1000 / 256),
            (unsigned)(sum & 1));
}

void ge_init(Psx *psx, int compare)
{
    ge.psx = psx;
    ge.compare = compare;
    if (compare) {
        SceUID uid = sceKernelAllocPartitionMemory(2, "psxrt ge compare", PSP_SMEM_Low, VRAM_W * VRAM_H * 2, NULL);
        ge.cmp = uid < 0 ? NULL : sceKernelGetBlockHeadAddr(uid);
        ge.compare = ge.cmp != NULL;
    }
    for (int n = PAGE_SLOTS; n >= 4 && !ge.npages; n /= 2) {
        SceUID uid = sceKernelAllocPartitionMemory(2, "psxrt ge pages", PSP_SMEM_Low, (SceSize)(n * PAGE_BYTES + 64), NULL);
        if (uid < 0)
            continue;
        u8 *b = (u8 *)(((u32)sceKernelGetBlockHeadAddr(uid) + 63) & ~63u);
        for (int i = 0; i < n; i++)
            ge.pages[i].data = b + i * PAGE_BYTES;
        ge.npages = n;
    }
    for (u32 b = RING_MAX; b >= 64 * 1024 && !fl.ring; b /= 2) {
        SceUID uid = sceKernelAllocPartitionMemory(2, "psxrt ge ring", PSP_SMEM_Low, b + 64, NULL);
        if (uid < 0)
            continue;
        u8 *r = (u8 *)(((u32)sceKernelGetBlockHeadAddr(uid) + 63) & ~63u);
        sceKernelDcacheWritebackInvalidateRange(r, b); /* first written through the uncached mirror */
        fl.ring_c = r, fl.ring_u = fl.ring = (u8 *)((u32)r | 0x40000000u), fl.ring_bytes = b;
    }
    if (fl.ring)
        ring_bench(psx), busy_bench(psx);
    for (int i = 0; i < 1 << FL_BITS; i++)
        fl_cache[i].k0 = ~0u; /* no key */
    sceKernelDcacheWritebackInvalidateRange(fl_arena, sizeof fl_arena); /* written uncached from now on */
    ge.r = (GpuRenderer){&ge, rd_triangle, rd_quad, rd_rect, rd_line, rd_fill, rd_copy, rd_read, rd_wrote, fl.ring ? rd_poly : NULL};
    ge.syncs = 1; /* no slot (used 0) is in the list */
    psx_log(psx, "ge: %d texture page slots, %u KB vertex ring", ge.npages, (unsigned)(fl.ring_bytes / 1024));
    rd_fill(&ge, 0, 0, VRAM_W, VRAM_H, 0); /* also makes the oracle create the framebuffer */
    ge_sync();
    newer_clear(); /* both copies are zero */
    memset(&ge.s, 0, sizeof ge.s);
    psx->gpu.renderer = &ge.r;
}

static int clampn(int n, int cap) { return n < 0 ? 0 : n >= cap ? cap - 1 : n; }

/* Compare mode: the GE's copy against RAM's, then RAM's sent up so each second starts equal. */
static int compare(char *buf, int cap)
{
    const Gpu *g = &ge.psx->gpu;
    if (g->xfer == 1) /* a transfer is half way: RAM is ahead */
        return clampn(snprintf(buf, (size_t)cap, "-- ge: no comparison, a transfer to %d,%d %dx%d is at row %d\n",
                               (int)g->xx, (int)g->xy, (int)g->xw, (int)g->xh, (int)g->xr),
                      cap);
    begin();
    sceKernelDcacheWritebackInvalidateRange(ge.cmp, VRAM_W * VRAM_H * 2);
    sceGuCopyImage(GU_PSM_5551, 0, 0, VRAM_W, VRAM_H, VRAM_W, VRAM_PTR, 0, 0, VRAM_W, ge.cmp);
    ge_sync();
    const u16 *ram = ge.psx->gpu.vram;
    /* The oracle does not dither, so differences of one step per channel are expected there; the
     * box covers the larger ones. */
    u32 diff = 0, bit15 = 0, big = 0;
    s32 x1 = VRAM_W, y1 = VRAM_H, x2 = -1, y2 = -1, fx = -1, fy = -1;
    for (s32 y = 0; y < VRAM_H; y++)
        for (s32 x = 0; x < VRAM_W; x++) {
            u32 a = ram[y * VRAM_W + x], b = ge.cmp[y * VRAM_W + x];
            if (a == b)
                continue;
            diff++;
            if (!((a ^ b) & 0x7FFF)) {
                bit15++;
                continue;
            }
            for (int sh = 0; sh < 15; sh += 5) {
                s32 d = (s32)(a >> sh & 31) - (s32)(b >> sh & 31);
                if (d > 1 || d < -1) {
                    if (!big++)
                        fx = x, fy = y;
                    x1 = x < x1 ? x : x1, y1 = y < y1 ? y : y1, x2 = x > x2 ? x : x2, y2 = y > y2 ? y : y2;
                    break;
                }
            }
        }
    u32 fa = fx < 0 ? 0 : ram[fy * VRAM_W + fx], fb = fx < 0 ? 0 : ge.cmp[fy * VRAM_W + fx];
    rd_wrote(&ge, 0, 0, VRAM_W, VRAM_H);
    return clampn(snprintf(buf, (size_t)cap,
                           "-- ge: diff %u px (bit 15 only %u, >1 step %u in %d,%d-%d,%d, first %d,%d sw %04X ge %04X)\n",
                           (unsigned)diff, (unsigned)bit15, (unsigned)big, (int)x1, (int)y1, (int)x2, (int)y2, (int)fx,
                           (int)fy, (unsigned)fa, (unsigned)fb),
                  cap);
}

int ge_log(char *buf, int cap)
{
    const GeStats *s = &ge.s;
    u32 kb = s->kicks * prof_bias / 16, kick_us = s->kick_us > kb ? s->kick_us - kb : 0; /* less one clock read each */
    int n = clampn(snprintf(buf, (size_t)cap,
                            "-- ge: tri %u+%u tex, rect %u+%u tex, fill %u, line %u (%u runs); sw tex15 %u texwin %u "
                            "copy %u+%u sw+%u null (%u kpx, %u from ge); pages %u cluts %u+%u in list, readback %u (%u tiles, %u ms), upload %u "
                            "(%u kpx), flush %u; bind %u tex %u clut, kick %u (%u ms), rd check %u; fast %u (%u setups %u ms, %u cached), slow tex %u big %u, flat %u, clipped %u; ring %s (wb %u ms), step %u\n",
                            (unsigned)s->tris, (unsigned)s->ttris, (unsigned)s->rects, (unsigned)s->trects,
                            (unsigned)s->fills, (unsigned)s->lines, (unsigned)s->line_runs, (unsigned)s->sw_tex15,
                            (unsigned)s->sw_texwin,
                            (unsigned)s->ge_copy, (unsigned)s->sw_copy, (unsigned)s->null_copy, (unsigned)(s->copy_px / 1000), (unsigned)s->copy_from_ge,
                            (unsigned)s->pages, (unsigned)s->cluts, (unsigned)s->clut_tmp, (unsigned)s->readbacks, (unsigned)s->read_tiles,
                            (unsigned)(s->read_us / 1000), (unsigned)s->uploads, (unsigned)(s->upload_px / 1000),
                            (unsigned)s->flushes, (unsigned)s->bind_tex, (unsigned)s->bind_clut, (unsigned)s->kicks, (unsigned)(kick_us / 1000),
                            (unsigned)s->rd_checks, (unsigned)s->fast, (unsigned)s->fast_setups, (unsigned)((s32)s->setup_us < 0 ? 0 : s->setup_us / 1000), (unsigned)s->fast_hits, (unsigned)s->slow_tex, (unsigned)s->slow_big, (unsigned)s->flat, (unsigned)s->clipped,
                            fl.cached ? "cached" : "uncached", (unsigned)(s->ring_wb_us / 1000), (unsigned)stall_step),
                   cap);
    memset(&ge.s, 0, sizeof ge.s);
    if (ring_ab)
        fl.want = !fl.cached; /* from the next sync */
    if (ge.compare && n < cap - 1)
        n += compare(buf + n, cap - n);
    return n;
}
