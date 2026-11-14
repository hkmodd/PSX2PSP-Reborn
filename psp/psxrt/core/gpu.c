/*
 * psxrt core: the GPU. GP0/GP1 command decoding, VRAM transfers and a software rasterizer that
 * follows the hardware rules: top-left fill convention, texture pages and CLUTs, texture windows,
 * modulation (0x80 = 1.0), the four semi-transparency modes, 4x4 dithering and mask bits.
 */
#include "psx.h"

#include <string.h>

#define VRAM(x, y) psx->gpu.vram[((y) & 511) * VRAM_W + ((x) & 1023)]

typedef GpuVtx Vtx;
typedef GpuPrim Prim;

/* With a host renderer, the software path brings the renderer's pixels into `vram` before it
 * reads them and hands back what it wrote (inclusive corners). */
static void sw_read(Psx *psx, s32 x0, s32 y0, s32 x1, s32 y1)
{
    const GpuRenderer *r = psx->gpu.renderer;
    if (r)
        r->read(r->self, x0, y0, x1 - x0 + 1, y1 - y0 + 1);
}

static void sw_wrote(Psx *psx, s32 x0, s32 y0, s32 x1, s32 y1)
{
    const GpuRenderer *r = psx->gpu.renderer;
    if (r)
        r->wrote(r->self, x0, y0, x1 - x0 + 1, y1 - y0 + 1);
}

/* The texture page and CLUT a textured primitive reads. */
static void sw_read_texture(Psx *psx, const Prim *p)
{
    if (!p->textured)
        return;
    s32 w = p->depth == 0 ? 64 : p->depth == 1 ? 128 : 256;
    sw_read(psx, (s32)p->tpx, (s32)p->tpy, (s32)p->tpx + w - 1, (s32)p->tpy + 255);
    if (p->depth < 2)
        sw_read(psx, (s32)p->clut_x, (s32)p->clut_y, (s32)p->clut_x + (p->depth ? 255 : 15), (s32)p->clut_y);
}

/* The per-pixel path is inlined into the loops that walk the pixels. */
#ifdef _MSC_VER
#define PIXEL static __forceinline
#else
#define PIXEL static inline __attribute__((always_inline))
#endif

static const s8 dither[4][4] = {{-4, 0, -3, 1}, {2, -2, 3, -1}, {-3, 1, -4, 0}, {3, -1, 2, -2}};

static s32 sext11(u32 v) { return (s32)(v << 21) >> 21; }

static int cmd_words(u32 c, int *poly_line)
{
    *poly_line = 0;
    if (c >= 0x20 && c < 0x40) {
        int verts = (c & 8) ? 4 : 3, tex = (c >> 2) & 1, gour = (c >> 4) & 1;
        return 1 + verts * (1 + tex) + (gour ? verts - 1 : 0);
    }
    if (c >= 0x40 && c < 0x60) {
        if (c & 8) {
            *poly_line = 1;
            return 0;
        }
        return (c & 0x10) ? 4 : 3;
    }
    if (c >= 0x60 && c < 0x80)
        return 2 + ((c >> 2) & 1) + (((c >> 3) & 3) == 0);
    if (c >= 0x80 && c < 0xA0)
        return 4;
    if (c >= 0xA0 && c < 0xE0)
        return 3;
    if (c == 0x02)
        return 3;
    return 1;
}

static void set_texpage(Psx *psx, u32 tp)
{
    Gpu *g = &psx->gpu;
    g->texpage = tp & 0x9FF;
    g->stat = (g->stat & ~0x87FFu) | (tp & 0x7FF) | ((tp & 0x800) << 4);
}

static void prim_from(Psx *psx, Prim *p, u32 cmd, u32 clut, u32 tp)
{
    p->textured = (cmd >> 2) & 1 && !(cmd >= 0x40 && cmd < 0x60);
    p->raw = cmd & 1;
    p->semi = (cmd >> 1) & 1;
    p->clut_x = (clut & 0x3F) * 16;
    p->clut_y = (clut >> 6) & 0x1FF;
    p->tpx = (tp & 0xF) * 64;
    p->tpy = ((tp >> 4) & 1) * 256;
    p->abr = (tp >> 5) & 3;
    p->depth = (tp >> 7) & 3;
    const Gpu *g = &psx->gpu;
    p->tw_and_u = 0xFF & ~(u32)(g->tw_mask_x * 8), p->tw_or_u = (u32)(g->tw_off_x & g->tw_mask_x) * 8;
    p->tw_and_v = 0xFF & ~(u32)(g->tw_mask_y * 8), p->tw_or_v = (u32)(g->tw_off_y & g->tw_mask_y) * 8;
    p->mask_check = g->mask_check;
    p->mask_set = g->mask_set ? 0x8000 : 0;
    p->gouraud = p->dither = 0;
}

