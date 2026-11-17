/*
 * psxrt: a GE translator dump (step 2 / probe 0 of the me-translator design note). Everything
 * ge.c is given over some frames of a game, recorded in the sim (ge_host_pc.c,
 * PSX2PSP_SIM_GE_DUMP), so that the same translation runs again anywhere: on a PC, and on the
 * PSP's two CPUs (psp/me_probe), each time with the same GE list hashes as the sim's.
 *
 * Little-endian u32 words: a header, then events [type, n, n words]. The GPU's state and VRAM
 * go in as they change, before the call that sees them; each renderer call carries its arguments
 * and, last, what it returned in the sim. A frame end carries the sim's list hash for the frame.
 */
#ifndef GE_DUMP_H
#define GE_DUMP_H

#include "core/psx.h"

#define GEDP_MAGIC 0x50444547u /* "GEDP" */
#define GEDP_VERSION 1u

/* header: magic, version, first frame, frames, VRAM_W, VRAM_H, GEDP_GPU_WORDS, GEDP_PRIM_WORDS */
#define GEDP_HEAD 8

enum {
    GEDP_STATE = 1, /* the Gpu fields below, and gpu_writes */
    GEDP_VRAM,      /* spans: y, x, n, then n pixels two to a word (low half first) */
    GEDP_INIT,      /* ge.c starts here, cold: the replay's ge_init; then a WROTE of all VRAM */
    GEDP_TRI,       /* prim, 3 vertices, ret */
    GEDP_QUAD,      /* prim, 4 vertices, ret */
    GEDP_RECT,      /* prim, x, y, w, h, vertex, flip_x, flip_y, ret */
    GEDP_LINE,      /* prim, 2 vertices, ret */
    GEDP_FILL,      /* x, y, w, h, color, ret */
    GEDP_COPY,      /* sx, sy, dx, dy, w, h, mask_check, mask_set, ret */
    GEDP_READ,      /* x, y, w, h */
    GEDP_WROTE,     /* x, y, w, h */
    GEDP_POLY,      /* the command's words, ret */
    GEDP_PACKET,    /* the packet's words, ret */
    GEDP_FRAME,     /* frame, lists, words, prims, calls, misses, unknown, hash hi, lo, has memory hash, hi, lo */
    GEDP_END
};

#define GEDP_FRAME_WORDS 12

/* The Gpu's fields but its two pointers and the u64 frame (two words, low first), in order. */
#define GEDP_GPU_FIELDS(X, A)                                                                                     \
    X(stat) X(gpuread) A(cmd, GPU_FIFO * 2) X(ncmd) X(need) X(polyline) X(xfer) X(xx) X(xy) X(xw) X(xh) X(xi) X(xr) \
    X(texpage) X(tex_flip_x) X(tex_flip_y) X(texwin) X(tw_mask_x) X(tw_mask_y) X(tw_off_x) X(tw_off_y) X(clip_x1)  \
    X(clip_y1) X(clip_x2) X(clip_y2) X(off_x) X(off_y) X(mask_set) X(mask_check) X(disp_x) X(disp_y) X(hrange1)    \
    X(hrange2) X(vrange1) X(vrange2) X(disp_enabled) X(dma_dir) X(us_draw) A(n_kind, 6) A(us_kind, 6) X(frags)     \
    X(tested) X(flips) A(load_rows, 16) X(load_x0) X(load_x1)

#define GEDP_ONE_(f) +1
#define GEDP_ARR_(f, n) +(n)
/* + frame (2) + gpu_writes */
#define GEDP_GPU_WORDS (0 GEDP_GPU_FIELDS(GEDP_ONE_, GEDP_ARR_) + 3)

static inline void gedp_gpu_save(const Gpu *g, u32 *w)
{
#define GEDP_SAVE_(f) *w++ = (u32)g->f;
#define GEDP_SAVEA_(f, n)                                                                                         \
    for (int i_ = 0; i_ < (n); i_++)                                                                              \
        *w++ = (u32)g->f[i_];
    GEDP_GPU_FIELDS(GEDP_SAVE_, GEDP_SAVEA_)
    *w++ = (u32)g->frame, *w++ = (u32)(g->frame >> 32);
    *w = gpu_writes;
#undef GEDP_SAVE_
#undef GEDP_SAVEA_
}

