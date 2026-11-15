/*
 * psxrt core: the PS1 BIOS, high level. Same model as Sony's kernel, so PsyQ libraries that poke
 * at it behave as on hardware:
 *  - an exception saves the registers into the current TCB (kernel RAM, found through [0x108]),
 *    runs the SysEnqIntRP priority chains (lists in guest RAM: +0 next, +4 func2, +8 func1), then
 *    leaves through the SetCustomExitFromException buffer (longjmp) or ReturnFromException;
 *  - events live in the EvCB array ([0x120]) with Sony's class/spec/mode/status values;
 *  - threads are the TCB array ([0x110]): ChangeThread saves the running one as an exception
 *    does and resumes the other through ReturnFromException;
 *  - kernel handlers (root counters, pads) sit in those same chains, as entries whose functions
 *    are magic BIOS ROM addresses this file runs in C.
 * The whole A0/B0/C0 table is answered (psx-spx "Kernel (BIOS)"): the C library in libc.c, the
 * memory card in card.c, files on the disc here through the ISO9660 reader, the BIOS CD and GPU
 * helpers synchronously. Kernel structures are read and written through host pointers into
 * guest RAM, never through the bus: the exception path runs on every interrupt.
 * Guest code is called through psx->call (interpreter or translated code).
 */
#include "psx.h"

#include <string.h>

/* kernel RAM (below the game's 0x80010000) */
#define K_EXCB 0x80006000u  /* 4 priorities x 8 bytes: chain head, unused */
#define K_PCB 0x80006020u   /* [0] = current TCB */
#define K_TCB 0x80006030u   /* 4 TCBs of 0xC0 bytes */
#define TCB_SIZE 0xC0u
#define N_TCB 4
#define K_EVCB 0x80006400u  /* MAX_EVENTS x 0x1C */
#define EVCB_SIZE 0x1Cu
#define K_ENTRIES 0x80006800u /* kernel chain entries, 16 bytes each: 0-3 root counters, 4 pads */
#define K_STACK 0x8000DF00u   /* stack for chain functions */
#define K_DCB 0x80007000u     /* device control blocks, 0x50 bytes each; names after them */
#define DCB_SIZE 0x50u
#define K_KHEAP 0x8000F000u   /* alloc_kernel_memory's heap, up to the boot EXE's header */
#define K_KHEAP_SIZE 0x800u
/* The pad driver as PsyQ's pad patches (libapi _patch_pad, send_pad, _remove_ChgclrPAD) reach
 * it: B0 table entry 0x5B points here and they work at Sony's offsets from it. +0x884 and +0x894
 * are the driver's start and stop, +0x7A0 sets the output (rumble) buffers: jumps to the magic
 * addresses below. +0x594 (slot change on abort) and +0x62C (the handler's IRQ acknowledge) are
 * code they overwrite with zeros: the second holds PAD_ACK words, and once they are gone the
 * handler stops acknowledging the vblank. */
#define K_PADDRV 0x80001000u
#define PAD_ACK 0x0BADC0DEu

/* the HLE's own kernel variables, at K_VARS (zero in a snapshot from before them: the defaults) */
#define V_ERRNO (K_VARS + 0x00)    /* GetLastError */
#define V_AUTOACK (K_VARS + 0x04)  /* SetIrqAutoAck flags, a bit per IRQ */
#define V_CARDFIND (K_VARS + 0x08) /* set_card_find_mode */
#define V_CONF (K_VARS + 0x0C)     /* SetConf: EvCBs, TCBs, stack top */
#define V_KHEAP (K_VARS + 0x18)    /* kernel heap: start, end */
#define V_CDLBA (K_VARS + 0x20)    /* CdAsyncSeekL's sector */
#define V_CDMODE (K_VARS + 0x24)   /* CdAsyncSetMode */
#define V_PADDEST (K_VARS + 0x28)  /* OutdatedPadInitAndStart's button_dest */
#define V_CDDIR (K_VARS + 0x2C)    /* firstfile on the disc: directory sector, size */
#define V_EXEC (K_VARS + 0x40)     /* LoadExec's exec header, 0x40 bytes */
#define V_SYSINFO (K_VARS + 0x80)  /* GetSystemInfo(2)'s string */
#define V_SIZE 0xA0u

/* magic BIOS ROM addresses, see bios_rom_entry(); entry n of the handlers: n 0-3 root counters,
 * n 4 the pads */
#define M_RCNT_V(n) (0xBFC0FF20u + (n) * 8)
#define M_RCNT_H(n) (0xBFC0FF24u + (n) * 8)
#define M_PAD_V 0xBFC0FF40u
#define M_PAD_H 0xBFC0FF44u
#define M_PAD_START 0xBFC0FF48u
#define M_PAD_STOP 0xBFC0FF4Cu
#define M_PAD_OUT 0xBFC0FF50u

enum { EV_FREE = 0, EV_DISABLED = 0x1000, EV_ENABLED = 0x2000, EV_READY = 0x4000 };
enum { EVMODE_CALLBACK = 0x1000, EVMODE_READY = 0x2000 };
enum { TH_FREE = 0x1000, TH_USED = 0x4000 };
enum { E_NOENT = 2, E_BADF = 9, E_INVAL = 0x16, E_MFILE = 0x18 };
#define FD_CD 2 /* BiosFile.slot of a file on the disc (card.c's are slots 0 and 1) */

#define G (psx->cpu.gpr)
enum { R_V0 = 2, R_V1, R_A0, R_A1, R_A2, R_A3, R_T0, R_T1, R_S0 = 16, R_GP = 28, R_SP, R_FP, R_RA };

/* Guest words. Kernel structures and the game's own lie in RAM: read there directly (a bus
 * access decodes the address for every word). Anything else takes the bus. */
static u32 r32(Psx *psx, u32 a)
{
    u32 p = a & 0x1FFFFFFF;
    if (p < 0x800000 && !(a & 3))
        return *(const u32 *)(psx->ram + (p & (RAM_SIZE - 1)));
    return psx_read(psx, a, 4);
}

static void w32(Psx *psx, u32 a, u32 v)
{
    u32 p = a & 0x1FFFFFFF;
    if (p < 0x800000 && !(a & 3) && !(psx->cpu.sr & 0x10000)) {
        *(u32 *)(psx->ram + (p & (RAM_SIZE - 1))) = v;
        return;
    }
    psx_write(psx, a, v, 4);
}

static u8 r8(Psx *psx, u32 a) { return (u8)psx_read(psx, a, 1); }
static void w8(Psx *psx, u32 a, u8 v) { psx_write(psx, a, v, 1); }

/* a word of the kernel's own RAM (word-aligned constant addresses) */
static u32 *kp(Psx *psx, u32 a) { return (u32 *)(psx->ram + (a & (RAM_SIZE - 4))); }

/* direct pointer to guest RAM for [a, a+n) when it is plain RAM, else NULL */
static u8 *ram_span(Psx *psx, u32 a, u32 n)
{
    u32 p = a & 0x1FFFFFFF;
    if (p >= 0x800000 || (p & (RAM_SIZE - 1)) + n > RAM_SIZE)
        return 0;
    return psx->ram + (p & (RAM_SIZE - 1));
}

static int rd_str(Psx *psx, u32 a, char *out, int max)
{
    u32 p = a & 0x1FFFFFFF;
    if (p < 0x800000) { /* RAM: one scan */
        u32 o = p & (RAM_SIZE - 1), room = RAM_SIZE - o;
        const u8 *s = psx->ram + o, *z = memchr(s, 0, room < (u32)max ? room : (u32)max);
        int n = z ? (int)(z - s) : (room < (u32)max ? (int)room : max - 1);
        if (n > max - 1)
            n = max - 1;
        memcpy(out, s, (size_t)n);
        out[n] = 0;
        return n;
    }
    int n = 0;
    while (n < max - 1) {
        char ch = (char)r8(psx, a + (u32)n);
        if (!ch)
            break;
        out[n++] = ch;
    }
    out[n] = 0;
    return n;
}

static void wr_bytes(Psx *psx, u32 dst, const void *src, u32 n)
{
    u8 *m = ram_span(psx, dst, n);
    if (m)
        memcpy(m, src, n);
    else
        for (u32 i = 0; i < n; i++)
            w8(psx, dst + i, ((const u8 *)src)[i]);
}

static void g_memset(Psx *psx, u32 dst, u8 v, u32 n)
{
    u8 *d = ram_span(psx, dst, n);
    if (d) {
        memset(d, v, n);
        return;
    }
    for (u32 i = 0; i < n; i++)
        w8(psx, dst + i, v);
}

static void g_memcpy(Psx *psx, u32 dst, u32 src, u32 n)
{
    u8 *d = ram_span(psx, dst, n), *s = ram_span(psx, src, n);
    if (d && s) {
        memmove(d, s, n);
        return;
    }
    for (u32 i = 0; i < n; i++)
        w8(psx, dst + i, r8(psx, src + i));
}

static void tty(Psx *psx, const char *s, int n)
{
    if (psx->host->tty && n > 0)
        psx->host->tty(psx->host->user, s, n);
}

static void set_errno(Psx *psx, u32 e) { *kp(psx, V_ERRNO) = e; }

/* SystemError, jump_to_00000000h, ioabort: the BIOS stops there */
static int fatal(Psx *psx, const char *what, u32 vector, u32 fn)
{
    psx_log(psx, "BIOS %02X(%02X): %s, ra %08X", vector, fn, what, G[R_RA]);
    psx->exiting = 2;
    return 1;
}

