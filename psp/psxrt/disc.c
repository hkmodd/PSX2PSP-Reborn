/*
 * psxrt: the PS1 disc, read from DATA.PSAR of the runtime's own EBOOT. The converter stores the
 * image as blocks of 16 raw sectors, each raw deflate or stored, behind an index of offsets
 * (README.md, "DATA.PSAR"). Two inflated blocks are kept: a CD reads sequentially, and a seek
 * back to the block before (file headers, XA interleave) does not inflate again. After the
 * blocks come the translated executables (native.c), read with disc_blob().
 */
#include "disc.h"

#include <pspiofilemgr.h>
#include <pspsysmem.h>
#include <stdio.h>
#include <string.h>
#include <zlib.h>

#define MAGIC "PSXRTDAT"
#define VERSION 3
#define HEADER_LEN 0xC0
#define BLOCK_SECTORS 16
#define RAW 2352
#define BLOCK_LEN (BLOCK_SECTORS * RAW)
#define PREGAP 150

typedef struct {
    char magic[8];
    u32 version, header_len;
    char disc_id[16];
    char title[128];
    u32 sectors, block_sectors, index_offset, blocks;
    u32 dir_offset, images, code_base, code_size;
} Header;

typedef struct {
    int block; /* -1 empty */
    u32 used;  /* last use, for LRU */
    u8 data[BLOCK_LEN];
} Cached;

static SceUID fd = -1;
static u32 psar;   /* file offset of DATA.PSAR */
static u32 sectors;
static u32 *offsets; /* blocks + 1 offsets */
static Cached *cache;
static u8 *zbuf;
static u32 clock;
static z_stream zs;

static void *alloc(const char *name, u32 size)
{
    SceUID uid = sceKernelAllocPartitionMemory(2, name, PSP_SMEM_Low, size, NULL);
    return uid < 0 ? NULL : sceKernelGetBlockHeadAddr(uid);
}

static int read_at(u32 off, void *dst, u32 len)
{
    return sceIoLseek32(fd, (int)(psar + off), PSP_SEEK_SET) == (int)(psar + off) && sceIoRead(fd, dst, len) == (int)len;
}

const char *disc_open(const char *path, DiscInfo *info)
{
    u32 pbp[10];
    Header h;
    fd = sceIoOpen(path, PSP_O_RDONLY, 0);
    if (fd < 0)
        return "cannot open the EBOOT";
    if (sceIoRead(fd, pbp, sizeof pbp) != (int)sizeof pbp || pbp[0] != 0x50425000u)
        return "not a PBP";
    psar = pbp[9];
    if (!read_at(0, &h, sizeof h) || memcmp(h.magic, MAGIC, 8))
        return "DATA.PSAR is not psxrt's";
    if (h.version != VERSION || h.header_len != HEADER_LEN || h.block_sectors != BLOCK_SECTORS)
        return "DATA.PSAR version mismatch: rebuild the EBOOT";
    if (!h.sectors || h.blocks != (h.sectors + BLOCK_SECTORS - 1) / BLOCK_SECTORS)
        return "DATA.PSAR has no disc";
    offsets = alloc("psxrt index", (h.blocks + 1) * 4);
    cache = alloc("psxrt cache", 2 * sizeof(Cached));
    zbuf = alloc("psxrt zbuf", BLOCK_LEN);
    if (!offsets || !cache || !zbuf)
        return "out of memory";
    if (!read_at(h.index_offset, offsets, (h.blocks + 1) * 4))
        return "cannot read the disc index";
    if (inflateInit2(&zs, -15) != Z_OK)
        return "zlib init failed";
    cache[0].block = cache[1].block = -1;
    sectors = h.sectors;
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
    if (h.images) {
        u32 len = h.images * (u32)sizeof(CodeEntry);
        if (h.images > 256 || sizeof(CodeEntry) != 0x40)
            return "DATA.PSAR code directory is damaged";
        info->dir = alloc("psxrt code dir", len);
        if (!info->dir || !read_at(h.dir_offset, info->dir, len))
            return "cannot read the code directory";
        info->images = h.images;
    }
    return NULL;
}

int disc_blob(u32 off, u32 stored, void *dst, u32 raw_len)
{
    if (stored == raw_len)
        return read_at(off, dst, raw_len) ? 0 : -1;
    if (stored > raw_len || inflateReset(&zs) != Z_OK)
        return -1;
    zs.next_out = dst, zs.avail_out = raw_len;
    int r = Z_OK;
    for (u32 done = 0; done < stored;) {
        u32 n = stored - done < BLOCK_LEN ? stored - done : BLOCK_LEN;
        if (!read_at(off + done, zbuf, n))
            return -1;
        done += n;
        zs.next_in = zbuf, zs.avail_in = n;
        r = inflate(&zs, done == stored ? Z_FINISH : Z_NO_FLUSH);
        if (r != Z_OK && r != Z_STREAM_END)
            break;
    }
    return r == Z_STREAM_END && !zs.avail_out ? 0 : -1;
}

static void fail(u32 b, const char *what, int code)
{
    char line[80];
    snprintf(line, sizeof line, "disc block %u: %s (%d)", (unsigned)b, what, code);
    disc_log(line);
}

static int load(Cached *c, u32 b)
{
    u32 off = offsets[b], len = offsets[b + 1] - offsets[b];
    u32 n = sectors - b * BLOCK_SECTORS;
    u32 raw_len = (n < BLOCK_SECTORS ? n : BLOCK_SECTORS) * RAW;
    c->block = -1;
    if (offsets[b + 1] < off || len > raw_len) {
        fail(b, "bad index", 0);
        return -1;
    }
    if (len == raw_len) {
        if (!read_at(off, c->data, len))
            goto io;
    } else {
        if (!read_at(off, zbuf, len))
            goto io;
        inflateReset(&zs);
        zs.next_in = zbuf, zs.avail_in = len;
        zs.next_out = c->data, zs.avail_out = raw_len;
        int r = inflate(&zs, Z_FINISH);
        if (r != Z_STREAM_END || zs.avail_out) {
            fail(b, "inflate failed", r);
            return -1;
        }
    }
    c->block = (int)b;
    return 0;
io:
    fail(b, "read error", 0);
    return -1;
}

int disc_read_sector(void *user, u32 lba, u8 *raw)
{
    (void)user;
    if (lba < PREGAP || lba - PREGAP >= sectors)
        return -1;
    u32 s = lba - PREGAP, b = s / BLOCK_SECTORS;
    Cached *c = cache[0].block == (int)b ? &cache[0] : cache[1].block == (int)b ? &cache[1] : NULL;
    if (!c) {
        c = cache[0].used <= cache[1].used ? &cache[0] : &cache[1];
        if (load(c, b) < 0)
            return -1;
    }
    c->used = ++clock;
    memcpy(raw, c->data + (s % BLOCK_SECTORS) * RAW, RAW);
    return 0;
}
