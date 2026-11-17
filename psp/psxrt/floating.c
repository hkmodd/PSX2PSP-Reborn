/* psxrt: floating code (floating.h). */
#include "floating.h"

#include <string.h>

/* words [a, a + n) inside a blob of `len` words */
static int inside(u32 a, u32 n, u32 len) { return a <= len && n <= len - a; }

int fl_open(Floating *f, const u32 *w, u32 len)
{
    if (len < 4 || w[0] > (len - 4) / 7 || w[1] > (len - 4 - 7 * w[0]) / 3)
        return -1;
    Floating g = {w, w[0], w[1]};
    for (u32 u = 0; u < g.units; u++) {
        const FlUnit *x = fl_unit(&g, u);
        if (!x->g_len || !inside(x->g_off, x->g_len, len) || !inside(x->h_off, x->h_len, len) || !inside(x->t_off, x->g_len, len) ||
            !inside(x->r_off, x->r_len, len))
            return -1;
        for (u32 j = 0; j < x->g_len; j++)
            if (w[x->t_off + j] != FL_NONE && w[x->t_off + j] >= x->h_len)
                return -1;
        for (u32 j = 0; j < x->r_len; j++) {
            u32 r = w[x->r_off + j], i = r >> 2, k = r & 3;
            if (i + (k == 1 || k == 3) >= x->h_len)
                return -1;
        }
    }
    const u32 *ix = w + 4 + 7 * g.units;
    for (u32 i = 0; i < g.index; i++) {
        const u32 *e = ix + 3 * i;
        if (e[1] >= g.units || e[2] + 2 > fl_unit(&g, e[1])->g_len || (i && e[0] < e[-3]))
            return -1;
    }
    *f = g;
    return 0;
}

u32 fl_key(const u8 *p)
{
    u32 h = 0x811C9DC5u;
    for (int i = 0; i < 8; i++)
        h = (h ^ p[i]) * 0x01000193u;
    return h;
}

int fl_find(const Floating *f, const u8 *ram, u32 p, u32 *at)
{
    if (!f->w || (p & 3) || p > RAM_SIZE - 8)
        return -1;
    const u32 *ix = f->w + 4 + 7 * f->units;
    u32 key = fl_key(ram + p), lo = 0, hi = f->index;
    while (lo < hi) {
        u32 m = lo + (hi - lo) / 2;
        if (ix[3 * m] < key)
            lo = m + 1;
        else
            hi = m;
    }
    int best = -1;
    for (; lo < f->index && ix[3 * lo] == key; lo++) {
        u32 u = ix[3 * lo + 1], off = 4 * ix[3 * lo + 2];
        const FlUnit *x = fl_unit(f, u);
        if (p < off || x->g_len > (RAM_SIZE - (p - off)) / 4 || (best >= 0 && fl_unit(f, (u32)best)->g_len >= x->g_len))
            continue;
        if (!memcmp(ram + p - off, f->w + x->g_off, 4 * x->g_len))
            best = (int)u, *at = p - off;
    }
    return best;
}

void fl_place(const Floating *f, u32 u, u32 *dst, u32 guest, u32 host)
{
    const FlUnit *x = fl_unit(f, u);
    const u32 *r = f->w + x->r_off;
    u32 gd = guest - f->w[2], hd = host - f->w[3];
    memcpy(dst, f->w + x->h_off, 4 * x->h_len);
    for (u32 j = 0; j < x->r_len; j++) {
        u32 i = r[j] >> 2, k = r[j] & 3, v;
        switch (k) {
        case 0:
            dst[i] += gd;
            break;
        case 2:
            v = (((dst[i] & 0x03FFFFFFu) << 2) | (f->w[3] & 0xF0000000u)) + hd;
            dst[i] = (dst[i] & 0xFC000000u) | ((v >> 2) & 0x03FFFFFFu);
            break;
        default:
            v = ((dst[i] << 16) | (dst[i + 1] & 0xFFFFu)) + (k == 1 ? gd : hd);
            dst[i] = (dst[i] & 0xFFFF0000u) | (v >> 16);
            dst[i + 1] = (dst[i + 1] & 0xFFFF0000u) | (v & 0xFFFFu);
            break;
        }
    }
}

int fl_holds(const Floating *f, const u8 *ram, const FlPlace *q)
{
    const FlUnit *x = fl_unit(f, q->unit);
    return q->at <= RAM_SIZE - 4 * x->g_len && !memcmp(ram + q->at, f->w + x->g_off, 4 * x->g_len);
}

int fl_placed(const Floating *f, const FlArena *a, const u8 *ram, u32 p, u32 base)
{
    for (u32 i = 0; i < a->n; i++) {
        const FlPlace *q = &a->p[i];
        const FlUnit *x = fl_unit(f, q->unit);
        if (p - q->at < 4 * x->g_len && q->guest == q->at + base && f->w[x->t_off + (p - q->at) / 4] != FL_NONE && fl_holds(f, ram, q))
            return (int)i;
    }
    return -1;
}

/* The first free slot of `len` bytes, and *k the index it takes; 0 when none. */
static u32 gap(const FlArena *a, u32 len, u32 *k)
{
    if (a->n == FL_PLACES)
        return 0;
    u32 from = a->lo;
    for (u32 i = 0; i < a->n; from = a->p[i].host + a->p[i].len, i++)
        if (a->p[i].host - from >= len)
            return *k = i, from;
    return a->hi - from >= len ? (*k = a->n, from) : 0;
}

int fl_alloc(const Floating *f, FlArena *a, const u8 *ram, u32 u, u32 at, u32 guest, void (*gone)(const FlPlace *q))
{
    u32 len = (4 * fl_unit(f, u)->h_len + 63) & ~63u, k, host = gap(a, len, &k);
    if (!host) {
        for (u32 i = a->n; i-- > 0;)
            if (!fl_holds(f, ram, &a->p[i])) {
                if (gone)
                    gone(&a->p[i]);
                memmove(a->p + i, a->p + i + 1, (a->n - i - 1) * sizeof *a->p);
                a->n--;
            }
        host = gap(a, len, &k);
    }
    if (!host)
        return -1;
    memmove(a->p + k + 1, a->p + k, (a->n - k) * sizeof *a->p);
    a->n++;
    a->p[k] = (FlPlace){at, u, host, len, guest};
    return (int)k;
}