/* ---- calls into guest code ---- */

static void k_rcnt_v(Psx *psx, int n);
static void k_rcnt_h(Psx *psx, int n);
static void k_pad_v(Psx *psx);
static void k_pad_h(Psx *psx);

/* Runs the function at `addr` (guest code or a kernel handler) as a call. */
static void invoke(Psx *psx, u32 addr)
{
    u32 k = addr - M_RCNT_V(0);
    if (k < 0x28 && !(k & 3)) {
        int n = (int)(k >> 3);
        if (n < 4)
            (k & 4) ? k_rcnt_h(psx, n) : k_rcnt_v(psx, n);
        else
            (k & 4) ? k_pad_h(psx) : k_pad_v(psx);
        return;
    }
    psx->call(psx, addr);
}

/* ---- events: EvCB +0 class, +4 status, +8 spec, +0xC mode, +0x10 handler ---- */

#define EVW (EVCB_SIZE / 4)

static u32 *ev_at(Psx *psx, int i) { return kp(psx, K_EVCB) + i * EVW; }

static int ev_index(u32 h)
{
    u32 i = h & 0xFFFF;
    return (h & 0xFFFF0000u) != 0xF1000000u || i >= MAX_EVENTS ? -1 : (int)i;
}

static u32 open_event(Psx *psx, u32 cls, u32 spec, u32 mode, u32 func)
{
    u32 *e = ev_at(psx, 0);
    for (int i = 0; i < MAX_EVENTS; i++, e += EVW)
        if (e[1] == EV_FREE) {
            e[0] = cls, e[1] = EV_DISABLED, e[2] = spec, e[3] = mode, e[4] = func;
            return 0xF1000000u | (u32)i;
        }
    psx_log(psx, "OpenEvent: no free event for %08X/%04X", cls, spec);
    return 0xFFFFFFFFu;
}

static void deliver_event(Psx *psx, u32 cls, u32 spec)
{
    u32 *e = ev_at(psx, 0);
    for (int i = 0; i < MAX_EVENTS; i++, e += EVW) {
        if (e[1] != EV_ENABLED || e[0] != cls || e[2] != spec)
            continue;
        if (e[3] == EVMODE_READY)
            e[1] = EV_READY;
        else if (e[3] == EVMODE_CALLBACK && e[4]) {
            Cpu save = psx->cpu;
            invoke(psx, e[4]);
            psx->cpu = save;
        }
    }
}

static void undeliver_event(Psx *psx, u32 cls, u32 spec)
{
    u32 *e = ev_at(psx, 0);
    for (int i = 0; i < MAX_EVENTS; i++, e += EVW)
        if (e[1] == EV_READY && e[0] == cls && e[2] == spec && e[3] == EVMODE_READY)
            e[1] = EV_ENABLED;
}

/* ---- exceptions and threads: TCB +0 status, +8 r0-r31, +0x88 epc, +0x8C hi, +0x90 lo,
 * +0x94 sr, +0x98 cause ---- */

static u32 tcb(Psx *psx) { return *kp(psx, K_PCB); }

static u32 *tcb_ptr(Psx *psx, u32 t) { return (t & 3) ? NULL : (u32 *)ram_span(psx, t, TCB_SIZE); }

static void save_ctx(Psx *psx, u32 t, u32 epc, u32 sr)
{
    u32 *w = tcb_ptr(psx, t);
    if (!w)
        return;
    memcpy(w + 2, G, 32 * 4);
    w[0x22] = epc, w[0x23] = psx->cpu.hi, w[0x24] = psx->cpu.lo, w[0x25] = sr, w[0x26] = psx->cpu.cause;
}

/* ReturnFromException: registers and SR from the TCB, rfe, continue at the saved EPC. */
static void return_from_exception(Psx *psx)
{
    u32 *w = tcb_ptr(psx, tcb(psx));
    if (w) {
        memcpy(G + 1, w + 3, 31 * 4);
        G[0] = 0;
        psx->cpu.hi = w[0x23], psx->cpu.lo = w[0x24], psx->cpu.sr = w[0x25];
        cpu_rfe(psx);
        psx->cpu.pc = w[0x22];
    } else {
        cpu_rfe(psx);
        psx->cpu.pc = psx->cpu.epc;
    }
    psx->bios.in_exception = 0;
    psx->redirect = 1;
}

/* Switches to the thread whose TCB is `t`: the running one was saved by the caller. */
static void switch_thread(Psx *psx, u32 t)
{
    *kp(psx, K_PCB) = t;
    return_from_exception(psx);
}

static void longjmp_to(Psx *psx, u32 buf, u32 v0)
{
    G[R_RA] = r32(psx, buf);
    G[R_SP] = r32(psx, buf + 4);
    G[R_FP] = r32(psx, buf + 8);
    for (int i = 0; i < 8; i++)
        G[R_S0 + i] = r32(psx, buf + 0xC + 4 * (u32)i);
    G[R_GP] = r32(psx, buf + 0x2C);
    G[R_V0] = v0;
    psx->cpu.pc = G[R_RA];
    psx->redirect = 1;
}

static void run_chains(Psx *psx)
{
    for (int prio = 0; prio < 4; prio++) {
        u32 node = *kp(psx, K_EXCB + 8 * (u32)prio);
        for (int guard = 0; node && guard < 64; guard++) {
            if (node & 3) {
                psx_log(psx, "chain %d: bad entry %08X", prio, node);
                break;
            }
            u32 next = r32(psx, node), f2 = r32(psx, node + 4), f1 = r32(psx, node + 8);
            if (f1) {
                G[R_SP] = K_STACK;
                invoke(psx, f1);
                u32 v = G[R_V0];
                if (v && f2) {
                    G[R_A0] = v;
                    G[R_SP] = K_STACK;
                    invoke(psx, f2);
                }
            }
            node = next;
        }
    }
}

void bios_exception(Psx *psx)
{
    Cpu *c = &psx->cpu;
    u32 code = (c->cause >> 2) & 0x1F;
    if (code == 8) { /* syscall(a0) */
        switch (G[R_A0]) {
        case 0: /* NoFunction */
            break;
        case 1: /* EnterCriticalSection: IEp and IM2 off, returns whether they were on */
            G[R_V0] = (c->sr & 0x404) == 0x404;
            c->sr &= ~0x404u;
            break;
        case 2: /* ExitCriticalSection */
            c->sr |= 0x404;
            G[R_V0] = 0;
            break;
        case 3: /* ChangeThreadSubFunction(a1 = TCB): the caller resumes after the syscall with 1 */
            G[R_V0] = 1;
            save_ctx(psx, tcb(psx), c->epc + 4, c->sr);
            switch_thread(psx, G[R_A1]);
            return;
        default: /* DeliverEvent(F0000010h, 4000h): "unknown syscall" */
            deliver_event(psx, 0xF0000010u, 0x4000);
            break;
        }
        cpu_rfe(psx);
        c->pc = c->epc + 4;
        psx->redirect = 1;
        return;
    }
    if (code != 0) {
        static const char *names[32] = {"Int", "Mod", "TLBL", "TLBS", "AdEL", "AdES", "IBE", "DBE", "Sys", "Bp", "RI", "CpU", "Ov"};
        psx_log(psx, "exception %s (%u) at %08X, badvaddr %08X, ra %08X", names[code] ? names[code] : "?", code, c->epc, c->badvaddr, G[R_RA]);
        psx->exiting = 2;
        return;
    }
    if (psx->trace & 8)
        psx_log(psx, "irq stat %03X mask %03X at %08X cyc %llu", psx->i_stat, psx->i_mask, c->epc, (unsigned long long)psx->cycles);
    psx->bios.in_exception = 1;
    save_ctx(psx, tcb(psx), c->epc, c->sr);
    run_chains(psx);
    if (psx->bios.hook)
        longjmp_to(psx, psx->bios.hook, 1);
    else
        return_from_exception(psx);
}

static void enq_int(Psx *psx, u32 prio, u32 entry)
{
    u32 *head = kp(psx, K_EXCB + 8 * (prio & 3));
    w32(psx, entry, *head);
    *head = entry;
}

static void deq_int(Psx *psx, u32 prio, u32 entry)
{
    u32 link = K_EXCB + 8 * (prio & 3);
    for (int guard = 0; guard < 64; guard++) {
        u32 node = r32(psx, link);
        if (!node)
            return;
        if (node == entry) {
            w32(psx, link, r32(psx, node));
            return;
        }
        link = node;
    }
}

/* ---- kernel interrupt handlers ---- */

static const u32 rcnt_irq[4] = {1u << IRQ_TMR0, 1u << (IRQ_TMR0 + 1), 1u << (IRQ_TMR0 + 2), 1u << IRQ_VBLANK};

static void k_rcnt_v(Psx *psx, int n) { G[R_V0] = psx->i_stat & psx->i_mask & rcnt_irq[n]; }

static void k_rcnt_h(Psx *psx, int n)
{
    if (psx->bios.rcnt_clear[n])
        psx->i_stat &= ~rcnt_irq[n];
    deliver_event(psx, 0xF2000000u | (u32)n, 0x0002);
    G[R_V0] = 0;
}

static void pad_word(Psx *psx, u32 buf, int port)
{
    if (!buf)
        return;
    u16 b = psx->sio.buttons[port];
    w8(psx, buf, 0x00);     /* status: connected */
    w8(psx, buf + 1, 0x41); /* digital pad, 1 halfword */
    w8(psx, buf + 2, (u8)b);
    w8(psx, buf + 3, (u8)(b >> 8));
}

