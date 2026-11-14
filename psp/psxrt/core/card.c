/*
 * psxrt core: the memory card, at the BIOS level. A slot's image is 128 KB in Sony's layout, so
 * card files are interchangeable with other emulators' .mcr:
 *  - block 0: frame 0 "MC", frames 1-15 one directory entry per data block (state, size, next
 *    block, name, XOR checksum in byte 0x7F), frames 16-35 the broken-sector list, frame 63 a
 *    copy of frame 0;
 *  - blocks 1-15: 8 KB of file data each, a file being a chain of blocks.
 * The libcard primitives (_card_info, _card_load, _card_read, _card_write) and the "bu00:" /
 * "bu10:" files of the BIOS file API work on the image and complete at once: their completion
 * events are delivered before the call returns, where the hardware takes a few milliseconds.
 * A slot whose host image is NULL has no card inserted.
 */
#include "psx.h"

#include <string.h>

#define FRAME 128u
#define BLOCK 0x2000u
enum { ST_FIRST = 0x51, ST_MID = 0x52, ST_LAST = 0x53, ST_FREE = 0xA0 };
enum { HW_CARD = 0xF0000011u, SW_CARD = 0xF4000001u, SP_IOE = 0x0004, SP_TIMEOUT = 0x0100 };
enum { O_RD = 1, O_WR = 2, O_CREAT = 0x200, O_ASYNC = 0x8000 };
#define FD_FIRST 2 /* 0 and 1 are the console */
#define FD_COUNT 16

#define G (psx->cpu.gpr)
enum { R_V0 = 2, R_A0 = 4, R_A1, R_A2 };

static u8 *image(Psx *psx, u32 slot) { return slot < 2 ? psx->host->card[slot] : NULL; }
static u8 *dir(u8 *c, u32 block) { return c + block * FRAME; } /* block 1..15 */
static u32 next_block(const u8 *d) { u32 n = (u32)d[8] | (u32)d[9] << 8; return n < 15 ? n + 1 : 0; }
static u32 rd32(const u8 *p) { return (u32)p[0] | (u32)p[1] << 8 | (u32)p[2] << 16 | (u32)p[3] << 24; }
static void wr32(u8 *p, u32 v) { p[0] = (u8)v, p[1] = (u8)(v >> 8), p[2] = (u8)(v >> 16), p[3] = (u8)(v >> 24); }

static void checksum(u8 *f)
{
    u8 x = 0;
    for (u32 i = 0; i < FRAME - 1; i++)
        x ^= f[i];
    f[FRAME - 1] = x;
}

void card_format(u8 *c)
{
    memset(c, 0, CARD_SIZE);
    c[0] = 'M', c[1] = 'C';
    checksum(c);
    for (u32 i = 1; i < 16; i++) {
        u8 *d = dir(c, i);
        d[0] = ST_FREE, d[8] = d[9] = 0xFF;
        checksum(d);
    }
    for (u32 i = 16; i < 36; i++) {
        u8 *d = c + i * FRAME;
        memset(d, 0xFF, 4);
        d[8] = d[9] = 0xFF;
        checksum(d);
    }
    memcpy(c + 63 * FRAME, c, FRAME);
}

static void changed(Psx *psx, u32 slot)
{
    if (psx->host->card_dirty)
        psx->host->card_dirty(psx->host->user, (int)slot);
}

static void from_guest(Psx *psx, u8 *dst, u32 src, u32 n)
{
    for (u32 i = 0; i < n; i++)
        dst[i] = (u8)psx_read(psx, src + i, 1);
}

static void to_guest(Psx *psx, u32 dst, const u8 *src, u32 n)
{
    for (u32 i = 0; i < n; i++)
        psx_write(psx, dst + i, src[i], 1);
}

/* both completion events of a card operation */
static void done(Psx *psx, u8 *c)
{
    bios_event(psx, HW_CARD, c ? SP_IOE : SP_TIMEOUT);
    bios_event(psx, SW_CARD, c ? SP_IOE : SP_TIMEOUT);
}

/* ---- files ---- */

/* "bu00:NAME" (port 0) or "bu10:NAME" (port 1): the slot and the name; -1 for another device */
static int card_path(const char *path, const char **name)
{
    if (strncmp(path, "bu", 2) || (path[2] != '0' && path[2] != '1') || path[3] != '0' || path[4] != ':')
        return -1;
    *name = path + 5;
    return path[2] - '0';
}

static int match(const char *pat, const char *name)
{
    for (;; pat++, name++) {
        if (*pat == '*')
            return 1;
        if (!*pat || !*name)
            return *pat == *name;
        if (*pat != '?' && *pat != *name)
            return 0;
    }
}

