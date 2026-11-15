/*
 * psxrt core: the BIOS C library, A0 table 0A-36. Sony's semantics, the documented bugs included
 * (psx-spx "Kernel (BIOS)"): games were tested against them, so memcmp compares the byte after
 * the mismatch, memmove's backward copy moves one byte too many, strspn/strcspn are swapped, etc.
 * Every function runs on host pointers into guest RAM or the scratchpad (one address decode per
 * call, then the host's own string routines), never byte by byte through the bus.
 */
#include "psx.h"

#include <string.h>

#define G (psx->cpu.gpr)
enum { R_V0 = 2, R_A0 = 4, R_A1, R_A2, R_A3, R_SP = 29 };

/* host view of guest memory at `a`, *room bytes to the end of its region (RAM or scratchpad);
 * NULL elsewhere (ROM, I/O: no string lives there) */
static u8 *at(Psx *psx, u32 a, u32 *room)
{
    u32 p = a & 0x1FFFFFFF;
    if (p < 0x800000) {
        u32 o = p & (RAM_SIZE - 1);
        *room = RAM_SIZE - o;
        return psx->ram + o;
    }
    if (p - 0x1F800000u < SCRATCH_SIZE) {
        *room = SCRATCH_SIZE - (p - 0x1F800000u);
        return psx->scratch + (p - 0x1F800000u);
    }
    *room = 0;
    return NULL;
}

/* a span of n bytes, clamped to its region */
static u8 *span(Psx *psx, u32 a, u32 *n)
{
    u32 room;
    u8 *p = at(psx, a, &room);
    if (*n > room)
        *n = room;
    return p;
}

/* a string: its pointer and length (the region's end terminates it); NULL outside memory */
static const u8 *str(Psx *psx, u32 a, u32 *len)
{
    u32 room;
    const u8 *p = at(psx, a, &room);
    if (!p) {
        *len = 0;
        return NULL;
    }
    const u8 *z = memchr(p, 0, room);
    *len = z ? (u32)(z - p) : room;
    return p;
}

static u32 todigit(u32 ch)
{
    ch &= 0xFF;
    if (ch >= '0' && ch <= '9')
        return ch - '0';
    if ((ch | 0x20) >= 'a' && (ch | 0x20) <= 'z')
        return (ch | 0x20) - 'a' + 10;
    return 9999999; /* 80h-FFh: garbage on the hardware */
}

/* strtol / strtoul / atoi: skips 09h-0Dh and blanks, '-' only when signed ('+' ends the number),
 * then the prefixes "0b", "0x" and "o" override the base (the bug that breaks bases above 11);
 * atol also reads a leading '0' as octal (atoi does not). Bases outside 2..36 are decimal. No overflow check. */
static u32 strtol_at(Psx *psx, u32 src, u32 *end, u32 base, int sign, int octal0)
{
    u32 len;
    const u8 *s = str(psx, src, &len), *e = s ? s + len : s;
    if (!s)
        return 0;
    const u8 *p = s;
    while (p < e && (*p == ' ' || (*p >= 9 && *p <= 13)))
        p++;
    int neg = 0;
    if (sign && p < e && *p == '-')
        neg = 1, p++;
    if (base < 2 || base > 36)
        base = 10;
    if (p + 1 < e && p[0] == '0' && (p[1] | 0x20) == 'x')
        base = 16, p += 2;
    else if (p + 1 < e && p[0] == '0' && (p[1] | 0x20) == 'b')
        base = 2, p += 2;
    else if (p < e && (p[0] | 0x20) == 'o')
        base = 8, p++;
    else if (octal0 && p < e && p[0] == '0')
        base = 8;
    u32 v = 0;
    for (u32 d; p < e && (d = todigit(*p)) < base; p++)
        v = v * base + d;
    if (end)
        *end = src + (u32)(p - s);
    return neg ? (u32)-(s32)v : v;
}

/* ---- calls back into guest code (qsort, lsearch, bsearch) ---- */

static s32 compare(Psx *psx, u32 fn, u32 a, u32 b)
{
    G[R_A0] = a;
    G[R_A1] = b;
    psx->call(psx, fn);
    return (s32)G[R_V0];
}