/* PAD_dr: pad 1 in the low halfword, pad 2 in the high one, each with its first button byte on
 * top (active low) */
static u32 pad_dr(Psx *psx)
{
    u32 a = psx->sio.buttons[0], b = psx->sio.buttons[1];
    return ((a & 0xFF) << 8 | a >> 8) | ((b & 0xFF) << 8 | b >> 8) << 16;
}

static void k_pad_v(Psx *psx) { G[R_V0] = psx->i_stat & psx->i_mask & 1u; }

static void k_pad_h(Psx *psx)
{
    pad_word(psx, psx->bios.pad_buf[0], 0);
    pad_word(psx, psx->bios.pad_buf[1], 1);
    u32 dest = *kp(psx, V_PADDEST); /* OutdatedPadInitAndStart's */
    if (dest)
        w32(psx, dest, pad_dr(psx));
    /* the acknowledge is gone once _remove_ChgclrPAD zeroed it (a snapshot from before
     * K_PADDRV existed has neither it nor the entries) */
    if (psx->bios.pad_clear && (*kp(psx, K_PADDRV + 0x62C) || !*kp(psx, K_PADDRV + 0x884)))
        psx->i_stat &= ~1u;
    G[R_V0] = 0;
}

/* The pad handler in the vblank chain (priority 2), or out of it. */
static void pad_start(Psx *psx)
{
    if (!psx->bios.pad_started) {
        *kp(psx, K_ENTRIES + 4 * 16 + 4) = M_PAD_H;
        *kp(psx, K_ENTRIES + 4 * 16 + 8) = M_PAD_V;
        enq_int(psx, 2, K_ENTRIES + 4 * 16);
        psx->bios.pad_started = 1;
    }
    psx->i_mask |= 1;
}

static void pad_stop(Psx *psx)
{
    if (psx->bios.pad_started) {
        deq_int(psx, 2, K_ENTRIES + 4 * 16);
        psx->bios.pad_started = 0;
    }
}

/* the BIOS boots the disc again: kernel and boot EXE from scratch */
static void reboot(Psx *psx)
{
    bios_init(psx);
    if (psx_boot(psx) < 0)
        psx->exiting = 2;
    psx->redirect = 1;
}

int bios_rom_entry(Psx *psx, u32 pc)
{
    if (pc - M_RCNT_V(0) < 0x28) { /* a kernel handler called as a function */
        invoke(psx, pc);
        psx->cpu.pc = G[R_RA];
        return 1;
    }
    if (pc >= M_PAD_START && pc <= M_PAD_OUT) { /* the pad driver's own entries, see K_PADDRV */
        if (pc == M_PAD_START)
            pad_start(psx);
        else if (pc == M_PAD_STOP)
            pad_stop(psx);
        G[R_V0] = 1;
        psx->cpu.pc = G[R_RA];
        return 1;
    }
    if (pc == BIOS_EXEC_RET) { /* an Exec'd program returned */
        u32 h = psx->bios.exec_hdr;
        if (h == V_EXEC) { /* LoadExec does not return */
            psx_log(psx, "%s returned from LoadExec: reboot", psx->exe_name);
            reboot(psx);
            return 1;
        }
        G[R_SP] = r32(psx, h + 0x28);
        G[R_FP] = r32(psx, h + 0x2C);
        G[R_GP] = r32(psx, h + 0x30);
        G[R_RA] = r32(psx, h + 0x34);
        G[R_S0] = r32(psx, h + 0x38);
        G[R_V0] = 1;
        psx->cpu.pc = G[R_RA];
        return 1;
    }
    return 0;
}

/* ---- heaps (InitHeap/malloc in the game's RAM, alloc_kernel_memory at K_KHEAP): first fit,
 * 4-byte headers size | used, walked through host pointers ---- */

static void heap_init(Psx *psx, u32 *lo, u32 *hi, u32 start, u32 size)
{
    start = (start + 3) & ~3u;
    size &= ~3u;
    if (size < 8 || !ram_span(psx, start, size)) {
        psx_log(psx, "InitHeap(%08X, %X): not RAM", start, size);
        size = 0;
    }
    *lo = start, *hi = start + size;
    if (size)
        *kp(psx, start) = size - 4;
}

static void heap_merge(Psx *psx, u32 lo, u32 hi)
{
    for (u32 p = lo; p + 4 <= hi;) {
        u32 *h = kp(psx, p), sz = *h & ~3u;
        if (sz > hi - p - 4)
            return; /* the game wrote over the heap */
        if (!(*h & 1)) {
            for (u32 q = p + 4 + sz; q + 4 <= hi && !(*kp(psx, q) & 1); q = p + 4 + sz) {
                u32 qs = *kp(psx, q) & ~3u;
                if (qs > hi - q - 4)
                    break;
                sz += 4 + qs;
            }
            *h = sz;
        }
        p += 4 + sz;
    }
}

static u32 heap_alloc(Psx *psx, u32 lo, u32 hi, u32 n)
{
    n = (n + 3) & ~3u;
    for (int pass = 0; pass < 2; pass++) {
        for (u32 p = lo; p + 4 <= hi;) {
            u32 *h = kp(psx, p), sz = *h & ~3u;
            if (sz > hi - p - 4)
                break;
            if (!(*h & 1) && sz >= n) {
                if (sz - n >= 8) {
                    *kp(psx, p + 4 + n) = sz - n - 4;
                    sz = n;
                }
                *h = sz | 1;
                return p + 4;
            }
            p += 4 + sz;
        }
        heap_merge(psx, lo, hi);
    }
    psx_log(psx, "malloc(%u) failed", n);
    return 0;
}

static void heap_free(Psx *psx, u32 lo, u32 hi, u32 a)
{
    if (a >= lo + 4 && a < hi && !(a & 3))
        *kp(psx, a - 4) &= ~1u;
}

/* the kernel heap, set up on first use */
static u32 *kheap(Psx *psx)
{
    u32 *v = kp(psx, V_KHEAP);
    if (!v[1])
        heap_init(psx, v, v + 1, K_KHEAP, K_KHEAP_SIZE);
    return v;
}

/* ---- printf ---- */

static u32 vararg(Psx *psx, int n) { return n < 4 ? G[R_A0 + n] : r32(psx, G[R_SP] + 4 * (u32)n); }

static int format(Psx *psx, u32 fmt, int argi, char *out, int max)
{
    int o = 0;
    char f[256];
    rd_str(psx, fmt, f, sizeof f);
    for (const char *s = f; *s && o < max - 1; s++) {
        if (*s != '%') {
            out[o++] = *s;
            continue;
        }
        s++;
        int left = 0, zero = 0, width = 0, prec = -1;
        for (;; s++) {
            if (*s == '-') left = 1;
            else if (*s == '0') zero = 1;
            else if (*s == '+' || *s == ' ' || *s == '#') ;
            else break;
        }
        if (*s == '*') {
            width = (int)vararg(psx, argi++);
            s++;
        }
        while (*s >= '0' && *s <= '9')
            width = width * 10 + (*s++ - '0');
        if (*s == '.') {
            prec = 0;
            for (s++; *s >= '0' && *s <= '9'; s++)
                prec = prec * 10 + (*s - '0');
        }
        while (*s == 'l' || *s == 'h')
            s++;
        char tmp[40];
        const char *str = tmp;
        int len = 0;
        switch (*s) {
        case 'd': case 'i': case 'u': case 'x': case 'X': case 'p': case 'o': {
            u32 v = vararg(psx, argi++);
            int neg = (*s == 'd' || *s == 'i') && (s32)v < 0;
            if (neg)
                v = (u32)-(s32)v;
            u32 base = (*s == 'x' || *s == 'X' || *s == 'p') ? 16 : *s == 'o' ? 8 : 10;
            const char *dig = *s == 'X' ? "0123456789ABCDEF" : "0123456789abcdef";
            char rev[16];
            int n = 0;
            do {
                rev[n++] = dig[v % base];
                v /= base;
            } while (v);
            if (neg)
                tmp[len++] = '-';
            while (zero && !left && len + n < width)
                tmp[len++] = '0';
            while (n)
                tmp[len++] = rev[--n];
            break;
        }
        case 'c':
            tmp[len++] = (char)vararg(psx, argi++);
            break;
        case 's': {
            u32 a = vararg(psx, argi++);
            len = a ? rd_str(psx, a, tmp, sizeof tmp) : 0;
            if (prec >= 0 && len > prec)
                len = prec;
            break;
        }
        case '%':
            tmp[len++] = '%';
            break;
        default:
            tmp[len++] = '%';
            if (*s)
                tmp[len++] = *s;
            else
                s--;
        }
        int pad = width > len ? width - len : 0;
        while (!left && pad-- > 0 && o < max - 1)
            out[o++] = ' ';
        for (int i = 0; i < len && o < max - 1; i++)
            out[o++] = str[i];
        while (left && pad-- > 0 && o < max - 1)
            out[o++] = ' ';
    }
    out[o] = 0;
    return o;
}

/* ---- the disc: ISO9660 ---- */

static u8 iso_buf[2352];

static const u8 *iso_raw(Psx *psx, u32 lba)
{
    return psx->host->read_sector(psx->host->user, lba + 150, iso_buf) < 0 ? NULL : iso_buf;
}

static const u8 *iso_sector(Psx *psx, u32 lba)
{
    const u8 *r = iso_raw(psx, lba);
    return r ? r + 24 : NULL; /* mode 2 form 1 user data */
}

