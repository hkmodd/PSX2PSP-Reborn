/*
 * psxrt core: the PS1 machine the translated game runs against (docs/recomp/PLAN.md).
 *
 * Shared by the PSP runtime (translated code, psp/psxrt/main.c) and the PC simulator
 * (crates/psx2psp-sim, interpreter), so every device is tested on the PC before it reaches the
 * PSP. Plain C99, no allocation after psx_init, no host headers: the host gives disc sectors,
 * text output and a clock through PsxHost.
 *
 * Time is counted in PS1 CPU cycles (33.8688 MHz). The interpreter advances it per instruction;
 * the PSP backend sets it from the real clock. Devices schedule their next event on that clock
 * and psx_update() runs whatever is due, raising interrupts.
 */
#ifndef PSXRT_PSX_H
#define PSXRT_PSX_H

#include <stddef.h>
#include <stdint.h>

typedef uint8_t u8;
typedef int8_t s8;
typedef uint16_t u16;
typedef int16_t s16;
typedef uint32_t u32;
typedef int32_t s32;
typedef uint64_t u64;
typedef int64_t s64;

#define PSX_CLOCK 33868800u
#define RAM_SIZE 0x200000u
#define SCRATCH_SIZE 0x400u
#define VRAM_W 1024
#define VRAM_H 512

/* Interrupt lines (I_STAT bits). */
enum {
    IRQ_VBLANK = 0,
    IRQ_GPU = 1,
    IRQ_CDROM = 2,
    IRQ_DMA = 3,
    IRQ_TMR0 = 4,
    IRQ_TMR1 = 5,
    IRQ_TMR2 = 6,
    IRQ_SIO0 = 7,
    IRQ_SIO = 8,
    IRQ_SPU = 9,
};

/* What the core needs from its host. */
typedef struct PsxHost {
    void *user;
    /* Reads one raw 2352-byte sector (sync + header + data) at absolute LBA (00:02:00 = 150
     * is LBA 150, the disc's first data sector is LBA 150). Returns 0, or -1 past the end. */
    int (*read_sector)(void *user, u32 lba, u8 *raw);
    u32 disc_end; /* absolute LBA of the lead-out (one data track) */
    /* Guest text output (BIOS putchar / printf), not NUL terminated. */
    void (*tty)(void *user, const char *s, int n);
    /* Runtime diagnostics, one line, NUL terminated. */
    void (*log)(void *user, const char *line);
    /* Called once per emulated vblank, after the display state is final for that frame. */
    void (*vblank)(void *user);
    /* Interleaved stereo 16-bit samples at 44100 Hz, `frames` pairs. May be NULL. */
    void (*audio)(void *user, const s16 *samples, int frames);
    /* Memory card file of slot 0/1: 128 KB, read at init, written back on change. */
    u8 *card[2];
    void (*card_dirty)(void *user, int slot);
    /* Optional microsecond clock for profiling (Gpu.us_draw); NULL: not measured. */
    u32 (*usec)(void *user);
} PsxHost;

/* ---- CPU state, shared by the interpreter and the translated code's context ---- */
typedef struct Cpu {
    u32 gpr[32];
    u32 hi, lo;
    u32 pc;
    u32 sr, cause, epc, badvaddr; /* COP0 12, 13, 14, 8 */
    /* interpreter only */
    u32 next_pc;
    int in_delay;
    int load_reg;
    u32 load_val;
} Cpu;

typedef struct Gte {
    s32 d[32]; /* data registers */
    s32 c[32]; /* control registers */
} Gte;

/* ---- devices ---- */
typedef struct DmaChan {
    u32 madr, bcr, chcr;
} DmaChan;

typedef struct Dma {
    DmaChan ch[7];
    u32 dpcr, dicr;
    u64 due[7];  /* completion time of a channel in flight */
    u32 pending; /* channels in flight */
    u32 n_run[7], us_run[7], words[7]; /* transfers per channel, their microseconds and words */
} Dma;

typedef struct Timer {
    u32 count, mode, target;
    u64 base; /* cycle at which count was last synced */
} Timer;

typedef struct Timers {
    Timer t[3];
} Timers;

