/*
 * psxrt: ge.c's host on a PC, for the sim (step 1c of the pure translator, me-translator design
 * note). There is no GE: each list is hashed as it ends (gel_hash.h) and nothing else is done
 * with it. ge.c runs in GE_SHADOW mode, so the software rasterizer draws everything and the sim's
 * pictures are its own; the hashes are the translator's output, frame by frame, to check a change
 * of ge.c against.
 *
 * The GE addresses 28 bits and the PC's pointers have 64: every block of memory a list addresses
 * is a region, and the GE address of a pointer in region i at offset o is i << 24 | o, o from the
 * region's first byte. On the PSP every region starts on 64 bytes at least (A64 statics, partition
 * blocks of 256), so o's low bits are the PSP's: ge.c's alignment choices (gel_addr(p) & 15) and
 * the hashes are the PSP's, wherever the PC put the memory (the sim's VRAM moves with sizeof(Psx)).
 * Pointers in no region and below 16 MB pass as they are (VRAM offsets, which pspgu's draw buffer
 * takes).
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "ge.h"
#include "ge_dump.h"
#include "ge_host.h"
#include "gel_hash.h"

#define REGIONS 16 /* ids 1-15: BASE carries 4 bits */

static struct {
    uintptr_t p;
    u32 bytes;
} reg[REGIONS];
static int nreg;
static u32 unknown; /* pointers geh_addr found in no region */

void geh_region(const void *p, u32 bytes)
{
    if (nreg == REGIONS - 1 || bytes >= 1u << 24)
        abort();
    reg[++nreg].p = (uintptr_t)p, reg[nreg].bytes = bytes;
}

uint32_t geh_addr(const void *p)
{
    uintptr_t a = (uintptr_t)p;
    for (int i = 1; i <= nreg; i++)
        if (a >= reg[i].p && a - reg[i].p <= reg[i].bytes)
            return (u32)i << 24 | (u32)(a - reg[i].p);
    if (a < 1u << 24)
        return (u32)a;
    unknown++;
    return 0x0FFFFFFFu;
}

static const void *resolve(uint32_t addr, uint32_t bytes)
{
    u32 i = addr >> 24, o = addr & 0xFFFFFF;
    if (!i || (int)i > nreg || o > reg[i].bytes || bytes > reg[i].bytes - o)
        return NULL;
    return (const void *)(reg[i].p + o);
}

/* a clock that only counts its reads: the sim's logs stay the same run to run */
u32 geh_us(void)
{
    static u32 t;
    return ++t;
}
void geh_wb(const void *p, u32 n) { (void)p, (void)n; }
void geh_wbinv(const void *p, u32 n) { (void)p, (void)n; }

/* The PSP's memory for ge.c, so that it decides as there: Sheep Dog's psxrt.log has its 32
 * texture page slots and 512 KB vertex ring, then 1531 KB free (no 15-bit copy). */
static u32 budget = (32 * 256 * 256 + 64) + (512 * 1024 + 64) + 1531 * 1024;

static void *region_alloc(u32 bytes)
{
    u8 *m = (u8 *)calloc(1, (size_t)bytes + 64);
    if (!m)
        abort();
    void *p = (void *)(((uintptr_t)m + 63) & ~(uintptr_t)63);
    geh_region(p, bytes);
    return p; /* for the sim's lifetime */
}

void *geh_alloc(const char *name, u32 bytes)
{
    (void)name;
    if (bytes + 64 > budget)
        return NULL;
    budget -= bytes + 64;
    return region_alloc(bytes);
}
u32 geh_free(void) { return budget; }

u16 *geh_vram;
static GelList pc_list;
GelList *geh_list = &pc_list;
GelScissor geh_scissor;

static GelHash fh = {GEL_HASH_SEED, 0, 0, 0, 0};
static u32 lists;

