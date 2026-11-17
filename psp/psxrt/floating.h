/* psxrt: floating code (engine native_code::floating_blob). Code the game runs wherever it loads
 * it (Crash 2's native GOOL pieces sit in the bytecode of entries it loads in any page; Spider-Man
 * links its CD.WAD modules where it loads them): units of words, each translated once at a fixed
 * guest and host address, recognised in RAM by their instructions and moved where RAM holds them.
 * Shared by the runtime (native.c) and the PC simulator (crates/psx2psp-sim), so that the coverage
 * the converter measures is the runtime's.
 *
 * RAM holds a unit where each word is the unit's, or differs only where a site of it takes the
 * word from RAM (fl_form): an immediate or shift amount its translation copies as it is, or the
 * target of a j/jal its translation holds in a lui/ori pair. A loader that links the code (jal
 * targets, lui/addiu halves of addresses) so rewrites only what the sites take.
 *
 * The blob, little-endian words:
 *   [0] units, [1] index entries, [2] guest address and [3] host address of the translations
 *   units x FlUnit: offsets (words, from the blob's start) and lengths of the unit's guest words,
 *     host code, table (one word per guest word: offset in the host code, or FL_NONE),
 *     relocations (host word offset << 2 | kind: 0 guest address, 1 guest address in the
 *     lui/ori pair from that word, 2 j/jal to the host code, 3 host address in the pair) and
 *     sites (2 words each, by guest word: guest word << 1 | kind, host word offset; kind 0 the
 *     host word is a copy of the guest word, 1 the lui/ori pair from it holds the guest word's
 *     j/jal target)
 *   index x 3 words, sorted by key: fl_key of the two words at an entry, unit, entry (word)
 *   the data */
#ifndef PSXRT_FLOATING_H
#define PSXRT_FLOATING_H

#include "psx.h"

#define FL_NONE 0xFFFFFFFFu

typedef struct FlUnit {
    u32 g_off, g_len, h_off, h_len, t_off, r_off, r_len, s_off, s_len;
} FlUnit;

typedef struct Floating {
    const u32 *w;
    u32 units, index;
} Floating;

/* Checks the blob of `words` words and opens it (kept, not copied): 0, or -1 when damaged. */
int fl_open(Floating *f, const u32 *blob, u32 words);

static inline const FlUnit *fl_unit(const Floating *f, u32 u) { return (const FlUnit *)(f->w + 4) + u; }

/* `w` without what a site takes from RAM: a j/jal's target, an immediate (addi as addiu) or a
 * shift amount. */
u32 fl_form(u32 w);

/* The index key of the two words at `p`: FNV-1a 32 of their fl_form. */
u32 fl_key(const u8 *p);

/* The longest unit with an entry at RAM offset `p` that RAM holds there: its number, and *at the
 * RAM offset of its first word; -1 when none. */
int fl_find(const Floating *f, const u8 *ram, u32 p, u32 *at);

/* Unit `u`'s host code moved to guest address `guest` (of its first word) and host address
 * `host`, with its sites taken from `ram` (the unit's words as RAM holds them; NULL: as the
 * unit's): written to `dst` (h_len words). */
void fl_place(const Floating *f, u32 u, u32 *dst, u32 guest, u32 host, const u32 *ram);

/* A unit placed for one RAM address: its code in a slot of the arena. */
typedef struct FlPlace {
    u32 at;    /* RAM offset of the unit's first word */
    u32 unit;
    u32 host;  /* the slot */
    u32 len;   /* bytes of the slot: the code's, rounded up to 64 */
    u32 guest; /* address of the unit's first word as the guest ran it */
    u32 sum;   /* FNV-1a 32 of RAM's words at the unit's sites when it was placed */
} FlPlace;

/* The slots of [lo, hi) (lo > 0): the placements, by host address. A placement stays while RAM
 * holds its words; one whose words are gone gives its slot back when the arena is full. A live one
 * is never moved for room: translated calls leave host return addresses in the guest's registers
 * and stack, and a frame in it would return into whatever took its slot. */
#define FL_PLACES 1024
typedef struct FlArena {
    u32 lo, hi, n;
    FlPlace p[FL_PLACES];
} FlArena;

/* Whether `ram` holds q's unit at q->at, with the words at its sites it was placed with. */
int fl_holds(const Floating *f, const u8 *ram, const FlPlace *q);

/* The placement over RAM offset `p` that translates the word there, was placed for guest
 * addresses `base` + offset and whose words RAM holds: its index, or -1. */
int fl_placed(const Floating *f, const FlArena *a, const u8 *ram, u32 p, u32 base);

/* A slot for unit `u` at RAM offset `at` and guest address `guest`: the index of the new
 * placement (its code is the caller's to write), or -1 when the arena has no room even after the
 * placements whose words are gone gave theirs back (each passed to `gone`, when not NULL, before
 * it goes). */
int fl_alloc(const Floating *f, FlArena *a, const u8 *ram, u32 u, u32 at, u32 guest, void (*gone)(const FlPlace *q));

#endif
