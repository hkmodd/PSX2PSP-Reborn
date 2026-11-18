/*
 * psxrt core: DMA. A transfer moves its data when it starts; the channel stays busy, and its
 * interrupt comes, one cycle per word later, as on hardware where the CPU is stalled meanwhile.
 */
#include "psx.h"

#include <string.h>

/* Word access to PS1 RAM; both hosts (PSP, PC) are little endian like the PS1. */
static u32 rd(Psx *psx, u32 a)
{
    u32 v;
    memcpy(&v, psx->ram + (a & (RAM_SIZE - 4)), 4);
    return v;
}

static void wr(Psx *psx, u32 a, u32 v)
{
    memcpy(psx->ram + (a & (RAM_SIZE - 4)), &v, 4);
}

static void update_master(Psx *psx, int raise)
{
    Dma *d = &psx->dma;
    int was = (d->dicr >> 31) & 1;
    int now = (d->dicr & 0x8000) || ((d->dicr & 0x800000) && ((d->dicr >> 24) & (d->dicr >> 16) & 0x7F));
    d->dicr = (d->dicr & 0x7FFFFFFFu) | ((u32)now << 31);
    if (raise && now && !was)
        psx_raise(psx, IRQ_DMA);
}

void dma_done(Psx *psx, int ch)
{
    Dma *d = &psx->dma;
    d->ch[ch].chcr &= ~0x11000000u;
    d->pending &= ~(1u << ch);
    if (d->dicr & (1u << (16 + ch)))
        d->dicr |= 1u << (24 + ch);
    update_master(psx, 1);
    if (psx->trace & 4)
        psx_log(psx, "dma%d done dicr %08X stat %03X", ch, d->dicr, psx->i_stat);
}

static void dev_write(Psx *psx, int ch, u32 v)
{
    switch (ch) {
    case 0: mdec_write(psx, 0, v); break;
    case 2: gpu_gp0(psx, v); break;
    case 4: spu_dma_write(psx, v); break;
    }
}

static u32 dev_read(Psx *psx, int ch)
{
    switch (ch) {
    case 1: return mdec_dma_out(psx);
    case 2: return gpu_read(psx, 0);
    case 3: return cd_dma_word(psx);
    case 4: return spu_dma_read(psx);
    }
    return 0xFFFFFFFFu;
}

static void transfer(Psx *psx, int ch);

/* a transfer is rare next to the clock read that times it */
static void run(Psx *psx, int ch)
{
    const PsxHost *h = psx->host;
    u32 t = h->usec ? h->usec(h->user) : 0;
    if (ch == 2 && h->gpu_dma)
        h->gpu_dma(h->user, 0);
    transfer(psx, ch);
    if (ch == 2 && h->gpu_dma)
        h->gpu_dma(h->user, 1);
    if (h->usec)
        psx->dma.us_run[ch] += h->usec(h->user) - t;
    psx->dma.n_run[ch]++;
}

/* The GPU's linked list (channel 2, sync mode 2) from addr, every packet to the GPU: its words.
 * dma_list_hook, when set, walks it instead (psxrt's Media Engine runs dma_list_walk there);
 * NULL everywhere else, and the walk is this one. */
u32 (*dma_list_hook)(Psx *psx, u32 addr);

u32 dma_list_walk(Psx *psx, u32 addr)
{
    u32 words = 0;
    const GpuRenderer *r = psx->gpu.renderer;
    u32 (*packet)(void *, const u32 *, u32) = r ? r->packet : NULL;
    for (int guard = 0; guard < 0x40000; guard++) {
        u32 h = rd(psx, addr), n = h >> 24;
        if (!n) { /* an empty ordering table entry */
        } else if (addr + 4 * (n + 1) <= RAM_SIZE) {
            const u32 *w = (const u32 *)(psx->ram + addr + 4);
            u32 k = packet && !psx->gpu.ncmd && psx->gpu.xfer != 1 ? packet(r->self, w, n) : 0;
            if (k < n)
                gpu_gp0_packet(psx, w + k, n - k);
        } else
            for (u32 i = 1; i <= n; i++)
                gpu_gp0(psx, rd(psx, addr + 4 * i));
        words += n + 1;
        addr = h & 0xFFFFFF;
        if (addr & 0x800000)
            break;
        addr &= 0x1FFFFC;
    }
    return words;
}

