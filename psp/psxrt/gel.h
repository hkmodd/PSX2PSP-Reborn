/*
 * psxrt: GE display-list words, written as pspgu writes them (the pspsdk's libpspgu, read from its
 * objects), with no system call and no state of pspgu's: step 1a of the pure translator
 * (me-translator design note). ge.c builds its lists through these, so the list is a function of
 * the GelList it is given and the GelScissor beside it, on the PSP, on the ME or in the sim.
 * gel_check.c compares every function here against pspgu's at boot.
 *
 * Not here: sceGuStart/Finish/Sync (the host's), Disable(GU_SCISSOR_TEST) (pspgu reads the draw
 * buffer's size for it) and GU_FRAGMENT_2X (pspgu keeps the texture function for it); ge.c uses
 * neither.
 */
#ifndef GEL_H
#define GEL_H

#include <stdint.h>

/* pspgu's GuListHead begins so: the words go at current. unsigned int, as pspgu's (psp-gcc's uint32_t is a long). */
typedef struct {
    unsigned int *start, *current;
} GelList;

/* pspgu's GuContext keeps these as scissor_enable, scissor_start[2], scissor_end[2] (inclusive).
 * gel_scissor stores them; while `on`, it also sends them, and gel_scissor_on sends the stored ones. */
typedef struct {
    int on;
    int32_t x1, y1, x2, y2;
} GelScissor;

static inline uint32_t gel_addr(const void *p) { return (uint32_t)(uintptr_t)p; }

static inline void gel_cmd(GelList *l, uint32_t cmd, uint32_t arg) { *l->current++ = cmd << 24 | (arg & 0xFFFFFF); }

/* sendCommandf: the float's top 24 bits */
static inline void gel_cmdf(GelList *l, uint32_t cmd, float f)
{
    union {
        float f;
        uint32_t u;
    } b = {f};
    *l->current++ = cmd << 24 | b.u >> 8;
}

/* The command of each pspgu state (GU_ALPHA_TEST 0 .. GU_PATCH_FACE 20); 0: not here. */
static const uint8_t gel_state_cmd[21] = {0x22, 0x23, 0,    0x24, 0x21, 0x1D, 0x20, 0x1F, 0x1C, 0x1E, 0x17,
                                          0x18, 0x19, 0x1A, 0x1B, 0x25, 0x26, 0x27, 0x28, 0x51, 0x38};

static inline void gel_enable(GelList *l, int state) { gel_cmd(l, gel_state_cmd[state], 1); }
static inline void gel_disable(GelList *l, int state) { gel_cmd(l, gel_state_cmd[state], 0); }

/* SCISSOR1/2, then REGION1/2 (this pspgu sets the region with the scissor) */
static inline void gel_scissor_send(GelList *l, const GelScissor *s)
{
    uint32_t end = (uint32_t)s->y2 << 10 | (uint32_t)s->x2;
    gel_cmd(l, 0xD4, (uint32_t)s->y1 << 10 | (uint32_t)s->x1);
    gel_cmd(l, 0xD5, end);
    gel_cmd(l, 0x15, 0);
    gel_cmd(l, 0x16, end);
}

/* sceGuEnable(GU_SCISSOR_TEST) */
static inline void gel_scissor_on(GelList *l, GelScissor *s)
{
    s->on = 1;
    gel_scissor_send(l, s);
}

static inline void gel_scissor(GelList *l, GelScissor *s, int x, int y, int w, int h)
{
    s->x1 = x, s->y1 = y, s->x2 = x + w - 1, s->y2 = y + h - 1;
    if (s->on)
        gel_scissor_send(l, s);
}

static inline void gel_alpha_func(GelList *l, int func, int ref, int mask)
{
    gel_cmd(l, 0xDB, ((uint32_t)mask << 16 & 0xFF0000) | (uint32_t)func | ((uint32_t)ref << 8 & 0xFFFF));
}

static inline void gel_stencil_func(GelList *l, int func, int ref, int mask)
{
    gel_cmd(l, 0xDC, ((uint32_t)mask << 16 & 0xFF0000) | (uint32_t)func | ((uint32_t)ref << 8 & 0xFFFF));
}

static inline void gel_stencil_op(GelList *l, int fail, int zfail, int zpass)
{
    gel_cmd(l, 0xDD, (uint32_t)zpass << 16 | (uint32_t)fail | (uint32_t)zfail << 8);
}

static inline void gel_blend_func(GelList *l, int op, int src, int dst, uint32_t srcfix, uint32_t dstfix)
{
    gel_cmd(l, 0xDF, (uint32_t)op << 8 | (uint32_t)src | (uint32_t)dst << 4);
    gel_cmd(l, 0xE0, srcfix);
    gel_cmd(l, 0xE1, dstfix);
}

static inline void gel_depth_mask(GelList *l, int mask) { gel_cmd(l, 0xE7, (uint32_t)mask); }

static inline void gel_pixel_mask(GelList *l, uint32_t mask)
{
    gel_cmd(l, 0xE8, mask);
    gel_cmd(l, 0xE9, mask >> 24);
}

static inline void gel_shade_model(GelList *l, int mode) { gel_cmd(l, 0x50, (uint32_t)mode); }

/* m: the 4x4 matrix row by row, as ScePspIMatrix4 */
static inline void gel_set_dither(GelList *l, const int *m)
{
    for (int r = 0; r < 4; r++, m += 4)
        gel_cmd(l, 0xE2 + (uint32_t)r,
                ((uint32_t)m[0] & 0xF) | ((uint32_t)m[1] << 4 & 0xF0) | ((uint32_t)m[2] << 8 & 0xF00) | ((uint32_t)m[3] << 12 & 0xF000));
}