int geh_list_start(void *buf)
{
    pc_list.start = pc_list.current = (unsigned int *)buf;
    return 1;
}
void geh_list_stall(int id, void *buf, u32 n) { (void)id, (void)buf, (void)n; }
/* as sceGuFinish ends a GU_SEND list: FINISH, END */
u32 geh_list_finish(void)
{
    gel_cmd(&pc_list, 0x0F, 0);
    gel_cmd(&pc_list, 0x0C, 0);
    gel_hash(&fh, pc_list.start, resolve);
    lists++;
    return (u32)((u8 *)pc_list.current - (u8 *)pc_list.start);
}
void geh_list_wait(void) {}
int geh_check(void *buf, u32 words, char *out, int cap) { return (void)buf, (void)words, (void)out, (void)cap, 0; }

/* ---- ge.c's start and frame end: the sim's, and a dump's replay (ge_host_pc.h) ---- */

/* ge.c from cold, as the PSP's boot starts it (once a process: ge.c is one) */
void geh_start(Psx *psx, int mode)
{
    geh_vram = (u16 *)region_alloc(2 * 1024 * 1024);
    ge_init(psx, mode);
    fh = (GelHash){GEL_HASH_SEED, 0, 0, 0, 0}, lists = 0; /* ge_init's own lists */
}

/* The lists of a frame end as the PSP's present ends them; their hash, and every 60 frames that
 * of all the memory the lists address too. */
void geh_frame(u32 frame, GehFrame *o)
{
    ge_sync();
    o->frame = frame, o->lists = lists, o->words = fh.words, o->prims = fh.prims, o->calls = fh.calls;
    o->misses = fh.misses, o->unknown = unknown, o->h = fh.h, o->mem = frame % 60 == 0, o->mh = GEL_HASH_SEED;
    fh = (GelHash){GEL_HASH_SEED, 0, 0, 0, 0}, lists = 0;
    for (int i = 1; o->mem && i <= nreg; i++)
        o->mh = gel_hash_bytes(o->mh, (const void *)reg[i].p, reg[i].bytes);
    ge_touch();
}

void geh_frame_log(Psx *psx, const GehFrame *o)
{
    psx_log(psx, "geh: frame %u, %u lists, %u words, %u prims, %u calls, %u misses, %u unknown: %08X%08X", (unsigned)o->frame,
            (unsigned)o->lists, (unsigned)o->words, (unsigned)o->prims, (unsigned)o->calls, (unsigned)o->misses,
            (unsigned)o->unknown, (unsigned)(o->h >> 32), (unsigned)o->h);
    if (o->mem)
        psx_log(psx, "geh: frame %u memory %08X%08X", (unsigned)o->frame, (unsigned)(o->mh >> 32), (unsigned)o->mh);
}

/* ---- PSX2PSP_SIM_GE_DUMP=<first>,<frames>:<file>: everything ge.c is given, to run again ----
 *
 * ge.c starts cold at the end of frame first - 1 (the sim runs without it until then: in
 * GE_SHADOW mode it changes no picture) and is given all VRAM, so a replay that starts it the
 * same way from the dumped VRAM and state runs the same translation. The renderer is wrapped:
 * each call the core makes is written with what it was given, after the GPU state and the VRAM
 * spans that changed since the last call. Calls ge.c makes back into the core and the core into
 * the renderer from there (gpu_poly_soft from rd_packet) run inside a call: the replay makes
 * them again, so they are not written. */

static struct {
    FILE *f;
    u32 first, frames;
    Psx *psx;
    int on;                    /* recording */
    const GpuRenderer *inner;  /* ge.c's */
    GpuRenderer r;             /* the wrapper */
    int depth;                 /* inside a call */
    u16 *seen;                 /* VRAM as the replay has it */
    u32 st[GEDP_GPU_WORDS];    /* the state as the replay has it */
    u32 buf[1 + VRAM_W / 2 + 3]; /* one VRAM span */
    u32 words;
} rec;

static void rec_put(const u32 *w, u32 n)
{
    if (fwrite(w, 4, n, rec.f) != n)
        abort();
    rec.words += n;
}

static void rec_head(u32 type, u32 n)
{
    u32 h[2] = {type, n};
    rec_put(h, 2);
}

