/*
 * psxrt: the PS1 disc, read from DATA.PSAR of the runtime's own EBOOT. The converter stores the
 * image as blocks of 16 raw sectors, each raw deflate or stored, behind an index of offsets
 * (README.md, "DATA.PSAR"). Three inflated blocks are kept: a CD reads sequentially, and a seek
 * back to the block before (file headers, XA interleave) does not inflate again. After a block
 * is served, a worker thread reads and inflates the next one into the third slot, with its own
 * file and zlib stream. Its priority is above the CPU thread's: a game that streams while the CPU
 * thread never sleeps (a level loading as it plays) would otherwise wait for every block, read and
 * inflate both. Above it, the worker's file read overlaps the frame (the thread blocks in the
 * driver) and only the inflate takes the CPU. If the CPU thread needs the block in flight it waits
 * for it. After the blocks come the translated executables (native.c), read with
 * disc_blob(). A game of more than one disc has each disc's blocks and index, listed in the disc
 * table; disc_select() changes the one in the drive.
 */
#include "disc.h"

#include <pspiofilemgr.h>
#include <pspsysmem.h>
#include <pspthreadman.h>
#include <stdio.h>
#include <string.h>
#include <zlib.h>

#define MAGIC "PSXRTDAT"
#define VERSION 7
#define HEADER_LEN 0xE4
#define BLOCK_SECTORS 16
#define RAW 2352
#define BLOCK_LEN (BLOCK_SECTORS * RAW)
#define PREGAP 150
#define SLOTS 3
/* above the CPU thread (0x20), below audio (0x12) and callbacks (0x11) */
#define PREFETCH_PRIORITY 0x1F

typedef struct {
    char magic[8];
    u32 version, header_len;
    char disc_id[16];
    char title[128];
    u32 sectors, block_sectors, index_offset, blocks;
    u32 dir_offset, images, code_base, code_size;
    u32 flags;
    u32 cpu_shift;
    u32 disc_table, discs; /* 0, 0 for one disc; else discs entries of 0x20 B (ID, sectors, index offset, blocks, 0) */
    u32 fl_offset, fl_stored, fl_len, fl_host, fl_arena; /* floating code, 0 for none */
} Header;

/* The disc table of a game of more than one disc (README.md, "DATA.PSAR"); entry 0 is disc 1. */
typedef struct {
    char id[16];
    u32 sectors, index_offset, blocks, zero;
} DiscEntry;

typedef struct {
    int block; /* -1 empty */
    u32 used;  /* last use, for LRU */
    u8 data[BLOCK_LEN];
} Cached;

/* A file and a zlib stream: one for the CPU thread, one for the worker. */
typedef struct {
    SceUID fd;
    u8 *zbuf;
    z_stream zs;
    int quiet; /* the worker does not log: a block it fails is loaded again, and reported, inline */
    int who;   /* its DiscStats index: 0 CPU thread, 1 worker */
} Reader;

DiscStats disc_stats;
static Reader rd = {.fd = -1}, pf_rd = {.fd = -1, .quiet = 1, .who = 1};
static u32 psar; /* file offset of DATA.PSAR */
static u32 sectors, blocks;
static u32 *offsets; /* blocks + 1 offsets of the disc in the drive */
static DiscEntry table[DISC_MAX];
static u32 ndiscs, cur;
static Cached *cache;
static u32 clock;
/* The worker's request. `pf_slot` is the worker's while `pf_busy`; `pf_done` is signalled when
 * block `pf_block` is in it (its `block` stays -1 on a failure). Only the CPU thread sets these. */
static SceUID pf_req = -1, pf_done = -1;
static Cached *pf_slot;
static u32 pf_block, pf_busy;

static void *alloc(const char *name, u32 size)
{
    SceUID uid = sceKernelAllocPartitionMemory(2, name, PSP_SMEM_Low, size, NULL);
    return uid < 0 ? NULL : sceKernelGetBlockHeadAddr(uid);
}

