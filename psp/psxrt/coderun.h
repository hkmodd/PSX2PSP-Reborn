/* psxrt: the runs of translated code that a rewrite of guest code cuts. When an I-cache flush finds
 * words of code changed, only those words, and the translated words that reach them without the
 * runtime, lose their translation; the rest of the block keeps it. Shared by the runtime
 * (native.c cache_flushed) and the PC simulator (crates/psx2psp-sim), so that the coverage the
 * converter measures is the runtime's. */
#ifndef PSXRT_CODERUN_H
#define PSXRT_CODERUN_H

#include "psx.h"

#define CR_WORDS 256u /* RAM words per block of the rewrite check (native.c BLK_SHIFT) */

/* An unconditional jump that does not link (j, jr, b as beq or bgez on $0): translated code does
 * not run past its delay slot. */
int cr_ends(u32 w);

/* The RAM word (of a 2 MB RAM) a direct branch, jump or call at RAM word `j` goes to, or -1. */
s32 cr_target(u32 j, u32 w);

/* The window is the block of RAM words [lo, lo + CR_WORDS) and the block before it (none when lo
 * is 0); `lo` is a multiple of CR_WORDS. bad[k] is RAM word lo - CR_WORDS + k. On entry it marks
 * the words whose translation no longer matches RAM; held(ctx, j) says whether RAM word j has a
 * translation that runs (a delay slot runs with its branch). Marks each held word of the window
 * whose translation reaches a marked word's without the runtime: by falling through (the delay
 * slot of an unconditional jump ends a run) or by a direct branch, jump or call into the window.
 * Returns the words it marked. Words of the next block are not seen: a caller going through
 * several blocks goes down, and starts each with what the one above marked in it. */
u32 cr_spread(const u32 *ram, u32 lo, u8 *bad, int (*held)(void *ctx, u32 j), void *ctx);

#endif
