/* PsyQ libpress `DecDCTvlc` run natively (vlc.h). Each part follows the guest routine's code
 * (Sheep Dog 'n' Wolf's SLES_028.95: 0x80111870), with its addresses in the comments, and counts
 * the instructions and the memory accesses the guest would make on the same path. */
#include "vlc.h"

typedef uint16_t __attribute__((may_alias)) a16;
typedef uint32_t __attribute__((may_alias)) a32;
#define R16(a) (*(a16 *)(ram + ((a) & VLC_RAM_MASK)))
#define R32(a) (*(a32 *)(ram + ((a) & VLC_RAM_MASK)))

/* v1 counts the bits taken from the low half of v0: past 16, the next halfword comes in. */
#define REFILL()                                         \
    do {                                                 \
        r->ins += 3;                                     \
        if (v1 & 16) {                                   \
            v0 |= (uint32_t)R16(a0) << (v1 & 15);        \
            a0 += 2, r->ins += 4, r->mem++;              \
        }                                                \
        v1 &= 15;                                        \
    } while (0)

void vlc_start(uint8_t *ram, const VlcVars *g, Vlc *r, uint32_t bs, uint32_t buf)
{
    uint32_t t1 = R32(g->size);
    r->ins = 8, r->mem = 1; /* 0x870-0x88C */
    if (!bs) {              /* 0x890: on from the saved state, in its own buffer */
        uint32_t s = g->save;
        r->a0 = R32(s), r->a1 = R32(s + 4), r->v0 = R32(s + 8), r->v1 = R32(s + 12), r->t4 = R32(s + 16);
        r->t5 = R32(s + 20), r->t7 = R32(s + 24), r->t8 = R32(s + 28), r->t9 = R32(s + 32);
        r->t6 = r->a1 + t1 + t1;
        r->at_dc = 0;
        r->ins += 14, r->mem += 9;
        return;
    }
    /* 0x8C8: the header, then the first 32 bits */
    uint32_t h = R32(bs), t2 = R16(bs + 6);
    r->t5 = r->t7 = r->t8 = r->t9 = 0;
    r->t6 = buf + t1 + t1;
    r->t4 = (uint32_t)R16(bs + 4) << 10;
    r->ins += 27;
    if ((int32_t)(t2 - 3) >= 0) /* version 3: DC codes are differences */
        r->t5 = 1, r->ins++;
    r->v0 = (uint32_t)R16(bs + 8) << 16 | R16(bs + 10);
    r->v1 = 0;
    r->a0 = bs + 12;
    R32(buf) = h; /* the MDEC's command word */
    R32(g->end) = buf + 4 + ((h & 0xFFFF) << 2);
    r->a1 = buf + 2;
    r->mem += 7;
    r->at_dc = 1;
}

