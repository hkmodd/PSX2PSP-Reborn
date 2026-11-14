/* psxrt: the PS1 disc, read from DATA.PSAR of the runtime's own EBOOT (format: README.md). */
#ifndef PSXRT_DISC_H
#define PSXRT_DISC_H

#include "psx.h"

typedef struct DiscInfo {
    char disc_id[16];
    char title[128];
    u32 sectors; /* raw sectors of the image; sector 0 is LBA 150 */
    u32 blocks;
} DiscInfo;

/* Opens the EBOOT at `path` and checks its DATA.PSAR. NULL, or what is wrong. */
const char *disc_open(const char *path, DiscInfo *info);
/* PsxHost.read_sector: 0, or -1 outside the image or on a read error (reported to disc_log). */
int disc_read_sector(void *user, u32 lba, u8 *raw);
/* Provided by the runtime: one line of diagnostics. */
void disc_log(const char *line);

#endif
