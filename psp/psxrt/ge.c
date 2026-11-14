/*
 * psxrt: the GE renderer of the PS1 GPU, slices S4a-S4b of docs/recomp/S4.md.
 *
 * The core (core/gpu.c) decodes the commands and offers each primitive here first. Triangles,
 * rectangles and fills are drawn by the GE into its copy of PS1 VRAM (PSP VRAM 0x000000, 5551,
 * stride 1024); lines, copies, 15-bit textures and texture windows stay in software on RAM's
 * copy. The two copies are kept in step per 64x32 tile: `newer` marks tiles the GE drew since RAM
 * last saw them, `read` brings all of VRAM back (one block transfer) before the CPU reads one of
 * those, `wrote` sends the CPU's pixels up as textured sprites. Textures come from RAM's copy:
 * 4/8-bit pages are copied into a cache of T4/T8 textures, CLUTs are converted into the list.
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
    u32 tris, ttris, rects, trects, fills, sw_tex15, sw_texwin, sw_line, sw_copy, pages, cluts;
    u32 readbacks, read_tiles, uploads, upload_px, flushes;
} GeStats;

/* A 4/8-bit texture page as a 256x256 T4/T8 texture. */
#define PAGE_SLOTS 32
#define PAGE_BYTES (256 * 256)
typedef struct {
    u32 key, stamp; /* key 0: empty; stamp: the page's tile generations, summed */
    u32 used, age;  /* used: the list (ge.syncs) that last drew with it */
    u8 *data;
} Page;

static unsigned int __attribute__((aligned(64))) list[LIST_BYTES / 4];
static int list_open, state_valid;

static struct {
    Psx *psx;
    GpuRenderer r;
    int compare;
    u16 *cmp;                  /* compare mode: the GE's copy, read back */
    u16 newer[TILES_Y];        /* per tile row, a bit per tile column: the GE's copy is newer */
    u32 gen[TILES_Y][TILES_X]; /* bumped whenever RAM's copy of the tile changes */
    /* GE state as last sent */
    int sc_x1, sc_y1, sc_x2, sc_y2;
    int st_check, st_set, blend, dither; /* -2: unknown */
    const void *tex_sent, *clut_sent;    /* NULL: unknown */
    int texfn;                           /* 0 modulate, 1 replace, -1 unknown */
    u32 fill_color;
    /* texture cache */
    Page pages[PAGE_SLOTS];
    int npages;
    u32 syncs, ages; /* syncs: counts the lists run */
    struct {
        u32 key, stamp, syncs;
        const u32 *ptr;
        int flags;
    } clut; /* the last CLUT converted, in the list `syncs` */
    GeStats s;
} ge;

/* The PS1 dither matrix, row = y & 3, column = x & 3; the GE adds it to 8-bit colours before
 * they are cut to 5 bits, as the PS1 does. */
static const ScePspIMatrix4 ps1_dither = {{-4, 0, -3, 1}, {2, -2, 3, -1}, {-3, 1, -4, 0}, {3, -1, 2, -2}};

#define DONE (ge.compare ? GPU_BOTH : GPU_DRAWN)

/* The list runs while it is written, but its stall address moves every STALL_STEP bytes and at
 * the end, not after every draw as GU_DIRECT does (one system call per draw): a GU_SEND list,
 * queued here with its stall at the start. pspgu writes it through the uncached mirror. */
#define STALL_STEP (16 * 1024)
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
}

/* Lets the GE run what is written so far, once STALL_STEP more bytes are there. */
static void ge_kick(void)
{
    u32 n = (u32)sceGuCheckList();
    if (n - list_stall >= STALL_STEP) {
        list_stall = n;
        sceGeListUpdateStallAddr(list_id, LIST_UNCACHED + n);
    }
}

void ge_sync(void)
{
    if (list_open) {
        u32 n = (u32)sceGuFinish();
        sceGeListUpdateStallAddr(list_id, LIST_UNCACHED + n);
        sceGuSync(0, 0);
        list_open = 0;
        ge.syncs++;
    }
    state_valid = 0;
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
    state_valid = 1;
}