/* the state and the VRAM spans that changed since the replay last had them */
static void rec_sync(void)
{
    const Gpu *g = &rec.psx->gpu;
    u32 st[GEDP_GPU_WORDS];
    gedp_gpu_save(g, st);
    if (memcmp(st, rec.st, sizeof st)) {
        rec_head(GEDP_STATE, GEDP_GPU_WORDS), rec_put(st, GEDP_GPU_WORDS);
        memcpy(rec.st, st, sizeof st);
    }
    u32 spans = 0, words = 0;
    for (int y = 0; y < VRAM_H; y++) {
        const u16 *a = g->vram + y * VRAM_W, *b = rec.seen + y * VRAM_W;
        int x0 = 0, x1 = VRAM_W;
        while (x0 < VRAM_W && a[x0] == b[x0])
            x0++;
        if (x0 == VRAM_W)
            continue;
        while (a[x1 - 1] == b[x1 - 1])
            x1--;
        spans++, words += 3 + (u32)(x1 - x0 + 1) / 2;
    }
    if (!spans)
        return;
    rec_head(GEDP_VRAM, 1 + words), rec_put(&spans, 1);
    for (int y = 0; y < VRAM_H; y++) {
        u16 *a = g->vram + y * VRAM_W, *b = rec.seen + y * VRAM_W;
        int x0 = 0, x1 = VRAM_W;
        while (x0 < VRAM_W && a[x0] == b[x0])
            x0++;
        if (x0 == VRAM_W)
            continue;
        while (a[x1 - 1] == b[x1 - 1])
            x1--;
        u32 n = (u32)(x1 - x0), *w = rec.buf;
        *w++ = (u32)y, *w++ = (u32)x0, *w++ = n;
        for (u32 i = 0; i < n; i += 2)
            *w++ = a[x0 + i] | (i + 1 < n ? (u32)a[x0 + i + 1] << 16 : 0);
        rec_put(rec.buf, (u32)(w - rec.buf));
        memcpy(b + x0, a + x0, n * 2);
    }
}

/* before a call the core makes: 1 when it is to be written (not inside another) */
static int rec_pre(void)
{
    if (rec.depth++ || !rec.on)
        return 0;
    rec_sync();
    return 1;
}

/* after it: what it returned, and the state and VRAM it left are the replay's too */
static void rec_post(int top, u32 ret)
{
    rec.depth--;
    if (!top)
        return;
    rec_put(&ret, 1);
    gedp_gpu_save(&rec.psx->gpu, rec.st);
    memcpy(rec.seen, rec.psx->gpu.vram, VRAM_W * VRAM_H * 2);
}

static int rec_triangle(void *self, const GpuPrim *p, const GpuVtx *v0, const GpuVtx *v1, const GpuVtx *v2)
{
    int top = rec_pre();
    if (top) {
        u32 w[GEDP_PRIM_WORDS + 3 * GEDP_VTX_WORDS], *e = gedp_prim_save(w, p);
        e = gedp_vtx_save(e, v0, 1), e = gedp_vtx_save(e, v1, 1), e = gedp_vtx_save(e, v2, 1);
        rec_head(GEDP_TRI, (u32)(e - w) + 1), rec_put(w, (u32)(e - w));
    }
    int r = rec.inner->triangle(self, p, v0, v1, v2);
    rec_post(top, (u32)r);
    return r;
}

static int rec_quad(void *self, const GpuPrim *p, const GpuVtx *v)
{
    int top = rec_pre();
    if (top) {
        u32 w[GEDP_PRIM_WORDS + 4 * GEDP_VTX_WORDS], *e = gedp_vtx_save(gedp_prim_save(w, p), v, 4);
        rec_head(GEDP_QUAD, (u32)(e - w) + 1), rec_put(w, (u32)(e - w));
    }
    int r = rec.inner->quad(self, p, v);
    rec_post(top, (u32)r);
    return r;
}