PIXEL u16 texel(Psx *psx, const Prim *p, u32 u, u32 v)
{
    u = (u & p->tw_and_u) | p->tw_or_u;
    v = (v & p->tw_and_v) | p->tw_or_v;
    switch (p->depth) {
    case 0: {
        u16 w = VRAM(p->tpx + u / 4, p->tpy + v);
        return VRAM(p->clut_x + ((w >> ((u & 3) * 4)) & 0xF), p->clut_y);
    }
    case 1: {
        u16 w = VRAM(p->tpx + u / 2, p->tpy + v);
        return VRAM(p->clut_x + ((w >> ((u & 1) * 8)) & 0xFF), p->clut_y);
    }
    default:
        return VRAM(p->tpx + u, p->tpy + v);
    }
}

/* Writes one pixel. r,g,b are 8-bit (modulated) unless `tex15` is a raw texel. */
PIXEL void plot(Psx *psx, const Prim *p, int x, int y, s32 r, s32 g, s32 b, int dith, int semi, u16 mask_bit)
{
    u16 *d = &VRAM(x, y);
    if (p->mask_check && (*d & 0x8000))
        return;
    if (dith) {
        int k = dither[y & 3][x & 3];
        r += k, g += k, b += k;
    }
    r = r < 0 ? 0 : r > 255 ? 255 : r;
    g = g < 0 ? 0 : g > 255 ? 255 : g;
    b = b < 0 ? 0 : b > 255 ? 255 : b;
    s32 fr = r >> 3, fg = g >> 3, fb = b >> 3;
    if (semi) {
        s32 br = *d & 31, bg = (*d >> 5) & 31, bb = (*d >> 10) & 31;
        switch (p->abr) {
        case 0: fr = (br + fr) >> 1, fg = (bg + fg) >> 1, fb = (bb + fb) >> 1; break;
        case 1: fr += br, fg += bg, fb += bb; break;
        case 2: fr = br - fr, fg = bg - fg, fb = bb - fb; break;
        case 3: fr = br + (fr >> 2), fg = bg + (fg >> 2), fb = bb + (fb >> 2); break;
        }
        fr = fr < 0 ? 0 : fr > 31 ? 31 : fr;
        fg = fg < 0 ? 0 : fg > 31 ? 31 : fg;
        fb = fb < 0 ? 0 : fb > 31 ? 31 : fb;
    }
    *d = (u16)(fr | fg << 5 | fb << 10 | mask_bit | p->mask_set);
}

/* Shades one fragment: texture lookup, modulation, transparency. */
PIXEL void fragment(Psx *psx, const Prim *p, int x, int y, s32 r, s32 g, s32 b, s32 u, s32 v, int dith)
{
    psx->gpu.frags++;
    if (!p->textured) {
        plot(psx, p, x, y, r, g, b, dith, p->semi, 0);
        return;
    }
    u16 t = texel(psx, p, (u32)u, (u32)v);
    if (!t)
        return;
    s32 tr = (t & 31) << 3, tg = ((t >> 5) & 31) << 3, tb = ((t >> 10) & 31) << 3;
    if (!p->raw) {
        tr = (tr * r) >> 7, tg = (tg * g) >> 7, tb = (tb * b) >> 7;
    } else
        dith = 0;
    plot(psx, p, x, y, tr, tg, tb, dith, p->semi && (t & 0x8000), t & 0x8000);
}

static s64 edge(const Vtx *a, const Vtx *b, s32 x, s32 y) { return (s64)(b->x - a->x) * (y - a->y) - (s64)(b->y - a->y) * (x - a->x); }

static int top_left(const Vtx *a, const Vtx *b)
{
    s32 dx = b->x - a->x, dy = b->y - a->y;
    return dy < 0 || (dy == 0 && dx > 0);
}