/* Before renderer commands: a list with room, in the renderer's state. */
static void begin(void)
{
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
    u32 v0 = (u32)v & (size - 1), v1 = v0 + (u32)n - 1;
    u64 m = ((2ull << (v1 / tile)) - 1) & ~((1ull << (v0 / tile)) - 1);
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
    u16 c = span(x1, x2 - x1 + 1, VRAM_W, TILE_W), r = span(y1, y2 - y1 + 1, VRAM_H, TILE_H);
    for (int ty = 0; ty < TILES_Y; ty++)
        if (r >> ty & 1)
            ge.newer[ty] |= c;
}

static void bump(s32 x, s32 y, s32 w, s32 h)
{
    u16 c = span(x, w, VRAM_W, TILE_W), r = span(y, h, VRAM_H, TILE_H);
    for (int ty = 0; ty < TILES_Y; ty++)
        if (r >> ty & 1)
            for (int tx = 0; tx < TILES_X; tx++)
                if (c >> tx & 1)
                    ge.gen[ty][tx]++;
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

/* A piece of RAM's copy drawn as a 5551 sprite (the texture starts 16-byte aligned, at most 512
 * texels wide). Two passes: every pixel with bit 15 clear, then the pixels whose bit 15 is set
 * again, through the alpha test and the stencil. */
static void upload_part(s32 x, s32 y, s32 w, s32 h)
{
    s32 x8 = x & ~7, u0 = x - x8;
    u16 *tex = ge.psx->gpu.vram + y * VRAM_W + x8;
    sceGuTexMode(GU_PSM_5551, 0, 0, 0);
    /* The oracle checks a texture in RAM for changes only when its address register changes, so
     * a second upload from the same place would draw the first one's pixels; a different address
     * first makes it look. */
    sceGuTexImage(0, 1, 1, 8, tex + 8);
    sceGuTexImage(0, 1 << pow2_log(u0 + w), 1 << pow2_log(h), VRAM_W, tex);
    sceGuTexFlush();
    UpVtx *v = (UpVtx *)sceGuGetMemory(2 * sizeof(UpVtx));
    v[0] = (UpVtx){(u16)u0, 0, (s16)x, (s16)y, 0};
    v[1] = (UpVtx){(u16)(u0 + w), (u16)h, (s16)(x + w), (s16)(y + h), 0};
    scissor(x, y, x + w - 1, y + h - 1);
    stencil(0, 0);
    sceGuDrawArray(GU_SPRITES, GU_TEXTURE_16BIT | GU_VERTEX_16BIT | GU_TRANSFORM_2D, 2, 0, v);
    stencil(0, 1);
    sceGuEnable(GU_ALPHA_TEST);
    sceGuDrawArray(GU_SPRITES, GU_TEXTURE_16BIT | GU_VERTEX_16BIT | GU_TRANSFORM_2D, 2, 0, v);
    sceGuDisable(GU_ALPHA_TEST);
}

/* A block transfer into the framebuffer would be exact on the PSP, but the oracle turns it into a
 * lossy redraw of the pixels; a textured sprite is exact on both. */
static void upload(s32 x, s32 y, s32 w, s32 h)
{
    u16 *ram = ge.psx->gpu.vram;
    sceKernelDcacheWritebackRange(ram + y * VRAM_W + x, (u32)((h - 1) * VRAM_W + w) * 2);
    blend(-1);
    dither(0);
    sceGuTexFunc(GU_TFX_REPLACE, GU_TCC_RGBA);
    sceGuTexFilter(GU_NEAREST, GU_NEAREST);
    sceGuAlphaFunc(GU_GREATER, 0, 0xFF);
    sceGuEnable(GU_TEXTURE_2D);
    for (s32 cx = x; cx < x + w;) {
        s32 cw = x + w - cx, room = 512 - (cx & 7);
        cw = cw > room ? room : cw;
        upload_part(cx, y, cw, h);
        cx += cw;
    }
    sceGuDisable(GU_TEXTURE_2D);
    ge.tex_sent = NULL, ge.texfn = -1;
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
    begin();
    sceKernelDcacheWritebackInvalidateRange(ram, VRAM_W * VRAM_H * 2);
    sceGuCopyImage(GU_PSM_5551, 0, 0, VRAM_W, VRAM_H, VRAM_W, VRAM_PTR, 0, 0, VRAM_W, ram);
    ge_sync();
    for (int ty = 0; ty < TILES_Y; ty++) {
        for (int tx = 0; tx < TILES_X; tx++)
            if (ge.newer[ty] >> tx & 1)
                ge.gen[ty][tx]++, ge.s.read_tiles++;
        ge.newer[ty] = 0;
    }
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
    u32 key = 1 + (p->tpx / 64 | p->tpy / 256 << 4 | p->depth << 5), st = stamp((s32)p->tpx, (s32)p->tpy, hw, 256);
    Page *s = NULL;
    for (int i = 0; i < ge.npages && !s; i++)
        if (ge.pages[i].key == key)
            s = &ge.pages[i];
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
        s->key = key, s->stamp = st;
        if (ge.tex_sent == s->data)
            ge.tex_sent = NULL;
        ge.s.pages++;
    }
    s->used = ge.syncs, s->age = ++ge.ages;
    return s->data;
}

