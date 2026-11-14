/*
 * psxrt core: the SPU's register file and sound RAM (transfers by FIFO and DMA). Voice playback,
 * ADSR and mixing arrive with S6; until then key-on/off only keep ENDX consistent.
 */
#include "psx.h"

#define R(off) psx->spu.regs[((off) - 0x1F801C00u) >> 1]

static void status(Spu *s)
{
    u32 mode = (s->ctrl >> 4) & 3;
    s->stat = (u16)((s->ctrl & 0x3F) | ((s->ctrl >> 5) & 1) << 7 | (mode == 2) << 8 | (mode == 3) << 9);
}

u16 spu_read(Psx *psx, u32 addr)
{
    Spu *s = &psx->spu;
    switch (addr) {
    case 0x1F801D9C: return (u16)s->endx;
    case 0x1F801D9E: return (u16)(s->endx >> 16);
    case 0x1F801DA6: return (u16)(s->xfer_addr >> 3);
    case 0x1F801DAA: return s->ctrl;
    case 0x1F801DAE: return s->stat;
    }
    if (addr < 0x1F801D80 && (addr & 0xF) == 0xC)
        return 0; /* current ADSR volume */
    return R(addr);
}

void spu_write(Psx *psx, u32 addr, u16 v)
{
    Spu *s = &psx->spu;
    R(addr) = v;
    switch (addr) {
    case 0x1F801D88: s->endx &= ~(u32)v; break;          /* KON: voices restart */
    case 0x1F801D8A: s->endx &= ~((u32)v << 16); break;
    case 0x1F801D8C: s->endx |= v; break;                 /* KOFF */
    case 0x1F801D8E: s->endx |= (u32)v << 16; break;
    case 0x1F801DA4: s->irq_addr = (u32)v << 3; break;
    case 0x1F801DA6: s->xfer_addr = (u32)v << 3; break;
    case 0x1F801DA8:
        s->ram[s->xfer_addr & 0x7FFFE] = (u8)v;
        s->ram[(s->xfer_addr & 0x7FFFE) + 1] = (u8)(v >> 8);
        s->xfer_addr = (s->xfer_addr + 2) & 0x7FFFF;
        break;
    case 0x1F801DAA:
        s->ctrl = v;
        status(s);
        break;
    }
}

void spu_dma_write(Psx *psx, u32 v)
{
    Spu *s = &psx->spu;
    for (int i = 0; i < 2; i++, v >>= 16) {
        s->ram[s->xfer_addr & 0x7FFFE] = (u8)v;
        s->ram[(s->xfer_addr & 0x7FFFE) + 1] = (u8)(v >> 8);
        s->xfer_addr = (s->xfer_addr + 2) & 0x7FFFF;
    }
}

u32 spu_dma_read(Psx *psx)
{
    Spu *s = &psx->spu;
    u32 v = 0;
    for (int i = 0; i < 2; i++) {
        u32 a = s->xfer_addr & 0x7FFFE;
        v |= (u32)(s->ram[a] | s->ram[a + 1] << 8) << (16 * i);
        s->xfer_addr = (s->xfer_addr + 2) & 0x7FFFF;
    }
    return v;
}

void spu_run(Psx *psx) { (void)psx; }
