/*
 * psxrt: GTE commands from translated code, through psxrt_gte_cmd (trampolines.S).
 *
 * This file is built with the caller-saved registers other than at, v0 and a0 declared
 * callee-saved (Makefile), so the trampoline saves only those, sp, gp, ra and HI/LO: each
 * function here saves what it uses itself. For that to hold, nothing here may call code built
 * without those flags; core/gte.c, included under other names, calls nothing outside itself.
 */
#define gte_command gte_command_cs
#define gte_read_data gte_read_data_cs
#define gte_write_data gte_write_data_cs
#define gte_read_ctrl gte_read_ctrl_cs
#define gte_write_ctrl gte_write_ctrl_cs
#include "core/gte.c"

#include "native.h"

Psx *gte_fast_psx;

/* The commands most games issue most, each in a function of its own: a function saves what any
 * of its paths uses, so the small ones must not share one with the rest. */
__attribute__((noinline)) static void op_nclip(Psx *psx)
{
    C[FLAG] = 0;
    nclip(psx);
    flag_sum(psx);
}

__attribute__((noinline)) static void op_avsz(Psx *psx, u32 cmd)
{
    C[FLAG] = 0;
    avsz(psx, cmd);
    flag_sum(psx);
}

__attribute__((noinline)) static void op_rtp(Psx *psx, u32 cmd)
{
    if (!rtp_fits(psx)) {
        gte_command_cs(psx, cmd);
        return;
    }
    C[FLAG] = 0;
    rtp_fast(psx, (cmd & 0x3F) == 0x30 ? 3 : 1, (cmd & (1u << 19)) ? 12 : 0, (cmd >> 10) & 1);
    flag_sum(psx);
}

void native_gte_fast(u32 cmd)
{
    Psx *psx = gte_fast_psx;
    psx->cycles += 10;
    native_stats.n_io[IO_GTEC]++;
    native_stats.n_gte[cmd & 0x3F]++;
    switch (cmd & 0x3F) {
    case 0x06: op_nclip(psx); return;
    case 0x2D: case 0x2E: op_avsz(psx, cmd); return;
    case 0x01: case 0x30: op_rtp(psx, cmd); return;
    default: gte_command_cs(psx, cmd); return;
    }
}
