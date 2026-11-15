/* PsyQ libpress `DecDCTvlc` (hooks.rs DEC_DCT_VLC), run natively: the same output, word for word,
 * the same saved state and the same instruction count as the guest routine. */
#ifndef VLC_H
#define VLC_H

#include <stdint.h>

#define VLC_RAM_MASK 0x1FFFFFu

/* The routine's globals, as guest addresses taken from its own code. */
typedef struct {
    uint32_t size; /* the output limit, DecDCTvlcSize's: words * 2, or 0xFFFFFF */
    uint32_t save; /* 9 words: a0 a1 v0 v1 t4 t5 t7 t8 t9 when it stops at the limit */
    uint32_t end;  /* where the output of this picture ends */
    uint32_t tab1; /* 8192 pairs of words by the next 13 bits; the DC tables 2 KB and 1 KB below */
    uint32_t tab2; /* 512 words by the next 9 bits, for the codes longer than 13 bits */
} VlcVars;

/* The routine's registers between slices, and the guest instructions and memory accesses it has
 * made so far (the translation charges a word 1 cycle, a memory access 2 more). */
typedef struct {
    uint32_t a0, a1, v0, v1, t4, t5, t6, t7, t8, t9;
    int at_dc; /* 1: at a block's DC code; 0: in its AC codes */
    uint32_t ins, mem;
} Vlc;

/* The call's entry, `bs` its a0 (0: go on from the saved state) and `buf` its a1. */
void vlc_start(uint8_t *ram, const VlcVars *g, Vlc *r, uint32_t bs, uint32_t buf);

/* Runs until the routine returns (its v0: 0 at the picture's end, 1 at the limit), or -1 once
 * `budget` cycles more are spent, at the start of a block. */
int vlc_run(uint8_t *ram, const VlcVars *g, Vlc *r, uint32_t budget);

#endif
