/*
 * psxrt core: the PS1 BIOS, high level. Same model as Sony's kernel, so PsyQ libraries that poke
 * at it behave as on hardware:
 *  - an exception saves the registers into the current TCB (kernel RAM, found through [0x108]),
 *    runs the SysEnqIntRP priority chains (lists in guest RAM: +0 next, +4 func2, +8 func1), then
 *    leaves through the SetCustomExitFromException buffer (longjmp) or ReturnFromException;
 *  - events live in the EvCB array ([0x120]) with Sony's class/spec/mode/status values;
 *  - kernel handlers (root counters, pads) sit in those same chains, as entries whose functions
 *    are magic BIOS ROM addresses this file runs in C.
 * Guest code is called through psx->call (interpreter or translated code).
 */
#include "psx.h"

#include <string.h>

/* kernel RAM (below the game's 0x80010000) */
#define K_EXCB 0x80006000u  /* 4 priorities x 8 bytes: chain head, unused */
#define K_PCB 0x80006020u   /* [0] = current TCB */
#define K_TCB 0x80006030u   /* 4 TCBs of 0xC0 bytes */
#define TCB_SIZE 0xC0u
#define K_EVCB 0x80006400u  /* MAX_EVENTS x 0x1C */
#define EVCB_SIZE 0x1Cu
#define K_ENTRIES 0x80006800u /* kernel chain entries, 16 bytes each */
#define K_STACK 0x8000DF00u   /* stack for chain functions */
#define K_DCB 0x80007000u     /* device control blocks, 0x50 bytes each; names after them */
#define DCB_SIZE 0x50u
/* B0(5Bh) ChangeClearPad's "address": PsyQ libapi's _patch_pad takes the BIOS pad code from it,
 * EnablePAD at +0x884 and DisablePAD at +0x894 (called through pointers), and zeroes 11 words at
 * +0x594. Here both are returns and the zeroed words are free kernel RAM (0x7200-0x722B). */
#define K_PADFN (0x80007200u - 0x594u)

/* magic BIOS ROM addresses, see bios_rom_entry() */
#define M_RCNT_V(n) (0xBFC0FF20u + (n) * 8)
#define M_RCNT_H(n) (0xBFC0FF24u + (n) * 8)
#define M_PAD_V 0xBFC0FF40u
#define M_PAD_H 0xBFC0FF44u

enum { EV_FREE = 0, EV_DISABLED = 0x1000, EV_ENABLED = 0x2000, EV_READY = 0x4000 };
enum { EVMODE_CALLBACK = 0x1000, EVMODE_READY = 0x2000 };

#define G (psx->cpu.gpr)
enum { R_V0 = 2, R_V1, R_A0, R_A1, R_A2, R_A3, R_T0, R_T1, R_S0 = 16, R_GP = 28, R_SP, R_FP, R_RA };

static u32 r32(Psx *psx, u32 a) { return psx_read(psx, a, 4); }
static void w32(Psx *psx, u32 a, u32 v) { psx_write(psx, a, v, 4); }
static u8 r8(Psx *psx, u32 a) { return (u8)psx_read(psx, a, 1); }
static void w8(Psx *psx, u32 a, u8 v) { psx_write(psx, a, v, 1); }

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

static void tty(Psx *psx, const char *s, int n)
{
    if (psx->host->tty && n > 0)
        psx->host->tty(psx->host->user, s, n);
}

/* ---- calls into guest code ---- */

static void k_rcnt_v(Psx *psx, int n);
static void k_rcnt_h(Psx *psx, int n);
static void k_pad_v(Psx *psx);
static void k_pad_h(Psx *psx);

/* Runs the function at `addr` (guest code or a kernel handler) as a call. */
static void invoke(Psx *psx, u32 addr)
{
    for (int n = 0; n < 4; n++) {
        if (addr == M_RCNT_V(n)) {
            k_rcnt_v(psx, n);
            return;
        }
        if (addr == M_RCNT_H(n)) {
            k_rcnt_h(psx, n);
            return;
        }
    }
    if (addr == M_PAD_V) {
        k_pad_v(psx);
        return;
    }
    if (addr == M_PAD_H) {
        k_pad_h(psx);
        return;
    }
    psx->call(psx, addr);
}