static int read_at(Reader *r, u32 off, void *dst, u32 len)
{
    return sceIoLseek32(r->fd, (int)(psar + off), PSP_SEEK_SET) == (int)(psar + off) && sceIoRead(r->fd, dst, len) == (int)len;
}

static const char *reader_open(Reader *r, const char *path)
{
    r->fd = sceIoOpen(path, PSP_O_RDONLY, 0);
    if (r->fd < 0)
        return "cannot open the EBOOT";
    r->zbuf = alloc("psxrt zbuf", BLOCK_LEN);
    if (!r->zbuf)
        return "out of memory";
    return inflateInit2(&r->zs, -15) != Z_OK ? "zlib init failed" : NULL;
}

static int load(Reader *r, Cached *c, u32 b);

static int prefetch_thread(SceSize args, void *argp)
{
    (void)args, (void)argp;
    while (sceKernelWaitSema(pf_req, 1, NULL) >= 0) {
        load(&pf_rd, pf_slot, pf_block);
        sceKernelSignalSema(pf_done, 1);
    }
    return 0;
}

const char *disc_open(const char *path, DiscInfo *info)
{
    u32 pbp[10];
    Header h;
    const char *err = reader_open(&rd, path);
    if (err)
        return err;
    if (sceIoRead(rd.fd, pbp, sizeof pbp) != (int)sizeof pbp || pbp[0] != 0x50425000u)
        return "not a PBP";
    psar = pbp[9];
    if (!read_at(&rd, 0, &h, sizeof h) || memcmp(h.magic, MAGIC, 8))
        return "DATA.PSAR is not psxrt's";
    if (h.version != VERSION || h.header_len != HEADER_LEN || h.block_sectors != BLOCK_SECTORS)
        return "DATA.PSAR version mismatch: rebuild the EBOOT";
    if (!h.sectors || h.blocks != (h.sectors + BLOCK_SECTORS - 1) / BLOCK_SECTORS)
        return "DATA.PSAR has no disc";
    memcpy(table[0].id, h.disc_id, sizeof table[0].id);
    table[0].sectors = h.sectors, table[0].index_offset = h.index_offset, table[0].blocks = h.blocks;
    ndiscs = 1, cur = 0;
    if (h.discs) {
        if (h.discs < 2 || h.discs > DISC_MAX || !read_at(&rd, h.disc_table, table, h.discs * (u32)sizeof *table) ||
            table[0].sectors != h.sectors || table[0].index_offset != h.index_offset)
            return "DATA.PSAR disc table is damaged";
        ndiscs = h.discs;
    }
    u32 most = 0;
    for (u32 k = 0; k < ndiscs; k++) {
        DiscEntry *e = &table[k];
        e->id[sizeof e->id - 1] = 0;
        if (!e->sectors || e->blocks != (e->sectors + BLOCK_SECTORS - 1) / BLOCK_SECTORS)
            return "DATA.PSAR disc table is damaged";
        most = e->blocks > most ? e->blocks : most;
    }
    offsets = alloc("psxrt index", (most + 1) * 4);
    cache = alloc("psxrt cache", SLOTS * sizeof(Cached));
    if (!offsets || !cache)
        return "out of memory";
    if (!read_at(&rd, h.index_offset, offsets, (h.blocks + 1) * 4))
        return "cannot read the disc index";
    for (int k = 0; k < SLOTS; k++)
        cache[k].block = -1, cache[k].used = 0;
    sectors = h.sectors, blocks = h.blocks;
    /* without the worker the disc still reads, inline */
    if (!reader_open(&pf_rd, path)) {
        pf_req = sceKernelCreateSema("psxrt pf req", 0, 0, 1, NULL);
        pf_done = sceKernelCreateSema("psxrt pf done", 0, 0, 1, NULL);
        SceUID th = sceKernelCreateThread("psxrt disc", prefetch_thread, PREFETCH_PRIORITY, 0x2000, PSP_THREAD_ATTR_USER, NULL);
        if (pf_req < 0 || pf_done < 0 || th < 0 || sceKernelStartThread(th, 0, NULL) < 0)
            pf_req = -1;
    }
    memcpy(info->disc_id, h.disc_id, sizeof info->disc_id);
    memcpy(info->title, h.title, sizeof info->title);
    info->disc_id[sizeof info->disc_id - 1] = 0;
    info->title[sizeof info->title - 1] = 0;
    info->sectors = h.sectors;
    info->blocks = h.blocks;
    info->images = 0;
    info->dir = NULL;
    info->code_base = h.code_base;
    info->code_size = h.code_size;
    info->flags = h.flags;
    info->cpu_shift = h.cpu_shift;
    info->discs = ndiscs;
    info->fl_offset = h.fl_offset, info->fl_stored = h.fl_stored, info->fl_len = h.fl_len;
    info->fl_host = h.fl_host, info->fl_arena = h.fl_arena;
    if (h.fl_len && (h.fl_len & 3 || h.fl_stored > h.fl_len || h.fl_host & 3))
        return "DATA.PSAR floating code is damaged";
    if (h.images) {
        u32 len = h.images * (u32)sizeof(CodeEntry);
        if (h.images > 256 || sizeof(CodeEntry) != 0x40)
            return "DATA.PSAR code directory is damaged";
        info->dir = alloc("psxrt code dir", len);
        if (!info->dir || !read_at(&rd, h.dir_offset, info->dir, len))
            return "cannot read the code directory";
        info->images = h.images;
    }
    return NULL;
}

