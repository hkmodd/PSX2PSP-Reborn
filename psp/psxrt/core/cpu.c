/*
 * psxrt core: R3000A interpreter with load delays, branch delays and precise exceptions.
 * The PC simulator runs whole games on it; the PSP runtime runs on it only the code the
 * translation lacks. Interrupts are taken at instruction boundaries and handed to the BIOS
 * (HLE) instead of the vector at 0x80000080.
 */
#include "psx.h"

#define R(n) c->gpr[n]

static void jump_to(Cpu *c, u32 pc)
{
    c->pc = pc;
    c->next_pc = pc + 4;
    c->in_delay = 0;
}

void cpu_rfe(Psx *psx)
{
    Cpu *c = &psx->cpu;
    c->sr = (c->sr & ~0xFu) | ((c->sr >> 2) & 0xF);
}

/* Enters exception `code` for the instruction at `pc`. The BIOS (HLE) decides what runs next. */
void cpu_exception(Psx *psx, u32 code, u32 pc, int in_delay)
{
    Cpu *c = &psx->cpu;
    if (c->load_reg) { /* the load in flight completes */
        c->gpr[c->load_reg] = c->load_val;
        c->load_reg = 0;
    }
    c->epc = in_delay ? pc - 4 : pc;
    c->cause = (c->cause & ~0x8000007Cu) | (code << 2) | (in_delay ? 0x80000000u : 0);
    c->sr = (c->sr & ~0x3Fu) | ((c->sr << 2) & 0x3F);
    jump_to(c, 0x80000080);
    bios_exception(psx);
    c->next_pc = c->pc + 4;
}

int cpu_cop0_read(Psx *psx, int r, u32 *v)
{
    Cpu *c = &psx->cpu;
    switch (r) {
    case 8: *v = c->badvaddr; return 0;
    case 12: *v = c->sr; return 0;
    case 13:
        *v = (c->cause & ~0x400u) | ((psx->i_stat & psx->i_mask) ? 0x400u : 0);
        return 0;
    case 14: *v = c->epc; return 0;
    case 15: *v = 2; return 0;
    case 3: case 5: case 6: case 7: case 9: case 11:
        *v = 0;
        return 0;
    }
    *v = 0;
    return 0;
}

void cpu_cop0_write(Psx *psx, int r, u32 v)
{
    Cpu *c = &psx->cpu;
    switch (r) {
    case 12: c->sr = v; break;
    case 13: c->cause = (c->cause & ~0x300u) | (v & 0x300u); break;
    case 14: c->epc = v; break;
    }
}

int psx_irq_pending(const Psx *psx)
{
    const Cpu *c = &psx->cpu;
    if (!(c->sr & 1))
        return 0;
    u32 ip = (c->cause & 0x300u) | ((psx->i_stat & psx->i_mask) ? 0x400u : 0);
    return (ip & c->sr & 0x700u) != 0;
}

static inline u32 load(Psx *psx, u32 a, int size)
{
    u32 p = a & 0x1FFFFFFF;
    if (p < 0x800000) {
        const u8 *m = psx->ram + (p & (RAM_SIZE - 1));
        return size == 1 ? m[0] : size == 2 ? (u32)(m[0] | m[1] << 8) : m[0] | m[1] << 8 | m[2] << 16 | (u32)m[3] << 24;
    }
    return psx_read(psx, a, size);
}