static void triangle(Psx *psx, const Prim *p, Vtx v0, Vtx v1, Vtx v2)
{
    Gpu *g = &psx->gpu;
    s64 area = edge(&v0, &v1, v2.x, v2.y);
    if (!area)
        return;
    if (area < 0) {
        Vtx t = v1;
        v1 = v2, v2 = t;
        area = -area;
    }
    s32 minx = v0.x, maxx = v0.x, miny = v0.y, maxy = v0.y;
    const Vtx *vs[3] = {&v0, &v1, &v2};
    for (int i = 1; i < 3; i++) {
        minx = vs[i]->x < minx ? vs[i]->x : minx, maxx = vs[i]->x > maxx ? vs[i]->x : maxx;
        miny = vs[i]->y < miny ? vs[i]->y : miny, maxy = vs[i]->y > maxy ? vs[i]->y : maxy;
    }
    if (maxx - minx > 1023 || maxy - miny > 511)
        return;
    const GpuRenderer *rd = g->renderer;
    int how = rd ? rd->triangle(rd->self, p, &v0, &v1, &v2) : GPU_SW;
    if (how == GPU_DRAWN)
        return;
    if (minx < g->clip_x1) minx = g->clip_x1;
    if (miny < g->clip_y1) miny = g->clip_y1;
    if (maxx > g->clip_x2) maxx = g->clip_x2;
    if (maxy > g->clip_y2) maxy = g->clip_y2;
    if (minx > maxx || miny > maxy)
        return;
    if (how == GPU_SW) {
        sw_read_texture(psx, p);
        sw_read(psx, minx, miny, maxx, maxy);
    }
    /* attribute planes: a(x,y) = a0 + dadx*(x-x0) + dady*(y-y0), 16.16 */
    s32 a0[5] = {v0.r, v0.g, v0.b, v0.u, v0.v}, a1[5] = {v1.r, v1.g, v1.b, v1.u, v1.v}, a2[5] = {v2.r, v2.g, v2.b, v2.u, v2.v};
    s64 dx[5], dy[5];
    for (int k = 0; k < 5; k++) {
        s64 d1 = a1[k] - a0[k], d2 = a2[k] - a0[k];
        dx[k] = ((d1 * (v2.y - v0.y) - d2 * (v1.y - v0.y)) << 16) / area;
        dy[k] = ((d2 * (v1.x - v0.x) - d1 * (v2.x - v0.x)) << 16) / area;
    }
    int b0 = top_left(&v1, &v2) ? 0 : 1, b1 = top_left(&v2, &v0) ? 0 : 1, b2 = top_left(&v0, &v1) ? 0 : 1;
    int dith = p->dither;
    /* Edge functions step by constants; the bounding box is at most 1024x512, so they fit in 32
     * bits. A row of a triangle is one span: the x where all three edges pass, found by division
     * instead of testing every pixel of the box. */
    s32 ex[3] = {v1.y - v2.y, v2.y - v0.y, v0.y - v1.y}, ey[3] = {v2.x - v1.x, v0.x - v2.x, v1.x - v0.x};
    s32 r[3] = {(s32)edge(&v1, &v2, minx, miny), (s32)edge(&v2, &v0, minx, miny), (s32)edge(&v0, &v1, minx, miny)};
    s32 bias[3] = {b0, b1, b2};
    s64 ar[5];
    u32 ax[5];
    for (int k = 0; k < 5; k++) {
        ar[k] = (s64)(minx - v0.x) * dx[k] + (s64)(miny - v0.y) * dy[k] + 0x8000;
        ax[k] = (u32)dx[k];
    }
    for (s32 y = miny; y <= maxy; y++) {
        /* the span as offsets t from minx where r + ex * t >= bias for every edge */
        s32 lo = 0, hi = maxx - minx;
        for (int i = 0; i < 3 && lo <= hi; i++) {
            s32 n = bias[i] - r[i], e = ex[i];
            if (e > 0) {
                s32 t = n <= 0 ? -(-n / e) : (n + e - 1) / e; /* ceil(n / e) */
                lo = t > lo ? t : lo;
            } else if (e < 0) {
                s32 t = n <= 0 ? -n / -e : -((n - e - 1) / -e); /* floor(n / e) */
                hi = t < hi ? t : hi;
            } else if (n > 0)
                hi = -1;
        }
        if (lo <= hi) {
            g->tested += (u32)(hi - lo + 1);
            /* Inside the triangle each 16.16 attribute sum is within a few units of +-255 << 16,
             * so stepping its low 32 bits gives the value the 64-bit sum would. */
            u32 a[5];
            for (int k = 0; k < 5; k++)
                a[k] = (u32)(ar[k] + (s64)lo * dx[k]);
            for (s32 x = minx + lo, xe = minx + hi; x <= xe; x++) {
                fragment(psx, p, x, y, a0[0] + ((s32)a[0] >> 16), a0[1] + ((s32)a[1] >> 16), a0[2] + ((s32)a[2] >> 16),
                         a0[3] + ((s32)a[3] >> 16), a0[4] + ((s32)a[4] >> 16), dith);
                for (int k = 0; k < 5; k++)
                    a[k] += ax[k];
            }
        }
        for (int i = 0; i < 3; i++)
            r[i] += ey[i];
        for (int k = 0; k < 5; k++)
            ar[k] += dy[k];
    }
    if (how == GPU_SW)
        sw_wrote(psx, minx, miny, maxx, maxy);
}