static void swap(u8 *a, u8 *b, u32 w)
{
    if (!(((uintptr_t)a | (uintptr_t)b | w) & 3))
        for (u32 i = 0; i < w; i += 4) {
            u32 t = *(u32 *)(a + i);
            *(u32 *)(a + i) = *(u32 *)(b + i);
            *(u32 *)(b + i) = t;
        }
    else
        for (u32 i = 0; i < w; i++) {
            u8 t = a[i];
            a[i] = b[i];
            b[i] = t;
        }
}

/* quicksort, median of three, insertion sort below 8 elements, recursion on the smaller part */
static void qsort_run(Psx *psx, u32 base, u8 *host, u32 n, u32 w, u32 fn)
{
    while (n > 1) {
        if (n < 8) {
            for (u32 i = 1; i < n; i++)
                for (u32 j = i; j && compare(psx, fn, base + (j - 1) * w, base + j * w) > 0; j--)
                    swap(host + (j - 1) * w, host + j * w, w);
            return;
        }
        u32 m = n / 2, l = n - 1;
        if (compare(psx, fn, base, base + m * w) > 0)
            swap(host, host + m * w, w);
        if (compare(psx, fn, base + m * w, base + l * w) > 0) {
            swap(host + m * w, host + l * w, w);
            if (compare(psx, fn, base, base + m * w) > 0)
                swap(host, host + m * w, w);
        }
        swap(host + m * w, host + (l - 1) * w, w); /* the pivot waits before the last element */
        u32 pv = l - 1, i = 0, j = pv;
        for (;;) {
            while (++i < pv && compare(psx, fn, base + i * w, base + pv * w) < 0)
                ;
            while (j > i && compare(psx, fn, base + --j * w, base + pv * w) > 0)
                ;
            if (i >= j)
                break;
            swap(host + i * w, host + j * w, w);
        }
        swap(host + i * w, host + pv * w, w);
        if (i < n - 1 - i) {
            qsort_run(psx, base, host, i, w, fn);
            base += (i + 1) * w, host += (i + 1) * w, n -= i + 1;
        } else {
            qsort_run(psx, base + (i + 1) * w, host + (i + 1) * w, n - i - 1, w, fn);
            n = i;
        }
    }
}

/* ---- strtok: Sony copies the string into a 100h-byte kernel buffer and hands out pieces of it;
 * runs of separators are skipped only when the list has a single separator ---- */

#define K_STRTOK_BUF K_LIBC
#define K_STRTOK_POS (K_LIBC + 0x100)

static u32 strtok_at(Psx *psx, u32 src, u32 list)
{
    u32 llen, room;
    const u8 *l = str(psx, list, &llen);
    u8 *buf = at(psx, K_STRTOK_BUF, &room);
    u32 *posp = (u32 *)at(psx, K_STRTOK_POS, &room);
    if (src) {
        u32 n;
        const u8 *s = str(psx, src, &n);
        if (n > 0xFF)
            n = 0xFF;
        if (s)
            memcpy(buf, s, n);
        buf[n] = 0;
        *posp = 0;
    }
    u32 pos = *posp;
    if (pos > 0xFF || !buf[pos])
        return 0;
    u32 start = pos;
    while (buf[pos] && !(l && memchr(l, buf[pos], llen)))
        pos++;
    if (buf[pos]) {
        u8 sep = buf[pos];
        buf[pos++] = 0;
        if (llen == 1)
            while (buf[pos] == sep)
                pos++;
    }
    *posp = pos;
    return K_STRTOK_BUF + start;
}