/* ---- events ---- */

static u32 ev_addr(int i) { return K_EVCB + (u32)i * EVCB_SIZE; }

static int ev_index(Psx *psx, u32 h)
{
    u32 i = h & 0xFFFF;
    if ((h & 0xFFFF0000u) != 0xF1000000u || i >= MAX_EVENTS)
        return -1;
    (void)psx;
    return (int)i;
}

static u32 open_event(Psx *psx, u32 cls, u32 spec, u32 mode, u32 func)
{
    for (int i = 0; i < MAX_EVENTS; i++) {
        u32 e = ev_addr(i);
        if (r32(psx, e + 4) == EV_FREE) {
            w32(psx, e + 0, cls);
            w32(psx, e + 4, EV_DISABLED);
            w32(psx, e + 8, spec);
            w32(psx, e + 0xC, mode);
            w32(psx, e + 0x10, func);
            return 0xF1000000u | (u32)i;
        }
    }
    psx_log(psx, "OpenEvent: no free event for %08X/%04X", cls, spec);
    return 0xFFFFFFFFu;
}

static void deliver_event(Psx *psx, u32 cls, u32 spec)
{
    for (int i = 0; i < MAX_EVENTS; i++) {
        u32 e = ev_addr(i);
        if (r32(psx, e + 4) != EV_ENABLED || r32(psx, e) != cls || r32(psx, e + 8) != spec)
            continue;
        u32 mode = r32(psx, e + 0xC), func = r32(psx, e + 0x10);
        if (mode == EVMODE_READY)
            w32(psx, e + 4, EV_READY);
        else if (mode == EVMODE_CALLBACK && func) {
            Cpu save = psx->cpu;
            invoke(psx, func);
            psx->cpu = save;
        }
    }
}

static void undeliver_event(Psx *psx, u32 cls, u32 spec)
{
    for (int i = 0; i < MAX_EVENTS; i++) {
        u32 e = ev_addr(i);
        if (r32(psx, e + 4) == EV_READY && r32(psx, e) == cls && r32(psx, e + 8) == spec && r32(psx, e + 0xC) == EVMODE_READY)
            w32(psx, e + 4, EV_ENABLED);
    }
}

/* ---- exceptions ---- */

static u32 tcb(Psx *psx) { return r32(psx, K_PCB); }

static void save_tcb(Psx *psx)
{
    u32 t = tcb(psx);
    u8 *p = ram_span(psx, t, TCB_SIZE);
    if (!p)
        return;
    for (int i = 0; i < 32; i++)
        w32(psx, t + 8 + 4 * (u32)i, G[i]);
    w32(psx, t + 0x88, psx->cpu.epc);
    w32(psx, t + 0x8C, psx->cpu.hi);
    w32(psx, t + 0x90, psx->cpu.lo);
    w32(psx, t + 0x94, psx->cpu.sr);
    w32(psx, t + 0x98, psx->cpu.cause);
}

