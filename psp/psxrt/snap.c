/*
 * psxrt: snapshots of the whole machine (docs/recomp/S9.md).
 *
 * A snapshot is taken at a host call of the top-level translated code (native.c): no guest call
 * of the BIOS is in progress, so the host stack holds nothing of the guest and the machine is
 * the Psx structure, its memories, the translation context and the dispatch table. Host return
 * addresses in guest RAM stay valid because every executable's code goes back to its fixed host
 * address. A snapshot is tied to the EBOOT's code directory and to the runtime's structure
 * sizes; anything else is refused and the disc boots.
 *
 * File (native endianness): header, Psx, PS1 RAM, scratchpad, VRAM, SPU RAM, MDEC input words,
 * then native.c's part (context, images, GRET stack, dispatch table, floating placements).
 */
#include "snap.h"

#include <pspiofilemgr.h>
#include <pspkernel.h>
#include <string.h>

#include "native.h"

#define SPU_RAM_SIZE (512u * 1024u)
#define SNAP_VERSION 2u

typedef struct {
    char magic[8];
    u32 version;
    u32 layout[10]; /* structure sizes */
    char disc_id[16];
    u32 dir_hash; /* FNV-1a of the code directory and its range */
    u32 host_pc, frames;
    u32 mdec_words;
} SnapHeader;

static u32 fnv(u32 h, const void *p, u32 n)
{
    for (u32 i = 0; i < n; i++)
        h = (h ^ ((const u8 *)p)[i]) * 0x01000193u;
    return h;
}

static void header(SnapHeader *h, const DiscInfo *info)
{
    memset(h, 0, sizeof *h);
    memcpy(h->magic, "PSXRTSNP", 8);
    h->version = SNAP_VERSION;
    u32 l[10] = {sizeof(Psx), sizeof(Cpu), sizeof(Gte), sizeof(Dma), sizeof(Timers), sizeof(Cdrom), sizeof(Gpu), sizeof(Spu), sizeof(Sio), sizeof(Mdec) | sizeof(Bios) << 16};
    memcpy(h->layout, l, sizeof l);
    memcpy(h->disc_id, info->disc_id, sizeof h->disc_id);
    u32 range[8] = {info->code_base, info->code_size, info->flags, info->fl_offset, info->fl_stored, info->fl_len, info->fl_host, info->fl_arena};
    h->dir_hash = fnv(fnv(0x811C9DC5u, range, sizeof range), info->dir, info->images * (u32)sizeof(CodeEntry));
}

static int put(SceUID fd, const void *p, u32 n) { return sceIoWrite(fd, p, n) == (int)n ? 0 : -1; }
static int get(SceUID fd, void *p, u32 n) { return sceIoRead(fd, p, n) == (int)n ? 0 : -1; }

int snap_save(const char *path, Psx *psx, const DiscInfo *info, u32 host_pc, u32 frames)
{
    gpu_sync(psx, GPU_SYNC_OTHER); /* the ME's walk done: its GPU state in psx */
    SnapHeader h;
    header(&h, info);
    h.host_pc = host_pc, h.frames = frames, h.mdec_words = (u32)psx->mdec.nin;
    sceKernelDcacheWritebackAll(); /* the driver may read memory by DMA */
    SceUID fd = sceIoOpen(path, PSP_O_WRONLY | PSP_O_CREAT | PSP_O_TRUNC, 0777);
    if (fd < 0)
        return -1;
    int bad = put(fd, &h, sizeof h) || put(fd, psx, sizeof *psx) || put(fd, psx->ram, RAM_SIZE) || put(fd, psx->scratch, SCRATCH_SIZE) ||
              put(fd, psx->gpu.vram, VRAM_W * VRAM_H * 2) || put(fd, psx->spu.ram, SPU_RAM_SIZE) || put(fd, psx->mdec.in, h.mdec_words * 4) ||
              native_save(fd);
    sceIoClose(fd);
    return bad ? -1 : 0;
}

int snap_load(const char *path, Psx *psx, const DiscInfo *info, u32 *host_pc, u32 *frames)
{
    gpu_sync(psx, GPU_SYNC_OTHER); /* no walk on the ME over the state read here */
    SceUID fd = sceIoOpen(path, PSP_O_RDONLY, 0);
    if (fd < 0)
        return -1;
    SnapHeader h, want;
    header(&want, info);
    if (get(fd, &h, sizeof h) || memcmp(h.magic, want.magic, 8) || h.version != want.version || memcmp(h.layout, want.layout, sizeof h.layout) ||
        memcmp(h.disc_id, want.disc_id, sizeof h.disc_id) || h.dir_hash != want.dir_hash || h.mdec_words > (u32)psx->mdec.in_cap) {
        sceIoClose(fd);
        return -2;
    }
    /* what belongs to this run: buffers, the host, the backend, the renderer, the trace bits */
    Gte *gte = psx->gte;
    u8 *ram = psx->ram, *scratch = psx->scratch, *spu_ram = psx->spu.ram;
    u16 *vram = psx->gpu.vram;
    const GpuRenderer *renderer = psx->gpu.renderer;
    u32 *mdec_in = psx->mdec.in;
    int mdec_cap = psx->mdec.in_cap, trace = psx->trace;
    const PsxHost *host = psx->host;
    void (*call)(Psx *, u32) = psx->call;
    void (*loaded)(Psx *, u32, u32) = psx->ram_loaded;
    sceKernelDcacheWritebackInvalidateAll();
    int bad = get(fd, psx, sizeof *psx);
    psx->gte = gte, psx->ram = ram, psx->scratch = scratch, psx->spu.ram = spu_ram, psx->gpu.vram = vram, psx->gpu.renderer = renderer;
    psx->mdec.in = mdec_in, psx->mdec.in_cap = mdec_cap, psx->trace = trace, psx->host = host, psx->call = call, psx->ram_loaded = loaded;
    psx->gpu.us_draw = psx->spu.us_mix = psx->mdec.us_decode = 0; /* the profile counts from here */
    memset(psx->us_upd, 0, sizeof psx->us_upd), psx->prof_scale = 0;
    bad = bad || get(fd, ram, RAM_SIZE) || get(fd, scratch, SCRATCH_SIZE) || get(fd, vram, VRAM_W * VRAM_H * 2) || get(fd, spu_ram, SPU_RAM_SIZE) ||
          get(fd, mdec_in, h.mdec_words * 4) || native_load(fd);
    sceIoClose(fd);
    sceKernelDcacheWritebackInvalidateAll();
    spu_ram_changed();
    if (bad)
        return -3;
    *host_pc = h.host_pc, *frames = h.frames;
    return 0;
}