const char *disc_name(u32 n) { return n < ndiscs ? table[n].id : ""; }

u32 disc_select(u32 n)
{
    if (n >= ndiscs)
        return 0;
    if (n == cur)
        return sectors;
    if (pf_busy) { /* the worker reads with the index about to change: let it finish */
        SceUInt t = 2000000;
        if (sceKernelWaitSema(pf_done, 1, &t) < 0) {
            disc_log("disc: read-ahead still busy, the disc stays");
            return 0;
        }
        pf_busy = 0;
    }
    const DiscEntry *e = &table[n];
    if (!read_at(&rd, e->index_offset, offsets, (e->blocks + 1) * 4)) {
        disc_log("disc: cannot read the new disc's index, the disc stays");
        if (!read_at(&rd, table[cur].index_offset, offsets, (table[cur].blocks + 1) * 4))
            pf_req = -1, sectors = 0; /* neither index: every read fails, as with no disc */
        return 0;
    }
    for (int k = 0; k < SLOTS; k++)
        cache[k].block = -1;
    sectors = e->sectors, blocks = e->blocks, cur = n;
    return sectors;
}

int disc_blob(u32 off, u32 stored, void *dst, u32 raw_len)
{
    if (stored == raw_len)
        return read_at(&rd, off, dst, raw_len) ? 0 : -1;
    if (stored > raw_len || inflateReset(&rd.zs) != Z_OK)
        return -1;
    rd.zs.next_out = dst, rd.zs.avail_out = raw_len;
    int r = Z_OK;
    for (u32 done = 0; done < stored;) {
        u32 n = stored - done < BLOCK_LEN ? stored - done : BLOCK_LEN;
        if (!read_at(&rd, off + done, rd.zbuf, n))
            return -1;
        done += n;
        rd.zs.next_in = rd.zbuf, rd.zs.avail_in = n;
        r = inflate(&rd.zs, done == stored ? Z_FINISH : Z_NO_FLUSH);
        if (r != Z_OK && r != Z_STREAM_END)
            break;
    }
    return r == Z_STREAM_END && !rd.zs.avail_out ? 0 : -1;
}

static void fail(Reader *r, u32 b, const char *what, int code)
{
    char line[80];
    if (r->quiet)
        return;
    snprintf(line, sizeof line, "disc block %u: %s (%d)", (unsigned)b, what, code);
    disc_log(line);
}

