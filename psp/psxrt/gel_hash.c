/*
 * psxrt: gel_hash.h. The GE's own reading of the list: BASE gives the next address its bits
 * 24-27; VADDR and IADDR set where PRIM reads, and PRIM moves past what it read; CALL, RET and
 * JUMP move the reading; END stops it. The vertex size is the GE's: each component aligned to its
 * own size, the whole to the largest.
 */
#include "gel_hash.h"

#define STEPS (16u << 20) /* a list that loops is cut here */
#define DEPTH 8

static uint64_t mix(uint64_t h, uint32_t w)
{
    h = (h ^ w) * 0x100000001B3ull;
    return h ^ h >> 29;
}

uint64_t gel_hash_bytes(uint64_t h, const void *p, uint32_t n)
{
    const uint8_t *b = (const uint8_t *)p;
    for (; n >= 4; n -= 4, b += 4)
        h = mix(h, (uint32_t)b[0] | (uint32_t)b[1] << 8 | (uint32_t)b[2] << 16 | (uint32_t)b[3] << 24);
    for (; n; n--)
        h = mix(h, *b++);
    return h;
}

/* bytes of one component of 1, 2 or 3 (8-bit, 16-bit, float) */
static uint32_t comp(uint32_t f) { return f == 3 ? 4 : f; }

static uint32_t vertex_size(uint32_t vt)
{
    static const uint8_t col[8] = {0, 0, 0, 0, 2, 2, 2, 4};
    uint32_t size = 0, big = 1, s;
#define PART(bytes, n)                                                                                                     \
    if ((s = (bytes)) != 0) {                                                                                              \
        size = (size + s - 1) / s * s + s * (n);                                                                           \
        big = s > big ? s : big;                                                                                           \
    }
    PART(comp(vt >> 9 & 3), (vt >> 14 & 7) + 1) /* weights */
    PART(comp(vt & 3), 2)                       /* texture */
    PART(col[vt >> 2 & 7], 1)                   /* color */
    PART(comp(vt >> 5 & 3), 3)                  /* normal */
    PART(comp(vt >> 7 & 3), 3)                  /* position */
#undef PART
    size = (size + big - 1) / big * big;
    return size * ((vt >> 18 & 7) + 1); /* morph targets */
}

void gel_hash(GelHash *g, const unsigned int *pc, GelResolve resolve)
{
    uint32_t base = 0, vaddr = 0, iaddr = 0, vtype = 0, cbp = 0;
    const unsigned int *stack[DEPTH];
    int sp = 0;
    for (uint32_t n = 0; n < STEPS; n++) {
        uint32_t w = *pc++, cmd = w >> 24, arg = w & 0xFFFFFF;
        g->h = mix(g->h, w), g->words++;
        switch (cmd) {
        case 0x01:
            vaddr = base | arg;
            break;
        case 0x02:
            iaddr = base | arg;
            break;
        case 0x04: {
            uint32_t count = arg & 0xFFFF, vs = vertex_size(vtype), is = comp(vtype >> 11 & 3), nv = count;
            g->prims++;
            if (is) {
                const uint8_t *ix = (const uint8_t *)resolve(iaddr, count * is);
                if (!ix) {
                    g->misses++;
                    break;
                }
                g->h = gel_hash_bytes(g->h, ix, count * is);
                nv = 0;
                for (uint32_t i = 0; i < count; i++) {
                    uint32_t v = is == 1 ? ix[i] : is == 2 ? (uint32_t)ix[2 * i] | (uint32_t)ix[2 * i + 1] << 8 : 0;
                    nv = v + 1 > nv ? v + 1 : nv;
                }
                iaddr += count * is;
            }
            if (vs && nv) {
                const void *v = resolve(vaddr, nv * vs);
                if (v)
                    g->h = gel_hash_bytes(g->h, v, nv * vs);
                else
                    g->misses++;
            }
            if (!is)
                vaddr += count * vs;
            break;
        }
        case 0x08:
        case 0x0A:
            if (cmd == 0x0A) {
                if (sp == DEPTH)
                    goto lost;
                stack[sp++] = pc, g->calls++;
            }
            if (!(pc = (const unsigned int *)resolve(base | arg, 4)))
                goto lost;
            break;
        case 0x0B:
            if (!sp)
                goto lost;
            pc = stack[--sp];
            break;
        case 0x0C:
            return;
        case 0x10:
            base = (arg & 0xF0000) << 8;
            break;
        case 0x12:
            vtype = arg;
            break;
        case 0xB0:
            cbp = (cbp & 0xFF000000u) | arg;
            break;
        case 0xB1:
            cbp = (cbp & 0xFFFFFF) | (arg & 0xF0000) << 8;
            break;
        case 0xC4: {
            const void *c = resolve(cbp, (arg & 0x3F) * 32);
            if (c)
                g->h = gel_hash_bytes(g->h, c, (arg & 0x3F) * 32);
            else
                g->misses++;
            break;
        }
        }
    }
lost: /* cut, or a call, return or jump that cannot be followed */
    g->misses++;
}