static void transfer(Psx *psx, int ch)
{
    DmaChan *c = &psx->dma.ch[ch];
    u32 sync = (c->chcr >> 9) & 3, addr = c->madr & 0x1FFFFC, words = 0;
    int from_ram = c->chcr & 1;
    u32 step = (c->chcr & 2) ? (u32)-4 : 4u;
    if (sync == 2) { /* linked list (GPU) */
        words = dma_list_hook ? dma_list_hook(psx, addr) : dma_list_walk(psx, addr);
        c->madr = 0xFFFFFF;
    } else {
        u32 bs = c->bcr & 0xFFFF;
        words = sync == 0 ? (bs ? bs : 0x10000) : bs * (c->bcr >> 16);
        if (ch == 6) { /* OTC: reverse-linked empty ordering table */
            for (u32 i = 0; i < words; i++, addr = (addr - 4) & 0x1FFFFC)
                wr(psx, addr, i == words - 1 ? 0xFFFFFF : (addr - 4) & 0x1FFFFF);
        } else if (from_ram) {
            for (u32 i = 0; i < words;) {
                /* image data for VRAM: whole rows at a time, up to the end of RAM */
                if (ch == 2 && step == 4 && psx->gpu.xfer == 1) {
                    u32 n = words - i, room = (RAM_SIZE - addr) / 4;
                    n = gpu_gp0_pixels(psx, (const u32 *)(psx->ram + addr), n < room ? n : room);
                    i += n, addr = (addr + 4 * n) & 0x1FFFFC;
                    continue;
                }
                if (ch == 0 && step == 4) { /* a decode command's data: a block copy */
                    u32 n = words - i, room = (RAM_SIZE - addr) / 4;
                    if ((n = mdec_dma_in(psx, (const u32 *)(psx->ram + addr), n < room ? n : room))) {
                        i += n, addr = (addr + 4 * n) & 0x1FFFFC;
                        continue;
                    }
                }
                dev_write(psx, ch, rd(psx, addr));
                i++, addr = (addr + step) & 0x1FFFFC;
            }
        } else {
            u32 start = addr;
            for (u32 i = 0; i < words;) {
                if (ch == 1 && step == 4) { /* decoded pixels: a macroblock at a time */
                    u32 n = words - i, room = (RAM_SIZE - addr) / 4;
                    n = mdec_dma_block(psx, (u32 *)(psx->ram + addr), n < room ? n : room);
                    i += n, addr = (addr + 4 * n) & 0x1FFFFC;
                    continue;
                }
                wr(psx, addr, dev_read(psx, ch));
                i++, addr = (addr + step) & 0x1FFFFC;
            }
            if (ch == 3 && step == 4 && psx->ram_loaded)
                psx->ram_loaded(psx, start, words * 4);
        }
        if (sync == 1) {
            c->madr = addr;
            c->bcr &= 0xFFFF;
        }
    }
    if (psx->trace & 4) {
        u32 sum = 0, a0 = (sync == 1 ? c->madr - 4 * words : c->madr) & 0x1FFFFC;
        for (u32 i = 0; sync < 2 && i < words; i++)
            sum |= rd(psx, a0 + 4 * i);
        psx_log(psx, "dma%d chcr %08X bcr %08X %u words or %08X", ch, c->chcr, c->bcr, words, sum);
    }
    psx->dma.words[ch] += words;
    psx->dma.pending |= 1u << ch;
    psx->dma.due[ch] = psx->cycles + (words ? words : 1);
    psx_schedule(psx, psx->dma.due[ch]);
}

void dma_update(Psx *psx)
{
    Dma *d = &psx->dma;
    for (int ch = 0; ch < 7; ch++)
        if (d->pending & (1u << ch)) {
            if (psx->cycles >= d->due[ch])
                dma_done(psx, ch);
            else
                psx_schedule(psx, d->due[ch]);
        }
}

u32 dma_read(Psx *psx, u32 addr)
{
    Dma *d = &psx->dma;
    if (addr == 0x1F8010F0)
        return d->dpcr;
    if (addr == 0x1F8010F4)
        return d->dicr;
    int ch = (int)((addr - 0x1F801080) >> 4);
    if (ch > 6)
        return 0;
    switch (addr & 0xC) {
    case 0: return d->ch[ch].madr;
    case 4: return d->ch[ch].bcr;
    case 8: return d->ch[ch].chcr;
    }
    return 0;
}

void dma_write(Psx *psx, u32 addr, u32 v)
{
    Dma *d = &psx->dma;
    if (addr == 0x1F8010F0) {
        d->dpcr = v;
        return;
    }
    if (addr == 0x1F8010F4) {
        if (psx->trace & 4)
            psx_log(psx, "dicr %08X <- %08X", d->dicr, v);
        d->dicr = (d->dicr & 0xFF000000u & ~(v & 0x7F000000u)) | (v & 0x00FF803Fu);
        update_master(psx, 0);
        return;
    }
    int ch = (int)((addr - 0x1F801080) >> 4);
    if (ch > 6)
        return;
    DmaChan *c = &d->ch[ch];
    switch (addr & 0xC) {
    case 0: c->madr = v & 0xFFFFFF; return;
    case 4: c->bcr = v; return;
    case 8:
        /* A start while the previous transfer's completion is still due: its words have moved
         * already, so it completes now. Native code can restart a channel within fewer emulated
         * cycles than the last transfer's length (libpress sends the IDCT tables back to back). */
        if ((v & 0x01000000u) && (d->pending & (1u << ch)))
            dma_done(psx, ch);
        if (ch == 6)
            c->chcr = (v & 0x51000000u) | 2;
        else
            c->chcr = v & 0x71770703u;
        break;
    default:
        return;
    }
    u32 sync = (c->chcr >> 9) & 3;
    int go = (c->chcr & 0x01000000u) && (sync != 0 || (c->chcr & 0x10000000u));
    if (go && (d->dpcr & (8u << (4 * ch))))
        run(psx, ch);
}