#define CD_FIFO 16
typedef struct CdResp {
    u8 irq, n;
    u8 b[CD_FIFO];
    int sector; /* INT1 of a data sector: the sector buffer is loaded when it is delivered */
} CdResp;

typedef struct Cdrom {
    u8 index;
    u8 params[CD_FIFO];
    int nparams;
    u8 resp[CD_FIFO];
    int nresp, rpos;
    u8 irq_flags, irq_mask;
    u8 stat;
    u8 mode;
    u8 filter_file, filter_chan;
    u32 setloc;  /* LBA set by Setloc */
    int setloc_pending;
    u32 pos;     /* next LBA to read */
    int reading; /* ReadN/ReadS active */
    /* responses not yet delivered, in order; one is shown to the guest at a time */
    CdResp q[4];
    int nq;
    u64 q_due;    /* when q[0] may be delivered */
    u8 cmd2;      /* command whose second response is pending, 0 none */
    u64 cmd2_due;
    u64 read_due; /* next sector of a read */
    /* data buffer */
    u8 sector[2352];      /* last sector read (header for GetlocL) */
    u8 pending[2352];     /* sector waiting in q */
    u8 data[2340];
    int data_len, data_pos;
    /* XA */
    s32 xa_prev[2][2];
    s16 xa_buf[2][4032 * 2];
    int xa_len;
    u8 vol[4]; /* LL LR RL RR */
    u8 vol_next[4];
    int muted;
} Cdrom;

#define GPU_FIFO 16

/* A primitive as the rasterizer sees it: vertices after the drawing offset (colours 0-255,
 * 0x80 for raw textures), and the draw state it was issued with. */
typedef struct GpuVtx {
    s32 x, y;
    s32 r, g, b;
    s32 u, v;
} GpuVtx;

typedef struct GpuPrim {
    int textured, raw, semi, gouraud, dither;
    u32 clut_x, clut_y;
    u32 tpx, tpy, depth, abr;
    u32 tw_and_u, tw_or_u, tw_and_v, tw_or_v; /* texture window, from E2 */
    int mask_check;
    u16 mask_set;
} GpuPrim;

/* A host renderer (the PSP's GE) drawing into its own copy of VRAM. `Gpu.vram` stays the copy
 * the core reads and writes: transfers, and whatever the renderer leaves to the software
 * rasterizer, which then asks for the pixels it reads and reports the ones it wrote. Each draw
 * returns GPU_SW (not drawn: the core draws it), GPU_DRAWN, or GPU_BOTH (drawn, and the core
 * draws it too without syncing: comparing the two). Rectangles may wrap around VRAM. */
enum { GPU_SW, GPU_DRAWN, GPU_BOTH };
typedef struct GpuRenderer {
    void *self;
    int (*triangle)(void *self, const GpuPrim *p, const GpuVtx *v0, const GpuVtx *v1, const GpuVtx *v2);
    /* v[4], the triangles 0,1,2 and 1,2,3, both within the size limits; GPU_SW: not drawn, the
     * core offers the two triangles to `triangle` */
    int (*quad)(void *self, const GpuPrim *p, const GpuVtx *v);
    int (*rect)(void *self, const GpuPrim *p, s32 x, s32 y, s32 w, s32 h, const GpuVtx *c, int flip_x, int flip_y);
    int (*line)(void *self, const GpuPrim *p, const GpuVtx *a, const GpuVtx *b);
    int (*fill)(void *self, s32 x, s32 y, s32 w, s32 h, u16 color);
    int (*copy)(void *self, s32 sx, s32 sy, s32 dx, s32 dy, s32 w, s32 h, int mask_check, u16 mask_set);
    void (*read)(void *self, s32 x, s32 y, s32 w, s32 h);  /* about to read `vram` there */
    void (*wrote)(void *self, s32 x, s32 y, s32 w, s32 h); /* wrote `vram` there */
} GpuRenderer;