static u32 le32(const u8 *p) { return p[0] | p[1] << 8 | p[2] << 16 | (u32)p[3] << 24; }

static int iso_root(Psx *psx, u32 *lba, u32 *size)
{
    const u8 *s = iso_sector(psx, 16);
    if (!s || memcmp(s + 1, "CD001", 5))
        return -1;
    *lba = le32(s + 156 + 2);
    *size = le32(s + 156 + 10);
    return 0;
}

static char upper(char c) { return c >= 'a' && c <= 'z' ? (char)(c - 32) : c; }

static int name_eq(const u8 *rec_name, int n, const char *want, int wn)
{
    int rn = n;
    for (int i = 0; i < n; i++)
        if (rec_name[i] == ';') {
            rn = i;
            break;
        }
    int ww = wn;
    for (int i = 0; i < wn; i++)
        if (want[i] == ';') {
            ww = i;
            break;
        }
    if (rn != ww)
        return 0;
    for (int i = 0; i < rn; i++)
        if (upper((char)rec_name[i]) != upper(want[i]))
            return 0;
    return 1;
}

/* `path` from the root ("" or "\": the root itself) */
int iso_find(Psx *psx, const char *path, u32 *lba, u32 *size)
{
    u32 dir, dsize;
    if (iso_root(psx, &dir, &dsize) < 0)
        return -1;
    while (*path == '\\' || *path == '/')
        path++;
    while (*path) {
        const char *end = path;
        while (*end && *end != '\\' && *end != '/')
            end++;
        int found = 0;
        for (u32 off = 0; off < dsize && !found; off += 2048) {
            const u8 *d = iso_sector(psx, dir + off / 2048);
            if (!d)
                return -1;
            for (u32 p = 0; p < 2048 && d[p];) {
                const u8 *r = d + p;
                if (name_eq(r + 33, r[32], path, (int)(end - path))) {
                    dir = le32(r + 2);
                    dsize = le32(r + 10);
                    found = 1;
                    break;
                }
                p += r[0];
            }
        }
        if (!found)
            return -1;
        path = *end ? end + 1 : end;
    }
    *lba = dir;
    *size = dsize;
    return 0;
}

/* `count` sectors from `lba` to guest memory: `size` bytes of each from `off` in the raw sector */
static int disc_read(Psx *psx, u32 lba, u32 count, u32 dst, u32 size, u32 off)
{
    for (u32 i = 0; i < count; i++) {
        const u8 *r = iso_raw(psx, lba + i);
        if (!r)
            return -1;
        wr_bytes(psx, dst + i * size, r + off, size);
    }
    if (psx->ram_loaded && count)
        psx->ram_loaded(psx, dst, count * size);
    return (int)count;
}

/* strips the device ("cdrom:") */
static const char *cd_path(const char *name)
{
    const char *p = strchr(name, ':');
    return p ? p + 1 : name;
}

static int is_cdrom(const char *name) { return !strncmp(name, "cdrom", 5) && strchr(name, ':'); }

/* The exec struct of a PS-X EXE (its header's 0x10-0x4B) to header_dst; the EXE's sector. */
static int exe_header(Psx *psx, const char *name, u32 header_dst, u32 *lba)
{
    u32 size;
    if (iso_find(psx, cd_path(name), lba, &size) < 0) {
        psx_log(psx, "load: %s not found", name);
        return -1;
    }
    const u8 *h = iso_sector(psx, *lba);
    if (!h || memcmp(h, "PS-X EXE", 8)) {
        psx_log(psx, "load: %s is not a PS-X EXE", name);
        return -1;
    }
    wr_bytes(psx, header_dst, h + 0x10, 0x3C);
    return 0;
}

/* Loads a PS-X EXE: the exec struct to header_dst, the text to t_addr. */
int bios_load_file(Psx *psx, const char *name, u32 header_dst)
{
    u32 lba;
    if (exe_header(psx, name, header_dst, &lba) < 0)
        return -1;
    u32 t_addr = r32(psx, header_dst + 8), t_size = r32(psx, header_dst + 0xC);
    for (u32 done = 0; done < t_size; done += 2048) {
        const u8 *d = iso_sector(psx, lba + 1 + done / 2048);
        if (!d)
            return -1;
        wr_bytes(psx, t_addr + done, d, t_size - done < 2048 ? t_size - done : 2048);
    }
    if (psx->ram_loaded)
        psx->ram_loaded(psx, t_addr, t_size);
    const char *base = strrchr(name, '\\');
    base = base ? base + 1 : cd_path(name);
    int n = 0;
    while (base[n] && base[n] != ';' && n < (int)sizeof psx->exe_name - 1) {
        psx->exe_name[n] = base[n];
        n++;
    }
    psx->exe_name[n] = 0;
    return 0;
}

/* Exec(header, param1, param2): bss cleared, the stack from the header's s_addr/s_size when
 * set, the caller's sp/fp/gp/ra/s0 kept in the header for the return (BIOS_EXEC_RET). */
int bios_exec(Psx *psx, u32 h, u32 param1, u32 param2)
{
    u32 b_addr = r32(psx, h + 0x18), b_size = r32(psx, h + 0x1C);
    u32 s_addr = r32(psx, h + 0x20), s_size = r32(psx, h + 0x24);
    if (b_size)
        g_memset(psx, b_addr, 0, (b_size + 3) & ~3u);
    w32(psx, h + 0x28, G[R_SP]);
    w32(psx, h + 0x2C, G[R_FP]);
    w32(psx, h + 0x30, G[R_GP]);
    w32(psx, h + 0x34, G[R_RA]);
    w32(psx, h + 0x38, G[R_S0]);
    if (s_addr)
        G[R_SP] = G[R_FP] = s_addr + s_size;
    G[R_GP] = r32(psx, h + 4);
    G[R_RA] = BIOS_EXEC_RET;
    G[R_A0] = param1;
    G[R_A1] = param2;
    psx->bios.exec_hdr = h;
    psx->cpu.pc = r32(psx, h);
    psx->redirect = 1;
    psx_log(psx, "Exec %s pc %08X sp %08X", psx->exe_name, psx->cpu.pc, G[R_SP]);
    return 0;
}

/* ---- files on the disc (the card's are card.c's) ---- */

static struct BiosFile *cd_file(Psx *psx, u32 fd)
{
    struct BiosFile *f = fd < 16 ? &psx->bios.fd[fd] : NULL;
    return f && f->used && f->slot == FD_CD ? f : NULL;
}

static u32 cd_open(Psx *psx, const char *name)
{
    u32 lba, size;
    if (iso_find(psx, cd_path(name), &lba, &size) < 0) {
        psx_log(psx, "open %s: not found", name);
        set_errno(psx, E_NOENT);
        return 0xFFFFFFFFu;
    }
    for (u32 i = 2; i < 16; i++) {
        struct BiosFile *f = &psx->bios.fd[i];
        if (!f->used) {
            f->used = 1, f->slot = FD_CD, f->size = (int)size, f->pos = 0, f->first_block = (int)lba, f->async = 0;
            return i;
        }
    }
    set_errno(psx, E_MFILE);
    return 0xFFFFFFFFu;
}

/* read: whole sectors' data, up to the end of the file's last sector */
static u32 cd_file_read(Psx *psx, struct BiosFile *f, u32 dst, u32 len)
{
    u32 pos = (u32)f->pos, size = (u32)f->size, end = (size + 0x7FF) & ~0x7FFu;
    if (pos >= size) {
        set_errno(psx, E_INVAL);
        return 0xFFFFFFFFu;
    }
    u32 n = len < end - pos ? len : end - pos;
    for (u32 done = 0; done < n;) {
        u32 p = pos + done, off = p & 0x7FF, k = 0x800 - off;
        const u8 *d = iso_sector(psx, (u32)f->first_block + p / 0x800);
        if (!d)
            return 0xFFFFFFFFu;
        k = k < n - done ? k : n - done;
        wr_bytes(psx, dst + done, d + off, k);
        done += k;
    }
    if (psx->ram_loaded && n)
        psx->ram_loaded(psx, dst, n);
    f->pos = (int)(pos + n);
    return n;
}

/* A0 00-07 and B0 32-39: open, lseek, read, write, close, ioctl, exit, isatty */
static int file_call(Psx *psx, u32 fn)
{
    u32 a0 = G[R_A0], a1 = G[R_A1], a2 = G[R_A2];
    struct BiosFile *f = fn >= 1 && fn <= 4 ? cd_file(psx, a0) : NULL;
    switch (fn) {
    case 0x00: { /* open(name, mode) */
        char name[64];
        rd_str(psx, a0, name, sizeof name);
        if (is_cdrom(name))
            G[R_V0] = cd_open(psx, name);
        else {
            psx_log(psx, "open(%s, %X): no such device", name, a1);
            set_errno(psx, E_NOENT);
            G[R_V0] = 0xFFFFFFFFu;
        }
        return 1;
    }
    case 0x01: /* lseek(fd, offset, whence) */
        if (f) {
            f->pos = (int)(a2 == 0 ? a1 : a2 == 1 ? (u32)f->pos + a1 : (u32)f->size + a1);
            G[R_V0] = (u32)f->pos;
            return 1;
        }
        break;
    case 0x02: /* read(fd, dst, len) */
        if (f) {
            G[R_V0] = cd_file_read(psx, f, a1, a2);
            return 1;
        }
        break;
    case 0x03: /* write(fd, src, len) */
        if (a0 == 1) {
            char buf[256];
            u32 n = a2 < sizeof buf ? a2 : sizeof buf;
            for (u32 i = 0; i < n; i++)
                buf[i] = (char)r8(psx, a1 + i);
            tty(psx, buf, (int)n);
            G[R_V0] = a2;
            return 1;
        }
        break;
    case 0x04: /* close(fd) */
        if (f) {
            f->used = 0;
            G[R_V0] = a0;
            return 1;
        }
        break;
    case 0x06: /* exit */
        psx_log(psx, "exit(%d)", (s32)a0);
        psx->exiting = 1;
        return 1;
    case 0x07: /* isatty */
        G[R_V0] = a0 <= 1;
        return 1;
    }
    set_errno(psx, E_BADF);
    G[R_V0] = 0xFFFFFFFFu;
    return 1;
}

