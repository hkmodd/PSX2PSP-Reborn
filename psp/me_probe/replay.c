/* me_probe: a GE translator dump run again through ge.c (replay.h). */
#include <string.h>

#include "ge.h"
#include "replay.h"

static u32 no_clock(void) { return 0; }

/* the polygon commands rd_packet ran: its whole commands within the words it took */
static u32 packet_polys(const u32 *w, u32 took)
{
    u32 k = 0, n = 0;
    while (k < took) {
        k += gedp_poly_words(w[k]);
        n++;
    }
    return n;
}

int replay_run(Psx *psx, const u32 *d, u32 n, int mode, const ReplayHooks *h, ReplayResult *r)
{
    static const ReplayHooks none;
    h = h ? h : &none;
    u32 (*clock)(void) = h->clock ? h->clock : no_clock;
    memset(r, 0, sizeof *r);
    if (n < GEDP_HEAD || d[0] != GEDP_MAGIC || d[1] != GEDP_VERSION || d[4] != VRAM_W || d[5] != VRAM_H ||
        d[6] != GEDP_GPU_WORDS || d[7] != GEDP_PRIM_WORDS)
        return r->err = "not a dump of this version, or of another Gpu", 0;
    Gpu *g = &psx->gpu;
    const GpuRenderer *rd = NULL;
    ReplayFrame f;
    memset(&f, 0, sizeof f);
    u32 k = GEDP_HEAD;
    for (;;) {
        if (k + 2 > n)
            return r->err = "the dump ends without END", 0;
        u32 type = d[k], len = d[k + 1];
        const u32 *a = d + k + 2;
        if (len > n - k - 2)
            return r->err = "an event past the dump's end", 0;
        k += 2 + len;
        r->events++;
        if (type == GEDP_END)
            return 1;
        if (type == GEDP_STATE) {
            if (len != GEDP_GPU_WORDS)
                return r->err = "STATE of another size", 0;
            gedp_gpu_load(g, a);
            continue;
        }
        if (type == GEDP_VRAM) {
            const u32 *w = a + 1, *e = a + len;
            for (u32 s = 0; s < a[0]; s++) {
                if (e - w < 3 || w[0] >= VRAM_H || w[1] >= VRAM_W || w[2] > VRAM_W - w[1] || (u32)(e - w) < 3 + (w[2] + 1) / 2)
                    return r->err = "a bad VRAM span", 0;
                u16 *p = g->vram + w[0] * VRAM_W + w[1];
                u32 m = w[2];
                w += 3;
                for (u32 i = 0; i < m; i++)
                    p[i] = (u16)(w[i / 2] >> (i & 1) * 16);
                w += (m + 1) / 2;
            }
            continue;
        }
        if (type == GEDP_INIT) {
            if (rd)
                return r->err = "a second INIT", 0;
            geh_start(psx, mode);
            rd = g->renderer;
            continue;
        }
        if (!rd)
            return r->err = "a call before INIT", 0;
        if (!len)
            return r->err = "an event without its return word", 0;
        u32 want = a[len - 1], got = 0, t = clock();
        GpuPrim p;
        GpuVtx v[4];
        switch (type) {
        case GEDP_TRI:
            gedp_vtx_load(gedp_prim_load(a, &p), v, 3);
            got = (u32)rd->triangle(rd->self, &p, &v[0], &v[1], &v[2]);
            f.polys++;
            break;
        case GEDP_QUAD:
            gedp_vtx_load(gedp_prim_load(a, &p), v, 4);
            got = (u32)rd->quad(rd->self, &p, v);
            f.polys++;
            break;
        case GEDP_RECT: {
            const u32 *w = gedp_prim_load(a, &p);
            gedp_vtx_load(w + 4, v, 1);
            got = (u32)rd->rect(rd->self, &p, (s32)w[0], (s32)w[1], (s32)w[2], (s32)w[3], v, (int)w[4 + GEDP_VTX_WORDS],
                                (int)w[5 + GEDP_VTX_WORDS]);
            break;
        }
        case GEDP_LINE:
            gedp_vtx_load(gedp_prim_load(a, &p), v, 2);
            got = (u32)rd->line(rd->self, &p, &v[0], &v[1]);
            break;
        case GEDP_FILL:
            got = (u32)rd->fill(rd->self, (s32)a[0], (s32)a[1], (s32)a[2], (s32)a[3], (u16)a[4]);
            break;
        case GEDP_COPY:
            got = (u32)rd->copy(rd->self, (s32)a[0], (s32)a[1], (s32)a[2], (s32)a[3], (s32)a[4], (s32)a[5], (int)a[6], (u16)a[7]);
            break;
        case GEDP_READ:
            rd->read(rd->self, (s32)a[0], (s32)a[1], (s32)a[2], (s32)a[3]);
            break;
        case GEDP_WROTE:
            rd->wrote(rd->self, (s32)a[0], (s32)a[1], (s32)a[2], (s32)a[3]);
            break;
        case GEDP_POLY:
            if (!rd->poly)
                return r->err = "POLY with no rd_poly", 0;
            got = (u32)rd->poly(rd->self, a);
            f.polys++;
            break;
        case GEDP_PACKET:
            if (!rd->packet)
                return r->err = "PACKET with no rd_packet", 0;
            got = rd->packet(rd->self, a, len - 1);
            f.polys += packet_polys(a, got);
            break;
        case GEDP_FRAME: {
            if (len != GEDP_FRAME_WORDS + 1)
                return r->err = "FRAME of another size", 0;
            geh_frame(a[0], &f.got);
            u32 te = clock() - t;
            const GehFrame *o = &f.got;
            f.want = a;
            f.match = o->frame == a[0] && o->lists == a[1] && o->words == a[2] && o->prims == a[3] && o->calls == a[4] &&
                      o->misses == a[5] && o->unknown == a[6] && (u32)(o->h >> 32) == a[7] && (u32)o->h == a[8] &&
                      (u32)o->mem == a[9] && (!o->mem || ((u32)(o->mh >> 32) == a[10] && (u32)o->mh == a[11]));
            f.t_end = te;
            r->frames_bad += mode == GE_SHADOW && !f.match;
            if (r->frames++) { /* the first frame warms ge.c */
                r->polys += f.polys, r->t_calls += f.t_calls, r->t_end += f.t_end;
            }
            if (h->frame)
                h->frame(h->user, &f);
            memset(&f, 0, sizeof f);
            continue;
        }
        default:
            return r->err = "an unknown event", 0;
        }
        f.t_calls += clock() - t;
        r->rets_bad += mode == GE_SHADOW && got != want;
    }
}