static void rect(Psx *psx, const Prim *p, s32 x0, s32 y0, s32 w, s32 h, s32 r, s32 gg, s32 b, s32 u0, s32 v0)
{
    Gpu *g = &psx->gpu;
    const GpuRenderer *rd = g->renderer;
    Vtx c = {x0, y0, r, gg, b, u0, v0};
    int how = rd ? rd->rect(rd->self, p, x0, y0, w, h, &c, g->tex_flip_x, g->tex_flip_y) : GPU_SW;
    if (how == GPU_DRAWN)
        return;
    s32 xa = g->clip_x1 - x0, xb = g->clip_x2 - x0 + 1, ya = g->clip_y1 - y0, yb = g->clip_y2 - y0 + 1;
    xa = xa < 0 ? 0 : xa, ya = ya < 0 ? 0 : ya, xb = xb > w ? w : xb, yb = yb > h ? h : yb;
    if (xa >= xb || ya >= yb)
        return;
    if (how == GPU_SW) {
        sw_read_texture(psx, p);
        sw_read(psx, x0 + xa, y0 + ya, x0 + xb - 1, y0 + yb - 1);
    }
    for (s32 y = ya; y < yb; y++) {
        s32 py = y0 + y;
        for (s32 x = xa; x < xb; x++) {
            s32 px = x0 + x;
            s32 u = g->tex_flip_x ? u0 - x : u0 + x, v = g->tex_flip_y ? v0 - y : v0 + y;
            fragment(psx, p, px, py, r, gg, b, u, v, 0);
        }
    }
    if (how == GPU_SW)
        sw_wrote(psx, x0 + xa, y0 + ya, x0 + xb - 1, y0 + yb - 1);
}

static void line(Psx *psx, const Prim *p, Vtx a, Vtx b)
{
    Gpu *g = &psx->gpu;
    s32 dx = b.x - a.x, dy = b.y - a.y;
    s32 adx = dx < 0 ? -dx : dx, ady = dy < 0 ? -dy : dy;
    if (adx > 1023 || ady > 511)
        return;
    const GpuRenderer *rd = g->renderer;
    int how = rd ? rd->line(rd->self, p, &a, &b) : GPU_SW;
    if (how == GPU_DRAWN)
        return;
    s32 x0 = a.x < b.x ? a.x : b.x, x1 = a.x < b.x ? b.x : a.x, y0 = a.y < b.y ? a.y : b.y, y1 = a.y < b.y ? b.y : a.y;
    x0 = x0 < g->clip_x1 ? g->clip_x1 : x0, y0 = y0 < g->clip_y1 ? g->clip_y1 : y0;
    x1 = x1 > g->clip_x2 ? g->clip_x2 : x1, y1 = y1 > g->clip_y2 ? g->clip_y2 : y1;
    if (x0 > x1 || y0 > y1)
        return;
    if (how == GPU_SW)
        sw_read(psx, x0, y0, x1, y1);
    s32 n = adx > ady ? adx : ady;
    int dith = p->dither;
    for (s32 i = 0; i <= n; i++) {
        s32 x = a.x + (n ? (dx * i + (dx >= 0 ? n / 2 : -n / 2)) / n : 0);
        s32 y = a.y + (n ? (dy * i + (dy >= 0 ? n / 2 : -n / 2)) / n : 0);
        if (x < g->clip_x1 || x > g->clip_x2 || y < g->clip_y1 || y > g->clip_y2)
            continue;
        s32 r = a.r + (n ? (b.r - a.r) * i / n : 0), gg = a.g + (n ? (b.g - a.g) * i / n : 0), bb = a.b + (n ? (b.b - a.b) * i / n : 0);
        plot(psx, p, x, y, r, gg, bb, dith, p->semi, 0);
    }
    if (how == GPU_SW)
        sw_wrote(psx, x0, y0, x1, y1);
}

static void vtx_pos(Psx *psx, Vtx *v, u32 w)
{
    v->x = sext11(w) + psx->gpu.off_x;
    v->y = sext11(w >> 16) + psx->gpu.off_y;
}

static void vtx_col(Vtx *v, u32 w)
{
    v->r = w & 0xFF, v->g = (w >> 8) & 0xFF, v->b = (w >> 16) & 0xFF;
}