/* firstfile/nextfile on the disc: the directory's records matching the pattern, '?' one char and
 * '*' the rest. A direntry: +0 name (the record's, ";1" included), +0x18 size, +0x20 sector. */
static int pattern_match(const char *pat, const u8 *name, int n)
{
    int i = 0;
    for (; *pat && *pat != ';'; pat++, i++) {
        if (*pat == '*')
            return 1;
        int end = i >= n || name[i] == ';';
        if (end || (*pat != '?' && upper(*pat) != upper((char)name[i])))
            return 0;
    }
    return i >= n || name[i] == ';';
}

static u32 cd_next(Psx *psx, u32 de)
{
    Bios *b = &psx->bios;
    u32 *v = kp(psx, V_CDDIR);
    while (b->dirent_pos < v[1]) {
        const u8 *d = iso_sector(psx, v[0] + b->dirent_pos / 2048);
        u32 off = b->dirent_pos % 2048;
        if (!d)
            return 0;
        if (!d[off]) { /* records do not cross sectors */
            b->dirent_pos = (b->dirent_pos / 2048 + 1) * 2048;
            continue;
        }
        const u8 *r = d + off;
        int nl = r[32];
        b->dirent_pos += r[0];
        if ((nl == 1 && r[33] <= 1) || !pattern_match(b->dirent_pattern, r + 33, nl))
            continue; /* "." and ".." */
        u8 out[0x28] = {0};
        memcpy(out, r + 33, nl < 19 ? (size_t)nl : 19);
        u32 size = le32(r + 10), lba = le32(r + 2);
        memcpy(out + 0x18, &size, 4);
        memcpy(out + 0x20, &lba, 4);
        wr_bytes(psx, de, out, sizeof out);
        return de;
    }
    return 0;
}

static u32 cd_firstfile(Psx *psx, const char *path, u32 de)
{
    Bios *b = &psx->bios;
    const char *p = cd_path(path), *slash = strrchr(p, '\\');
    char dir[64] = "";
    if (slash && slash - p < (int)sizeof dir) {
        memcpy(dir, p, (size_t)(slash - p));
        dir[slash - p] = 0;
    }
    const char *pat = slash ? slash + 1 : p;
    u32 *v = kp(psx, V_CDDIR);
    if (iso_find(psx, dir, &v[0], &v[1]) < 0)
        return 0;
    size_t n = strlen(pat), max = sizeof b->dirent_pattern - 1;
    memcpy(b->dirent_pattern, pat, n < max ? n : max);
    b->dirent_pattern[n < max ? n : max] = 0;
    b->dirent_slot = FD_CD;
    b->dirent_pos = 0;
    return cd_next(psx, de);
}

/* ---- GPU helpers ---- */

/* n words from guest memory to GP0: image data a row at a time while a CPU->VRAM transfer is
 * open (as DMA2 does), anything else word by word */
static void gp0_words(Psx *psx, u32 src, u32 n)
{
    while (n) {
        u32 p = src & 0x1FFFFFFF;
        if (psx->gpu.xfer == 1 && p < 0x800000 && !(p & 3)) {
            u32 o = p & (RAM_SIZE - 1), room = (RAM_SIZE - o) / 4;
            u32 k = gpu_gp0_pixels(psx, (const u32 *)(psx->ram + o), n < room ? n : room);
            if (k) {
                n -= k, src += 4 * k;
                continue;
            }
        }
        gpu_gp0(psx, r32(psx, src));
        n--, src += 4;
    }
}

/* the CPU->VRAM transfer of a w x h rectangle at x, y */
static void gpu_image(Psx *psx, u32 x, u32 y, u32 w, u32 h, u32 src)
{
    gpu_gp0(psx, 0xA0000000u);
    gpu_gp0(psx, (y & 0xFFFF) << 16 | (x & 0xFFFF));
    gpu_gp0(psx, (h & 0xFFFF) << 16 | (w & 0xFFFF));
    gp0_words(psx, src, ((w & 0xFFFF) * (h & 0xFFFF) + 1) / 2);
}

/* a DMA2 linked list, as dma.c walks it */
static void gpu_list(Psx *psx, u32 addr)
{
    addr &= 0x1FFFFC;
    for (int guard = 0; guard < 0x40000; guard++) {
        u32 h = *kp(psx, addr), n = h >> 24;
        if (addr + 4 * (n + 1) <= RAM_SIZE)
            gpu_gp0_packet(psx, (const u32 *)(psx->ram + addr + 4), n);
        else
            for (u32 i = 1; i <= n; i++)
                gpu_gp0(psx, r32(psx, addr + 4 * i));
        addr = h & 0xFFFFFF;
        if (addr & 0x800000)
            break;
        addr &= 0x1FFFFC;
    }
}

/* ---- the call tables ---- */

static u32 bcd(u8 v) { return (u32)(v >> 4) * 10 + (v & 15); }

