/*
 * psxrt: a hash of what a GE display list makes the GE do, step 1c of the pure translator
 * (me-translator design note): its words, through the calls and jumps it makes, and the bytes it
 * reads as vertices, indices and CLUTs. Two translators that hash the same drew the same.
 * Addresses go through the host's resolve, so the walk runs where the list's addresses are not
 * the CPU's own (the sim's are region and offset).
 */
#ifndef GEL_HASH_H
#define GEL_HASH_H

#include <stdint.h>

/* the bytes [addr, addr + bytes) as the GE addresses them; NULL when they are not all known */
typedef const void *(*GelResolve)(uint32_t addr, uint32_t bytes);

typedef struct {
    uint64_t h;
    uint32_t words, prims, calls, misses; /* misses: addresses resolve did not know */
} GelHash;

#define GEL_HASH_SEED 0xCBF29CE484222325ull

/* Walks the list at start to its END, folding it into g. */
void gel_hash(GelHash *g, const unsigned int *start, GelResolve resolve);
/* n bytes at p folded into h */
uint64_t gel_hash_bytes(uint64_t h, const void *p, uint32_t n);

#endif