static void draw_polygon(Psx *psx, const u32 *c)
{
    u32 cmd = c[0] >> 24;
    int quad = (cmd >> 3) & 1, tex = (cmd >> 2) & 1, gour = (cmd >> 4) & 1, n = quad ? 4 : 3;
    Vtx v[4];
    u32 clut = 0, tp = psx->gpu.texpage;
    int k = 0;
    for (int i = 0; i < n; i++) {
        if (i == 0 || gour)
            vtx_col(&v[i], c[k++]);
        else
            v[i].r = v[0].r, v[i].g = v[0].g, v[i].b = v[0].b;
        vtx_pos(psx, &v[i], c[k++]);
        if (tex) {
            u32 w = c[k++];
            v[i].u = w & 0xFF, v[i].v = (w >> 8) & 0xFF;
            if (i == 0)
                clut = w >> 16;
            if (i == 1) {
                tp = w >> 16;
                set_texpage(psx, (psx->gpu.texpage & ~0x1FFu) | (tp & 0x1FF));
                tp = psx->gpu.texpage;
            }
        } else
            v[i].u = v[i].v = 0;
    }
    Prim p;
    prim_from(psx, &p, cmd, clut, tp);
    p.gouraud = gour;
    p.dither = (psx->gpu.stat & 0x200) && (gour || (tex && !p.raw));
    if (tex && p.raw)
        for (int i = 0; i < n; i++)
            v[i].r = v[i].g = v[i].b = 0x80;
    const GpuRenderer *rd = psx->gpu.renderer;
    if (quad && rd && rd->quad) {
        s32 minx = v[0].x, maxx = v[0].x, miny = v[0].y, maxy = v[0].y;
        for (int i = 1; i < 4; i++) {
            minx = v[i].x < minx ? v[i].x : minx, maxx = v[i].x > maxx ? v[i].x : maxx;
            miny = v[i].y < miny ? v[i].y : miny, maxy = v[i].y > maxy ? v[i].y : maxy;
        }
        /* within the size limits as a whole: both triangles are, and the renderer may draw them
         * in one go */
        if (maxx - minx <= 1023 && maxy - miny <= 511 && rd->quad(rd->self, &p, v) != GPU_SW)
            return;
    }
    triangle(psx, &p, v[0], v[1], v[2]);
    if (quad)
        triangle(psx, &p, v[1], v[2], v[3]);
}

static void draw_rect(Psx *psx, const u32 *c)
{
    u32 cmd = c[0] >> 24;
    Prim p;
    int tex = (cmd >> 2) & 1, size = (cmd >> 3) & 3;
    Vtx v;
    vtx_col(&v, c[0]);
    vtx_pos(psx, &v, c[1]);
    s32 u = 0, vv = 0, w, h;
    u32 clut = 0;
    int k = 2;
    if (tex) {
        u = c[2] & 0xFF, vv = (c[2] >> 8) & 0xFF, clut = c[2] >> 16;
        k = 3;
    }
    if (size == 0)
        w = c[k] & 0x3FF, h = (c[k] >> 16) & 0x1FF;
    else
        w = h = size == 1 ? 1 : size == 2 ? 8 : 16;
    prim_from(psx, &p, cmd, clut, psx->gpu.texpage);
    if (tex && p.raw)
        v.r = v.g = v.b = 0x80;
    rect(psx, &p, v.x, v.y, w, h, v.r, v.g, v.b, u, vv);
}

static void draw_line(Psx *psx, const u32 *c, int n)
{
    u32 cmd = c[0] >> 24;
    int gour = (cmd >> 4) & 1;
    Prim p;
    prim_from(psx, &p, cmd, 0, psx->gpu.texpage);
    p.textured = 0;
    p.gouraud = gour;
    p.dither = (psx->gpu.stat & 0x200) && gour;
    Vtx prev = {0}, cur = {0};
    int k = 0;
    vtx_col(&prev, c[k++]);
    vtx_pos(psx, &prev, c[k++]);
    while (k < n) {
        if (gour)
            vtx_col(&cur, c[k++]);
        else
            cur.r = prev.r, cur.g = prev.g, cur.b = prev.b;
        if (k >= n)
            break;
        vtx_pos(psx, &cur, c[k++]);
        line(psx, &p, prev, cur);
        prev = cur;
    }
}

static void fill(Psx *psx, const u32 *c)
{
    u32 col = c[0];
    u16 px = (u16)(((col >> 3) & 31) | ((col >> 11) & 31) << 5 | ((col >> 19) & 31) << 10);
    s32 x0 = c[1] & 0x3F0, y0 = (c[1] >> 16) & 0x1FF;
    s32 w = ((c[2] & 0x3FF) + 0xF) & ~0xF, h = (c[2] >> 16) & 0x1FF;
    if (!w || !h)
        return;
    const GpuRenderer *rd = psx->gpu.renderer;
    int how = rd ? rd->fill(rd->self, x0, y0, w, h, px) : GPU_SW;
    if (how == GPU_DRAWN)
        return;
    for (s32 y = 0; y < h; y++)
        for (s32 x = 0; x < w; x++)
            VRAM(x0 + x, y0 + y) = px;
    if (how == GPU_SW)
        sw_wrote(psx, x0, y0, x0 + w - 1, y0 + h - 1);
}