static u32 find(u8 *c, const char *name)
{
    for (u32 b = 1; b < 16; b++)
        if (dir(c, b)[0] == ST_FIRST && !strncmp((const char *)dir(c, b) + 0xA, name, 21))
            return b;
    return 0;
}

/* a chain of `n` free (or deleted) blocks for `name`; its first block, 0 when there is no room */
static u32 create(u8 *c, const char *name, u32 n)
{
    u32 free_blocks[15], k = 0;
    for (u32 b = 1; b < 16 && k < n; b++)
        if ((dir(c, b)[0] & 0xF0) == ST_FREE)
            free_blocks[k++] = b;
    if (!n || k < n)
        return 0;
    for (u32 i = 0; i < n; i++) {
        u8 *d = dir(c, free_blocks[i]);
        memset(d, 0, FRAME);
        d[0] = n == 1 || i == n - 1 ? (i ? ST_LAST : ST_FIRST) : i ? ST_MID : ST_FIRST;
        if (!i) {
            wr32(d + 4, n * BLOCK);
            size_t len = strlen(name);
            memcpy(d + 0xA, name, len < 20 ? len : 20);
        }
        u32 nx = i + 1 < n ? free_blocks[i + 1] - 1 : 0xFFFF;
        d[8] = (u8)nx, d[9] = (u8)(nx >> 8);
        checksum(d);
    }
    return free_blocks[0];
}

/* the image byte at `pos` of the file starting at `first`, NULL past its chain */
static u8 *file_byte(u8 *c, u32 first, u32 pos)
{
    u32 b = first;
    for (u32 k = pos / BLOCK; k && b; k--)
        b = next_block(dir(c, b));
    return b ? c + b * BLOCK + pos % BLOCK : NULL;
}

static int f_open(Psx *psx, const char *path, u32 mode)
{
    const char *name = "";
    int slot = card_path(path, &name);
    if (slot < 0)
        return 0;
    u8 *c = image(psx, (u32)slot);
    s32 fd = -1;
    u32 first = c ? find(c, name) : 0;
    if (c && (mode & O_CREAT) && !first) {
        u32 n = mode >> 16;
        first = create(c, name, n ? n : 1);
        if (first)
            changed(psx, (u32)slot);
    } else if (mode & O_CREAT)
        first = 0; /* exists already */
    for (int i = FD_FIRST; first && i < FD_COUNT && fd < 0; i++)
        if (!psx->bios.fd[i].used) {
            psx->bios.fd[i].used = 1, psx->bios.fd[i].slot = slot, psx->bios.fd[i].pos = 0;
            psx->bios.fd[i].first_block = (int)first, psx->bios.fd[i].size = (int)rd32(dir(c, first) + 4);
            psx->bios.fd[i].async = (mode & O_ASYNC) != 0;
            fd = i;
        }
    psx_log(psx, "card open %s mode %X: fd %d", path, mode, (int)fd);
    G[R_V0] = (u32)fd;
    return 1;
}

static int f_io(Psx *psx, int op, u32 fd, u32 a1, u32 a2)
{
    if (fd < FD_FIRST || fd >= FD_COUNT || !psx->bios.fd[fd].used)
        return 0;
    struct BiosFile *f = &psx->bios.fd[fd];
    u8 *c = image(psx, (u32)f->slot);
    switch (op) {
    case 1: /* lseek(fd, offset, whence) */
        f->pos = (int)(a2 == 1 ? (u32)f->pos + a1 : a1);
        G[R_V0] = (u32)f->pos;
        return 1;
    case 2: case 3: { /* read / write (fd, buf, len) */
        if (!c) {
            G[R_V0] = 0xFFFFFFFFu;
            return 1;
        }
        u32 n = a2, left = f->pos < f->size ? (u32)(f->size - f->pos) : 0;
        n = n > left ? left : n;
        for (u32 done_n = 0; done_n < n;) {
            u32 pos = (u32)f->pos + done_n, run = BLOCK - pos % BLOCK;
            run = run > n - done_n ? n - done_n : run;
            u8 *p = file_byte(c, (u32)f->first_block, pos);
            if (!p)
                break;
            if (op == 2)
                to_guest(psx, a1 + done_n, p, run);
            else
                from_guest(psx, p, a1 + done_n, run);
            done_n += run;
        }
        f->pos += (int)n;
        if (op == 3 && n)
            changed(psx, (u32)f->slot);
        if (f->async) {
            done(psx, c);
            G[R_V0] = 0;
        } else
            G[R_V0] = n;
        return 1;
    }
    case 4: /* close */
        f->used = 0;
        G[R_V0] = fd;
        return 1;
    }
    return 0;
}