int vlc_run(uint8_t *ram, const VlcVars *g, Vlc *rp, uint32_t budget)
{
    Vlc rr = *rp, *const r = &rr; /* a local copy: the stores into RAM cannot alias it */
    uint32_t a0 = r->a0, a1 = r->a1, v0 = r->v0, v1 = r->v1, t4 = r->t4, t5 = r->t5, t6 = r->t6;
    uint32_t t7 = r->t7, t8 = r->t8, t9 = r->t9, t0, t1, t2, t3, at;
    uint32_t stop = r->ins + 2 * r->mem + budget;
    if (!r->at_dc)
        goto ac;
dc: /* 0x938: a block's DC code */
    if ((int32_t)(r->ins + 2 * r->mem - stop) >= 0) {
        r->a0 = a0, r->a1 = a1, r->v0 = v0, r->v1 = v1, r->t4 = t4, r->t5 = t5;
        r->t7 = t7, r->t8 = t8, r->t9 = t9, r->at_dc = 1;
        return *rp = rr, -1;
    }
    t0 = v0 >> 22;
    r->ins += 5, a1 += 2;
    if (!t5) { /* 0xA10: version 2, the DC code is 10 bits */
        if (t0 == 0x1FF)
            goto end;
        v0 <<= 10, v1 += 10, r->ins += 2;
        REFILL();
        R16(a1) = (uint16_t)(t4 | t0);
        r->ins += 2, r->mem++;
    } else { /* 0x940: version 3, a difference from the last block of its kind */
        if (t0 == 0x3FF)
            goto end;
        at = g->tab1 - 1024; /* Cr and Cb */
        r->ins += 3;
        if ((int32_t)(t5 - 3) >= 0)
            at -= 1024, r->ins++; /* Y */
        t0 = (v0 >> 24 << 2) + at;
        t1 = R16(t0), t2 = R16(t0 + 2);
        r->ins += 8, r->mem += 2;
        v0 <<= t1 & 31;
        t0 = 0;
        if (t2) { /* 0x97C: t2 bits of difference; a leading 0 means a negative one */
            at = 32 - t2;
            t0 = v0 >> (at & 31);
            int neg = (int32_t)v0 >= 0;
            v0 <<= t2 & 31;
            r->ins += 5;
            if (neg)
                t0 -= 0xFFFFFFFFu >> (at & 31), r->ins += 3;
            v1 += t2, r->ins++;
        }
        v1 += t1, r->ins++;
        REFILL();
        r->ins += 3; /* 0x9C0 */
        if ((int32_t)(t5 - 2) > 0)
            t1 = t9 + t0, t9 += t0, r->ins++;
        else if (t5 == 2)
            t1 = t8 + t0, t8 += t0, r->ins += 4;
        else
            t1 = t7 + t0, t7 += t0, r->ins += 5;
        R16(a1) = (uint16_t)((t1 << 2 & 0x3FF) | t4);
        t5++, r->ins += 7, r->mem++;
        if (t5 == 7)
            t5 = 1, r->ins += 2;
    }
    /* 0xA48: the limit is checked once a block */
    at = a1 - t6;
    a1 += 2, r->ins += 3;
    if ((int32_t)at >= 0)
        goto save;
ac:
    for (;;) { /* 0xA54: up to three codes from the next 13 bits, or one from the next 17 */
        t0 = (v0 >> 19 << 3) + g->tab1;
        t1 = R32(t0);
        r->ins += 7, r->mem++;
        if (t1) {
            t3 = R32(t0 + 4);
            r->ins++, r->mem++;
        } else {
            v0 <<= 8, v1 += 8, r->ins += 2;
            REFILL();
            t1 = R32((v0 >> 23 << 2) + g->tab2);
            t3 = 0;
            r->ins += 7, r->mem++;
        }
        at = t1 & 0xFF; /* the bits they take */
        v0 <<= at & 31, v1 += at, r->ins += 2;
        REFILL();
        t1 >>= 16, r->ins += 4;
        if (t1 == 0x7C1F)
            goto esc;
        R16(a1) = (uint16_t)t1;
        r->ins += 2, r->mem++;
        if (t1 == 0xFE00)
            goto dc;
        a1 += 2, r->ins += 2;
        if (!t3)
            continue;
        t2 = t3 & 0xFFFF, r->ins += 4;
        if (t2 == 0x7C1F)
            goto esc;
        R16(a1) = (uint16_t)t2;
        r->ins += 2, r->mem++;
        if (t2 == 0xFE00)
            goto dc;
        t2 = t3 >> 16;
        a1 += 2, r->ins += 3;
        if (!t2)
            continue;
        r->ins += 3;
        if (t2 == 0x7C1F)
            goto esc;
        R16(a1) = (uint16_t)t2;
        r->ins += 2, r->mem++;
        if (t2 == 0xFE00)
            goto dc;
        a1 += 2, r->ins += 2;
        continue;
    esc: /* 0xB38: an escape, the next 16 bits as they are */
        R16(a1) = (uint16_t)(v0 >> 16);
        a1 += 2;
        v0 = v0 << 16 | (uint32_t)R16(a0) << (v1 & 31);
        a0 += 2, r->ins += 9, r->mem += 2;
    }
end: /* 0xB5C: the picture's end, the rest of its output filled */
    t1 = R32(g->end + 0);
    r->ins += 4, r->mem++;
    while ((int32_t)(a1 - t1) < 0)
        R16(a1) = 0xFE00, a1 += 2, r->ins += 6, r->mem++;
    r->ins += 5;
    r->a0 = a0, r->a1 = a1;
    return *rp = rr, 0;
save: /* 0xB8C */
    R32(g->save) = a0, R32(g->save + 4) = a1, R32(g->save + 8) = v0, R32(g->save + 12) = v1;
    R32(g->save + 16) = t4, R32(g->save + 20) = t5, R32(g->save + 24) = t7, R32(g->save + 28) = t8;
    R32(g->save + 32) = t9;
    r->ins += 13, r->mem += 9;
    r->a0 = a0, r->a1 = a1;
    return *rp = rr, 1;
}