static int rec_rect(void *self, const GpuPrim *p, s32 x, s32 y, s32 w_, s32 h, const GpuVtx *c, int flip_x, int flip_y)
{
    int top = rec_pre();
    if (top) {
        u32 w[GEDP_PRIM_WORDS + 6 + GEDP_VTX_WORDS], *e = gedp_prim_save(w, p);
        *e++ = (u32)x, *e++ = (u32)y, *e++ = (u32)w_, *e++ = (u32)h;
        e = gedp_vtx_save(e, c, 1);
        *e++ = (u32)flip_x, *e++ = (u32)flip_y;
        rec_head(GEDP_RECT, (u32)(e - w) + 1), rec_put(w, (u32)(e - w));
    }
    int r = rec.inner->rect(self, p, x, y, w_, h, c, flip_x, flip_y);
    rec_post(top, (u32)r);
    return r;
}

static int rec_line(void *self, const GpuPrim *p, const GpuVtx *a, const GpuVtx *b)
{
    int top = rec_pre();
    if (top) {
        u32 w[GEDP_PRIM_WORDS + 2 * GEDP_VTX_WORDS], *e = gedp_prim_save(w, p);
        e = gedp_vtx_save(e, a, 1), e = gedp_vtx_save(e, b, 1);
        rec_head(GEDP_LINE, (u32)(e - w) + 1), rec_put(w, (u32)(e - w));
    }
    int r = rec.inner->line(self, p, a, b);
    rec_post(top, (u32)r);
    return r;
}

static int rec_fill(void *self, s32 x, s32 y, s32 w_, s32 h, u16 color)
{
    int top = rec_pre();
    if (top) {
        u32 w[5] = {(u32)x, (u32)y, (u32)w_, (u32)h, color};
        rec_head(GEDP_FILL, 6), rec_put(w, 5);
    }
    int r = rec.inner->fill(self, x, y, w_, h, color);
    rec_post(top, (u32)r);
    return r;
}

static int rec_copy(void *self, s32 sx, s32 sy, s32 dx, s32 dy, s32 w_, s32 h, int mask_check, u16 mask_set)
{
    int top = rec_pre();
    if (top) {
        u32 w[8] = {(u32)sx, (u32)sy, (u32)dx, (u32)dy, (u32)w_, (u32)h, (u32)mask_check, mask_set};
        rec_head(GEDP_COPY, 9), rec_put(w, 8);
    }
    int r = rec.inner->copy(self, sx, sy, dx, dy, w_, h, mask_check, mask_set);
    rec_post(top, (u32)r);
    return r;
}

/* read and wrote return nothing: their ret word is 0 */
static void rec_read(void *self, s32 x, s32 y, s32 w_, s32 h)
{
    int top = rec_pre();
    if (top) {
        u32 w[4] = {(u32)x, (u32)y, (u32)w_, (u32)h};
        rec_head(GEDP_READ, 5), rec_put(w, 4);
    }
    rec.inner->read(self, x, y, w_, h);
    rec_post(top, 0);
}

static void rec_wrote(void *self, s32 x, s32 y, s32 w_, s32 h)
{
    int top = rec_pre();
    if (top) {
        u32 w[4] = {(u32)x, (u32)y, (u32)w_, (u32)h};
        rec_head(GEDP_WROTE, 5), rec_put(w, 4);
    }
    rec.inner->wrote(self, x, y, w_, h);
    rec_post(top, 0);
}

static int rec_poly(void *self, const u32 *c)
{
    int top = rec_pre();
    if (top) {
        u32 n = gedp_poly_words(c[0]);
        rec_head(GEDP_POLY, n + 1), rec_put(c, n);
    }
    int r = rec.inner->poly(self, c);
    rec_post(top, (u32)r);
    return r;
}

static u32 rec_packet(void *self, const u32 *w, u32 n)
{
    int top = rec_pre();
    if (top)
        rec_head(GEDP_PACKET, n + 1), rec_put(w, n);
    u32 r = rec.inner->packet(self, w, n);
    rec_post(top, r);
    return r;
}