int libc_call(Psx *psx, u32 fn)
{
    u32 a0 = G[R_A0], a1 = G[R_A1], a2 = G[R_A2];
    u32 n, m;
    u8 *d;
    const u8 *s, *t;
    switch (fn) {
    case 0x0A: G[R_V0] = todigit(a0); return 1;
    case 0x0C: case 0x0D: { /* strtoul / strtol (src, src_end, base) */
        u32 end = 0, v = strtol_at(psx, a0, &end, a2, fn == 0x0D, 0);
        if (a0 && a1 && (n = 4, d = span(psx, a1, &n)) && n == 4)
            memcpy(d, &end, 4);
        G[R_V0] = v;
        return 1;
    }
    case 0x0E: case 0x0F: G[R_V0] = (s32)a0 < 0 ? (u32)-(s32)a0 : a0; return 1; /* abs, labs */
    case 0x10: case 0x11: G[R_V0] = strtol_at(psx, a0, NULL, 10, 1, fn == 0x11); return 1; /* atoi, atol */
    case 0x12: { /* atob(src, num_dst): the value to [num_dst], returns the end */
        u32 end = a0, v = strtol_at(psx, a0, &end, 10, 1, 0);
        if ((n = 4, d = span(psx, a1, &n)) && n == 4)
            memcpy(d, &v, 4);
        G[R_V0] = end;
        return 1;
    }
    case 0x15: /* strcat */
    case 0x16: { /* strncat(dst, src, maxlen) */
        G[R_V0] = 0;
        u32 room;
        if (!a0 || !a1 || !(s = str(psx, a1, &m)) || !(d = at(psx, a0, &room)))
            return 1;
        str(psx, a0, &n);
        if (fn == 0x16 && m > a2)
            m = a2;
        if (n + m >= room) {
            if (n >= room)
                return 1;
            m = room - n - 1;
        }
        memmove(d + n, s, m);
        d[n + m] = 0;
        G[R_V0] = a0;
        return 1;
    }
    case 0x17: case 0x18: { /* strcmp / strncmp */
        s = str(psx, a0, &n), t = str(psx, a1, &m);
        u32 lim = fn == 0x18 ? a2 : 0xFFFFFFFFu, i = 0;
        s32 r = 0;
        if (s && t)
            for (; i < lim; i++) {
                u32 x = i < n ? s[i] : 0, y = i < m ? t[i] : 0;
                if (x != y || !x) {
                    r = (s32)x - (s32)y;
                    break;
                }
            }
        G[R_V0] = (u32)r;
        return 1;
    }
    case 0x19: /* strcpy */
    case 0x1A: { /* strncpy: zero-pads to n */
        G[R_V0] = 0;
        if (!a0 || !a1 || !(s = str(psx, a1, &m)))
            return 1;
        n = fn == 0x1A ? a2 : m + 1;
        if (!(d = span(psx, a0, &n)))
            return 1;
        u32 c = m < n ? m : n;
        memmove(d, s, c);
        if (n > c)
            memset(d + c, 0, fn == 0x1A ? n - c : 1);
        G[R_V0] = a0;
        return 1;
    }
    case 0x1B: str(psx, a0, &n); G[R_V0] = a0 ? n : 0; return 1;
    case 0x1C: case 0x1E: /* index / strchr */
    case 0x1D: case 0x1F: { /* rindex / strrchr */
        G[R_V0] = 0;
        if (!(s = str(psx, a0, &n)))
            return 1;
        const u8 *hit = (u8)a1 ? (fn & 1 ? NULL : memchr(s, (u8)a1, n)) : s + n;
        if ((u8)a1 && (fn & 1))
            for (u32 i = n; i--;)
                if (s[i] == (u8)a1) {
                    hit = s + i;
                    break;
                }
        G[R_V0] = hit ? a0 + (u32)(hit - s) : 0;
        return 1;
    }
    case 0x20: { /* strpbrk: no match returns src, unless src is empty */
        G[R_V0] = 0;
        if (!(s = str(psx, a0, &n)) || !(t = str(psx, a1, &m)))
            return 1;
        u32 i = 0;
        while (i < n && !memchr(t, s[i], m))
            i++;
        G[R_V0] = i < n ? a0 + i : n ? a0 : 0;
        return 1;
    }
    case 0x21: case 0x22: { /* strspn: the first char in list; strcspn: the first not in it */
        G[R_V0] = 0;
        if (!(s = str(psx, a0, &n)) || !(t = str(psx, a1, &m)))
            return 1;
        u32 i = 0;
        int want = fn == 0x21;
        while (i < n && (memchr(t, s[i], m) != NULL) != want)
            i++;
        G[R_V0] = i;
        return 1;
    }
    case 0x23: G[R_V0] = strtok_at(psx, a0, a1); return 1;
    case 0x24: { /* strstr: a failed partial match does not restart one char later ("aab" in "aaab") */
        G[R_V0] = 0;
        if (!(s = str(psx, a0, &n)) || !(t = str(psx, a1, &m)))
            return 1;
        if (!m) {
            G[R_V0] = a0;
            return 1;
        }
        for (u32 i = 0, j = 0; i < n; i++) {
            if (s[i] == t[j]) {
                if (++j == m) {
                    G[R_V0] = a0 + i + 1 - m;
                    return 1;
                }
            } else if (j) {
                j = s[i] == t[0];
            }
        }
        return 1;
    }
    case 0x25: G[R_V0] = (a0 >= 'a' && a0 <= 'z') ? a0 - 32 : a0; return 1;
    case 0x26: G[R_V0] = (a0 >= 'A' && a0 <= 'Z') ? a0 + 32 : a0; return 1;
    case 0x27: /* bcopy(src, dst, len): returns src */
        if (a0 && a2 <= 0x7FFFFFFFu) {
            n = m = a2;
            u8 *x = span(psx, a1, &n), *y = span(psx, a0, &m);
            if (x && y)
                memmove(x, y, n < m ? n : m);
        }
        G[R_V0] = a0;
        return 1;
    case 0x28: /* bzero */
    case 0x2B: /* memset */
        n = fn == 0x28 ? a1 : a2;
        if (a0 && n <= 0x7FFFFFFFu && (d = span(psx, a0, &n)))
            memset(d, fn == 0x28 ? 0 : (u8)a1, n);
        G[R_V0] = a0;
        return 1;
    case 0x29: case 0x2D: { /* bcmp / memcmp: returns the difference of the bytes AFTER the mismatch */
        G[R_V0] = 0;
        n = m = a2 + 1;
        s = span(psx, a0, &n), t = span(psx, a1, &m);
        if (!a0 || !s || !t)
            return 1;
        u32 lim = n < m ? n : m;
        for (u32 i = 0; i < a2 && i + 1 < lim; i++)
            if (s[i] != t[i]) {
                G[R_V0] = (u32)((s32)s[i + 1] - (s32)t[i + 1]);
                break;
            }
        return 1;
    }
    case 0x2A: /* memcpy: forward, byte by byte: an overlapping dst > src repeats the pattern */
        if (a0 && a2 <= 0x7FFFFFFFu) {
            n = m = a2;
            u8 *x = span(psx, a0, &n), *y = span(psx, a1, &m);
            u32 c = n < m ? n : m;
            if (x && y) {
                if (x > y && x < y + c)
                    for (u32 i = 0; i < c; i++)
                        x[i] = y[i];
                else
                    memmove(x, y, c);
            }
        }
        G[R_V0] = a0;
        return 1;
    case 0x2C: /* memmove: the backward copy (src < dst) moves len+1 bytes */
        if (a0 && a2 <= 0x7FFFFFFFu) {
            n = m = a2 + (a1 < a0);
            u8 *x = span(psx, a0, &n), *y = span(psx, a1, &m);
            if (x && y)
                memmove(x, y, n < m ? n : m);
        }
        G[R_V0] = a0;
        return 1;
    case 0x2E: /* memchr */
        G[R_V0] = 0;
        n = a2;
        if (a0 && a2 <= 0x7FFFFFFFu && (s = span(psx, a0, &n)) && (t = memchr(s, (u8)a1, n)))
            G[R_V0] = a0 + (u32)(t - s);
        return 1;
    case 0x31: { /* qsort(base, nel, width, callback) */
        n = a1 * a2;
        Cpu save = psx->cpu;
        if (a1 > 1 && a2 && (d = span(psx, a0, &n)) && n == a1 * a2)
            qsort_run(psx, a0, d, a1, a2, G[R_A3]);
        psx->cpu = save;
        return 1;
    }
    case 0x35: case 0x36: { /* lsearch / bsearch(key, base, nel, width, callback) */
        u32 w = G[R_A3], cb = psx_read(psx, G[R_SP] + 0x10, 4), hit = 0;
        Cpu save = psx->cpu;
        if (fn == 0x35) {
            for (u32 i = 0; i < a2 && !hit; i++)
                if (!compare(psx, cb, a0, a1 + i * w))
                    hit = a1 + i * w;
        } else {
            for (u32 lo = 0, hi = a2; lo < hi && !hit;) {
                u32 mid = lo + (hi - lo) / 2;
                s32 r = compare(psx, cb, a0, a1 + mid * w);
                if (!r)
                    hit = a1 + mid * w;
                else if (r < 0)
                    hi = mid;
                else
                    lo = mid + 1;
            }
        }
        psx->cpu = save;
        G[R_V0] = hit;
        return 1;
    }
    }
    return 0;
}