typedef struct Gpu {
    u16 *vram; /* VRAM_W * VRAM_H */
    u32 stat;
    u32 gpuread;
    /* GP0 command assembly */
    u32 cmd[GPU_FIFO * 2];
    int ncmd, need;
    int polyline;
    /* VRAM transfer in progress */
    int xfer; /* 0 none, 1 CPU->VRAM, 2 VRAM->CPU */
    int xx, xy, xw, xh;
    int xi, xr; /* next pixel: column and row in the rectangle */
    /* draw state */
    u32 texpage;
    int tex_flip_x, tex_flip_y;
    u32 texwin;
    int tw_mask_x, tw_mask_y, tw_off_x, tw_off_y;
    int clip_x1, clip_y1, clip_x2, clip_y2;
    int off_x, off_y;
    int mask_set, mask_check;
    /* display */
    int disp_x, disp_y;
    int hrange1, hrange2, vrange1, vrange2;
    int disp_enabled;
    int dma_dir;
    u64 frame;
    const GpuRenderer *renderer; /* NULL: software only */
    u32 us_draw; /* microseconds spent drawing, when PsxHost.usec is set */
    /* per command kind (polygon, line, rectangle, copy, fill, other): count and microseconds;
     * fragments shaded and pixels tested. The host reads and clears them. */
    u32 n_kind[6], us_kind[6], frags, tested;
} Gpu;

typedef struct SpuVoice {
    u16 vol_l, vol_r, pitch, start, adsr1, adsr2, repeat;
    s16 adsr_vol;
    u32 addr;    /* current address in 8-byte units */
    u32 counter; /* pitch counter, 12 fraction bits */
    s16 buf[28];
    s16 hist[2];
    int phase; /* ADSR phase: 0 off, 1 attack, 2 decay, 3 sustain, 4 release */
    s32 env_counter;
    int on;
    int ended;
} SpuVoice;

typedef struct Spu {
    u8 *ram; /* 512 KB */
    SpuVoice v[24];
    u16 regs[0x200]; /* raw copy of 1F801C00-1F801FFF */
    u32 xfer_addr;
    u16 ctrl, stat;
    u32 irq_addr;
    u32 endx;
    u64 last;  /* cycle of the last mixed sample */
    s16 out[2 * 1024];
    int nout;
    double resample; /* 44100 output phase */
} Spu;

typedef struct Sio {
    u16 mode, ctrl, baud, stat;
    u8 rx;
    int rx_full;
    int device; /* 0 none, 1 pad, 2 card */
    int step;
    int port;
    u64 ack_due;
    u16 buttons[2];
    /* memory card command state */
    u8 card_cmd;
    u16 card_sector;
    u8 card_chk;
    u8 card_flag[2];
} Sio;

typedef struct Mdec {
    u32 cmd;
    int words_left; /* parameter words the current command still expects */
    int state;      /* 0 idle, 1 decoding, 2 quant table, 3 scale table */
    int color;
    int depth;
    int sign, bit15;
    u32 ctl; /* control register: bit 30 DMA in, bit 29 DMA out */
    u8 iq_y[64], iq_uv[64];
    s16 scale[64];
    u32 *in; /* parameter words of the command, from the host (in_cap words) */
    int nin, in_cap;
    int hpos; /* next halfword of `in` to decode */
    int block; /* block being decoded, for the status register */
    u32 out[192]; /* one decoded macroblock */
    int nout, rout;
    u32 us_decode; /* microseconds spent decoding, when PsxHost.usec is set */
} Mdec;

/* ---- BIOS (HLE) ---- */
#define MAX_EVENTS 32
typedef struct Bios {
    int crit;          /* critical section depth */
    u32 chain[4];      /* SysEnqIntRP priority chains */
    u32 hook;          /* SetCustomExitFromException buffer, 0 = none */
    u32 tcb_cur;       /* address of the current TCB in guest RAM */
    u32 ev_class[MAX_EVENTS], ev_spec[MAX_EVENTS], ev_mode[MAX_EVENTS], ev_func[MAX_EVENTS];
    u32 ev_status[MAX_EVENTS];
    u32 heap_start, heap_end;
    int rcnt_clear[4];
    u32 pad_buf[2];
    int pad_started;
    int pad_clear;
    int card_started;
    u32 exec_hdr; /* exec struct of the running Exec'd program */
    u32 vsync_count;
    u32 rand_seed;
    int in_exception;
    /* open files: 0 = stdin/out, 1..15 bu00/bu10 handles */
    struct BiosFile {
        int used, slot, size, pos, first_block, async;
    } fd[16];
    int card_last_slot, card_event_due;
    u32 dirent_pos;
    char dirent_pattern[24];
    int dirent_slot;
} Bios;