/* PSX2PSP_SIM_GE_DUMP's value: 1 when it reads */
static int rec_parse(const char *s)
{
    char *e;
    rec.first = (u32)strtoul(s, &e, 10);
    if (e == s || *e != ',' || rec.first < 2)
        return 0;
    s = e + 1, rec.frames = (u32)strtoul(s, &e, 10);
    if (e == s || *e != ':' || !rec.frames || !e[1])
        return 0;
    rec.f = fopen(e + 1, "wb");
    return rec.f != NULL;
}

/* at the end of frame first - 1: the header, the state and VRAM as they are, and ge.c from cold */
static void rec_begin(Psx *psx)
{
    rec.seen = (u16 *)calloc(VRAM_W * VRAM_H, 2);
    if (!rec.seen)
        abort();
    u32 h[GEDP_HEAD] = {GEDP_MAGIC, GEDP_VERSION, rec.first, rec.frames, VRAM_W, VRAM_H, GEDP_GPU_WORDS, GEDP_PRIM_WORDS};
    rec_put(h, GEDP_HEAD);
    rec.on = 1;
    gedp_gpu_save(&psx->gpu, rec.st);
    rec_head(GEDP_STATE, GEDP_GPU_WORDS), rec_put(rec.st, GEDP_GPU_WORDS);
    rec_sync();
    rec_head(GEDP_INIT, 0);
    geh_start(psx, GE_SHADOW);
    rec.inner = psx->gpu.renderer;
    rec.r = (GpuRenderer){rec.inner->self, rec_triangle, rec_quad, rec_rect, rec_line, rec_fill, rec_copy, rec_read, rec_wrote,
                          rec.inner->poly ? rec_poly : NULL, rec.inner->packet ? rec_packet : NULL};
    psx->gpu.renderer = &rec.r;
    rec.r.wrote(rec.r.self, 0, 0, VRAM_W, VRAM_H); /* ge.c's VRAM was cleared: it takes the core's */
    psx_log(psx, "geh: dump of frames %u-%u begins", (unsigned)rec.first, (unsigned)(rec.first + rec.frames - 1));
}

static void rec_end(Psx *psx)
{
    rec_head(GEDP_END, 0);
    rec.on = 0;
    psx->gpu.renderer = rec.inner;
    fclose(rec.f), rec.f = NULL;
    free(rec.seen), rec.seen = NULL;
    psx_log(psx, "geh: dump ends, %u KB", (unsigned)(rec.words / 256));
}

/* ---- the sim's two calls ---- */

static int shadow;

/* After psx_init: ge.c as the renderer, in GE_SHADOW mode (once a process: ge.c is one). With
 * a dump, from the end of frame first - 1. */
void ge_shadow_init(Psx *psx)
{
    if (shadow++)
        return;
    const char *d = getenv("PSX2PSP_SIM_GE_DUMP");
    if (d) {
        if (!rec_parse(d)) {
            psx_log(psx, "geh: PSX2PSP_SIM_GE_DUMP is <first>,<frames>:<file> (first >= 2, a file it can write): %s", d);
            abort();
        }
        rec.psx = psx;
        return;
    }
    geh_start(psx, GE_SHADOW);
}

/* At the end of a frame: the frame's list hash to the log (and to a dump). */
void ge_shadow_frame(Psx *psx, u32 frame)
{
    if (!shadow)
        return;
    if (rec.psx && !rec.on && !rec.inner) { /* a dump that has not begun */
        if (frame + 1 == rec.first)
            rec_begin(psx);
        return;
    }
    GehFrame o;
    int top = rec.on && rec_pre();
    geh_frame(frame, &o);
    if (top) {
        u32 w[GEDP_FRAME_WORDS] = {o.frame,       o.lists, o.words, o.prims,           o.calls, o.misses,
                                   o.unknown,     (u32)(o.h >> 32), (u32)o.h, (u32)o.mem, (u32)(o.mh >> 32), (u32)o.mh};
        rec_head(GEDP_FRAME, GEDP_FRAME_WORDS + 1), rec_put(w, GEDP_FRAME_WORDS);
    }
    if (rec.on)
        rec_post(top, 0);
    geh_frame_log(psx, &o);
    if (rec.on && frame + 1 == rec.first + rec.frames)
        rec_end(psx);
}
