/* psxrt: floating code (floating.h). */
#include "floating.h"

#include <string.h>

#define UNIT_WORDS (sizeof(FlUnit) / 4)

/* words [a, a + n) inside a blob of `len` words */
static int inside(u32 a, u32 n, u32 len) { return a <= len && n <= len - a; }

int fl_open(Floating *f, const u32 *w, u32 len)
{
    if (len < 4 || w[0] > (len - 4) / UNIT_WORDS || w[1] > (len - 4 - UNIT_WORDS * w[0]) / 3)
        return -1;
    Floating g = {w, w[0], w[1]};
    for (u32 u = 0; u < g.units; u++) {
        const FlUnit *x = fl_unit(&g, u);
        if (!x->g_len || x->g_len > RAM_SIZE / 4 || !inside(x->g_off, x->g_len, len) || x->s_len > len / 2 ||
            !inside(x->s_off, 2 * x->s_len, len) || !inside(x->m_off, (x->g_len + 31) / 32, len) || x->h_len > RAM_SIZE ||
            x->r_len > RAM_SIZE || x->b_len > 4 * fl_body_words(x))
            return -1;
        for (u32 j = 0; j < x->s_len; j++) {
            const u32 *s = w + x->s_off + 2 * j;
            if ((s[0] >> 1) >= x->g_len || s[1] + (s[0] & 1) >= x->h_len || (j && s[0] >> 1 < s[-2] >> 1))
                return -1;
        }
    }
    const u32 *ix = w + 4 + UNIT_WORDS * g.units;
    for (u32 i = 0; i < g.index; i++) {
        const u32 *e = ix + 3 * i;
        if (e[1] >= g.units || e[2] + 2 > fl_unit(&g, e[1])->g_len || (i && e[0] < e[-3]))
            return -1;
    }
    *f = g;
    return 0;
}

u32 fl_form(u32 w)
{
    u32 op = w >> 26, fn = w & 0x3F;
    if (op == 2 || op == 3)
        return w & 0xFC000000u;
    if (op >= 8 && op <= 0xF)
        return (op == 8 ? w | 1u << 26 : w) & 0xFFFF0000u;
    if (op == 0 && (fn == 0 || fn == 2 || fn == 3))
        return w & ~(0x1Fu << 6);
    return w;
}

u32 fl_key(const u8 *p)
{
    u32 h = 0x811C9DC5u;
    for (int i = 0; i < 2; i++) {
        u32 w;
        memcpy(&w, p + 4 * i, 4);
        w = fl_form(w);
        for (int b = 0; b < 4; b++)
            h = (h ^ ((w >> (8 * b)) & 0xFFu)) * 0x01000193u;
    }
    return h;
}

/* Whether `body`, unit x's, is whole: its table agrees with the map and points into the code, its
 * relocations are in the code. */
static int body_ok(const Floating *f, const FlUnit *x, const u32 *body)
{
    const u32 *t = body + x->h_len, *r = t + x->g_len;
    for (u32 j = 0; j < x->g_len; j++)
        if ((t[j] != FL_NONE) != fl_translates(f, x, j) || (t[j] != FL_NONE && t[j] >= x->h_len))
            return 0;
    for (u32 j = 0; j < x->r_len; j++) {
        u32 i = r[j] >> 2, k = r[j] & 3;
        if (i + (k == 1 || k == 3) >= x->h_len)
            return 0;
    }
    return 1;
}

/* Whether the words `r` are unit x's: each the unit's, or of its form at a site. */
static int match(const Floating *f, const FlUnit *x, const u32 *r)
{
    const u32 *g = f->w + x->g_off, *s = f->w + x->s_off, *end = s + 2 * x->s_len;
    for (u32 j = 0; j < x->g_len; j++) {
        if (r[j] == g[j])
            continue;
        while (s < end && s[0] >> 1 < j)
            s += 2;
        if (s == end || s[0] >> 1 != j || fl_form(r[j]) != fl_form(g[j]))
            return 0;
    }
    return 1;
}