static int call_a(Psx *psx, u32 fn)
{
    u32 a0 = G[R_A0], a1 = G[R_A1], a2 = G[R_A2], a3 = G[R_A3];
    if (fn - 0x0A < 0x2D) { /* 0A-36: the C library */
        int r = libc_call(psx, fn);
        if (r)
            return r;
    }
    switch (fn) {
    case 0x08: /* getc(fd): no input */
    case 0x3B: /* getchar */
        G[R_V0] = 0xFFFFFFFFu;
        return 1;
    case 0x09: /* putc(ch, fd) */
        if (a1 == 1) {
            char ch = (char)a0;
            tty(psx, &ch, 1);
        }
        G[R_V0] = a0;
        return 1;
    case 0x0B: case 0x32: /* atof, strtod: unusable on hardware (no FPU emulation) */
        G[R_V0] = G[R_V1] = 0;
        return 1;
    case 0x13: /* setjmp */
        w32(psx, a0, G[R_RA]);
        w32(psx, a0 + 4, G[R_SP]);
        w32(psx, a0 + 8, G[R_FP]);
        for (int i = 0; i < 8; i++)
            w32(psx, a0 + 0xC + 4 * (u32)i, G[R_S0 + i]);
        w32(psx, a0 + 0x2C, G[R_GP]);
        G[R_V0] = 0;
        return 1;
    case 0x14: /* longjmp */
        longjmp_to(psx, a0, a1 ? a1 : 1);
        return 2;
    case 0x2F: /* rand */
        psx->bios.rand_seed = psx->bios.rand_seed * 1103515245u + 12345u;
        G[R_V0] = (psx->bios.rand_seed >> 16) & 0x7FFF;
        return 1;
    case 0x30: psx->bios.rand_seed = a0; return 1;
    case 0x33: G[R_V0] = heap_alloc(psx, psx->bios.heap_start, psx->bios.heap_end, a0); return 1;
    case 0x34: heap_free(psx, psx->bios.heap_start, psx->bios.heap_end, a0); return 1;
    case 0x37: { /* calloc */
        u32 n = a0 * a1, p = heap_alloc(psx, psx->bios.heap_start, psx->bios.heap_end, n);
        if (p)
            g_memset(psx, p, 0, n);
        G[R_V0] = p;
        return 1;
    }
    case 0x38: { /* realloc */
        u32 lo = psx->bios.heap_start, hi = psx->bios.heap_end;
        if (!a0) {
            G[R_V0] = heap_alloc(psx, lo, hi, a1);
            return 1;
        }
        u32 old = r32(psx, a0 - 4) & ~3u, p = heap_alloc(psx, lo, hi, a1);
        if (p) {
            g_memcpy(psx, p, a0, old < a1 ? old : a1);
            heap_free(psx, lo, hi, a0);
        }
        G[R_V0] = p;
        return 1;
    }
    case 0x39: heap_init(psx, &psx->bios.heap_start, &psx->bios.heap_end, a0, a1); return 1;
    case 0x3A: psx_log(psx, "_exit(%d)", (s32)a0); psx->exiting = 1; return 1;
    case 0x3C: { /* putchar */
        char ch = (char)a0;
        tty(psx, &ch, 1);
        G[R_V0] = a0;
        return 1;
    }
    case 0x3D: /* gets(dst): no input */
        w8(psx, a0, 0);
        G[R_V0] = a0;
        return 1;
    case 0x3E: { /* puts */
        char buf[256];
        int n = rd_str(psx, a0, buf, sizeof buf - 1);
        buf[n++] = '\n';
        tty(psx, buf, n);
        return 1;
    }
    case 0x3F: { /* printf */
        char buf[512];
        int n = format(psx, a0, 1, buf, sizeof buf);
        tty(psx, buf, n);
        G[R_V0] = (u32)n;
        return 1;
    }
    case 0x40: return fatal(psx, "SystemErrorUnresolvedException", 0xA0, fn);
    case 0x41: case 0x42: { /* LoadTest / Load(name, headerbuf) */
        char name[64];
        u32 lba;
        rd_str(psx, a0, name, sizeof name);
        if (fn == 0x41)
            G[R_V0] = exe_header(psx, name, a1, &lba) < 0 ? 0 : r32(psx, a1);
        else
            G[R_V0] = bios_load_file(psx, name, a1) == 0;
        return 1;
    }
    case 0x43: /* Exec(header, param1, param2) */
        bios_exec(psx, a0, a1, a2);
        return 2;
    case 0x44: /* FlushCache: code stores wrote may run from here */
        if (psx->ram_loaded)
            psx->ram_loaded(psx, 0, 0);
        return 1;
    case 0x45: /* init_a0_b0_c0_vectors: the HLE answers the vectors itself */
        return 1;
    case 0x46: /* GPU_dw(x, y, w, h, src) */
    case 0x47: /* gpu_send_dma(x, y, w, h, src) */
        gpu_image(psx, a0, a1, a2, a3, r32(psx, G[R_SP] + 0x10));
        if (fn == 0x47)
            G[R_V0] = 0x1F801810u;
        return 1;
    case 0x48: gpu_gp1(psx, a0); return 1; /* SendGP1Command */
    case 0x49: gpu_gp0(psx, a0); return 1; /* GPU_cw */
    case 0x4A: gp0_words(psx, a0, a1); G[R_V0] = 0; return 1; /* GPU_cwp(src, n) */
    case 0x4B: gpu_list(psx, a0); return 1; /* send_gpu_linked_list */
    case 0x4C: /* gpu_abort_dma */
        gpu_gp1(psx, 0x04000000u);
        gpu_gp1(psx, 0x02000000u);
        gpu_gp1(psx, 0x01000000u);
        G[R_V0] = 0x1F801814u;
        return 1;
    case 0x4D: G[R_V0] = gpu_read(psx, 1); return 1; /* GetGPUStatus */
    case 0x4E: /* gpu_sync: waits for DMA2 */
        if (psx->dma.pending & (1u << 2))
            return 3;
        G[R_V0] = 0;
        return 1;
    case 0x4F: case 0x50: case 0x53: case 0x9A: case 0x9B:
        return fatal(psx, "SystemError", 0xA0, fn);
    case 0x51: { /* LoadExec(name, stackbase, stackoffset) */
        char name[64];
        rd_str(psx, a0, name, sizeof name);
        if (bios_load_file(psx, name, V_EXEC) < 0) {
            G[R_V0] = 0;
            return 1;
        }
        w32(psx, V_EXEC + 0x20, a1);
        w32(psx, V_EXEC + 0x24, a2);
        bios_exec(psx, V_EXEC, 1, 0);
        return 2;
    }
    case 0x52: G[R_V0] = K_STACK; return 1; /* GetSysSp */
    case 0x54: case 0x55: case 0x56: /* _96_init, _bu_init, _96_remove */
    case 0x70: case 0x71: case 0x72:
        return 1;
    case 0x5B: case 0x5C: case 0x5D: case 0x5E: /* dev_tty_*: no DUART */
        G[R_V0] = 0;
        return 1;
    /* dev_cd_* and dev_card_* (5F-6E): the drivers' own entries, on FCBs the HLE does not keep:
     * the file calls above answer for them */
    case 0x78: { /* CdAsyncSeekL(src): BCD minute, second, sector */
        *kp(psx, V_CDLBA) = (bcd(r8(psx, a0)) * 60 + bcd(r8(psx, a0 + 1))) * 75 + bcd(r8(psx, a0 + 2)) - 150;
        deliver_event(psx, 0xF0000003u, 0x20);
        G[R_V0] = 1;
        return 1;
    }
    case 0x7C: w8(psx, a0, 0x02); G[R_V0] = 1; return 1; /* CdAsyncGetStatus(dst): motor on */
    case 0x7E: { /* CdAsyncReadSector(count, dst, mode) */
        u32 *lba = kp(psx, V_CDLBA);
        u32 size = a2 & 0x20 ? 0x924 : a2 & 0x10 ? 0x918 : 0x800, off = size == 0x924 ? 12 : 24;
        int ok = disc_read(psx, *lba, a0, a1, size, off) >= 0;
        *lba += a0;
        if (ok)
            deliver_event(psx, 0xF0000003u, 0x20);
        G[R_V0] = (u32)ok;
        return 1;
    }
    case 0x81: *kp(psx, V_CDMODE) = a0; G[R_V0] = 1; return 1; /* CdAsyncSetMode */
    case 0x94: /* CdromGetInt5errCode(dst1, dst2) */
        w32(psx, a0, 0);
        w32(psx, a1, 0);
        return 1;
    case 0x90: case 0x91: case 0x92: case 0x93: case 0x95: /* CD IRQ functions, CdInitSubFunc */
    case 0x96: case 0x97: case 0x98: case 0x99: /* Add*Device */
    case 0x9E: /* SetCdromIrqAutoAbort */
    case 0xA2: case 0xA3: /* EnqueueCdIntr / DequeueCdIntr */
    case 0xA7: case 0xA8: case 0xA9: case 0xAA: case 0xAD: case 0xAE: /* bufs_cb_*, _card_auto */
        return 1;
    case 0x9C: { /* SetConf(EvCBs, TCBs, stacktop): events and threads start over */
        u32 *v = kp(psx, V_CONF);
        v[0] = a0, v[1] = a1, v[2] = a2;
        memset(ev_at(psx, 0), 0, MAX_EVENTS * EVCB_SIZE);
        for (u32 i = 0; i < N_TCB; i++)
            *kp(psx, K_TCB + i * TCB_SIZE) = i ? TH_FREE : TH_USED;
        *kp(psx, K_PCB) = K_TCB;
        return 1;
    }
    case 0x9D: { /* GetConf(EvCBs_dst, TCBs_dst, stacktop_dst) */
        u32 *v = kp(psx, V_CONF);
        w32(psx, a0, v[0] ? v[0] : 16);
        w32(psx, a1, v[1] ? v[1] : N_TCB);
        w32(psx, a2, v[2] ? v[2] : 0x801FFF00u);
        return 1;
    }
    case 0x9F: { /* SetMemSize(megabytes): RAM_SIZE bits 8-10 and [0x60] */
        u32 v = psx->io_ram_size & ~0x700u;
        if (a0 == 4)
            v |= 0x400;
        else if (a0 == 8)
            v |= 0x300;
        else if (a0 == 16)
            v |= 0x700;
        else if (a0 != 2) {
            G[R_V0] = 0xFFFFFFFFu;
            return 1;
        }
        psx->io_ram_size = v;
        *kp(psx, 0x80000060u) = a0;
        G[R_V0] = 0;
        return 1;
    }
    case 0xA0: reboot(psx); return 2; /* WarmBoot */
    case 0xA1: return fatal(psx, "SystemErrorBootOrDiskFailure", 0xA0, fn);
    case 0xA4: { /* CdGetLbn(name) */
        char name[64];
        u32 lba, size;
        rd_str(psx, a0, name, sizeof name);
        G[R_V0] = iso_find(psx, cd_path(name), &lba, &size) < 0 ? 0xFFFFFFFFu : lba;
        return 1;
    }
    case 0xA5: /* CdReadSector(count, sector, dst) */
        G[R_V0] = (u32)disc_read(psx, a1, a0, a2, 0x800, 24);
        return 1;
    case 0xA6: G[R_V0] = 0x02; return 1; /* CdGetStatus */
    case 0xB2: return fatal(psx, "ioabort_raw", 0xA0, fn);
    case 0xB4: { /* GetSystemInfo(index) */
        static const u32 info[15] = {0x19951204u, 3, 0, 0, 0, 2048, 0, 0x400, 0, 0x200, 0, 0, 1, 1, 1};
        if (a0 == 2) {
            static const char ver[] = "CEX-3000/1001/1002 by K.S.";
            wr_bytes(psx, V_SYSINFO, ver, sizeof ver);
            G[R_V0] = V_SYSINFO;
        } else
            G[R_V0] = a0 < 15 ? info[a0] : 0;
        return 1;
    }
    }
    if (fn <= 0x07)
        return file_call(psx, fn);
    /* the entries that only return 0 */
    if ((fn >= 0x57 && fn <= 0x5A) || fn == 0x6F || (fn >= 0x73 && fn <= 0x77) || (fn >= 0x79 && fn <= 0x7B) ||
        fn == 0x7D || fn == 0x7F || fn == 0x80 || (fn >= 0x82 && fn <= 0x8F) || fn == 0xB0 || fn == 0xB1 || fn == 0xB3) {
        G[R_V0] = 0;
        return 1;
    }
    if (fn >= 0x5F && fn <= 0x6E) { /* dev_cd_*, dev_card_* */
        G[R_V0] = 0xFFFFFFFFu;
        return 1;
    }
    return 0;
}