/* ---- the machine ---- */
typedef struct Psx Psx;

struct Psx {
    Cpu cpu;
    Gte *gte;    /* gte_own, or where the host wants the registers (psxrt: translated code reads them) */
    Gte gte_own;
    u8 *ram;     /* RAM_SIZE */
    u8 *scratch; /* SCRATCH_SIZE */
    u32 i_stat, i_mask;
    Dma dma;
    Timers tmr;
    Cdrom cd;
    Gpu gpu;
    Spu spu;
    Sio sio;
    Mdec mdec;
    Bios bios;
    u64 cycles;      /* current time */
    u64 next_event;  /* earliest due device event */
    u64 next_vblank;
    u64 frame_cycles;
    int pal;
    u32 io_ram_size, io_cache_ctrl;
    u32 exp_regs[16];
    const PsxHost *host;
    /* Runs guest code at `addr` as a call: returns when it returns. Set by the backend. */
    void (*call)(Psx *psx, u32 addr);
    /* Called once disc data has reached RAM (CD DMA, BIOS file loads): `len` bytes from `addr`.
     * The PSP backend drops translations of the code it overwrote. NULL: nothing to do. */
    void (*ram_loaded)(Psx *psx, u32 addr, u32 len);
    /* Set when control must leave the current guest context (exception, Exec, longjmp hook):
     * the backend resumes at cpu.pc with the registers in cpu. */
    int redirect;
    int exiting;
    int trace; /* diagnostics: bit 0 BIOS calls, bit 1 CD commands, bit 2 DMA */
    char exe_name[64];
};

/* psx.c */
void psx_init(Psx *psx, const PsxHost *host, u8 *ram, u8 *scratch, u16 *vram, u8 *spu_ram, u32 *mdec_in, int mdec_cap);
int psx_boot(Psx *psx); /* reads SYSTEM.CNF, loads the boot EXE, sets cpu; 0 ok */
void psx_update(Psx *psx); /* runs due device events */
void psx_raise(Psx *psx, int irq);
int psx_irq_pending(const Psx *psx); /* an unmasked interrupt is waiting and the CPU takes it */
void psx_log(Psx *psx, const char *fmt, ...);
void psx_schedule(Psx *psx, u64 due);
void psx_set_region(Psx *psx, int pal);
u32 psx_line_cycles(const Psx *psx);

/* mem.c */
u32 psx_read(Psx *psx, u32 addr, int size);
void psx_write(Psx *psx, u32 addr, u32 v, int size);
static inline u8 *psx_ram_ptr(Psx *psx, u32 addr) { return psx->ram + (addr & (RAM_SIZE - 1)); }
static inline u32 ram_r32(Psx *psx, u32 addr)
{
    const u8 *p = psx_ram_ptr(psx, addr & ~3u);
    return p[0] | p[1] << 8 | p[2] << 16 | (u32)p[3] << 24;
}
static inline void ram_w32(Psx *psx, u32 addr, u32 v)
{
    u8 *p = psx_ram_ptr(psx, addr & ~3u);
    p[0] = (u8)v, p[1] = (u8)(v >> 8), p[2] = (u8)(v >> 16), p[3] = (u8)(v >> 24);
}
static inline u16 ram_r16(Psx *psx, u32 addr)
{
    const u8 *p = psx_ram_ptr(psx, addr & ~1u);
    return (u16)(p[0] | p[1] << 8);
}
static inline void ram_w16(Psx *psx, u32 addr, u16 v)
{
    u8 *p = psx_ram_ptr(psx, addr & ~1u);
    p[0] = (u8)v, p[1] = (u8)(v >> 8);
}

/* cpu.c: interpreter (PC simulator; on the PSP for code the translation lacks) */
void cpu_exception(Psx *psx, u32 code, u32 pc, int in_delay);
int cpu_step(Psx *psx); /* one instruction; 0 ok, -1 fatal */
void cpu_run_call(Psx *psx, u32 addr); /* psx->call for the interpreter */
u32 cpu_run(Psx *psx, u64 until); /* runs until the cycle `until`, an exit or a fatal error */
int cpu_cop0_read(Psx *psx, int r, u32 *v);
void cpu_cop0_write(Psx *psx, int r, u32 v);
void cpu_rfe(Psx *psx);