static inline void gel_offset(GelList *l, int x, int y)
{
    gel_cmd(l, 0x4C, (uint32_t)x << 4);
    gel_cmd(l, 0x4D, (uint32_t)y << 4);
}

static inline void gel_viewport(GelList *l, int cx, int cy, int w, int h)
{
    gel_cmdf(l, 0x42, (float)w * 0.5f);
    gel_cmdf(l, 0x43, (float)h * -0.5f);
    gel_cmdf(l, 0x45, (float)cx);
    gel_cmdf(l, 0x46, (float)cy);
}

static inline void gel_draw_buffer_list(GelList *l, int psm, const void *fbp, int width)
{
    uint32_t a = gel_addr(fbp);
    gel_cmd(l, 0xD2, (uint32_t)psm);
    gel_cmd(l, 0x9C, a);
    gel_cmd(l, 0x9D, (a >> 8 & 0xFF0000) | (uint32_t)width);
}

static inline void gel_tex_wrap(GelList *l, int u, int v) { gel_cmd(l, 0xC7, (uint32_t)v << 8 | (uint32_t)u); }
static inline void gel_tex_filter(GelList *l, int min, int mag) { gel_cmd(l, 0xC6, (uint32_t)mag << 8 | (uint32_t)min); }
static inline void gel_tex_func(GelList *l, int tfx, int tcc) { gel_cmd(l, 0xC9, (uint32_t)tcc << 8 | (uint32_t)tfx); }
static inline void gel_tex_flush(GelList *l) { gel_cmd(l, 0xCB, 0); }

static inline void gel_tex_mode(GelList *l, int tpsm, int maxmips, int a2, int swizzle)
{
    gel_cmd(l, 0xC2, (uint32_t)maxmips << 16 | (uint32_t)a2 << 8 | (uint32_t)swizzle);
    gel_cmd(l, 0xC3, (uint32_t)tpsm);
    gel_tex_flush(l);
}

/* 31 - clz(n & 0x3FF), as pspgu: -1 for 0 */
static inline int gel_log2(int n)
{
    uint32_t v = (uint32_t)n & 0x3FF;
    int r = -1;
    while (v)
        v >>= 1, r++;
    return r;
}

static inline void gel_tex_image(GelList *l, int mip, int w, int h, int tbw, const void *tbp)
{
    uint32_t a = gel_addr(tbp);
    gel_cmd(l, 0xA0 + (uint32_t)mip, a);
    gel_cmd(l, 0xA8 + (uint32_t)mip, (a >> 8 & 0xF0000) | (uint32_t)tbw);
    *l->current++ = (0xB8 + (uint32_t)mip) << 24 | (uint32_t)gel_log2(h) << 8 | (uint32_t)gel_log2(w); /* not cut to 24 bits */
    gel_tex_flush(l);
}

static inline void gel_clut_mode(GelList *l, int cpsm, int shift, int mask, int a3)
{
    gel_cmd(l, 0xC5, (uint32_t)a3 << 16 | (uint32_t)cpsm | (uint32_t)mask << 8 | (uint32_t)shift << 2);
}

static inline void gel_clut_load(GelList *l, int blocks, const void *cbp)
{
    uint32_t a = gel_addr(cbp);
    gel_cmd(l, 0xB0, a);
    gel_cmd(l, 0xB1, a >> 8 & 0xF0000);
    gel_cmd(l, 0xC4, (uint32_t)blocks);
}

/* size bytes inside the list, jumped over */
static inline void *gel_get_memory(GelList *l, int size)
{
    uint8_t *p = (uint8_t *)(l->current + 2), *next = p + (size + 3) / 4 * 4;
    uint32_t a = gel_addr(next);
    l->current[0] = 0x10u << 24 | (a >> 8 & 0xF0000);
    l->current[1] = 0x08u << 24 | (a & 0xFFFFFF);
    l->current = (unsigned int *)next;
    return p;
}

static inline void gel_draw_array(GelList *l, int prim, int vtype, int count, const void *indices, const void *vertices)
{
    if (vtype)
        gel_cmd(l, 0x12, (uint32_t)vtype);
    if (indices) {
        gel_cmd(l, 0x10, gel_addr(indices) >> 8 & 0xF0000);
        gel_cmd(l, 0x02, gel_addr(indices));
    }
    if (vertices) {
        gel_cmd(l, 0x10, gel_addr(vertices) >> 8 & 0xF0000);
        gel_cmd(l, 0x01, gel_addr(vertices));
    }
    gel_cmd(l, 0x04, (uint32_t)prim << 16 | (uint32_t)count);
}

static inline void gel_copy_image(GelList *l, int psm, int sx, int sy, int w, int h, int srcw, const void *src, int dx, int dy,
                                  int destw, const void *dest)
{
    uint32_t s = gel_addr(src), d = gel_addr(dest);
    gel_cmd(l, 0xB2, s);
    gel_cmd(l, 0xB3, (s >> 8 & 0xFF0000) | (uint32_t)srcw);
    gel_cmd(l, 0xEB, (uint32_t)sy << 10 | (uint32_t)sx);
    gel_cmd(l, 0xB4, d);
    gel_cmd(l, 0xB5, (d >> 8 & 0xFF0000) | (uint32_t)destw);
    gel_cmd(l, 0xEC, (uint32_t)dy << 10 | (uint32_t)dx);
    gel_cmd(l, 0xEE, (uint32_t)(h - 1) << 10 | (uint32_t)(w - 1));
    gel_cmd(l, 0xEA, psm == 3); /* GU_PSM_8888: 32-bit texels */
}

#endif