/* fills the DIRENTRY at `de` with the next directory entry matching the search; 0 at the end */
static u32 next_match(Psx *psx, u32 de)
{
    Bios *b = &psx->bios;
    u8 *c = image(psx, (u32)b->dirent_slot);
    while (c && b->dirent_pos < 16) {
        u8 *d = dir(c, b->dirent_pos);
        u32 blk = b->dirent_pos++;
        char name[21];
        memcpy(name, d + 0xA, 20);
        name[20] = 0;
        if (d[0] != ST_FIRST || !match(b->dirent_pattern, name))
            continue;
        u8 out[0x28] = {0};
        memcpy(out, name, 20);
        wr32(out + 0x14, 0x50);       /* attribute */
        wr32(out + 0x18, rd32(d + 4)); /* size */
        wr32(out + 0x20, blk);         /* first block */
        to_guest(psx, de, out, sizeof out);
        return de;
    }
    return 0;
}

/* ---- dispatch ---- */

int card_call(Psx *psx, u32 vector, u32 fn)
{
    u32 a0 = G[R_A0], a1 = G[R_A1], a2 = G[R_A2];
    int op = vector == 0xA0 && fn <= 4 ? (int)fn : vector == 0xB0 && fn >= 0x32 && fn <= 0x36 ? (int)fn - 0x32 : -1;
    if (op == 0) {
        char path[40] = {0};
        for (u32 i = 0; i < sizeof path - 1 && (path[i] = (char)psx_read(psx, a0 + i, 1)); i++)
            ;
        return f_open(psx, path, a1);
    }
    if (op > 0)
        return f_io(psx, op, a0, a1, a2);
    if (vector == 0xA0 && (fn == 0xAB || fn == 0xAC)) { /* _card_info / _card_load (chan) */
        done(psx, image(psx, a0 >> 4 & 0xF));
        G[R_V0] = 1;
        return 1;
    }
    if (vector != 0xB0)
        return 0;
    if (fn == 0x41 || fn == 0x45) /* format, erase: rare */
        psx_log(psx, "card B(%X) %X", fn, a0);
    switch (fn) {
    case 0x41: { /* format("bu00:") */
        char path[8] = {0};
        const char *name = "";
        for (u32 i = 0; i < 5; i++)
            path[i] = (char)psx_read(psx, a0 + i, 1);
        int slot = card_path(path, &name);
        u8 *c = slot < 0 ? NULL : image(psx, (u32)slot);
        if (c)
            card_format(c), changed(psx, (u32)slot);
        G[R_V0] = c != NULL;
        return 1;
    }
    case 0x42: { /* firstfile(pattern, direntry) */
        char path[40] = {0};
        const char *name = "";
        for (u32 i = 0; i < sizeof path - 1 && (path[i] = (char)psx_read(psx, a0 + i, 1)); i++)
            ;
        int slot = card_path(path, &name);
        psx->bios.dirent_slot = slot < 0 ? 2 : slot;
        psx->bios.dirent_pos = 1;
        size_t len = strlen(name), max = sizeof psx->bios.dirent_pattern - 1;
        memcpy(psx->bios.dirent_pattern, name, len < max ? len : max);
        psx->bios.dirent_pattern[len < max ? len : max] = 0;
        G[R_V0] = slot < 0 ? 0 : next_match(psx, a1);
        return 1;
    }
    case 0x43: /* nextfile(direntry) */
        G[R_V0] = next_match(psx, a0);
        return 1;
    case 0x45: { /* erase(name) */
        char path[40] = {0};
        const char *name = "";
        for (u32 i = 0; i < sizeof path - 1 && (path[i] = (char)psx_read(psx, a0 + i, 1)); i++)
            ;
        int slot = card_path(path, &name);
        u8 *c = slot < 0 ? NULL : image(psx, (u32)slot);
        u32 b = c ? find(c, name) : 0;
        G[R_V0] = b != 0;
        for (; b; b = next_block(dir(c, b))) {
            dir(c, b)[0] |= 0xF0; /* 0x51-0x53 -> 0xA1-0xA3: deleted */
            dir(c, b)[0] &= 0xA3;
            checksum(dir(c, b));
        }
        if (G[R_V0])
            changed(psx, (u32)slot);
        return 1;
    }
    case 0x4E: case 0x4F: { /* _card_write / _card_read (chan, sector, buf) */
        u8 *c = image(psx, a0 >> 4 & 0xF);
        if (c && a1 < CARD_SIZE / FRAME) {
            if (fn == 0x4F)
                to_guest(psx, a2, c + a1 * FRAME, FRAME);
            else
                from_guest(psx, c + a1 * FRAME, a2, FRAME), changed(psx, a0 >> 4 & 0xF);
        }
        bios_event(psx, HW_CARD, c ? SP_IOE : SP_TIMEOUT);
        G[R_V0] = 1;
        return 1;
    }
    case 0x50: /* _new_card */
        return 1;
    }
    return 0;
}