static int call_b(Psx *psx, u32 fn)
{
    u32 a0 = G[R_A0], a1 = G[R_A1], a2 = G[R_A2], a3 = G[R_A3];
    switch (fn) {
    case 0x00: { /* alloc_kernel_memory(size) */
        u32 *h = kheap(psx);
        G[R_V0] = heap_alloc(psx, h[0], h[1], a0);
        return 1;
    }
    case 0x01: { /* free_kernel_memory(buf) */
        u32 *h = kheap(psx);
        heap_free(psx, h[0], h[1], a0);
        return 1;
    }
    case 0x02: { /* init_timer(t, reload, flags) */
        u32 t = a0 & 0xFFFF, r = 0x1F801100u + 16 * t, m = 0x48;
        if (t > 2) {
            G[R_V0] = 0;
            return 1;
        }
        if (a2 & 0x10)
            m |= 0x01; /* gate */
        if (!(a2 & 1))
            m |= 0x100; /* not the system clock */
        if (a2 & 0x1000)
            m |= 0x10; /* IRQ at the target */
        timer_write(psx, r + 4, 0);
        timer_write(psx, r + 8, a1 & 0xFFFF);
        timer_write(psx, r + 4, m);
        G[R_V0] = 1;
        return 1;
    }
    case 0x03: { /* get_timer(t) */
        u32 t = a0 & 0xFFFF;
        G[R_V0] = t > 2 ? 0 : timer_read(psx, 0x1F801100u + 16 * t);
        return 1;
    }
    case 0x04: case 0x05: { /* enable / disable_timer_irq(t): t 3 is the vblank */
        u32 t = a0 & 0xFFFF, bit = t < 3 ? 1u << (IRQ_TMR0 + t) : t == 3 ? 1u << IRQ_VBLANK : 0;
        if (fn == 0x04) {
            psx->i_mask |= bit;
            G[R_V0] = t < 3;
        } else {
            psx->i_mask &= ~bit;
            G[R_V0] = 1;
        }
        return 1;
    }
    case 0x06: { /* restart_timer(t) */
        u32 t = a0 & 0xFFFF;
        if (t <= 2)
            timer_write(psx, 0x1F801100u + 16 * t, 0);
        G[R_V0] = t <= 2;
        return 1;
    }
    case 0x07: deliver_event(psx, a0, a1); return 1;
    case 0x08: G[R_V0] = open_event(psx, a0, a1, a2, a3); return 1;
    case 0x09: { /* CloseEvent */
        int i = ev_index(a0);
        if (i >= 0)
            ev_at(psx, i)[1] = EV_FREE;
        G[R_V0] = 1;
        return 1;
    }
    case 0x0A: { /* WaitEvent: spins (taking interrupts) until the event is ready */
        int i = ev_index(a0);
        u32 *e = i >= 0 ? ev_at(psx, i) : NULL;
        if (e && e[1] == EV_READY) {
            e[1] = EV_ENABLED;
            G[R_V0] = 1;
            return 1;
        }
        if (!e || e[1] != EV_ENABLED) {
            G[R_V0] = 0;
            return 1;
        }
        return 3; /* busy: retry the call */
    }
    case 0x0B: { /* TestEvent */
        int i = ev_index(a0);
        u32 *e = i >= 0 ? ev_at(psx, i) : NULL;
        G[R_V0] = e && e[1] == EV_READY;
        if (G[R_V0])
            e[1] = EV_ENABLED;
        return 1;
    }
    case 0x0C: case 0x0D: { /* EnableEvent / DisableEvent */
        int i = ev_index(a0);
        if (i >= 0 && ev_at(psx, i)[1] != EV_FREE)
            ev_at(psx, i)[1] = fn == 0x0C ? EV_ENABLED : EV_DISABLED;
        G[R_V0] = 1;
        return 1;
    }
    case 0x0E: { /* OpenThread(pc, sp_fp, gp) */
        for (u32 i = 0; i < N_TCB; i++) {
            u32 *w = kp(psx, K_TCB + i * TCB_SIZE);
            if (w[0] == TH_USED)
                continue;
            w[0] = TH_USED, w[1] = 0x1000;
            w[0x22] = a0;
            w[0x25] = 0x40000404u; /* SR as the exception pushed it: COP2, IM2, interrupts on after rfe */
            w[2 + R_SP] = w[2 + R_FP] = a1;
            w[2 + R_GP] = a2;
            G[R_V0] = 0xFF000000u | i;
            return 1;
        }
        G[R_V0] = 0xFFFFFFFFu;
        return 1;
    }
    case 0x0F: { /* CloseThread */
        u32 i = a0 & 0xFFFF;
        if (i < N_TCB)
            *kp(psx, K_TCB + i * TCB_SIZE) = TH_FREE;
        G[R_V0] = 1;
        return 1;
    }
    case 0x10: { /* ChangeThread: the caller resumes at ra with 1, its SR pushed as an exception's */
        u32 i = a0 & 0xFFFF, t = K_TCB + i * TCB_SIZE, sr = psx->cpu.sr;
        if (i >= N_TCB || *kp(psx, t) != TH_USED) {
            G[R_V0] = 0;
            return 1;
        }
        G[R_V0] = 1;
        save_ctx(psx, tcb(psx), G[R_RA], (sr & ~0x3Fu) | ((sr << 2) & 0x3Cu));
        switch_thread(psx, t);
        return 2;
    }
    case 0x12: /* InitPAD(buf1, siz1, buf2, siz2) */
        psx->bios.pad_buf[0] = a0;
        psx->bios.pad_buf[1] = a2;
        G[R_V0] = 2;
        return 1;
    case 0x13: /* StartPAD */
    case 0x4B: /* StartCARD: the same PadCard handler, which reads the pads InitPAD set up */
        pad_start(psx);
        psx->bios.pad_clear = 1;
        G[R_V0] = 1;
        return 1;
    case 0x14: /* StopPAD */
    case 0x4C: /* StopCARD: stops the pads too */
        pad_stop(psx);
        G[R_V0] = 1;
        return 1;
    case 0x15: /* OutdatedPadInitAndStart(type, button_dest, ...): PAD_dr to it every vblank */
        if (a0 != 0x20000000u && a0 != 0x20000001u) {
            G[R_V0] = 0;
            return 1;
        }
        *kp(psx, V_PADDEST) = a1;
        psx->bios.pad_buf[0] = psx->bios.pad_buf[1] = 0;
        pad_start(psx);
        psx->bios.pad_clear = 1;
        G[R_V0] = 2;
        return 1;
    case 0x16: G[R_V0] = pad_dr(psx); return 1; /* OutdatedPadGetButtons */
    case 0x17: return_from_exception(psx); return 2;
    case 0x18: psx->bios.hook = 0; return 1;
    case 0x19: psx->bios.hook = a0; return 1;
    case 0x20: undeliver_event(psx, a0, a1); return 1;
    case 0x3A: case 0x3C: /* getc(fd), getchar: no input */
        G[R_V0] = 0xFFFFFFFFu;
        return 1;
    case 0x3B: /* putc(ch, fd) */
        if (a1 == 1) {
            char ch = (char)a0;
            tty(psx, &ch, 1);
        }
        G[R_V0] = a0;
        return 1;
    case 0x3D: { /* putchar */
        char ch = (char)a0;
        tty(psx, &ch, 1);
        return 1;
    }
    case 0x3E: /* gets */
        w8(psx, a0, 0);
        G[R_V0] = a0;
        return 1;
    case 0x3F: { /* puts */
        char buf[256];
        int n = rd_str(psx, a0, buf, sizeof buf - 1);
        tty(psx, buf, n);
        return 1;
    }
    case 0x40: { /* cd(name): paths are absolute here, the directory has only to exist */
        char name[64];
        u32 lba, size;
        rd_str(psx, a0, name, sizeof name);
        G[R_V0] = iso_find(psx, cd_path(name), &lba, &size) == 0;
        return 1;
    }
    case 0x42: { /* firstfile2(pattern, direntry) on the disc (card.c answers "bu") */
        char name[64];
        rd_str(psx, a0, name, sizeof name);
        G[R_V0] = cd_firstfile(psx, name, a1);
        return 1;
    }
    case 0x43: /* nextfile(direntry) */
        G[R_V0] = psx->bios.dirent_slot == FD_CD ? cd_next(psx, a0) : 0;
        return 1;
    case 0x44: case 0x46: /* rename, undelete: on the disc they fail (card.c answers "bu") */
        G[R_V0] = 0;
        return 1;
    case 0x47: G[R_V0] = 1; return 1; /* AddDrv */
    case 0x48: G[R_V0] = 1; return 1; /* DelDrv */
    case 0x49: case 0x59: return 1;   /* PrintInstalledDevices, testdevice */
    /* InitCard, StartCard, StopCard: the cards are images at the BIOS level (card.c) */
    case 0x4A:
        G[R_V0] = 1;
        return 1;
    case 0x51: case 0x53: /* Krom2RawAdd / Krom2Offset: no ROM, no font */
        G[R_V0] = 0xFFFFFFFFu;
        return 1;
    case 0x54: G[R_V0] = *kp(psx, V_ERRNO); return 1; /* GetLastError */
    case 0x55: /* GetLastFileError(fd) */
        G[R_V0] = a0 < 16 && (a0 <= 1 || psx->bios.fd[a0].used) ? 0 : 0xFFFFFFFFu;
        return 1;
    /* Function tables where the real BIOS keeps them. Code that patches the BIOS exception
     * handler (PsyQ libapi) compares its instructions first; this one holds none, so the
     * patch is skipped. */
    case 0x56: G[R_V0] = 0x00000674u; return 1; /* GetC0Table */
    case 0x57: G[R_V0] = 0x00000874u; return 1; /* GetB0Table */
    case 0x58: G[R_V0] = (u32)psx->bios.card_last_slot << 4; return 1; /* _card_chan */
    case 0x5B: /* ChangeClearPad */
        psx->bios.pad_clear = (int)a0;
        return 1;
    case 0x5C: case 0x5D: G[R_V0] = 1; return 1; /* _card_status / _card_wait: ready */
    case 0x11: case 0x24: case 0x25: case 0x26: case 0x27: case 0x28: case 0x29:
    case 0x2C: case 0x2D: case 0x2E: case 0x2F: case 0x30: case 0x31:
        return fatal(psx, "jump_to_00000000h", 0xB0, fn);
    case 0x1A: case 0x1B: case 0x1C: case 0x1D: case 0x1E: case 0x1F: case 0x21: case 0x22:
    case 0x23: case 0x2A: case 0x2B: case 0x52: case 0x5A:
        return fatal(psx, "SystemError", 0xB0, fn);
    }
    if (fn >= 0x32 && fn <= 0x39)
        return file_call(psx, fn - 0x32);
    return 0;
}

