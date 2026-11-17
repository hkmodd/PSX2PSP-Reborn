/* psxrt: floating code (engine native_code::floating_blob). Code the game runs wherever it loads
 * it (Crash 2's native GOOL pieces sit in the bytecode of entries it loads in any page): units of
 * words, each translated once at a fixed guest and host address, recognised in RAM by their
 * words and moved where RAM holds them. Shared by the runtime (native.c) and the PC simulator
 * (crates/psx2psp-sim), so that the coverage the converter measures is the runtime's.
 *
 * The blob, little-endian words:
 *   [0] units, [1] index entries, [2] guest address and [3] host address of the translations
 *   units x FlUnit: offsets (words, from the blob's start) and lengths of the unit's guest words,
 *     host code, table (one word per guest word: offset in the host code, or FL_NONE) and
 *     relocations (host word offset << 2 | kind: 0 guest address, 1 guest address in the
 *     lui/ori pair from that word, 2 j/jal to the host code, 3 host address in the pair)
 *   index x 3 words, sorted by key: fl_key of the two words at an entry, unit, entry (word)
 *   the data */
#ifndef PSXRT_FLOATING_H
#define PSXRT_FLOATING_H

#include "psx.h"

#define FL_NONE 0xFFFFFFFFu

typedef struct FlUnit {
    u32 g_off, g_len, h_off, h_len, t_off, r_off, r_len;
} FlUnit;

typedef struct Floating {
    const u32 *w;
    u32 units, index;
} Floating;

/* Checks the blob of `words` words and opens it (kept, not copied): 0, or -1 when damaged. */
int fl_open(Floating *f, const u32 *blob, u32 words);

static inline const FlUnit *fl_unit(const Floating *f, u32 u) { return (const FlUnit *)(f->w + 4) + u; }

/* The index key of the 8 bytes at `p`: FNV-1a 32. */
u32 fl_key(const u8 *p);

/* The longest unit with an entry at RAM offset `p` whose words RAM holds there: its number, and
 * *at the RAM offset of its first word; -1 when none. */
int fl_find(const Floating *f, const u8 *ram, u32 p, u32 *at);

/* Unit `u`'s host code moved to guest address `guest` (of its first word) and host address
 * `host`: written to `dst` (h_len words). */
void fl_place(const Floating *f, u32 u, u32 *dst, u32 guest, u32 host);

#endif
