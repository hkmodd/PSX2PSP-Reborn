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
#include <stdlib.h>
#include <string.h>

#include "ge.h"
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

/* ---- the sim's two calls ---- */

static int shadow;

/* After psx_init: ge.c as the renderer, in GE_SHADOW mode (once a process: ge.c is one). */
void ge_shadow_init(Psx *psx)
{
    if (shadow++)
        return;
    geh_vram = (u16 *)region_alloc(2 * 1024 * 1024);
    ge_init(psx, GE_SHADOW);
    fh = (GelHash){GEL_HASH_SEED, 0, 0, 0, 0}, lists = 0; /* ge_init's own lists */
}

/* At the end of a frame: the lists of the frame end as the PSP's present ends them, and their
 * hash goes to the log; every 60 frames, all the memory the lists address too. */
void ge_shadow_frame(Psx *psx, u32 frame)
{
    if (!shadow)
        return;
    ge_sync();
    psx_log(psx, "geh: frame %u, %u lists, %u words, %u prims, %u calls, %u misses, %u unknown: %08X%08X", (unsigned)frame,
            (unsigned)lists, (unsigned)fh.words, (unsigned)fh.prims, (unsigned)fh.calls, (unsigned)fh.misses, (unsigned)unknown,
            (unsigned)(fh.h >> 32), (unsigned)fh.h);
    fh = (GelHash){GEL_HASH_SEED, 0, 0, 0, 0}, lists = 0;
    if (frame % 60 == 0) {
        uint64_t h = GEL_HASH_SEED;
        for (int i = 1; i <= nreg; i++)
            h = gel_hash_bytes(h, (const void *)reg[i].p, reg[i].bytes);
        psx_log(psx, "geh: frame %u memory %08X%08X", (unsigned)frame, (unsigned)(h >> 32), (unsigned)h);
    }
    ge_touch();
}