/* Executes one instruction. Returns 0, or -1 when the CPU cannot continue (logged). */
int cpu_step(Psx *psx)
{
    Cpu *c = &psx->cpu;
    u32 pc = c->pc;
    int delay = c->in_delay;

    if (psx_irq_pending(psx)) {
        cpu_exception(psx, 0, pc, delay);
        return 0;
    }

    if ((pc & 0x1FFFFFFF) < 0x200 || (pc & 0x1FF00000) == 0x1FC00000) {
        u32 p = pc & 0x1FFFFFFF;
        if (p == 0xA0 || p == 0xB0 || p == 0xC0) {
            if (c->load_reg) {
                c->gpr[c->load_reg] = c->load_val;
                c->load_reg = 0;
            }
            if (!bios_call(psx, p)) {
                psx_log(psx, "BIOS %02X(%02X) not implemented, ra %08X", p, c->gpr[9] & 0xFF, c->gpr[31]);
                jump_to(c, c->gpr[31]);
            }
            c->next_pc = c->pc + 4;
            return 0;
        }
        if ((p & 0x1FF00000) == 0x1FC00000 && bios_rom_entry(psx, pc)) {
            c->next_pc = c->pc + 4;
            return 0;
        }
        if (pc == 0x80000080 || p == 0x80) {
            psx_log(psx, "jump to the exception vector, ra %08X", c->gpr[31]);
            return -1;
        }
    }

    if (pc & 3) {
        c->badvaddr = pc;
        cpu_exception(psx, 4, pc, delay);
        return 0;
    }
    u32 in = load(psx, pc, 4);

    int pr = c->load_reg;
    u32 pv = c->load_val;
    c->load_reg = 0;
    c->in_delay = 0;
    c->pc = c->next_pc;
    c->next_pc += 4;
    psx->cycles += 2;

    u32 op = in >> 26, rs = (in >> 21) & 31, rt = (in >> 16) & 31, rd = (in >> 11) & 31;
    u32 sa = (in >> 6) & 31, fn = in & 63;
    u32 imm = in & 0xFFFF;
    u32 simm = (u32)(s32)(s16)imm;
    u32 s = R(rs), t = R(rt);

#define WR(r, v) do { u32 _v = (v); if (r) { c->gpr[r] = _v; if ((int)(r) == pr) pr = 0; } } while (0)
#define LD(r, v) do { u32 _v = (v); if ((int)(r) == pr) pr = 0; if (r) { c->load_reg = (r); c->load_val = _v; } } while (0)
#define BRANCH(cond) do { if (cond) c->next_pc = pc + 4 + (simm << 2); c->in_delay = 1; } while (0)
#define EXC(code) do { c->pc = pc; c->next_pc = pc + 4; c->in_delay = delay; if (pr) c->gpr[pr] = pv; cpu_exception(psx, code, pc, delay); return 0; } while (0)

    switch (op) {
    case 0x00:
        switch (fn) {
        case 0x00: WR(rd, t << sa); break;
        case 0x02: WR(rd, t >> sa); break;
        case 0x03: WR(rd, (u32)((s32)t >> sa)); break;
        case 0x04: WR(rd, t << (s & 31)); break;
        case 0x06: WR(rd, t >> (s & 31)); break;
        case 0x07: WR(rd, (u32)((s32)t >> (s & 31))); break;
        case 0x08:
            c->next_pc = s;
            c->in_delay = 1;
            break;
        case 0x09:
            WR(rd, pc + 8);
            c->next_pc = s;
            c->in_delay = 1;
            break;
        case 0x0C: EXC(8);
        case 0x0D: EXC(9);
        case 0x10: WR(rd, c->hi); break;
        case 0x11: c->hi = s; break;
        case 0x12: WR(rd, c->lo); break;
        case 0x13: c->lo = s; break;
        case 0x18: {
            s64 r = (s64)(s32)s * (s32)t;
            c->lo = (u32)r, c->hi = (u32)((u64)r >> 32);
            break;
        }
        case 0x19: {
            u64 r = (u64)s * t;
            c->lo = (u32)r, c->hi = (u32)(r >> 32);
            break;
        }
        case 0x1A:
            if (t == 0) {
                c->hi = s;
                c->lo = (s32)s >= 0 ? 0xFFFFFFFFu : 1;
            } else if (s == 0x80000000u && t == 0xFFFFFFFFu) {
                c->hi = 0;
                c->lo = 0x80000000u;
            } else {
                c->lo = (u32)((s32)s / (s32)t);
                c->hi = (u32)((s32)s % (s32)t);
            }
            break;
        case 0x1B:
            if (t == 0) {
                c->hi = s;
                c->lo = 0xFFFFFFFFu;
            } else {
                c->lo = s / t;
                c->hi = s % t;
            }
            break;
        case 0x20: {
            u32 r = s + t;
            if (~(s ^ t) & (s ^ r) & 0x80000000u)
                EXC(12);
            WR(rd, r);
            break;
        }
        case 0x21: WR(rd, s + t); break;
        case 0x22: {
            u32 r = s - t;
            if ((s ^ t) & (s ^ r) & 0x80000000u)
                EXC(12);
            WR(rd, r);
            break;
        }
        case 0x23: WR(rd, s - t); break;
        case 0x24: WR(rd, s & t); break;
        case 0x25: WR(rd, s | t); break;
        case 0x26: WR(rd, s ^ t); break;
        case 0x27: WR(rd, ~(s | t)); break;
        case 0x2A: WR(rd, (s32)s < (s32)t); break;
        case 0x2B: WR(rd, s < t); break;
        default: EXC(10);
        }
        break;
    case 0x01: {
        int link = (rt & 0x1E) == 0x10;
        int cond = (rt & 1) ? (s32)s >= 0 : (s32)s < 0;
        if (link)
            WR(31, pc + 8);
        BRANCH(cond);
        break;
    }
    case 0x02:
        c->next_pc = (pc & 0xF0000000u) | ((in & 0x3FFFFFF) << 2);
        c->in_delay = 1;
        break;
    case 0x03:
        WR(31, pc + 8);
        c->next_pc = (pc & 0xF0000000u) | ((in & 0x3FFFFFF) << 2);
        c->in_delay = 1;
        break;
    case 0x04: BRANCH(s == t); break;
    case 0x05: BRANCH(s != t); break;
    case 0x06: BRANCH((s32)s <= 0); break;
    case 0x07: BRANCH((s32)s > 0); break;
    case 0x08: {
        u32 r = s + simm;
        if (~(s ^ simm) & (s ^ r) & 0x80000000u)
            EXC(12);
        WR(rt, r);
        break;
    }
    case 0x09: WR(rt, s + simm); break;
    case 0x0A: WR(rt, (s32)s < (s32)simm); break;
    case 0x0B: WR(rt, s < simm); break;
    case 0x0C: WR(rt, s & imm); break;
    case 0x0D: WR(rt, s | imm); break;
    case 0x0E: WR(rt, s ^ imm); break;
    case 0x0F: WR(rt, imm << 16); break;
    case 0x10: /* COP0 */
        switch (rs) {
        case 0x00: {
            u32 v;
            cpu_cop0_read(psx, rd, &v);
            LD(rt, v);
            break;
        }
        case 0x04: cpu_cop0_write(psx, rd, t); break;
        case 0x10:
            if (fn == 0x10)
                cpu_rfe(psx);
            break;
        default: EXC(10);
        }
        break;
    case 0x12: /* COP2 (GTE) */
        if (rs & 0x10) {
            gte_command(psx, in & 0x1FFFFFF);
            psx->cycles += 8;
        } else
            switch (rs) {
            case 0x00: LD(rt, gte_read_data(psx, rd)); break;
            case 0x02: LD(rt, gte_read_ctrl(psx, rd)); break;
            case 0x04: gte_write_data(psx, rd, t); break;
            case 0x06: gte_write_ctrl(psx, rd, t); break;
            default: EXC(10);
            }
        break;
    case 0x20: case 0x21: case 0x23: case 0x24: case 0x25: {
        u32 a = s + simm;
        int size = (op == 0x23) ? 4 : (op & 1) ? 2 : 1;
        if (a & (size - 1)) {
            c->badvaddr = a;
            EXC(4);
        }
        u32 v = load(psx, a, size);
        if (op == 0x20) v = (u32)(s32)(s8)v;
        if (op == 0x21) v = (u32)(s32)(s16)v;
        LD(rt, v);
        break;
    }
    case 0x22: case 0x26: { /* LWL / LWR merge with the value in flight */
        u32 a = s + simm;
        u32 cur = (rt && (int)rt == pr) ? pv : t;
        u32 w = load(psx, a & ~3u, 4), sh = a & 3, v;
        if (op == 0x22)
            v = (cur & (0x00FFFFFFu >> (8 * sh))) | (w << (8 * (3 - sh)));
        else
            v = (cur & (0xFFFFFF00u << (8 * (3 - sh)))) | (w >> (8 * sh));
        LD(rt, v);
        break;
    }
    case 0x28: case 0x29: case 0x2B: {
        u32 a = s + simm;
        int size = op == 0x2B ? 4 : op == 0x29 ? 2 : 1;
        if (a & (size - 1)) {
            c->badvaddr = a;
            EXC(5);
        }
        psx_write(psx, a, t, size);
        break;
    }
    case 0x2A: case 0x2E: {
        u32 a = s + simm, sh = a & 3;
        u32 w = psx_read(psx, a & ~3u, 4), v;
        if (op == 0x2A)
            v = (w & (0xFFFFFF00u << (8 * sh))) | (t >> (8 * (3 - sh)));
        else
            v = (w & (0x00FFFFFFu >> (8 * (3 - sh)))) | (t << (8 * sh));
        psx_write(psx, a & ~3u, v, 4);
        break;
    }
    case 0x32: { /* LWC2 */
        u32 a = s + simm;
        if (a & 3) {
            c->badvaddr = a;
            EXC(4);
        }
        gte_write_data(psx, rt, load(psx, a, 4));
        break;
    }
    case 0x3A: { /* SWC2 */
        u32 a = s + simm;
        if (a & 3) {
            c->badvaddr = a;
            EXC(5);
        }
        psx_write(psx, a, gte_read_data(psx, rt), 4);
        break;
    }
    default:
        EXC(10);
    }
    if (pr)
        c->gpr[pr] = pv;
    c->gpr[0] = 0;
    return 0;
#undef WR
#undef LD
#undef BRANCH
#undef EXC
}

static int run_until(Psx *psx, u32 stop_pc, u64 until)
{
    Cpu *c = &psx->cpu;
    while (!psx->exiting) {
        if (stop_pc && c->pc == stop_pc && !c->in_delay)
            return 1;
        if (!stop_pc && psx->cycles >= until)
            return 0;
        if (psx->cycles >= psx->next_event)
            psx_update(psx);
        if (cpu_step(psx) < 0) {
            psx->exiting = 2;
            return -1;
        }
    }
    return -1;
}

/* Calls the guest function at `addr` with the current registers; returns when it returns. */
void cpu_run_call(Psx *psx, u32 addr)
{
    Cpu *c = &psx->cpu;
    u32 pc = c->pc, next = c->next_pc, ra = c->gpr[31];
    int delay = c->in_delay;
    c->gpr[31] = BIOS_RET_MAGIC;
    jump_to(c, addr);
    run_until(psx, BIOS_RET_MAGIC, 0);
    c->gpr[31] = ra;
    c->pc = pc, c->next_pc = next, c->in_delay = delay;
}

u32 cpu_run(Psx *psx, u64 until)
{
    run_until(psx, 0, until);
    return psx->cpu.pc;
}
