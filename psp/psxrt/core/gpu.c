/*
 * psxrt core: the GPU. GP0/GP1 command decoding, VRAM transfers and a software rasterizer that
 * follows the hardware rules: top-left fill convention, texture pages and CLUTs, texture windows,
 * modulation (0x80 = 1.0), the four semi-transparency modes, 4x4 dithering and mask bits.
 */
#include "psx.h"

#include <string.h>

#define VRAM(x, y) psx->gpu.vram[((y) & 511) * VRAM_W + ((x) & 1023)]

typedef struct {
    s32 x, y;
    s32 r, g, b;
    s32 u, v;
} Vtx;

typedef struct {
    int textured, raw, semi, gouraud;
    u32 clut_x, clut_y;
    u32 tpx, tpy, depth, abr;
} Prim;

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
    (void)psx;
}

static u16 texel(Psx *psx, const Prim *p, u32 u, u32 v)
{
    Gpu *g = &psx->gpu;
    u &= 0xFF, v &= 0xFF;
    u = (u & ~(u32)(g->tw_mask_x * 8)) | ((g->tw_off_x & g->tw_mask_x) * 8);
    v = (v & ~(u32)(g->tw_mask_y * 8)) | ((g->tw_off_y & g->tw_mask_y) * 8);
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
static void plot(Psx *psx, const Prim *p, int x, int y, s32 r, s32 g, s32 b, int dith, int semi, u16 mask_bit)
{
    Gpu *gp = &psx->gpu;
    u16 *d = &VRAM(x, y);
    if (gp->mask_check && (*d & 0x8000))
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
    *d = (u16)(fr | fg << 5 | fb << 10 | mask_bit | (gp->mask_set ? 0x8000 : 0));
}

/* Shades one fragment: texture lookup, modulation, transparency. */
static void fragment(Psx *psx, const Prim *p, int x, int y, s32 r, s32 g, s32 b, s32 u, s32 v, int dith)
{
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
    if (minx < g->clip_x1) minx = g->clip_x1;
    if (miny < g->clip_y1) miny = g->clip_y1;
    if (maxx > g->clip_x2) maxx = g->clip_x2;
    if (maxy > g->clip_y2) maxy = g->clip_y2;
    if (minx > maxx || miny > maxy)
        return;
    /* attribute planes: a(x,y) = a0 + dadx*(x-x0) + dady*(y-y0), 16.16 */
    s32 a0[5] = {v0.r, v0.g, v0.b, v0.u, v0.v}, a1[5] = {v1.r, v1.g, v1.b, v1.u, v1.v}, a2[5] = {v2.r, v2.g, v2.b, v2.u, v2.v};
    s64 dx[5], dy[5];
    for (int k = 0; k < 5; k++) {
        s64 d1 = a1[k] - a0[k], d2 = a2[k] - a0[k];
        dx[k] = ((d1 * (v2.y - v0.y) - d2 * (v1.y - v0.y)) << 16) / area;
        dy[k] = ((d2 * (v1.x - v0.x) - d1 * (v2.x - v0.x)) << 16) / area;
    }
    int b0 = top_left(&v1, &v2) ? 0 : 1, b1 = top_left(&v2, &v0) ? 0 : 1, b2 = top_left(&v0, &v1) ? 0 : 1;
    int dith = (g->stat & 0x200) && (p->gouraud || (p->textured && !p->raw));
    for (s32 y = miny; y <= maxy; y++) {
        for (s32 x = minx; x <= maxx; x++) {
            s64 w0 = edge(&v1, &v2, x, y), w1 = edge(&v2, &v0, x, y), w2 = edge(&v0, &v1, x, y);
            if (w0 < b0 || w1 < b1 || w2 < b2)
                continue;
            s32 at[5];
            for (int k = 0; k < 5; k++)
                at[k] = a0[k] + (s32)(((s64)(x - v0.x) * dx[k] + (s64)(y - v0.y) * dy[k] + 0x8000) >> 16);
            fragment(psx, p, x, y, at[0], at[1], at[2], at[3], at[4], dith);
        }
    }
}

static void rect(Psx *psx, const Prim *p, s32 x0, s32 y0, s32 w, s32 h, s32 r, s32 gg, s32 b, s32 u0, s32 v0)
{
    Gpu *g = &psx->gpu;
    for (s32 y = 0; y < h; y++) {
        s32 py = y0 + y;
        if (py < g->clip_y1 || py > g->clip_y2)
            continue;
        for (s32 x = 0; x < w; x++) {
            s32 px = x0 + x;
            if (px < g->clip_x1 || px > g->clip_x2)
                continue;
            s32 u = g->tex_flip_x ? u0 - x : u0 + x, v = g->tex_flip_y ? v0 - y : v0 + y;
            fragment(psx, p, px, py, r, gg, b, u, v, 0);
        }
    }
}

static void line(Psx *psx, const Prim *p, Vtx a, Vtx b)
{
    Gpu *g = &psx->gpu;
    s32 dx = b.x - a.x, dy = b.y - a.y;
    s32 adx = dx < 0 ? -dx : dx, ady = dy < 0 ? -dy : dy;
    if (adx > 1023 || ady > 511)
        return;
    s32 n = adx > ady ? adx : ady;
    int dith = (g->stat & 0x200) && p->gouraud;
    for (s32 i = 0; i <= n; i++) {
        s32 x = a.x + (n ? (dx * i + (dx >= 0 ? n / 2 : -n / 2)) / n : 0);
        s32 y = a.y + (n ? (dy * i + (dy >= 0 ? n / 2 : -n / 2)) / n : 0);
        if (x < g->clip_x1 || x > g->clip_x2 || y < g->clip_y1 || y > g->clip_y2)
            continue;
        s32 r = a.r + (n ? (b.r - a.r) * i / n : 0), gg = a.g + (n ? (b.g - a.g) * i / n : 0), bb = a.b + (n ? (b.b - a.b) * i / n : 0);
        plot(psx, p, x, y, r, gg, bb, dith, p->semi, 0);
    }
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
    if (tex && p.raw)
        for (int i = 0; i < n; i++)
            v[i].r = v[i].g = v[i].b = 0x80;
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
    p.gouraud = 0;
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
    Vtx prev, cur;
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
    for (s32 y = 0; y < h; y++)
        for (s32 x = 0; x < w; x++)
            VRAM(x0 + x, y0 + y) = px;
}

static void copy(Psx *psx, const u32 *c)
{
    Gpu *g = &psx->gpu;
    s32 sx = c[1] & 0x3FF, sy = (c[1] >> 16) & 0x1FF, dx = c[2] & 0x3FF, dy = (c[2] >> 16) & 0x1FF;
    s32 w = ((c[3] - 1) & 0x3FF) + 1, h = (((c[3] >> 16) - 1) & 0x1FF) + 1;
    for (s32 y = 0; y < h; y++)
        for (s32 x = 0; x < w; x++) {
            u16 s = VRAM(sx + x, sy + y);
            u16 *d = &VRAM(dx + x, dy + y);
            if (g->mask_check && (*d & 0x8000))
                continue;
            *d = s | (g->mask_set ? 0x8000 : 0);
        }
}

static void xfer_start(Psx *psx, const u32 *c, int dir)
{
    Gpu *g = &psx->gpu;
    g->xx = c[1] & 0x3FF, g->xy = (c[1] >> 16) & 0x1FF;
    g->xw = ((c[2] - 1) & 0x3FF) + 1, g->xh = (((c[2] >> 16) - 1) & 0x1FF) + 1;
    g->xi = 0;
    g->xfer = dir;
}

static int xfer_pixel(Psx *psx, u16 *io, int write)
{
    Gpu *g = &psx->gpu;
    int x = g->xx + g->xi % g->xw, y = g->xy + g->xi / g->xw;
    u16 *d = &VRAM(x, y);
    if (write) {
        if (!(g->mask_check && (*d & 0x8000)))
            *d = *io | (g->mask_set ? 0x8000 : 0);
    } else
        *io = *d;
    if (++g->xi >= g->xw * g->xh) {
        g->xfer = 0;
        return 1;
    }
    return 0;
}

static void execute(Psx *psx)
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
    void *r = g->renderer;
    memset(g, 0, sizeof *g);
    g->vram = vram;
    g->renderer = r;
    g->stat = 0x14802000u;
    g->disp_enabled = 0;
    g->hrange1 = 0x200, g->hrange2 = 0xC00;
    g->vrange1 = 0x10, g->vrange2 = 0x100;
    g->clip_x2 = 1023, g->clip_y2 = 511;
}

void gpu_gp1(Psx *psx, u32 v)
{
    Gpu *g = &psx->gpu;
    switch (v >> 24) {
    case 0x00: reset(psx); break;
    case 0x01: g->ncmd = 0, g->xfer = 0; break;
    case 0x02: g->stat &= ~(1u << 24); break;
    case 0x03:
        g->stat = (g->stat & ~(1u << 23)) | ((v & 1) << 23);
        g->disp_enabled = !(v & 1);
        break;
    case 0x04:
        g->dma_dir = v & 3;
        g->stat = (g->stat & ~0x60000000u) | ((v & 3) << 29);
        break;
    case 0x05: g->disp_x = v & 0x3FE, g->disp_y = (v >> 10) & 0x1FF; break;
    case 0x06: g->hrange1 = v & 0xFFF, g->hrange2 = (v >> 12) & 0xFFF; break;
    case 0x07: g->vrange1 = v & 0x3FF, g->vrange2 = (v >> 10) & 0x3FF; break;
    case 0x08:
        g->stat = (g->stat & ~0x7F4000u) | ((v & 0x3F) << 17) | ((v & 0x40) << 10) | ((v & 0x80) << 7);
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