static void copy(Psx *psx, const u32 *c)
{
    Gpu *g = &psx->gpu;
    s32 sx = c[1] & 0x3FF, sy = (c[1] >> 16) & 0x1FF, dx = c[2] & 0x3FF, dy = (c[2] >> 16) & 0x1FF;
    s32 w = ((c[3] - 1) & 0x3FF) + 1, h = (((c[3] >> 16) - 1) & 0x1FF) + 1;
    const GpuRenderer *rd = g->renderer;
    int how = rd ? rd->copy(rd->self, sx, sy, dx, dy, w, h, g->mask_check, g->mask_set ? 0x8000 : 0) : GPU_SW;
    if (how == GPU_DRAWN)
        return;
    if (how == GPU_SW) {
        sw_read(psx, sx, sy, sx + w - 1, sy + h - 1);
        if (g->mask_check)
            sw_read(psx, dx, dy, dx + w - 1, dy + h - 1);
    }
    for (s32 y = 0; y < h; y++)
        for (s32 x = 0; x < w; x++) {
            u16 s = VRAM(sx + x, sy + y);
            u16 *d = &VRAM(dx + x, dy + y);
            if (g->mask_check && (*d & 0x8000))
                continue;
            *d = s | (g->mask_set ? 0x8000 : 0);
        }
    if (how == GPU_SW)
        sw_wrote(psx, dx, dy, dx + w - 1, dy + h - 1);
}

static void xfer_start(Psx *psx, const u32 *c, int dir)
{
    Gpu *g = &psx->gpu;
    g->xx = c[1] & 0x3FF, g->xy = (c[1] >> 16) & 0x1FF;
    g->xw = ((c[2] - 1) & 0x3FF) + 1, g->xh = (((c[2] >> 16) - 1) & 0x1FF) + 1;
    g->xi = g->xr = 0;
    g->xfer = dir;
    /* reading VRAM, or writing it under the mask bits: the renderer's pixels first */
    if (dir == 2 || g->mask_check)
        sw_read(psx, g->xx, g->xy, g->xx + g->xw - 1, g->xy + g->xh - 1);
}

/* A CPU->VRAM transfer cut short (GP1 00/01): hands the renderer the pixels written so far. */
static void xfer_abort(Psx *psx)
{
    Gpu *g = &psx->gpu;
    if (g->xfer == 1) {
        if (g->xr)
            sw_wrote(psx, g->xx, g->xy, g->xx + g->xw - 1, g->xy + g->xr - 1);
        if (g->xi)
            sw_wrote(psx, g->xx, g->xy + g->xr, g->xx + g->xi - 1, g->xy + g->xr);
    }
    g->xfer = 0;
}

static int xfer_pixel(Psx *psx, u16 *io, int write)
{
    Gpu *g = &psx->gpu;
    u16 *d = &VRAM(g->xx + g->xi, g->xy + g->xr);
    if (write) {
        if (!(g->mask_check && (*d & 0x8000)))
            *d = *io | (g->mask_set ? 0x8000 : 0);
    } else
        *io = *d;
    if (++g->xi < g->xw)
        return 0;
    g->xi = 0;
    if (++g->xr < g->xh)
        return 0;
    if (write)
        sw_wrote(psx, g->xx, g->xy, g->xx + g->xw - 1, g->xy + g->xh - 1);
    g->xfer = 0;
    return 1;
}

static void draw(Psx *psx);

static void execute(Psx *psx)
{
    /* the clock may be a syscall: one command in 16 is timed, its time scaled */
    static u32 n;
    const PsxHost *h = psx->host;
    u32 cmd = psx->gpu.cmd[0] >> 24;
    int k = cmd >= 0x20 && cmd < 0x40 ? 0 : cmd >= 0x40 && cmd < 0x60 ? 1 : cmd >= 0x60 && cmd < 0x80 ? 2
          : cmd >= 0x80 && cmd < 0xA0 ? 3 : cmd == 0x02 ? 4 : 5;
    psx->gpu.n_kind[k]++;
    if (!h->usec || n++ % 16) {
        draw(psx);
        return;
    }
    u32 t = h->usec(h->user);
    draw(psx);
    t = (h->usec(h->user) - t) * 16;
    psx->gpu.us_draw += t;
    psx->gpu.us_kind[k] += t;
}

