/* psxrt core: the PS1 bus. RAM and scratchpad are plain memory; everything else is a device. */
#include "psx.h"

static u32 io_read(Psx *psx, u32 p, int size)
{
    if (p >= 0x1F801040 && p < 0x1F801060)
        return sio_read(psx, p, size);
    if (p >= 0x1F801080 && p < 0x1F801100)
        return dma_read(psx, p & ~3u) >> (8 * (p & 3));
    if (p >= 0x1F801100 && p < 0x1F801130)
        return timer_read(psx, p & ~3u);
    if (p >= 0x1F801800 && p < 0x1F801804) {
        u32 v = cd_read(psx, p & 3);
        if (size > 1) /* 16/32-bit reads of the CD registers repeat the byte register */
            v |= (u32)cd_read(psx, p & 3) << 8;
        if (size > 2)
            v |= (u32)cd_read(psx, p & 3) << 16 | (u32)cd_read(psx, p & 3) << 24;
        return v;
    }
    if (p >= 0x1F801810 && p < 0x1F801818)
        return gpu_read(psx, (p >> 2) & 1);
    if (p >= 0x1F801820 && p < 0x1F801828)
        return mdec_read(psx, (p >> 2) & 1);
    if (p >= 0x1F801C00 && p < 0x1F802000) {
        u32 v = spu_read(psx, p & ~1u);
        if (size == 4)
            v |= (u32)spu_read(psx, (p & ~1u) + 2) << 16;
        return v;
    }
    switch (p & ~3u) {
    case 0x1F801070:
        return psx->i_stat >> (8 * (p & 3));
    case 0x1F801074:
        return psx->i_mask >> (8 * (p & 3));
    case 0x1F801060:
        return psx->io_ram_size;
    }
    if (p >= 0x1F801000 && p < 0x1F801024)
        return psx->exp_regs[(p - 0x1F801000) >> 2];
    return 0; /* expansion regions, BIOS ROM (HLE: empty) */
}

static void io_write(Psx *psx, u32 p, u32 v, int size)
{
    if (p >= 0x1F801040 && p < 0x1F801060) {
        sio_write(psx, p, v, size);
        return;
    }
    if (p >= 0x1F801080 && p < 0x1F801100) {
        if (size == 4)
            dma_write(psx, p, v);
        else { /* narrow writes: merge into the register */
            u32 a = p & ~3u, sh = 8 * (p & 3), m = (size == 1 ? 0xFFu : 0xFFFFu) << sh;
            dma_write(psx, a, (dma_read(psx, a) & ~m) | ((v << sh) & m));
        }
        return;
    }
    if (p >= 0x1F801100 && p < 0x1F801130) {
        timer_write(psx, p & ~3u, v);
        return;
    }
    if (p >= 0x1F801800 && p < 0x1F801804) {
        cd_write(psx, p & 3, (u8)v);
        return;
    }
    if (p >= 0x1F801810 && p < 0x1F801818) {
        if (p & 4)
            gpu_gp1(psx, v);
        else
            gpu_gp0(psx, v);
        return;
    }
    if (p >= 0x1F801820 && p < 0x1F801828) {
        mdec_write(psx, (p >> 2) & 1, v);
        return;
    }
    if (p >= 0x1F801C00 && p < 0x1F802000) {
        spu_write(psx, p & ~1u, (u16)v);
        if (size == 4)
            spu_write(psx, (p & ~1u) + 2, (u16)(v >> 16));
        return;
    }
    switch (p & ~3u) {
    case 0x1F801070: /* I_STAT: writing 0 acknowledges */
        psx->i_stat &= v | ~0x7FFu;
        psx->i_stat &= 0x7FF;
        return;
    case 0x1F801074:
        psx->i_mask = v & 0x7FF;
        return;
    case 0x1F801060:
        psx->io_ram_size = v;
        return;
    }
    if (p >= 0x1F801000 && p < 0x1F801024)
        psx->exp_regs[(p - 0x1F801000) >> 2] = v;
}

u32 psx_read(Psx *psx, u32 addr, int size)
{
    if (addr == 0xFFFE0130)
        return psx->io_cache_ctrl;
    u32 p = addr & 0x1FFFFFFF;
    if (p < 0x800000) {
        const u8 *m = psx->ram + (p & (RAM_SIZE - 1));
        return size == 1 ? m[0] : size == 2 ? (u32)(m[0] | m[1] << 8) : m[0] | m[1] << 8 | m[2] << 16 | (u32)m[3] << 24;
    }
    if (p >= 0x1F800000 && p < 0x1F800400) {
        const u8 *m = psx->scratch + (p & 0x3FF);
        return size == 1 ? m[0] : size == 2 ? (u32)(m[0] | m[1] << 8) : m[0] | m[1] << 8 | m[2] << 16 | (u32)m[3] << 24;
    }
    u32 v = io_read(psx, p, size);
    return size == 1 ? v & 0xFF : size == 2 ? v & 0xFFFF : v;
}

void psx_write(Psx *psx, u32 addr, u32 v, int size)
{
    if (addr == 0xFFFE0130) {
        psx->io_cache_ctrl = v;
        return;
    }
    /* SR bit 16 (isolate cache): writes go to the I-cache, not memory (BIOS cache flush). */
    if (psx->cpu.sr & 0x10000)
        return;
    u32 p = addr & 0x1FFFFFFF;
    u8 *m = 0;
    if (p < 0x800000)
        m = psx->ram + (p & (RAM_SIZE - 1));
    else if (p >= 0x1F800000 && p < 0x1F800400)
        m = psx->scratch + (p & 0x3FF);
    if (m) {
        m[0] = (u8)v;
        if (size > 1)
            m[1] = (u8)(v >> 8);
        if (size > 2)
            m[2] = (u8)(v >> 16), m[3] = (u8)(v >> 24);
        return;
    }
    io_write(psx, p, v, size);
}