/* gte.c */
u32 gte_read_data(Psx *psx, int r);
void gte_write_data(Psx *psx, int r, u32 v);
u32 gte_read_ctrl(Psx *psx, int r);
void gte_write_ctrl(Psx *psx, int r, u32 v);
void gte_command(Psx *psx, u32 cmd);

/* bios.c */
#define BIOS_RET_MAGIC 0xBFC0FF00u /* return address of host-made guest calls */
#define BIOS_EXEC_RET 0xBFC0FF10u  /* return address of an Exec'd program */
void bios_init(Psx *psx);
int bios_rom_entry(Psx *psx, u32 pc); /* the CPU reached a magic BIOS ROM address: 1 handled */
int bios_call(Psx *psx, u32 vector); /* A0/B0/C0 with t1: 1 handled (pc = ra or redirected) */
void bios_event(Psx *psx, u32 cls, u32 spec); /* DeliverEvent */

/* card.c: memory card images and the BIOS calls on them */
#define CARD_SIZE 0x20000u
void card_format(u8 *card);
int card_call(Psx *psx, u32 vector, u32 fn); /* 0: not a card call */
void bios_exception(Psx *psx);       /* CPU took an exception (EPC, cause set) */
void bios_vblank(Psx *psx);
int bios_exec(Psx *psx, u32 header, u32 sp, u32 sp_size);
int bios_load_file(Psx *psx, const char *name, u32 header_dst);
int iso_find(Psx *psx, const char *path, u32 *lba, u32 *size);

/* dma.c */
u32 dma_read(Psx *psx, u32 addr);
void dma_write(Psx *psx, u32 addr, u32 v);
void dma_done(Psx *psx, int ch);
void dma_update(Psx *psx); /* completes channels whose transfer time has passed */

/* timers.c */
u32 timer_read(Psx *psx, u32 addr);
void timer_write(Psx *psx, u32 addr, u32 v);
void timers_update(Psx *psx);
u64 timers_next(Psx *psx);
void timers_hblank_mode(Psx *psx);
/* CPU cycles per tick of the counter at addr (a count register), 0 for other addresses or a
 * stopped counter: how far time may jump between two reads without the reader missing a value */
u32 timer_tick(Psx *psx, u32 addr);

/* cdrom.c */
u8 cd_read(Psx *psx, int reg);
void cd_write(Psx *psx, int reg, u8 v);
void cd_update(Psx *psx);
u32 cd_dma_word(Psx *psx);
void cd_mix(Psx *psx, s32 *l, s32 *r, int n44100);

/* gpu.c */
void gpu_init(Psx *psx);
u32 gpu_read(Psx *psx, int reg);
void gpu_gp0(Psx *psx, u32 v);
/* GP0 words while a CPU->VRAM transfer is in progress, a row at a time: the words used (it
 * stops where the transfer ends; a lone last pixel uses its whole word) */
u32 gpu_gp0_pixels(Psx *psx, const u32 *w, u32 n);
void gpu_gp1(Psx *psx, u32 v);
void gpu_vblank(Psx *psx);

/* spu.c */
u16 spu_read(Psx *psx, u32 addr);
void spu_write(Psx *psx, u32 addr, u16 v);
void spu_dma_write(Psx *psx, u32 v);
u32 spu_dma_read(Psx *psx);
void spu_run(Psx *psx);

/* sio.c */
u32 sio_read(Psx *psx, u32 addr, int size);
void sio_write(Psx *psx, u32 addr, u32 v, int size);
void sio_update(Psx *psx);

/* mdec.c */
u32 mdec_read(Psx *psx, int reg);
void mdec_write(Psx *psx, int reg, u32 v);
u32 mdec_dma_out(Psx *psx);
/* up to n output words into dst, at most one macroblock's worth: the words written (a 0 when
 * nothing is left, as mdec_dma_out) */
u32 mdec_dma_block(Psx *psx, u32 *dst, u32 n);
int mdec_out_ready(Psx *psx);

#endif
