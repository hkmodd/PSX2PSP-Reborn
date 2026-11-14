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
    u32 pad[2];
} CodeEntry;

typedef struct DiscInfo {
    char disc_id[16];
    char title[128];
    u32 sectors; /* raw sectors of the image; sector 0 is LBA 150 */
    u32 blocks;
    /* translated code: `images` entries, run from code_base..+code_size; none for 0 */
    u32 images, code_base, code_size;
    CodeEntry *dir;
    u32 flags; /* DISC_PAL60 */
} DiscInfo;

/* DiscInfo.flags: a PAL picture runs at 60 vblanks a second (patch database `pal60`). */
#define DISC_PAL60 1u

/* Opens the EBOOT at `path` and checks its DATA.PSAR. NULL, or what is wrong. */
const char *disc_open(const char *path, DiscInfo *info);
/* PsxHost.read_sector: 0, or -1 outside the image or on a read error (reported to disc_log). */
int disc_read_sector(void *user, u32 lba, u8 *raw);
/* Reads `raw_len` bytes stored at `off` (`stored` bytes, raw deflate unless equal) into `dst`:
 * 0, or -1. */
int disc_blob(u32 off, u32 stored, void *dst, u32 raw_len);
/* Provided by the runtime: one line of diagnostics. */
void disc_log(const char *line);

#endif