/* The CLUT as 8888 in the list, its alpha saying what the texel means: 0 for 0x0000 (not
 * drawn), 0x80 with STP set, 0xFF otherwise. flags: 1 some texels without STP, 2 some with. */
static const u32 *clut(const GpuPrim *p, int *flags)
{
    int n = p->depth ? 256 : 16;
    u32 key = 1 + (p->clut_x / 16 | p->clut_y << 6 | p->depth << 15), st = stamp((s32)p->clut_x, (s32)p->clut_y, n, 1);
    if (ge.clut.key != key || ge.clut.stamp != st || ge.clut.syncs != ge.syncs) {
        u32 *c = (u32 *)(((u32)sceGuGetMemory(n * 4 + 16) + 15) & ~15u);
        const u16 *row = ge.psx->gpu.vram + p->clut_y * VRAM_W;
        int f = 0;
        for (int i = 0; i < n; i++) {
            u32 t = row[(p->clut_x + (u32)i) & (VRAM_W - 1)];
            u32 a = !t ? 0 : t & 0x8000 ? 0x80 : 0xFF;
            f |= !t ? 0 : t & 0x8000 ? 2 : 1;
            c[i] = a << 24 | (t >> 10 & 31) << 19 | (t >> 5 & 31) << 11 | (t & 31) << 3;
        }
        ge.clut.key = key, ge.clut.stamp = st, ge.clut.syncs = ge.syncs, ge.clut.ptr = c, ge.clut.flags = f;
        ge.s.cluts++;
    }
    *flags = ge.clut.flags;
    return ge.clut.ptr;
}