static int load(Reader *r, Cached *c, u32 b)
{
    u32 off = offsets[b], len = offsets[b + 1] - offsets[b];
    u32 n = sectors - b * BLOCK_SECTORS;
    u32 raw_len = (n < BLOCK_SECTORS ? n : BLOCK_SECTORS) * RAW;
    u32 t0 = sceKernelGetSystemTimeLow(), t1;
    c->block = -1;
    if (offsets[b + 1] < off || len > raw_len) {
        fail(r, b, "bad index", 0);
        return -1;
    }
    if (len == raw_len) {
        if (!read_at(r, off, c->data, len))
            goto io;
        t1 = sceKernelGetSystemTimeLow();
    } else {
        if (!read_at(r, off, r->zbuf, len))
            goto io;
        t1 = sceKernelGetSystemTimeLow();
        inflateReset(&r->zs);
        r->zs.next_in = r->zbuf, r->zs.avail_in = len;
        r->zs.next_out = c->data, r->zs.avail_out = raw_len;
        int z = inflate(&r->zs, Z_FINISH);
        if (z != Z_STREAM_END || r->zs.avail_out) {
            fail(r, b, "inflate failed", z);
            return -1;
        }
    }
    c->block = (int)b;
    disc_stats.us_read[r->who] += t1 - t0;
    disc_stats.us_inflate[r->who] += sceKernelGetSystemTimeLow() - t1;
    return 0;
io:
    fail(r, b, "read error", 0);
    return -1;
}

/* The slot holding block `b` (the worker's only once it is done), or NULL. */
static Cached *find(int b)
{
    for (int k = 0; k < SLOTS; k++)
        if (cache[k].block == b && !(pf_busy && &cache[k] == pf_slot))
            return &cache[k];
    return NULL;
}

/* The least recently used slot that is neither `keep` nor the worker's. */
static Cached *victim(const Cached *keep)
{
    Cached *v = NULL;
    for (int k = 0; k < SLOTS; k++) {
        Cached *c = &cache[k];
        if (c != keep && !(pf_busy && c == pf_slot) && (!v || c->used < v->used))
            v = c;
    }
    return v;
}

int disc_read_sector(void *user, u32 lba, u8 *raw)
{
    (void)user;
    if (lba < PREGAP || lba - PREGAP >= sectors)
        return -1;
    u32 s = lba - PREGAP, b = s / BLOCK_SECTORS;
    if (pf_busy) { /* collected if done; the block in flight is waited for */
        int done = sceKernelPollSema(pf_done, 1) >= 0;
        if (!done && pf_block == b) {
            SceUInt t = 200000; /* a block takes a few ms: the worker never holds the CPU thread */
            int r = sceKernelWaitSema(pf_done, 1, &t);
            done = r >= 0, disc_stats.waited += done;
            if (!done) { /* its slot stays out of use; the disc reads inline from here */
                char line[96];
                snprintf(line, sizeof line, "disc: read-ahead of block %u not done (%08X): off", (unsigned)b, (unsigned)r);
                disc_log(line);
                pf_req = -1;
            }
        }
        if (done)
            pf_busy = 0, disc_stats.prefetched += pf_slot->block >= 0;
    }
    Cached *c = find((int)b);
    if (!c) {
        c = victim(NULL);
        disc_stats.loads++;
        if (load(&rd, c, b) < 0)
            return -1;
    }
    c->used = ++clock;
    memcpy(raw, c->data + (s % BLOCK_SECTORS) * RAW, RAW);
    if (pf_req >= 0 && !pf_busy && b + 1 < blocks && !find((int)b + 1)) {
        /* as recent as `c`: a miss before b + 1 is asked for does not evict it */
        pf_slot = victim(c), pf_slot->block = -1, pf_slot->used = clock, pf_block = b + 1, pf_busy = 1;
        sceKernelSignalSema(pf_req, 1);
    }
    return 0;
}