/* FNV-1a 32 of the words `r` at x's sites. */
static u32 site_sum(const Floating *f, const FlUnit *x, const u32 *r)
{
    const u32 *s = f->w + x->s_off;
    u32 h = 0x811C9DC5u;
    for (u32 k = 0; k < x->s_len; k++)
        h = (h ^ r[s[2 * k] >> 1]) * 0x01000193u;
    return h;
}

int fl_find(const Floating *f, const u8 *ram, u32 p, u32 *at)
{
    if (!f->w || (p & 3) || p > RAM_SIZE - 4)
        return -1;
    const u32 *ix = f->w + 4 + UNIT_WORDS * f->units;
    int best = -1;
    for (u32 k = 0; k < FL_WALK && 4 * k <= p; k++) { /* an entry at e = p - 4k, of a unit with p in it */
        u32 e = p - 4 * k;
        if (e > RAM_SIZE - 8)
            continue;
        u32 key = fl_key(ram + e), lo = 0, hi = f->index;
        while (lo < hi) {
            u32 m = lo + (hi - lo) / 2;
            if (ix[3 * m] < key)
                lo = m + 1;
            else
                hi = m;
        }
        for (; lo < f->index && ix[3 * lo] == key; lo++) {
            u32 u = ix[3 * lo + 1], off = 4 * ix[3 * lo + 2], j = ix[3 * lo + 2] + k;
            const FlUnit *x = fl_unit(f, u);
            if (e < off || j >= x->g_len || !fl_translates(f, x, j) || x->g_len > (RAM_SIZE - (e - off)) / 4 ||
                (best >= 0 && fl_unit(f, (u32)best)->g_len >= x->g_len))
                continue;
            if (match(f, x, (const u32 *)(ram + e - off)))
                best = (int)u, *at = e - off;
        }
    }
    return best;
}

int fl_place(const Floating *f, u32 u, u32 *dst, u32 guest, u32 host, const u32 *ram)
{
    const FlUnit *x = fl_unit(f, u);
    const u32 *r = dst + x->h_len + x->g_len, *s = f->w + x->s_off;
    u32 gd = guest - f->w[2], hd = host - f->w[3];
    if (!body_ok(f, x, dst))
        return -1;
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
    for (u32 k = 0; ram && k < x->s_len; k++) {
        u32 j = s[2 * k] >> 1, i = s[2 * k + 1], w = ram[j], t;
        if (s[2 * k] & 1) { /* the target as the CPU takes it at this guest pc */
            t = ((guest + 4 * j + 4) & 0xF0000000u) | (w & 0x03FFFFFFu) << 2;
            dst[i] = (dst[i] & 0xFFFF0000u) | (t >> 16);
            dst[i + 1] = (dst[i + 1] & 0xFFFF0000u) | (t & 0xFFFFu);
        } else
            dst[i] = w >> 26 == 8 ? w | 1u << 26 : w; /* addi as addiu, as the translation copies it */
    }
    return 0;
}

int fl_holds(const Floating *f, const u8 *ram, const FlPlace *q)
{
    const FlUnit *x = fl_unit(f, q->unit);
    if (q->at > RAM_SIZE - 4 * x->g_len)
        return 0;
    const u32 *r = (const u32 *)(ram + q->at);
    return match(f, x, r) && site_sum(f, x, r) == q->sum;
}

int fl_placed(const Floating *f, const FlArena *a, const u8 *ram, u32 p, u32 base)
{
    for (u32 i = 0; i < a->n; i++) {
        const FlPlace *q = &a->p[i];
        const FlUnit *x = fl_unit(f, q->unit);
        if (p - q->at < 4 * x->g_len && q->guest == q->at + base && fl_translates(f, x, (p - q->at) / 4) && fl_holds(f, ram, q))
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
    const FlUnit *x = fl_unit(f, u);
    u32 len = (4 * fl_body_words(x) + 63) & ~63u, k, host = gap(a, len, &k);
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
    a->p[k] = (FlPlace){at, u, host, len, guest, site_sum(f, x, (const u32 *)(ram + at))};
    return (int)k;
}