static void bind(const GpuPrim *p, const u8 *pg, const u32 *cl)
{
    if (cl != ge.clut_sent) {
        sceGuClutMode(GU_PSM_8888, 0, 0xFF, 0);
        sceGuClutLoad(p->depth ? 32 : 2, cl); /* blocks of 8 entries */
        ge.clut_sent = cl;
    }
    if (pg != ge.tex_sent) {
        sceGuTexMode(p->depth ? GU_PSM_T8 : GU_PSM_T4, 0, 0, 0);
        sceGuTexImage(0, 1, 1, 64, pg + 16); /* another address first: see upload_part */
        sceGuTexImage(0, 256, 256, 256, pg);
        sceGuTexFlush();
        ge.tex_sent = pg;
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
    s32 hw = p->depth ? 128 : 64;
    rd_read(&ge, (s32)p->tpx, (s32)p->tpy, hw, 256);
    rd_read(&ge, (s32)p->clut_x, (s32)p->clut_y, p->depth ? 256 : 16, 1);
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
    sceGuAlphaFunc(func, ref, 0xFF);
    blend(mode);
    stencil(p->mask_check, set);
    sceGuDrawArray(prim, TEX_VTX, n, NULL, v);
}

/* Texels without STP are opaque and written with the mask setting; texels with STP are blended
 * when the primitive is semi-transparent and set bit 15. A CLUT with both takes two passes. */
static void tex_draw(const GpuPrim *p, int flags, int prim, int n, const TexVtx *v)
{
    int semi = p->semi ? (int)p->abr : -1, set = p->mask_set != 0;
    sceGuEnable(GU_TEXTURE_2D);
    sceGuEnable(GU_ALPHA_TEST);
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
    sceGuDisable(GU_ALPHA_TEST);
    sceGuDisable(GU_TEXTURE_2D);
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

static int rd_triangle(void *self, const GpuPrim *p, const GpuVtx *v0, const GpuVtx *v1, const GpuVtx *v2)
{
    (void)self;
    const GpuVtx *v[3] = {v0, v1, v2};
    s32 minx = v0->x, maxx = v0->x, miny = v0->y, maxy = v0->y;
    for (int i = 1; i < 3; i++) {
        minx = v[i]->x < minx ? v[i]->x : minx, maxx = v[i]->x > maxx ? v[i]->x : maxx;
        miny = v[i]->y < miny ? v[i]->y : miny, maxy = v[i]->y > maxy ? v[i]->y : maxy;
    }
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
        ge.s.ttris++;
    else
        ge.s.tris++;
    prim_state(p, x1, y1, x2, y2);
    mark(bx1, by1, bx2, by2);
    ClipVtx a[8], b[8], *in = a, *out = b, *t;
    for (int i = 0; i < 3; i++)
        a[i] = (ClipVtx){(float)v[i]->x, (float)v[i]->y, (float)v[i]->r, (float)v[i]->g, (float)v[i]->b, (float)v[i]->u, (float)v[i]->v};
    int n = 3;
    /* left of or above the VRAM origin: the GE's handling is unknown, so cut at -0.25 */
    if (minx < 0)
        n = clip_axis(in, n, out, 0), t = in, in = out, out = t;
    if (miny < 0 && n >= 3)
        n = clip_axis(in, n, out, 1), t = in, in = out, out = t;
    if (n < 3)
        return DONE;
    /* +0.5: the GE samples pixel centres, the PS1 integer corners; texels too, where the PS1
     * rounds and the GE floors */
    if (!p->textured) {
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

static int rd_line(void *self, const GpuPrim *p, const GpuVtx *a, const GpuVtx *b)
{
    (void)self, (void)p, (void)a, (void)b;
    ge.s.sw_line++;
    return GPU_SW;
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

/* Copies stay in software: the oracle does not carry out a block transfer between two places of
 * this framebuffer (the destination keeps its old pixels). The source is rarely a place the GE
 * drew, so a copy costs an upload of the destination. */
static int rd_copy(void *self, s32 sx, s32 sy, s32 dx, s32 dy, s32 w, s32 h, int mask_check, u16 mask_set)
{
    (void)self, (void)sx, (void)sy, (void)dx, (void)dy, (void)w, (void)h, (void)mask_check, (void)mask_set;
    ge.s.sw_copy++;
    return GPU_SW;
}

/* ---- setup and log ---- */

void ge_init(Psx *psx, int compare)
{
    ge.psx = psx;
    ge.compare = compare;
    ge.r = (GpuRenderer){&ge, rd_triangle, rd_rect, rd_line, rd_fill, rd_copy, rd_read, rd_wrote};
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
    ge.syncs = 1; /* no slot (used 0) is in the list */
    psx_log(psx, "ge: %d texture page slots", ge.npages);
    rd_fill(&ge, 0, 0, VRAM_W, VRAM_H, 0); /* also makes the oracle create the framebuffer */
    ge_sync();
    memset(ge.newer, 0, sizeof ge.newer); /* both copies are zero */
    memset(&ge.s, 0, sizeof ge.s);
    psx->gpu.renderer = &ge.r;
}

static int clampn(int n, int cap) { return n < 0 ? 0 : n >= cap ? cap - 1 : n; }

/* Compare mode: the GE's copy against RAM's, then RAM's sent up so each second starts equal. */
static int compare(char *buf, int cap)
{
    if (ge.psx->gpu.xfer == 1)
        return 0; /* a transfer is half way: RAM is ahead */
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
    int n = clampn(snprintf(buf, (size_t)cap,
                            "-- ge: tri %u+%u tex, rect %u+%u tex, fill %u; sw tex15 %u texwin %u line %u copy %u; "
                            "pages %u cluts %u, readback %u (%u tiles), upload %u (%u kpx), flush %u\n",
                            (unsigned)s->tris, (unsigned)s->ttris, (unsigned)s->rects, (unsigned)s->trects,
                            (unsigned)s->fills, (unsigned)s->sw_tex15, (unsigned)s->sw_texwin, (unsigned)s->sw_line,
                            (unsigned)s->sw_copy, (unsigned)s->pages, (unsigned)s->cluts, (unsigned)s->readbacks,
                            (unsigned)s->read_tiles, (unsigned)s->uploads, (unsigned)(s->upload_px / 1000),
                            (unsigned)s->flushes),
                   cap);
    memset(&ge.s, 0, sizeof ge.s);
    if (ge.compare && n < cap - 1)
        n += compare(buf + n, cap - n);
    return n;
}
