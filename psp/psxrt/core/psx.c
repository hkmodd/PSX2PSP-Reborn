/* psxrt core: machine setup, boot from the disc, and the event scheduler. */
#include "psx.h"

#include <stdarg.h>
#include <stdio.h>
#include <string.h>

#define K_BOOT_HDR 0x8000F800u /* exec struct of the boot EXE */

/* Video timing follows the GPU's video mode (GP1(08h) bit 3, GPUSTAT bit 20), as on the
 * hardware: a PAL console shows a game that selects NTSC at 60 Hz. psx->pal is the console. */
static int video_pal(const Psx *psx) { return (psx->gpu.stat >> 20) & 1; }
static u32 line_cycles(const Psx *psx) { return video_pal(psx) ? 2168 : 2153; }
static u32 lines(const Psx *psx) { return video_pal(psx) && !psx->pal60 ? 314 : 263; }

void psx_video_mode(Psx *psx)
{
    u64 f = (u64)line_cycles(psx) * lines(psx);
    if (psx->frame_cycles && f != psx->frame_cycles)
        psx_log(psx, "video mode %s", video_pal(psx) ? psx->pal60 ? "PAL60" : "PAL" : "NTSC");
    psx->frame_cycles = f;
}

/* The console's region; its BIOS leaves the GPU in the matching video mode. */
void psx_set_region(Psx *psx, int pal)
{
    psx->pal = pal;
    psx->gpu.stat = (psx->gpu.stat & ~(1u << 20)) | (u32)!!pal << 20;
    psx_video_mode(psx);
}

u32 psx_line_cycles(const Psx *psx) { return line_cycles(psx); }

void psx_init(Psx *psx, const PsxHost *host, u8 *ram, u8 *scratch, u16 *vram, u8 *spu_ram, u32 *mdec_in, int mdec_cap)
{
    memset(psx, 0, sizeof *psx);
    psx->host = host;
    psx->gte = &psx->gte_own;
    psx->ram = ram;
    psx->scratch = scratch;
    psx->gpu.vram = vram;
    psx->spu.ram = spu_ram;
    psx->mdec.in = mdec_in;
    psx->mdec.in_cap = mdec_cap;
    memset(ram, 0, RAM_SIZE);
    memset(scratch, 0, SCRATCH_SIZE);
    memset(spu_ram, 0, 512 * 1024);
    psx->call = cpu_run_call;
    psx->io_ram_size = 0xB88;
    psx->cpu.sr = 0x10900000u;
    psx->cpu.next_pc = 4;
    psx_set_region(psx, 0);
    psx->next_vblank = psx->frame_cycles;
    psx->next_event = psx->next_vblank;
    for (int i = 0; i < 3; i++)
        psx->tmr.t[i].mode = 0x400;
    gpu_init(psx);
    bios_init(psx);
    psx->sio.buttons[0] = psx->sio.buttons[1] = 0xFFFF;
    /* the BIOS booted from the disc: CD interrupts enabled, motor spinning */
    psx->cd.irq_mask = 0x1F;
    psx->cd.stat = 0x02;
}

static int cnf_value(const char *cnf, const char *key, char *out, int max)
{
    const char *p = strstr(cnf, key);
    if (!p)
        return -1;
    p += strlen(key);
    while (*p == ' ' || *p == '\t' || *p == '=')
        p++;
    int n = 0;
    while (*p && *p != '\r' && *p != '\n' && *p != ' ' && n < max - 1)
        out[n++] = *p++;
    out[n] = 0;
    return n;
}

static u32 hex(const char *s)
{
    u32 v = 0;
    for (; *s; s++) {
        char ch = *s;
        int d = ch >= '0' && ch <= '9' ? ch - '0' : ch >= 'a' && ch <= 'f' ? ch - 'a' + 10 : ch >= 'A' && ch <= 'F' ? ch - 'A' + 10 : -1;
        if (d < 0)
            break;
        v = v << 4 | (u32)d;
    }
    return v;
}