static int call_c(Psx *psx, u32 fn)
{
    u32 a0 = G[R_A0], a1 = G[R_A1];
    switch (fn) {
    case 0x00: /* EnqueueTimerAndVblankIrqs(priority): the root counter handlers move there */
        for (u32 n = 0; n < 4; n++) {
            u32 e = K_ENTRIES + 16 * n;
            for (u32 p = 0; p < 4; p++)
                deq_int(psx, p, e);
            enq_int(psx, a0, e);
        }
        return 1;
    case 0x02: enq_int(psx, a0, a1); G[R_V0] = 0; return 1;
    case 0x03: deq_int(psx, a0, a1); G[R_V0] = 0; return 1;
    case 0x04: { /* get_free_EvCB_slot */
        u32 *e = ev_at(psx, 0);
        G[R_V0] = 0xFFFFFFFFu;
        for (u32 i = 0; i < MAX_EVENTS; i++, e += EVW)
            if (e[1] == EV_FREE) {
                G[R_V0] = i;
                break;
            }
        return 1;
    }
    case 0x05: /* get_free_TCB_slot */
        G[R_V0] = 0xFFFFFFFFu;
        for (u32 i = 0; i < N_TCB; i++)
            if (*kp(psx, K_TCB + i * TCB_SIZE) != TH_USED) {
                G[R_V0] = i;
                break;
            }
        return 1;
    case 0x08: { /* SysInitMemory(addr, size): the kernel heap */
        u32 *v = kp(psx, V_KHEAP);
        heap_init(psx, v, v + 1, a0, a1);
        return 1;
    }
    case 0x0A: { /* ChangeClearRCnt(t, flag) */
        u32 t = a0 & 3, old = (u32)psx->bios.rcnt_clear[t];
        psx->bios.rcnt_clear[t] = (int)a1;
        G[R_V0] = old;
        return 1;
    }
    case 0x0B: return fatal(psx, "SystemError", 0xC0, fn);
    case 0x0D: { /* SetIrqAutoAck(irq, flag) */
        u32 *v = kp(psx, V_AUTOACK), bit = 1u << (a0 & 31);
        G[R_V0] = (*v & bit) != 0;
        *v = a1 ? *v | bit : *v & ~bit;
        return 1;
    }
    case 0x0E: case 0x0F: case 0x10: case 0x11: case 0x14: /* dev_sio_*: no SIO console */
        G[R_V0] = 0;
        return 1;
    case 0x01: case 0x06: case 0x07: case 0x09: case 0x0C: case 0x12: case 0x13: /* kernel set-up */
    case 0x15: case 0x16: case 0x17: case 0x18: case 0x1B: case 0x1C:         /* tty internals */
        return 1;
    case 0x19: { /* ioabort(txt1, txt2) */
        char t1[64], t2[64];
        rd_str(psx, a0, t1, sizeof t1);
        rd_str(psx, a1, t2, sizeof t2);
        psx_log(psx, "ioabort: %s %s", t1, t2);
        return fatal(psx, "ioabort", 0xC0, fn);
    }
    case 0x1A: *kp(psx, V_CARDFIND) = a0; return 1; /* set_card_find_mode */
    case 0x1D: G[R_V0] = *kp(psx, V_CARDFIND); return 1; /* get_card_find_mode */
    }
    return 0;
}

int bios_call(Psx *psx, u32 vector)
{
    u32 fn = G[R_T1] & 0xFF, ra = G[R_RA];
    u32 a0 = G[R_A0], a1 = G[R_A1], a2 = G[R_A2];
    int r = 0;
    if (vector == 0xA0) {
        if (fn <= 4 || fn >= 0xAB)
            r = card_call(psx, vector, fn);
        if (!r)
            r = call_a(psx, fn);
    } else if (vector == 0xB0) {
        if (fn - 0x32 <= 0x50 - 0x32)
            r = card_call(psx, vector, fn);
        if (!r)
            r = call_b(psx, fn);
    } else
        r = call_c(psx, fn);
    if ((psx->trace & 1) && r != 3)
        psx_log(psx, "%c0.%02X(%08X, %08X, %08X) = %08X ra %08X", 'A' + (int)(vector - 0xA0) / 16, fn, a0, a1, a2, G[R_V0], ra);
    if (!r)
        return 0;
    if (r == 1)
        psx->cpu.pc = ra;
    else if (r == 3) { /* the call did not complete: idle until the next device event, then retry */
        psx->cpu.pc = vector;
        psx->cycles = psx->next_event > psx->cycles ? psx->next_event : psx->cycles + 2;
    }
    return 1;
}

void bios_vblank(Psx *psx) { psx->bios.vsync_count++; }

void bios_event(Psx *psx, u32 cls, u32 spec) { deliver_event(psx, cls, spec); }

void bios_init(Psx *psx)
{
    Bios *b = &psx->bios;
    memset(b, 0, sizeof *b);
    for (int i = 0; i < 4; i++)
        b->rcnt_clear[i] = 1;
    b->rand_seed = 0x24040001u;
    /* kernel tables */
    w32(psx, 0x80000100u, K_EXCB);
    w32(psx, 0x80000104u, 4 * 8);
    w32(psx, 0x80000108u, K_PCB);
    w32(psx, 0x8000010Cu, 4);
    w32(psx, 0x80000110u, K_TCB);
    w32(psx, 0x80000114u, N_TCB * TCB_SIZE);
    w32(psx, 0x80000120u, K_EVCB);
    w32(psx, 0x80000124u, MAX_EVENTS * EVCB_SIZE);
    g_memset(psx, K_EXCB, 0, 0x800);
    g_memset(psx, K_VARS, 0, V_SIZE);
    /* Devices: libapi's firstfile looks its device up here by name before calling B(42) (and
     * points the entry's firstfile at its own fix, unused: the HLE answers B(42) itself). */
    static const struct { const char *name; u32 flags, sector; } dev[] = {
        {"cdrom", 0x14, 0x800}, {"bu", 0x14, 0x80}, {"tty", 0x01, 0x01}};
    u32 n_dev = sizeof dev / sizeof *dev, name = K_DCB + n_dev * DCB_SIZE;
    g_memset(psx, K_DCB, 0, n_dev * DCB_SIZE + 0x20);
    for (u32 i = 0; i < n_dev; i++) {
        u32 e = K_DCB + i * DCB_SIZE;
        w32(psx, e + 0, name), w32(psx, e + 4, dev[i].flags), w32(psx, e + 8, dev[i].sector);
        size_t len = strlen(dev[i].name) + 1;
        wr_bytes(psx, name, dev[i].name, (u32)len);
        name += (u32)len;
    }
    w32(psx, 0x80000150u, K_DCB);
    w32(psx, 0x80000154u, n_dev * DCB_SIZE);
    /* threads: TCB 0 runs, the others are free */
    w32(psx, K_PCB, K_TCB);
    for (u32 i = 0; i < N_TCB; i++)
        w32(psx, K_TCB + i * TCB_SIZE, i ? TH_FREE : TH_USED);
    /* jumps at the call vectors, for code that reads them */
    w32(psx, 0x800000A0u, 0x3C080000u); /* lui t0, 0 (the HLE intercepts the address itself) */
    w32(psx, 0x800000B0u, 0x3C080000u);
    w32(psx, 0x800000C0u, 0x3C080000u);
    w32(psx, 0x80000674u + 6 * 4, 0x00000C80u); /* C0 table: exception handler entry */
    /* the pad driver, see K_PADDRV: B0(5B) points at it, its entries jump to the magic ones */
    g_memset(psx, K_PADDRV, 0, 0x8A0);
    w32(psx, 0x80000874u + 0x5B * 4, K_PADDRV & 0x1FFFFFFFu);
    static const struct { u32 off, magic; } pad_entry[] = {{0x884, M_PAD_START}, {0x894, M_PAD_STOP}, {0x7A0, M_PAD_OUT}};
    for (u32 i = 0; i < sizeof pad_entry / sizeof *pad_entry; i++) {
        u32 e = K_PADDRV + pad_entry[i].off, m = pad_entry[i].magic;
        w32(psx, e, 0x3C080000u | m >> 16);              /* lui t0, hi */
        w32(psx, e + 4, 0x35080000u | (m & 0xFFFF));     /* ori t0, t0, lo */
        w32(psx, e + 8, 0x01000008u);                    /* jr t0 */
        w32(psx, e + 12, 0);                             /* nop */
    }
    for (u32 i = 0; i < 9; i++)
        w32(psx, K_PADDRV + 0x62C + 4 * i, PAD_ACK);
    /* root counter and vblank handlers, priority 1 */
    for (int n = 0; n < 4; n++) {
        u32 e = K_ENTRIES + 16 * (u32)n;
        w32(psx, e + 4, M_RCNT_H(n));
        w32(psx, e + 8, M_RCNT_V(n));
        enq_int(psx, 1, e);
    }
    heap_init(psx, &b->heap_start, &b->heap_end, 0x8000E000u, 0x1000);
    kheap(psx);
}