static inline void gedp_gpu_load(Gpu *g, const u32 *w)
{
/* each field from its word (an int field takes its u32 back as it was saved) */
#define GEDP_LOAD_(f) g->f = *w++;
#define GEDP_LOADA_(f, n)                                                                                         \
    for (int i_ = 0; i_ < (n); i_++)                                                                              \
        g->f[i_] = *w++;
    GEDP_GPU_FIELDS(GEDP_LOAD_, GEDP_LOADA_)
    g->frame = (u64)w[0] | (u64)w[1] << 32;
    gpu_writes = w[2];
#undef GEDP_LOAD_
#undef GEDP_LOADA_
}

/* GpuPrim and GpuVtx, field by field */
#define GEDP_PRIM_WORDS 17
#define GEDP_VTX_WORDS 7

static inline u32 *gedp_prim_save(u32 *w, const GpuPrim *p)
{
    *w++ = (u32)p->textured, *w++ = (u32)p->raw, *w++ = (u32)p->semi, *w++ = (u32)p->gouraud, *w++ = (u32)p->dither;
    *w++ = p->clut_x, *w++ = p->clut_y, *w++ = p->tpx, *w++ = p->tpy, *w++ = p->depth, *w++ = p->abr;
    *w++ = p->tw_and_u, *w++ = p->tw_or_u, *w++ = p->tw_and_v, *w++ = p->tw_or_v;
    *w++ = (u32)p->mask_check, *w++ = p->mask_set;
    return w;
}

static inline const u32 *gedp_prim_load(const u32 *w, GpuPrim *p)
{
    p->textured = (int)w[0], p->raw = (int)w[1], p->semi = (int)w[2], p->gouraud = (int)w[3], p->dither = (int)w[4];
    p->clut_x = w[5], p->clut_y = w[6], p->tpx = w[7], p->tpy = w[8], p->depth = w[9], p->abr = w[10];
    p->tw_and_u = w[11], p->tw_or_u = w[12], p->tw_and_v = w[13], p->tw_or_v = w[14];
    p->mask_check = (int)w[15], p->mask_set = (u16)w[16];
    return w + GEDP_PRIM_WORDS;
}

static inline u32 *gedp_vtx_save(u32 *w, const GpuVtx *v, int n)
{
    for (int i = 0; i < n; i++, v++)
        *w++ = (u32)v->x, *w++ = (u32)v->y, *w++ = (u32)v->r, *w++ = (u32)v->g, *w++ = (u32)v->b, *w++ = (u32)v->u,
        *w++ = (u32)v->v;
    return w;
}

static inline const u32 *gedp_vtx_load(const u32 *w, GpuVtx *v, int n)
{
    for (int i = 0; i < n; i++, v++, w += GEDP_VTX_WORDS)
        v->x = (s32)w[0], v->y = (s32)w[1], v->r = (s32)w[2], v->g = (s32)w[3], v->b = (s32)w[4], v->u = (s32)w[5],
        v->v = (s32)w[6];
    return w;
}

/* ge_host_pc.c, for the sim and a replay alike: ge.c from cold (the replay's GEDP_INIT), and a
 * frame's end with what GEDP_FRAME carries. */
typedef struct {
    u32 frame, lists, words, prims, calls, misses, unknown;
    u64 h;
    int mem; /* mh: every 60 frames, a hash of all the memory the lists address */
    u64 mh;
} GehFrame;
void geh_start(Psx *psx, int mode);
void geh_frame(u32 frame, GehFrame *o);
void geh_frame_log(Psx *psx, const GehFrame *o);

/* The words of a polygon command (GP0 20-3F), as rd_packet counts them. */
static inline u32 gedp_poly_words(u32 c0)
{
    u32 cmd = c0 >> 24, quad = cmd >> 3 & 1, tex = cmd >> 2 & 1, gour = cmd >> 4 & 1, nv = quad ? 4 : 3;
    return 1 + nv * (1 + tex) + (gour ? nv - 1 : 0);
}

#endif