int psx_boot(Psx *psx)
{
    u32 lba, size;
    char cnf[512], boot[64] = "cdrom:\\PSX.EXE;1", stack[16] = "801FFF00";
    if (iso_find(psx, "SYSTEM.CNF;1", &lba, &size) == 0) {
        u8 raw[2352];
        if (size > sizeof cnf - 1)
            size = sizeof cnf - 1;
        if (psx->host->read_sector(psx->host->user, lba + 150, raw) < 0)
            return -1;
        memcpy(cnf, raw + 24, size);
        cnf[size] = 0;
        cnf_value(cnf, "BOOT", boot, sizeof boot);
        cnf_value(cnf, "STACK", stack, sizeof stack);
    }
    if (bios_load_file(psx, boot, K_BOOT_HDR) < 0)
        return -1;
    psx_set_region(psx, !strncmp(psx->exe_name, "SLES", 4) || !strncmp(psx->exe_name, "SCES", 4) || !strncmp(psx->exe_name, "SCED", 4));
    psx->next_vblank = psx->cycles + psx->frame_cycles;
    psx->next_event = psx->next_vblank;
    Cpu *c = &psx->cpu;
    c->sr = 0x40000401u; /* COP2 usable, interrupts on (IM2) */
    u32 sp = hex(stack);
    c->gpr[29] = c->gpr[30] = sp;
    c->gpr[28] = psx_read(psx, K_BOOT_HDR + 4, 4);
    c->gpr[31] = BIOS_EXEC_RET;
    psx->bios.exec_hdr = K_BOOT_HDR;
    psx_write(psx, K_BOOT_HDR + 0x28, sp, 4);
    psx_write(psx, K_BOOT_HDR + 0x2C, sp, 4);
    psx_write(psx, K_BOOT_HDR + 0x34, BIOS_EXEC_RET, 4);
    c->pc = psx_read(psx, K_BOOT_HDR, 4);
    c->next_pc = c->pc + 4;
    psx_log(psx, "boot %s (%s) pc %08X sp %08X", boot, psx->pal ? "PAL" : "NTSC", c->pc, sp);
    return 0;
}

void psx_raise(Psx *psx, int irq) { psx->i_stat |= 1u << irq; }

void psx_schedule(Psx *psx, u64 due)
{
    if (due < psx->next_event)
        psx->next_event = due;
}

/* Ends part `k` of a profiled psx_update, begun at `t`: returns when the next one begins. */
static u32 upd_mark(Psx *psx, int k, u32 t)
{
    u32 now = psx->host->usec(psx->host->user);
    psx->us_upd[k] += (now - t) * psx->prof_scale - psx->prof_bias;
    psx->prof_reads++;
    return now;
}

void psx_update(Psx *psx)
{
    int prof = psx->prof_scale != 0;
    u32 t = prof ? (psx->prof_reads++, psx->host->usec(psx->host->user)) : 0;
#ifdef PSXRT_PC_TRACE
    psxrt_io_trace(0, 2);
#endif
#define MARK(k) if (prof) t = upd_mark(psx, k, t)
    if (psx->cycles >= psx->next_vblank) {
        psx->next_vblank += psx->frame_cycles;
        if (psx->next_vblank <= psx->cycles) /* the host fell behind: do not replay frames */
            psx->next_vblank = psx->cycles + psx->frame_cycles;
        if (psx->trace & 16)
            psx_log(psx, "vblank pc %08X ra %08X sr %08X stat %03X mask %03X", psx->cpu.pc, psx->cpu.gpr[31], psx->cpu.sr, psx->i_stat, psx->i_mask);
        gpu_vblank(psx);
        psx_raise(psx, IRQ_VBLANK);
        bios_vblank(psx);
        MARK(UPD_VBL);
        spu_run(psx);
        MARK(UPD_SPU);
        if (psx->host->vblank)
            psx->host->vblank(psx->host->user);
        MARK(UPD_HOST);
    }
    psx->next_event = psx->next_vblank;
    timers_update(psx);
    psx_schedule(psx, timers_next(psx));
    MARK(UPD_TIMERS);
    dma_update(psx);
    MARK(UPD_DMA);
    cd_update(psx);
    MARK(UPD_CD);
    sio_update(psx);
    MARK(UPD_SIO);
    spu_beacon(psx); /* the SPU on another core may run up to here */
#undef MARK
}

void psx_log(Psx *psx, const char *fmt, ...)
{
    if (!psx->host->log)
        return;
    char line[256];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(line, sizeof line, fmt, ap);
    va_end(ap);
    psx->host->log(psx->host->user, line);
}