/* ReturnFromException: registers and SR from the TCB, rfe, continue at the saved EPC. */
static void return_from_exception(Psx *psx)
{
    u32 t = tcb(psx);
    for (int i = 1; i < 32; i++)
        G[i] = r32(psx, t + 8 + 4 * (u32)i);
    G[0] = 0;
    psx->cpu.hi = r32(psx, t + 0x8C);
    psx->cpu.lo = r32(psx, t + 0x90);
    psx->cpu.sr = r32(psx, t + 0x94);
    cpu_rfe(psx);
    psx->cpu.pc = r32(psx, t + 0x88);
    psx->bios.in_exception = 0;
    psx->redirect = 1;
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
        u32 node = r32(psx, K_EXCB + 8 * (u32)prio);
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
    if (code == 8) { /* syscall: critical sections */
        switch (G[R_A0]) {
        case 1: /* EnterCriticalSection: IEp and IM2 off, returns whether they were on */
            G[R_V0] = (c->sr & 0x404) == 0x404;
            c->sr &= ~0x404u;
            break;
        case 2: /* ExitCriticalSection */
            c->sr |= 0x404;
            G[R_V0] = 0;
            break;
        case 3: /* ChangeThreadSubFunction */
            w32(psx, K_PCB, G[R_A1]);
            G[R_V0] = 1;
            break;
        default:
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
    save_tcb(psx);
    run_chains(psx);
    if (psx->bios.hook)
        longjmp_to(psx, psx->bios.hook, 1);
    else
        return_from_exception(psx);
}

static void enq_int(Psx *psx, u32 prio, u32 entry)
{
    u32 head = K_EXCB + 8 * (prio & 3);
    w32(psx, entry, r32(psx, head));
    w32(psx, head, entry);
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

static const u32 rcnt_irq[4] = {1u << IRQ_TMR0, 1u << IRQ_TMR1, 1u << IRQ_TMR2, 1u << IRQ_VBLANK};

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

static void k_pad_v(Psx *psx) { G[R_V0] = psx->i_stat & psx->i_mask & 1u; }

static void k_pad_h(Psx *psx)
{
    pad_word(psx, psx->bios.pad_buf[0], 0);
    pad_word(psx, psx->bios.pad_buf[1], 1);
    if (psx->bios.pad_clear)
        psx->i_stat &= ~1u;
    G[R_V0] = 0;
}

int bios_rom_entry(Psx *psx, u32 pc)
{
    if (pc == BIOS_EXEC_RET) { /* an Exec'd program returned */
        u32 h = psx->bios.exec_hdr;
        G[R_SP] = r32(psx, h + 0x28);
        G[R_FP] = r32(psx, h + 0x2C);
        G[R_GP] = r32(psx, h + 0x30);
        G[R_RA] = r32(psx, h + 0x34);
        G[R_V0] = 1;
        psx->cpu.pc = G[R_RA];
        return 1;
    }
    for (u32 a = M_RCNT_V(0); a <= M_PAD_H; a += 4)
        if (pc == a) {
            invoke(psx, pc);
            psx->cpu.pc = G[R_RA];
            return 1;
        }
    return 0;
}

/* ---- heap (InitHeap/malloc/free): first fit, 4-byte headers: size | used ---- */

static void heap_init(Psx *psx, u32 start, u32 size)
{
    start = (start + 3) & ~3u;
    size &= ~3u;
    psx->bios.heap_start = start;
    psx->bios.heap_end = start + size;
    if (size >= 8)
        w32(psx, start, size - 4);
}

static void heap_merge(Psx *psx)
{
    Bios *b = &psx->bios;
    for (u32 p = b->heap_start; p + 4 <= b->heap_end;) {
        u32 h = r32(psx, p), sz = h & ~3u;
        if (!(h & 1)) {
            u32 q = p + 4 + sz;
            while (q + 4 <= b->heap_end && !(r32(psx, q) & 1)) {
                sz += 4 + (r32(psx, q) & ~3u);
                q = p + 4 + sz;
            }
            w32(psx, p, sz);
        }
        p += 4 + sz;
    }
}

static u32 heap_alloc(Psx *psx, u32 n)
{
    Bios *b = &psx->bios;
    n = (n + 3) & ~3u;
    for (int pass = 0; pass < 2; pass++) {
        for (u32 p = b->heap_start; p + 4 <= b->heap_end;) {
            u32 h = r32(psx, p), sz = h & ~3u;
            if (!(h & 1) && sz >= n) {
                if (sz - n >= 8) {
                    w32(psx, p + 4 + n, sz - n - 4);
                    sz = n;
                }
                w32(psx, p, sz | 1);
                return p + 4;
            }
            p += 4 + sz;
        }
        heap_merge(psx);
    }
    psx_log(psx, "malloc(%u) failed", n);
    return 0;
}

static void heap_free(Psx *psx, u32 a)
{
    if (a >= psx->bios.heap_start + 4 && a < psx->bios.heap_end)
        w32(psx, a - 4, r32(psx, a - 4) & ~1u);
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

/* ---- ISO9660 on the disc ---- */

static u8 iso_buf[2352];

static const u8 *iso_sector(Psx *psx, u32 lba)
{
    if (psx->host->read_sector(psx->host->user, lba + 150, iso_buf) < 0)
        return 0;
    return iso_buf + 24; /* mode 2 form 1 user data */
}

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
    for (int i = 0; i < rn; i++) {
        char a = (char)rec_name[i], b = want[i];
        if (a >= 'a' && a <= 'z') a -= 32;
        if (b >= 'a' && b <= 'z') b -= 32;
        if (a != b)
            return 0;
    }
    return 1;
}

int iso_find(Psx *psx, const char *path, u32 *lba, u32 *size)
{
    const u8 *s = iso_sector(psx, 16);
    if (!s || memcmp(s + 1, "CD001", 5))
        return -1;
    const u8 *root = s + 156;
    u32 dir = root[2] | root[3] << 8 | root[4] << 16 | (u32)root[5] << 24;
    u32 dsize = root[10] | root[11] << 8 | root[12] << 16 | (u32)root[13] << 24;
    while (*path == '\\' || *path == '/')
        path++;
    for (;;) {
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
                    dir = r[2] | r[3] << 8 | r[4] << 16 | (u32)r[5] << 24;
                    dsize = r[10] | r[11] << 8 | r[12] << 16 | (u32)r[13] << 24;
                    found = 1;
                    break;
                }
                p += r[0];
            }
        }
        if (!found)
            return -1;
        if (!*end) {
            *lba = dir;
            *size = dsize;
            return 0;
        }
        path = end + 1;
    }
}

