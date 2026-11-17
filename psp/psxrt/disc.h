/* psxrt: the PS1 disc, read from DATA.PSAR of the runtime's own EBOOT (format: README.md). */
#ifndef PSXRT_DISC_H
#define PSXRT_DISC_H

#include "psx.h"

/* Entry of the code directory (README.md, "DATA.PSAR"): one translated executable. Offsets are
 * from the start of DATA.PSAR; a stored length equal to the raw one means stored as is. */
typedef struct CodeEntry {
    u32 text_addr, text_size, hash, host_base;
    u32 code_len, code_off, code_stored;
    u32 table_off, table_stored; /* the raw table is one word per text word */
    char name[16];
    u32 head_hash; /* FNV-1a of the first HEAD_LEN bytes of the text */
    u32 patch_off, patch_groups; /* the patch table (native.c patch_group), 0 for none */
} CodeEntry;

typedef struct DiscInfo {
    char disc_id[16];
    char title[128];
    u32 sectors; /* raw sectors of the image; sector 0 is LBA 150 */
    u32 blocks;
    /* translated code: `images` entries, run from code_base..+code_size; none for 0 */
    u32 images, code_base, code_size;
    CodeEntry *dir;
    u32 flags; /* DISC_PAL60, DISC_MANUAL_DISC */
    /* translated loops charge their PS1 cycles >> cpu_shift (patch database `cpu_shift`, as
     * POPS config word 19): the game gets more CPU per frame */
    u32 cpu_shift;
    u32 discs; /* discs of the game, 1..DISC_MAX; disc_id and title are disc 1's */
    /* floating code (floating.h): the head at fl_offset, fl_stored bytes (deflate, or as is when
     * fl_len), fl_len raw bytes loaded at fl_host; the units' bodies from fl_bodies, read into the
     * fl_arena bytes from code_base where they are placed. fl_len 0 for none. */
    u32 fl_offset, fl_stored, fl_len, fl_host, fl_arena;
    u32 fl_bodies;
} DiscInfo;

#define DISC_MAX 5

/* DiscInfo.flags: a PAL picture runs at 60 vblanks a second (patch database `pal60`). */
#define DISC_PAL60 1u
/* DiscInfo.flags: the player changes the disc, not the core (patch database `disc_swap = manual`). */
#define DISC_MANUAL_DISC 2u

/* Disc blocks since boot: inflated by the CPU thread, inflated ahead by the worker (and how many
 * of those the CPU thread had to wait for); microseconds reading the file and inflating, by the
 * CPU thread [0] and the worker [1]. */
typedef struct DiscStats {
    u32 loads, prefetched, waited;
    u32 us_read[2], us_inflate[2];
} DiscStats;
extern DiscStats disc_stats;

/* Opens the EBOOT at `path` and checks its DATA.PSAR, and starts the read-ahead worker. NULL, or
 * what is wrong. */
const char *disc_open(const char *path, DiscInfo *info);
/* Puts disc `n` (0-based) in the drive: its index replaces the current one and the cache empties.
 * Its sector count, or 0 when `n` is not a disc of the game or its index cannot be read (the
 * drive keeps the disc it had). CPU thread only, with the shell open (cd_lid). */
u32 disc_select(u32 n);
/* The ID of disc `n` (0-based), "" past the last. */
const char *disc_name(u32 n);
/* PsxHost.read_sector: 0, or -1 outside the image or on a read error (reported to disc_log). */
int disc_read_sector(void *user, u32 lba, u8 *raw);
/* Reads `raw_len` bytes stored at `off` (`stored` bytes, raw deflate unless equal) into `dst`:
 * 0, or -1. */
int disc_blob(u32 off, u32 stored, void *dst, u32 raw_len);
/* Provided by the runtime: one line of diagnostics. */
void disc_log(const char *line);

#endif