static void draw(Psx *psx)
{
    Gpu *g = &psx->gpu;
    u32 *c = g->cmd, cmd = c[0] >> 24;
    if (cmd >= 0x20 && cmd < 0x40)
        draw_polygon(psx, c);
    else if (cmd >= 0x40 && cmd < 0x60)
        draw_line(psx, c, g->ncmd);
    else if (cmd >= 0x60 && cmd < 0x80)
        draw_rect(psx, c);
    else if (cmd >= 0x80 && cmd < 0xA0)
        copy(psx, c);
    else if (cmd >= 0xA0 && cmd < 0xC0)
        xfer_start(psx, c, 1);
    else if (cmd >= 0xC0 && cmd < 0xE0)
        xfer_start(psx, c, 2);
    else
        switch (cmd) {
        case 0x02: fill(psx, c); break;
        case 0x1F: g->stat |= 1u << 24; psx_raise(psx, IRQ_GPU); break;
        case 0xE1:
            set_texpage(psx, c[0]);
            g->tex_flip_x = (c[0] >> 12) & 1, g->tex_flip_y = (c[0] >> 13) & 1;
            break;
        case 0xE2:
            g->texwin = c[0] & 0xFFFFF;
            g->tw_mask_x = c[0] & 31, g->tw_mask_y = (c[0] >> 5) & 31;
            g->tw_off_x = (c[0] >> 10) & 31, g->tw_off_y = (c[0] >> 15) & 31;
            break;
        case 0xE3: g->clip_x1 = c[0] & 0x3FF, g->clip_y1 = (c[0] >> 10) & 0x3FF; break;
        case 0xE4: g->clip_x2 = c[0] & 0x3FF, g->clip_y2 = (c[0] >> 10) & 0x3FF; break;
        case 0xE5: g->off_x = sext11(c[0]), g->off_y = sext11(c[0] >> 11); break;
        case 0xE6:
            g->mask_set = c[0] & 1, g->mask_check = (c[0] >> 1) & 1;
            g->stat = (g->stat & ~0x1800u) | ((c[0] & 3) << 11);
            break;
        }
}

u32 gpu_gp0_pixels(Psx *psx, const u32 *w, u32 n)
{
    Gpu *g = &psx->gpu;
    const u16 *p = (const u16 *)w;
    u32 left = 2 * n, used = 0;
    u16 set = g->mask_set ? 0x8000 : 0;
    while (g->xfer == 1 && used < left) {
        u32 run = (u32)(g->xw - g->xi), x = (u32)(g->xx + g->xi) & 1023;
        if (run > left - used)
            run = left - used;
        u16 *row = &g->vram[((g->xy + g->xr) & 511) * VRAM_W];
        const u16 *src = p + used;
        if (!g->mask_check && !set && x + run <= VRAM_W)
            memcpy(row + x, src, run * 2);
        else
            for (u32 i = 0; i < run; i++) {
                u16 *d = &row[(x + i) & 1023];
                if (!(g->mask_check && (*d & 0x8000)))
                    *d = src[i] | set;
            }
        used += run;
        if ((g->xi += (int)run) < g->xw)
            break;
        g->xi = 0;
        if (++g->xr < g->xh)
            continue;
        sw_wrote(psx, g->xx, g->xy, g->xx + g->xw - 1, g->xy + g->xh - 1);
        g->xfer = 0;
    }
    return (used + 1) / 2;
}

void gpu_gp0(Psx *psx, u32 v)
{
    Gpu *g = &psx->gpu;
    if (g->xfer == 1) {
        u16 lo = (u16)v, hi = (u16)(v >> 16);
        if (!xfer_pixel(psx, &lo, 1))
            xfer_pixel(psx, &hi, 1);
        return;
    }
    if (g->ncmd == 0) {
        g->need = cmd_words(v >> 24, &g->polyline);
        if (g->polyline)
            g->need = 0;
    } else if (g->polyline && g->ncmd >= 3 && (v & 0xF000F000u) == 0x50005000u) {
        execute(psx);
        g->ncmd = 0;
        return;
    }
    if (g->ncmd < (int)(sizeof g->cmd / sizeof g->cmd[0]))
        g->cmd[g->ncmd++] = v;
    if ((!g->polyline && g->ncmd >= g->need)) {
        execute(psx);
        g->ncmd = 0;
    } else if (g->polyline && g->ncmd >= (int)(sizeof g->cmd / sizeof g->cmd[0])) {
        execute(psx); /* very long polyline: draw what fits */
        g->ncmd = 0;
    }
}

static void reset(Psx *psx)
{
    Gpu *g = &psx->gpu;
    u16 *vram = g->vram;
    const GpuRenderer *r = g->renderer;
    memset(g, 0, sizeof *g);
    g->vram = vram;
    g->renderer = r;
    g->stat = 0x14802000u;
    g->disp_enabled = 0;
    g->hrange1 = 0x200, g->hrange2 = 0xC00;
    g->vrange1 = 0x10, g->vrange2 = 0x100;
    g->clip_x2 = 1023, g->clip_y2 = 511;
}

