/* psxrt: the runs of translated code that a rewrite of guest code cuts (coderun.h). */
#include "coderun.h"

#define RAM_WORDS (0x200000u / 4)

int cr_ends(u32 w)
{
    u32 op = w >> 26;
    return op == 2 || (op == 0 && (w & 0x3F) == 8) || (op == 4 && !(w >> 16 & 0x3FF)) || (op == 1 && (w >> 16 & 0x3FF) == 1);
}

s32 cr_target(u32 j, u32 w)
{
    u32 op = w >> 26;
    if (op == 2 || op == 3)
        return (s32)(w & (RAM_WORDS - 1));
    /* every REGIMM rt is a branch on the R3000A (psx-spx: bit 0 picks bgez, rt 0x10/0x11 link) */
    if (op == 1 || (op >= 4 && op <= 7) || (op >= 0x10 && op <= 0x13 && (w >> 21 & 0x1F) == 8))
        return (s32)((j + 1 + (u32)(s32)(s16)w) & (RAM_WORDS - 1));
    return -1;
}

u32 cr_spread(const u32 *ram, u32 lo, u8 *bad, int (*held)(void *ctx, u32 j), void *ctx)
{
    u32 base = lo - CR_WORDS, first = lo ? 0 : CR_WORDS, added = 0;
    for (int more = 1; more;) {
        more = 0;
        for (u32 k = 2 * CR_WORDS; k-- > first;) {
            u32 j = base + k;
            if (bad[k] || !held(ctx, j))
                continue;
            s32 t = cr_target(j, ram[j]);
            u32 tk = (u32)t - base;
            int fall = k + 1 < 2 * CR_WORDS && bad[k + 1] && !(j && held(ctx, j - 1) && cr_ends(ram[j - 1]));
            if (fall || (t >= 0 && tk < 2 * CR_WORDS && tk >= first && bad[tk]))
                bad[k] = 1, added++, more = 1;
        }
    }
    return added;
}