/* Reads a whole file to guest memory at `dst`. Returns its size, or -1. */
static int iso_load(Psx *psx, u32 lba, u32 size, u32 dst)
{
    for (u32 done = 0; done < size; done += 2048) {
        const u8 *d = iso_sector(psx, lba + done / 2048);
        if (!d)
            return -1;
        u32 n = size - done < 2048 ? size - done : 2048;
        u8 *m = ram_span(psx, dst + done, n);
        if (m)
            memcpy(m, d, n);
        else
            for (u32 i = 0; i < n; i++)
                w8(psx, dst + done + i, d[i]);
    }
    if (psx->ram_loaded)
        psx->ram_loaded(psx, dst, size);
    return (int)size;
}

/* strips "cdrom:" and a leading backslash */
static const char *cd_path(const char *name)
{
    const char *p = strchr(name, ':');
    return p ? p + 1 : name;
}

/* Loads a PS-X EXE: the 2 KB header's exec struct goes to header_dst, the text to t_addr. */
int bios_load_file(Psx *psx, const char *name, u32 header_dst)
{
    u32 lba, size;
    if (iso_find(psx, cd_path(name), &lba, &size) < 0) {
        psx_log(psx, "load: %s not found", name);
        return -1;
    }
    const u8 *h = iso_sector(psx, lba);
    if (!h || memcmp(h, "PS-X EXE", 8)) {
        psx_log(psx, "load: %s is not a PS-X EXE", name);
        return -1;
    }
    for (u32 i = 0; i < 0x3C; i += 4)
        w32(psx, header_dst + i, h[0x10 + i] | h[0x11 + i] << 8 | h[0x12 + i] << 16 | (u32)h[0x13 + i] << 24);
    u32 t_addr = r32(psx, header_dst + 8), t_size = r32(psx, header_dst + 0xC);
    if (iso_load(psx, lba + 1, t_size, t_addr) < 0)
        return -1;
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

int bios_exec(Psx *psx, u32 h, u32 sp, u32 sp_size)
{
    u32 b_addr = r32(psx, h + 0x18), b_size = r32(psx, h + 0x1C);
    for (u32 i = 0; i < b_size; i += 4)
        w32(psx, b_addr + i, 0);
    w32(psx, h + 0x28, G[R_SP]);
    w32(psx, h + 0x2C, G[R_FP]);
    w32(psx, h + 0x30, G[R_GP]);
    w32(psx, h + 0x34, G[R_RA]);
    w32(psx, h + 0x38, G[R_S0]);
    if (sp) {
        G[R_SP] = sp + sp_size;
        G[R_FP] = G[R_SP];
    }
    G[R_GP] = r32(psx, h + 4);
    G[R_RA] = BIOS_EXEC_RET;
    G[R_A0] = 1;
    G[R_A1] = 0;
    psx->bios.exec_hdr = h;
    psx->cpu.pc = r32(psx, h);
    psx->redirect = 1;
    psx_log(psx, "Exec %s pc %08X sp %08X", psx->exe_name, psx->cpu.pc, G[R_SP]);
    return 0;
}

/* ---- memory helpers ---- */

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

static int g_strncmp(Psx *psx, u32 a, u32 b, u32 n)
{
    for (u32 i = 0; i < n; i++) {
        u8 x = r8(psx, a + i), y = r8(psx, b + i);
        if (x != y)
            return x - y;
        if (!x)
            return 0;
    }
    return 0;
}

/* ---- the call tables ---- */

static int file_call(Psx *psx, u32 fn)
{
    switch (fn) {
    case 0x00: { /* open */
        char name[64];
        rd_str(psx, G[R_A0], name, sizeof name);
        psx_log(psx, "open(%s, %X): unsupported", name, G[R_A1]);
        G[R_V0] = 0xFFFFFFFFu;
        return 1;
    }
    case 0x03: /* write */
        if (G[R_A0] == 1) {
            char buf[256];
            u32 n = G[R_A2] < sizeof buf ? G[R_A2] : sizeof buf;
            for (u32 i = 0; i < n; i++)
                buf[i] = (char)r8(psx, G[R_A1] + i);
            tty(psx, buf, (int)n);
            G[R_V0] = G[R_A2];
        } else
            G[R_V0] = 0xFFFFFFFFu;
        return 1;
    case 0x01: case 0x02: case 0x04: case 0x05:
        G[R_V0] = 0xFFFFFFFFu;
        return 1;
    case 0x06: /* exit */
        psx_log(psx, "exit(%d)", (s32)G[R_A0]);
        psx->exiting = 1;
        return 1;
    case 0x07: /* isatty */
        G[R_V0] = G[R_A0] <= 1;
        return 1;
    }
    return 0;
}

static int call_a(Psx *psx, u32 fn)
{
    u32 a0 = G[R_A0], a1 = G[R_A1], a2 = G[R_A2];
    switch (fn) {
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
    case 0x15: { /* strcat */
        u32 d = a0;
        while (r8(psx, d))
            d++;
        u8 ch;
        do {
            ch = r8(psx, a1++);
            w8(psx, d++, ch);
        } while (ch);
        G[R_V0] = a0;
        return 1;
    }
    case 0x17: G[R_V0] = (u32)g_strncmp(psx, a0, a1, 0xFFFFFFFFu); return 1;
    case 0x18: G[R_V0] = (u32)g_strncmp(psx, a0, a1, a2); return 1;
    case 0x19: { /* strcpy */
        u32 i = 0;
        u8 ch;
        do {
            ch = r8(psx, a1 + i);
            w8(psx, a0 + i++, ch);
        } while (ch);
        G[R_V0] = a0;
        return 1;
    }
    case 0x1A: { /* strncpy */
        u32 i = 0;
        for (; i < a2; i++) {
            u8 ch = r8(psx, a1 + i);
            w8(psx, a0 + i, ch);
            if (!ch)
                break;
        }
        for (; i < a2; i++)
            w8(psx, a0 + i, 0);
        G[R_V0] = a0;
        return 1;
    }
    case 0x1B: { /* strlen */
        u32 n = 0;
        while (r8(psx, a0 + n))
            n++;
        G[R_V0] = n;
        return 1;
    }
    case 0x1E: case 0x1C: { /* strchr / index */
        for (u32 p = a0;; p++) {
            u8 ch = r8(psx, p);
            if (ch == (u8)a1) {
                G[R_V0] = p;
                return 1;
            }
            if (!ch)
                break;
        }
        G[R_V0] = 0;
        return 1;
    }
    case 0x25: G[R_V0] = (a0 >= 'a' && a0 <= 'z') ? a0 - 32 : a0; return 1;
    case 0x26: G[R_V0] = (a0 >= 'A' && a0 <= 'Z') ? a0 + 32 : a0; return 1;
    case 0x27: g_memcpy(psx, a1, a0, a2); G[R_V0] = a1; return 1; /* bcopy */
    case 0x28: g_memset(psx, a0, 0, a1); G[R_V0] = a0; return 1; /* bzero */
    case 0x29: case 0x2D: { /* bcmp / memcmp */
        s32 r = 0;
        for (u32 i = 0; i < a2 && !r; i++)
            r = (s32)r8(psx, a0 + i) - (s32)r8(psx, a1 + i);
        G[R_V0] = (u32)r;
        return 1;
    }
    case 0x2A: g_memcpy(psx, a0, a1, a2); G[R_V0] = a0; return 1;
    case 0x2B: g_memset(psx, a0, (u8)a1, a2); G[R_V0] = a0; return 1;
    case 0x2C: g_memcpy(psx, a0, a1, a2); G[R_V0] = a0; return 1;
    case 0x2F: /* rand */
        psx->bios.rand_seed = psx->bios.rand_seed * 1103515245u + 12345u;
        G[R_V0] = (psx->bios.rand_seed >> 16) & 0x7FFF;
        return 1;
    case 0x30: psx->bios.rand_seed = a0; return 1;
    case 0x33: G[R_V0] = heap_alloc(psx, a0); return 1;
    case 0x34: heap_free(psx, a0); return 1;
    case 0x37: { /* calloc */
        u32 n = a0 * a1, p = heap_alloc(psx, n);
        if (p)
            g_memset(psx, p, 0, n);
        G[R_V0] = p;
        return 1;
    }
    case 0x38: { /* realloc */
        if (!a0) {
            G[R_V0] = heap_alloc(psx, a1);
            return 1;
        }
        u32 old = r32(psx, a0 - 4) & ~3u, p = heap_alloc(psx, a1);
        if (p) {
            g_memcpy(psx, p, a0, old < a1 ? old : a1);
            heap_free(psx, a0);
        }
        G[R_V0] = p;
        return 1;
    }
    case 0x39: heap_init(psx, a0, a1); return 1;
    case 0x3A: psx_log(psx, "_exit(%d)", (s32)a0); psx->exiting = 1; return 1;
    case 0x3C: { /* putchar */
        char ch = (char)a0;
        tty(psx, &ch, 1);
        G[R_V0] = a0;
        return 1;
    }
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
    case 0x43: /* Exec */
        bios_exec(psx, a0, a1, a2);
        return 2;
    case 0x44: /* FlushCache: code stores wrote may run from here */
        if (psx->ram_loaded)
            psx->ram_loaded(psx, 0, 0);
        return 1;
    case 0x49: /* GPU_cw */
        gpu_gp0(psx, a0);
        return 1;
    case 0x72: /* _96_remove: the BIOS CD handlers are not installed */
    case 0x71: /* _96_init */
        return 1;
    case 0x70: /* _bu_init */
        return 1;
    }
    if (fn <= 0x07)
        return file_call(psx, fn);
    return 0;
}

static int call_b(Psx *psx, u32 fn)
{
    u32 a0 = G[R_A0], a1 = G[R_A1], a2 = G[R_A2], a3 = G[R_A3];
    switch (fn) {
    case 0x07: deliver_event(psx, a0, a1); return 1;
    case 0x08: G[R_V0] = open_event(psx, a0, a1, a2, a3); return 1;
    case 0x09: { /* CloseEvent */
        int i = ev_index(psx, a0);
        if (i >= 0)
            w32(psx, ev_addr(i) + 4, EV_FREE);
        G[R_V0] = 1;
        return 1;
    }
    case 0x0A: { /* WaitEvent: spins (taking interrupts) until the event is ready */
        int i = ev_index(psx, a0);
        u32 st = i >= 0 ? r32(psx, ev_addr(i) + 4) : 0;
        if (st == EV_READY) {
            w32(psx, ev_addr(i) + 4, EV_ENABLED);
            G[R_V0] = 1;
            return 1;
        }
        if (st != EV_ENABLED) {
            G[R_V0] = 0;
            return 1;
        }
        return 3; /* busy: retry the call */
    }
    case 0x0B: { /* TestEvent */
        int i = ev_index(psx, a0);
        if (i >= 0 && r32(psx, ev_addr(i) + 4) == EV_READY) {
            w32(psx, ev_addr(i) + 4, EV_ENABLED);
            G[R_V0] = 1;
        } else
            G[R_V0] = 0;
        return 1;
    }
    case 0x0C: case 0x0D: { /* EnableEvent / DisableEvent */
        int i = ev_index(psx, a0);
        if (i >= 0 && r32(psx, ev_addr(i) + 4) != EV_FREE)
            w32(psx, ev_addr(i) + 4, fn == 0x0C ? EV_ENABLED : EV_DISABLED);
        G[R_V0] = 1;
        return 1;
    }
    case 0x12: /* InitPAD(buf1, siz1, buf2, siz2) */
        psx->bios.pad_buf[0] = a0;
        psx->bios.pad_buf[1] = a2;
        G[R_V0] = 2;
        return 1;
    case 0x13: /* StartPAD */
    case 0x4B: /* StartCARD: the same PadCard handler, which reads the pads InitPAD set up */
        if (!psx->bios.pad_started) {
            w32(psx, K_ENTRIES + 4 * 16 + 4, M_PAD_H);
            w32(psx, K_ENTRIES + 4 * 16 + 8, M_PAD_V);
            enq_int(psx, 2, K_ENTRIES + 4 * 16);
            psx->bios.pad_started = 1;
        }
        psx->i_mask |= 1;
        psx->bios.pad_clear = 1;
        G[R_V0] = 1;
        return 1;
    case 0x14: /* StopPAD */
    case 0x4C: /* StopCARD: stops the pads too */
        if (psx->bios.pad_started) {
            deq_int(psx, 2, K_ENTRIES + 4 * 16);
            psx->bios.pad_started = 0;
        }
        G[R_V0] = 1;
        return 1;
    case 0x15: /* OutdatedPadInitAndStart(type, dst, ...) */
        psx_log(psx, "OutdatedPadInitAndStart(%X, %08X)", a0, a1);
        G[R_V0] = 2;
        return 1;
    case 0x17: return_from_exception(psx); return 2;
    case 0x18: psx->bios.hook = 0; return 1;
    case 0x19: psx->bios.hook = a0; return 1;
    case 0x20: undeliver_event(psx, a0, a1); return 1;
    case 0x5B: /* ChangeClearPad */
        psx->bios.pad_clear = (int)a0;
        return 1;
    /* InitCard: the cards are images at the BIOS level (card.c) */
    case 0x4A:
        G[R_V0] = 1;
        return 1;
    /* Function tables where the real BIOS keeps them. Code that patches the BIOS exception
     * handler (PsyQ libapi) compares its instructions first; this one holds none, so the
     * patch is skipped. */
    case 0x56: G[R_V0] = 0x00000674u; return 1; /* GetC0Table */
    case 0x57: G[R_V0] = 0x00000874u; return 1; /* GetB0Table */
    }
    if (fn >= 0x32 && fn <= 0x39)
        return file_call(psx, fn - 0x32);
    if (fn == 0x3D) { /* putchar */
        char ch = (char)a0;
        tty(psx, &ch, 1);
        return 1;
    }
    if (fn == 0x3F) { /* puts */
        char buf[256];
        int n = rd_str(psx, a0, buf, sizeof buf - 1);
        tty(psx, buf, n);
        return 1;
    }
    return 0;
}

static int call_c(Psx *psx, u32 fn)
{
    u32 a0 = G[R_A0], a1 = G[R_A1];
    switch (fn) {
    case 0x02: enq_int(psx, a0, a1); G[R_V0] = 0; return 1;
    case 0x03: deq_int(psx, a0, a1); G[R_V0] = 0; return 1;
    case 0x0A: { /* ChangeClearRCnt(t, flag) */
        u32 t = a0 & 3, old = (u32)psx->bios.rcnt_clear[t];
        psx->bios.rcnt_clear[t] = (int)a1;
        G[R_V0] = old;
        return 1;
    }
    }
    return 0;
}

int bios_call(Psx *psx, u32 vector)
{
    u32 fn = G[R_T1] & 0xFF, ra = G[R_RA];
    u32 a0 = G[R_A0], a1 = G[R_A0 + 1], a2 = G[R_A0 + 2];
    int r = card_call(psx, vector, fn);
    if (!r)
        r = vector == 0xA0 ? call_a(psx, fn) : vector == 0xB0 ? call_b(psx, fn) : call_c(psx, fn);
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
    w32(psx, 0x80000114u, 4 * TCB_SIZE);
    w32(psx, 0x80000120u, K_EVCB);
    w32(psx, 0x80000124u, MAX_EVENTS * EVCB_SIZE);
    g_memset(psx, K_EXCB, 0, 0x800);
    /* Devices: libapi's firstfile looks its device up here by name before calling B(42) (and
     * points the entry's firstfile at its own fix, unused: the HLE answers B(42) itself). */
    static const struct { const char *name; u32 flags, sector; } dev[] = {
        {"cdrom", 0x14, 0x800}, {"bu", 0x14, 0x80}, {"tty", 0x01, 0x01}};
    u32 n_dev = sizeof dev / sizeof *dev, name = K_DCB + n_dev * DCB_SIZE;
    g_memset(psx, K_DCB, 0, n_dev * DCB_SIZE + 0x20);
    for (u32 i = 0; i < n_dev; i++) {
        u32 e = K_DCB + i * DCB_SIZE;
        w32(psx, e + 0, name), w32(psx, e + 4, dev[i].flags), w32(psx, e + 8, dev[i].sector);
        for (const char *s = dev[i].name;; s++, name++) {
            psx_write(psx, name, (u8)*s, 1);
            if (!*s)
                break;
        }
        name++;
    }
    w32(psx, 0x80000150u, K_DCB);
    w32(psx, 0x80000154u, n_dev * DCB_SIZE);
    w32(psx, K_PCB, K_TCB);
    w32(psx, K_TCB, 0x4000); /* TCB 0 in use */
    /* jumps at the call vectors, for code that reads them */
    w32(psx, 0x800000A0u, 0x3C080000u); /* lui t0, 0 (the HLE intercepts the address itself) */
    w32(psx, 0x800000B0u, 0x3C080000u);
    w32(psx, 0x800000C0u, 0x3C080000u);
    w32(psx, 0x80000674u + 6 * 4, 0x00000C80u); /* C0 table: exception handler entry */
    w32(psx, 0x80000874u + 0x5B * 4, K_PADFN);  /* B0 table: ChangeClearPad, for _patch_pad */
    g_memset(psx, K_PADFN + 0x594u, 0, 0x884u - 0x594u + 0x18u);
    w32(psx, K_PADFN + 0x884u, 0x03E00008u); /* EnablePAD: jr ra */
    w32(psx, K_PADFN + 0x894u, 0x03E00008u); /* DisablePAD: jr ra */
    /* root counter and vblank handlers, priority 1 */
    for (int n = 0; n < 4; n++) {
        u32 e = K_ENTRIES + 16 * (u32)n;
        w32(psx, e + 4, M_RCNT_H(n));
        w32(psx, e + 8, M_RCNT_V(n));
        enq_int(psx, 1, e);
    }
    heap_init(psx, 0x8000E000u, 0x1000);
}