/* Lines of VRAM the display shows: the game's vertical range, doubled when interlaced. Buffer
 * spacing says nothing about it (Sheep Dog draws 240-line NTSC frames 256 lines apart). */
int gpu_display_height(const Psx *psx)
{
    const Gpu *g = &psx->gpu;
    u32 st = g->stat;
    int h = g->vrange2 - g->vrange1;
    if ((st & 0x80000) && (st & 0x400000))
        h *= 2;
    if (h <= 0 || h > 512)
        h = (st & 0x100000) ? 256 : 240;
    return h;
}

void gpu_gp1(Psx *psx, u32 v)
{
    Gpu *g = &psx->gpu;
    switch (v >> 24) {
    case 0x00: xfer_abort(psx), reset(psx), psx_video_mode(psx); break;
    case 0x01: xfer_abort(psx), g->ncmd = 0; break;
    case 0x02: g->stat &= ~(1u << 24); break;
    case 0x03:
        g->stat = (g->stat & ~(1u << 23)) | ((v & 1) << 23);
        g->disp_enabled = !(v & 1);
        break;
    case 0x04:
        g->dma_dir = v & 3;
        g->stat = (g->stat & ~0x60000000u) | ((v & 3) << 29);
        break;
    case 0x05: {
        int x = (int)(v & 0x3FE), y = (int)((v >> 10) & 0x1FF);
        if (g->disp_x != x || g->disp_y != y) {
            g->flips++;
            if (psx->trace & 32)
                psx_log(psx, "gpu: flip %d,%d", x, y); /* the game's own frame rate: flips per vblank */
        }
        g->disp_x = x, g->disp_y = y;
        break;
    }
    case 0x06:
        if ((psx->trace & 32) && (g->hrange1 != (int)(v & 0xFFF) || g->hrange2 != (int)((v >> 12) & 0xFFF)))
            psx_log(psx, "gpu: hrange %03X-%03X", v & 0xFFF, (v >> 12) & 0xFFF);
        g->hrange1 = v & 0xFFF, g->hrange2 = (v >> 12) & 0xFFF;
        break;
    case 0x07:
        if ((psx->trace & 32) && (g->vrange1 != (int)(v & 0x3FF) || g->vrange2 != (int)((v >> 10) & 0x3FF)))
            psx_log(psx, "gpu: vrange %03X-%03X", v & 0x3FF, (v >> 10) & 0x3FF);
        g->vrange1 = v & 0x3FF, g->vrange2 = (v >> 10) & 0x3FF;
        break;
    case 0x08:
        if (psx->trace & 32)
            psx_log(psx, "gpu: mode %02X", v & 0xFF);
        g->stat = (g->stat & ~0x7F4000u) | ((v & 0x3F) << 17) | ((v & 0x40) << 10) | ((v & 0x80) << 7);
        psx_video_mode(psx);
        break;
    case 0x10:
        switch (v & 7) {
        case 2: g->gpuread = g->texwin; break;
        case 3: g->gpuread = (u32)g->clip_x1 | (u32)g->clip_y1 << 10; break;
        case 4: g->gpuread = (u32)g->clip_x2 | (u32)g->clip_y2 << 10; break;
        case 5: g->gpuread = ((u32)g->off_x & 0x7FF) | ((u32)g->off_y & 0x7FF) << 11; break;
        case 7: g->gpuread = 2; break;
        }
        break;
    }
}

u32 gpu_read(Psx *psx, int reg)
{
    Gpu *g = &psx->gpu;
    if (reg) {
        u32 s = g->stat | 0x1C000000u; /* ready for commands, VRAM reads and DMA blocks */
        u32 dir = (s >> 29) & 3;
        s &= ~(1u << 25);
        if (dir == 1 || dir == 2)
            s |= 1u << 25;
        else if (dir == 3 && g->xfer == 2)
            s |= 1u << 25;
        return s;
    }
    if (g->xfer == 2) {
        u16 lo = 0, hi = 0;
        if (!xfer_pixel(psx, &lo, 0))
            xfer_pixel(psx, &hi, 0);
        g->gpuread = lo | (u32)hi << 16;
    }
    return g->gpuread;
}

void gpu_vblank(Psx *psx)
{
    Gpu *g = &psx->gpu;
    g->frame++;
    if (g->stat & (1u << 22))
        g->stat ^= 1u << 31; /* interlaced: field changes */
    else
        g->stat &= ~(1u << 31);
}

void gpu_init(Psx *psx)
{
    memset(psx->gpu.vram, 0, VRAM_W * VRAM_H * 2);
    reset(psx);
}
