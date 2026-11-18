# POPS: total reference (pops_01g.prx, firmware 6.61)

Status: complete map of Sony's PS1 emulator for PSP, read from the disassembly 2026-11-18.
POPS is **reference only** (AGENTS.md §8): this document describes mechanisms, numbers and
pseudo-code; it contains no POPS code and nothing here may be copied into psxrt. Purpose: never go
back to the disassembly for a question this file already answers.

- Binary: `D:\Temp\PSX2PSP-Reborn\fw661\pops\pops_01g.prx` (decrypted, ELF PRX, load base 0).
  Segment 0 = va 0..0xFECD4 (text, embedded BIOS at 0x53C20, rodata); segment 1 = data from va
  0xFED00. File offset = va + 0xC0 for both segments.
- Tools (`D:\Temp\PSX2PSP-Reborn\scripts`, run with `python -I`):
  - `popsdis.py START END`: disassembly; `-w VA N` words; `-h VA N` hex.
  - `pops_compat_db.py <prx> <IDs...>`: dumps/looks up the built-in per-game database.
  - `pops_iomap.py`: resolves the `0x1a050` I/O registrations.
  - `pops_fn.py`, `pops_data.py`, `pops_jt.py`, `pops_gelist.py`: function bounds, data dumps, jump
    tables, GE list decoding.
- Conventions: addresses are module offsets unless they are PSP absolutes (0x08/0x09xxxxxx user RAM,
  0x49xxxxxx its uncached alias, 0x04xxxxxx GE eDRAM, 0x44xxxxxx its uncached alias). `gp` =
  0x00010000 (PSP scratchpad = emulator context). `t9` = PS1 cycles left to the next event.
  **[V]** = read in the disassembly, **[I]** = inferred (each says what would confirm it).
  Untagged facts in parts A-G were verified by the report author; spot checks are listed in §3.
  A "§n" inside a part refers to that part's own sections.

Contents: §0 executive model · §1 why it is fast · §2 what it means for psxrt · §3 verification and
corrections · §4 open items · Part A CPU dynarec · B per-game config · C threads and memory ·
D I/O map · E GPU → GE · F SPU, Media Engine, CD-ROM, disc, SIO · G GTE, MDEC, IRQ/COP0, timers,
DMA, BIOS, cycle model.

---

## 0. Executive model

POPS is **one ordinary user-mode thread running a classic block dynarec over the real PS1 BIOS**,
plus device emulation in C on the same thread, plus the SPU mixer on the Media Engine. Nothing in
it is exotic; its speed comes from cheap mechanisms applied everywhere.

| Layer | What POPS does | Part |
|---|---|---|
| CPU | Block JIT (code cache 0x09540000-0x097B0000, 2.44 MB). Any guest word can start a block (per-word block table 0x09C00000). Static register map: 23 of 31 guest regs in host registers (11 GPR + 12 FPU regs). Constant propagation. Blocks chained by patching the exit `jal`. Memory access = `jal` to a helper that rewrites its own call site to the region it saw (RAM helper = 4 instructions). | A |
| Time | `t9` counts PS1 cycles down; costs are precharged per straight-line segment (`addiu t9,t9,-N`); `blez t9` only at block exits. One sorted event list drives every device. | A, G §3, G §5 |
| SMC | No per-store check. The BIOS FlushCache sequence (store 0x804 to 0xFFFE0130) flushes the whole translation cache; per-game entry checksums where the database asks; DMA/CD writes into RAM clear block-table pages that hold code. | A, G §5 |
| BIOS | Real Sony BIOS v4.5 embedded and executed (LLE). No A0/B0/C0 HLE at all. Only 7 hooks (G §6). | G §6 |
| GPU | High-level translation to **one GE display list per frame**, kept stalled, advanced per DMA batch. PS1 VRAM = real 1024×512 5551 surface in eDRAM; the GE rasterises into it. Precompiled GE templates reached by relative CALL. 24-bit display through TRX + 3 T8 passes with LOGICOP OR. | E |
| SPU | Full psx-spx-structured voice engine running **on the Media Engine**, one call per 44.1 kHz sample. CPU writes an uncached register shadow + dirty/KON/KOFF masks under a Peterson lock. XA decoded on the ME. | F §2 |
| Clock | **Audio is the master clock**: emulated time may lead the ME sample counter by at most 3387 cycles (100 µs); otherwise `sceKernelDelayThread`. | F §3, E §4.5 |
| GTE | One native Allegrex routine per op. Bit-exact UNR divide (128 KB table). FLAG in a VFPU register, computed only when a CFC2 can read it. Lighting/colour ops in VFPU float (not bit-exact). | G §1 |
| MDEC | VFPU float AAN IDCT on the main CPU, inside the DMA handler; output to PS1 RAM. | G §4 |
| CD | Register-exact controller with psx-spx-like timings. Disc = PSISOIMG 16-sector LZRC blocks, 17-block LRU cache, read-ahead on a priority-22 worker thread. | F §4-5 |
| Config | One general emulator + a built-in database of 820 per-game entries (33 words each: hack bitmask + 32 tuning words), overridable from the EBOOT's PSISOIMG header. | B |
| Memory | Everything fits in the standard 24 MB user partition (works on PSP-1000). | C |

## 1. Why it is fast (ranked by estimated weight)

1. **Audio-slaved pacing instead of cycle-accurate throttling**: it sleeps when ahead; when behind it
   simply runs, and audio never starves because the ME produces it independently.
2. **The GE does all pixel work** and the CPU only appends ~3 control words per primitive to a
   stalled list. No per-primitive kernel call, no copy of DMA2 packets (parsed in place in PS1 RAM).
3. **SPU on the second CPU.** The main CPU pays ~10 instructions per SPU register write.
4. **Dynarec with 23 guest regs in host registers, block chaining, self-patching memory helpers.**
5. **No per-store SMC check**: invalidation is tied to FlushCache and DMA writes.
6. **Busy-wait accelerators**: polls of I_STAT, GPUSTAT, SIO STAT/DATA and CD status skip forward to
   the next event (G §2.1, E §6, F §7.1).
7. **Precharged cycle counting**: one `addiu` per segment, one `blez` per exit.
8. **Approximations Sony accepted**: float lighting and IDCT, no hblank event, dot clock =
   sysclk/8, hblank = 2048 cycles, zero-cycle CD DMA, MDEC host time not charged.

What POPS does **not** do: no primitive merging, no frame skipping, no VFPU in the primitive path,
no HLE of the BIOS, no use of the extra 32 MB of the slim models.

## 2. What this means for psxrt

psxrt is a static recompiler (code translated before the game runs, fixed guest registers, direct
calls) on top of a native runtime that implements the PS1 hardware. Compared point by point:

| Area | POPS | psxrt direction |
|---|---|---|
| Translation | JIT at run time; compile cost paid in game; cache flushed whole when full or on FlushCache | AOT: zero compile cost, whole-program analysis (all registers allocatable, cross-block constant propagation, no linking stubs). Our advantage only holds if the interpreter stays at 0 steps (goal §1.1). |
| SMC | FlushCache = universal invalidation point | Same trigger is the right check point for us: on FlushCache, verify that loaded code matches a translated image (hash per unit) instead of checking stores. |
| Time | Precharged per segment; costs 1 / load 5 with overlap / MULT 12 / DIV 35 / GTE table / fetch penalty `>>1` (≈1.5 CPI outside 4 KB loops) | These are Sony's tuned numbers that run the whole catalogue; reuse them as the cycle model of the recompiler (G §5), with the GTE costs from psx-spx where POPS differs (NCT, DCPL, DPCT). |
| Pacing | Master clock = ME sample counter, lead budget 100 µs | Make the sceSas/ME output position the master clock of psxrt as well; never pace on vblank alone. |
| GPU | Translation on the main CPU is cheap enough for full speed on a 333 MHz Allegrex | GP0→GE translation does not need the Media Engine. If the ME is used, the SPU is the proven tenant (POPS), GPU translation is the experiment (S1 probe). |
| GPU list | One stalled list per frame, stall advanced per batch, templates + relative CALL, VRAM as real eDRAM surface | Matches the S1 GE lane; the template/CALL trick and the per-batch stall are the parts to check against ge.c. |
| 24-bit video | GE TRX + 3 T8 passes OR-ed, no CPU per pixel | Directly applicable to our 24-bit present path. |
| GTE | Exact divide via 128 KB UNR table + `maddu`; FLAG only when read; lighting in float | Keep our integer GTE (more exact). Take the FLAG-liveness idea: the recompiler knows statically which CFC2 reads FLAG. |
| MDEC | Float IDCT on the CPU, 3072 cycles per macroblock charged | Our VFPU IDCT is the same idea; the 3072 cycles/macroblock is a timing reference. |
| CD | 16-sector LZRC blocks, LRU 17, read-ahead thread prio 22 | We own the disc format (DATA.PSAR): with 64 MB we can cache far more; keep a read-ahead worker. |
| Config | 820-entry DB keyed by disc ID; EBOOT can override | Same model as `patches.ini` (R7). The POPS entry for a game is a hint about where that game is fragile (e.g. Crash 2: SPU/CD word15); re-derive the fix, do not copy the table. |
| Memory | 24 MB | We target 64 MB: full translated images in RAM, bigger disc cache. |

How we beat it: no compile cost, no flush-and-recompile stalls, whole-program register allocation,
PsyQ functions replaced by native code (POPS runs every library routine instruction by instruction),
and 64 MB of cache. Where we must match it: audio-clock pacing, a stalled GE list with batch
advances, poll accelerators, and the cycle model above.

## 3. Verification and corrections

Spot checks against the disassembly (2026-11-18):
- CPU core (Part A): read directly.
- Config DB hash: checked on real IDs (SCUS94154 Crash 2 → key 0xC75DD407 → {word4, word15};
  SLES02895 Sheep Dog → no entry).
- SPU: three claims checked, all correct (write handler, Peterson lock bytes, mixer at module 0).
- GPU: list base 0x49a00000 and the CALL-word trick checked.
- GTE: exception and memory-map corrections checked at 0x94c4-0x9538 and 0x1a334.

Corrections applied in this document (the reports disagreed or were wrong):

| Item | Wrong | Right | Evidence |
|---|---|---|---|
| DMA handler addresses in the GPU report | 0x19158 / 0x192a4 / 0x18b1c / 0x18cac | 0x9158 / 0x92a4 / 0x8b1c / 0x8cac | prologues at those addresses |
| NTSC half-line in the GPU report | 1084.275 cycles, 566,489 cycles/frame (240 lines) | 1076.275 (float 0x448688CE = 2152.55/2); 263 lines × 2152.55 = 566,121 cycles = 59.83 Hz | float decode |
| cfg[17] in the config report | "encoded instruction pattern" | a guest PC (bit 0 = flag): compared with the PC being compiled at 0x51d8 (`ins v0,zero,0,1; xor v0,s1,v0`); at 0x60a0 the compiler emits `sh rX,0x1c0(gp)` = translation-cache flush request | [V] 0x51d8, 0x609c-0x60bc |
| cfg[19] in the config report | "frame/menu side" | fetch-penalty shift copied to gp+0x1C2 at 0x19f28 | G §5.2 |
| "24-bit mode" test in the pacing routine | config bit / 24-bit described differently | gp+0x3668 holds the GP1(08) display mode << 8: bit 11 PAL, bit 12 24-bit, bit 13 interlace | GP1(08) bit layout (psx-spx) matches all three readers |
| DMA channel table in the SPU report | gp+500+28·ch | struct at gp+0x1E8+28·ch; +0xC (= gp+500 for ch0) is the event callback | G §3.4 |
| SPU RAM in the GTE report | 0x09F40000-0x09FBFFFF | shadow 0x09F40000 (0x2C0 B), SPU RAM 0x09F402C0-0x09FC02C0; the init clears both | F §1 |
| SPU write range | iomap: 0x1C00-0x1E5F | handler 0x7f00 covers 0x1C00-0x1DFF; 0x1E00+ writes go to the default (ignored) | F §2.1 |
| BIOS exception vector with BEV=1 | 0xBFC00180 | 0xBFC00000 (POPS simplification) | [V] 0x952c |
| GTE op table location (old brief) | data segment | rodata in segment 0 | G intro |
| scePopsMan_0090B2C8 (old brief) | decompression | fatal error/exit report with 0x80000004 | F §5.4 |

## 4. Open items

None of these blocks psxrt work; each has its cheapest confirmation.

| Item | Confirm by |
|---|---|
| 0xe02c is LZRC (structure matches; not decoded bit by bit) | decompress one PSISOIMG block with a port of the documented LZRC format and compare |
| 0x3b300 = ATRAC3-like CD-DA decoder (384-B frames → 1024 samples) | read its tables |
| popsman: who sets the game framebuffer, who writes the sample counter 0x49f40294, what NID 7014C540 waits for, the ME loop at popsman+0x2F88 | disassemble popsman.prx |
| Exact GTE MAC overflow flags in the float lighting ops | read 0xf898-0x10184 fully |
| LZCR off by one for v = -2^k | one-off test |
| SR.IsC stores (record type 19, emitter 0x7a88) | read 0x7a88 |
| Meaning of the individual hack-bitmask bits | the function around each reader (Part B) |
| GPU: frame-tail words, line templates, second CLUT path, fog, 0x1b6ec / 0x3728c / 0x34350 / 0x30c24 | see Part E §8 |
| SPU: noise LFSR step detail, MC jump table 0xd4734 | read 0x11c-0x184 and 0xd4734 |

---

## Part A. POPS CPU core: the dynarec (verified 2026-11-18 on pops_01g, fw 6.61)

Addresses are module offsets of pops_01g (load base 0); host addresses 0x08xxxxxx/0x09xxxxxx are
absolute PSP user RAM, 0x0001xxxx is the PSP scratchpad, 0x04xxxxxx is GE eDRAM.

### Host memory map
| Host | Size | Content | Evidence |
|---|---|---|---|
| 0x00010000 ($gp) | 16 KB | emulator context (guest PC, COP0, cycle counters, event list, register spill slots, compiler state, I/O handler table at +0x1000) | `ori a2,gp,0xf800` vs `addr>>12` = 0x1f800 |
| 0x00013000 | 1 KB | PS1 scratchpad | `ins a0,0x4c,10,22` |
| 0x041B0000 | ~64 KB | compiler's per-instruction analysis array (16 B per guest instruction), in GE eDRAM | `lui 0x41b` in 0x58c0/0x5154 |
| 0x09540000-0x097B0000 | 2.44 MB | translated code cache (bump allocator, gp+0x1cc) | 0x7e60 resets to 0x09540000; 0x58c0 flushes at >= 0x097B0004 |
| 0x097C0000-0x097E0000 | 128 KB | GTE UNR divide table (32K words, built at init) | `lui a1,0x97e` at 0x1a334 |
| below 0x09800000 | <= 128 KB | host stack while in translated code (C helpers push below `sp` = RAM base; extent inferred) | `lui sp,0x980` at 0x1a00 |
| below 0x09BF0000 | ? | popsmain's C stack (top 0x09BF0000; extent not measured) | `lui sp,0x9bf` at 0x16088 |
| 0x09800000 | 2 MB | PS1 RAM; host `$sp` is pinned to this base (`ori rt,sp,imm` builds RAM addresses) | 0x1a90 `ins a0,0x4c,21,11` |
| 0x09C00000 | 2 MB | block table, RAM: one 4-byte host pointer per guest word | dispatcher 0x2650 |
| 0x09E00000 | 512 KB used | block table, BIOS (PC bit 23 selects it) | 0x2650, 0x55b8 |
| module +0x53C20 | 512 KB | PS1 BIOS ROM "System ROM Version 4.5 05/25/00 J" embedded: the real BIOS runs (LLE) | read helpers `lui 0x4045; lb 0x3c20(addr)` |

### Guest register file (static, global allocation; template copied per block from 0xd40c8)
| Guest | Home | Guest | Home | Guest | Home | Guest | Home |
|---|---|---|---|---|---|---|---|
| at | k1 | a0 | s4 | t0 | s0 | t4 | f22 |
| v0 | s2 | a1 | s5 | t1 | s1 | t6 | k0 |
| v1 | s3 | a2 | s6 | t2 | f20 | sp | s7 |
| | | a3 | fp | t3 | f21 | ra | f23 |
| s0..s7 | f24..f31 | t5,t7,t8,t9,k0,k1,gp,fp | scratchpad slots | | | | |

- 11 guest registers live in host GPRs, 12 in FPU registers (moved with `mfc1`/`mtc1`, the
  emitter peepholes a `mtc1` followed by the matching `mfc1` into a `move`), 8 in the scratchpad.
- Host temporaries for the scratchpad-resident ones and for helpers (template at 0xd40e8):
  t0-t7, t8, v1, at, a3. Host `t9` = cycle countdown, `gp` = context, `sp` = PS1 RAM base,
  a0/a1/a2 = helper arguments (address, value, kind), `ra` = helper/link return.
- At emulator start (0x1a00) all homes are zeroed and `sp` = 0x09800000.

### Dispatch, blocks and linking
- Dispatcher 0x2650: `entry = *(0x09C00000 | pc & 0x1FFFFF | (pc>>23&1)<<21)`; 0 -> compile (0x2868 ->
  0x58c0, which returns the host address); `jr entry`. Guest PC lives at gp+0x1a0.
- Any guest word can be a block entry (the table is per word): a jump into the middle of a block
  compiles a new block from there; blocks overlap freely.
- Block size: scan limit pc+0xC00 (768 instructions), or pc+0x2000 with a block flag, clipped at
  gp+0x6e0. A block header word at -4(host) holds its guest length (used by invalidation).
- Exit sequence emitted at every block end (0x6768): write back dirty homes, then
  `blez t9, <trampoline>` / `jal <target host block or link helper 0x2888>` /
  `sw a0, 0x1a0(gp)` (delay slot: guest PC). The trampoline (shared within +-128 KB, gp+0x3cf4 /
  0x3cf8, or emitted locally) is `jal 0x1a80; sw a0,0x1a0(gp)`: run the event scheduler, then
  dispatch.
- Linking (0x2888): looks the target up, compiles it if needed, then rewrites the low 24 bits of the
  exit `jal` (`sll a0,v0,6; swl a0,-6(ra)`) so the next pass jumps straight to the target block.
  `cache 0x1b` + `sync` + `cache 8` on the patched line. Blocks therefore chain directly; the
  dispatcher is only reached through the event trampoline, exceptions and computed jumps.
- Exceptions (0x94c4): Cause.ExcCode set, SR KU/IE stack pushed, PC = 0x80000080, or with SR.BEV
  set 0xBFC00000 (host block = BIOS table entry 0; real hardware uses 0xBFC00180: a POPS
  simplification, harmless because games never run with BEV=1). The host block for 0x80000080 is
  cached at gp+0x1d4 to skip the lookup. No EPC write is visible here (done by the callers).

### Memory access from translated code
- Each guest load/store becomes `jal <helper>` with the address in a0 (value in a1). Helpers are
  per access width and per region; the first one is generic (0x1b18-style chain: scratchpad?
  RAM? BIOS? I/O?). On first use the generic helper rewrites the call site (`sw jal, -8(ra)`,
  `cache 0x1a`, `cache 8`, `sync`) to the helper of the region it saw; a specialised helper that
  sees another region falls back to the generic one, which re-patches. So each load/store site
  costs one `jal` + 3-4 instructions once it settles.
  - RAM: `ext v0,a0,23,6; bnez -> generic; ins a0,0x4c,21,11; jr ra; lb v0,(a0)` (mirrors and
    KUSEG/KSEG0/KSEG1 folded; no cycle adjustment).
  - Scratchpad: `ins a0,0x4c,10,22` (0x00013000), `t9 += 4` (refund).
  - BIOS: module +0x53c20, extra cycles (`t9 -= 1..3` by width; ROM writes cost `(1<<size)+15`).
  - I/O 0x1F801xxx: `handler = *(gp+0x1000 + (addr & 0xff8))` (+4 for the write slot), called
    with a1/a2 = width kind; ra and t9 saved in gp+0xfc / gp+0x1b0 around it.
  - 0x1F802xxx (expansion 2): small table at gp+0x2000; 0xFFFE0130 (cache control) handled.
- Stores to RAM do NOT check for code: plain `sb/sh/sw` into 0x09800000.

### Self-modifying code: three mechanisms, none per store
1. Cache-control write (0x89a0): a store of 0x804 to 0xFFFE0130 (the BIOS FlushCache sequence,
   which every PS1 program must run after loading code) flushes the whole translation cache
   (0x7e60) and charges 0x20 cycles.
2. Per-block checksum, only where the per-game config asks (0x4b44): if gp+0x6d8 >= 3 (word count;
   0xffff = whole block when block flag 0x4000), the block's entry sums the first N guest words and
   compares with the sum taken at compile time (inline `lw`/`subu`/`bne`, or `jal 0x2918` for long
   runs); mismatch -> 0x2878 -> 0x4e18: zero the block's table entries, turn its entry into
   `j 0x2888; lw a0,0x1a0(gp)`, recompile. gp+0x6e4 = one PC that always gets flag 0x4007; gp+0x6b0
   bit 0 = flush everything instead. These words come from the game's config: the exceptions are
   data, not code.
3. Code-page bitmap gp+0x1d8 (one bit per 64 KB of PS1 RAM holding translated code): used by the
   flush (only marked pages of the block table are cleared, with uncached VFPU quad stores) and by
   the bulk RAM writer at 0xce20 (DMA-style copies into RAM check it).
- Flush (0x7e60): clear the marked pages of the block table, code pointer back to 0x09540000,
  linking trampolines forgotten. No LRU, no partial eviction: a full cache simply restarts.
- Deferred requests: gp+0x1c0 halfword checked by the scheduler: > 0 -> flush, < 0 -> emulator
  reset (0x24b58, zeroes the context); set by the frame/menu side (0x11a50).

### Compiler (0x2950-0x7e60, about 21 KB)
- 0x58c0 block driver: region checks (address error -> exception 4), code cache space, per-game
  flags, scan pass, analysis passes, emission, header, table entry, page bitmap.
- 0x5154 decode/scan: classifies each guest instruction through a 0x6b-entry jump table at 0xd4150
  into a 16-byte record at 0x041B0000+ (flags, registers read/written, class, cumulative cycles at
  +0xf, branch target). Per-instruction fetch cost by region (gp+0xb42): 0 in KUSEG/KSEG0 (I-cache
  assumed to hit), 5 in KSEG1 RAM, 2 in BIOS.
- Analysis passes over the records (0x5a5c-0x5cxx): delay slots, load delays, liveness of the
  homes, block end.
- Constant propagation (0x2a88 and its callers): bitmask gp+0xb58 of guest registers with known
  values, values in gp+0xb5c+; immediates are synthesised as `addiu`/`ori` from a register already
  holding a nearby value, `ori rt, sp, imm` for RAM addresses, `ori rt, gp, imm` for context ones.
- 0x6914 per-instruction translator (largest, 0x154c bytes); emitters for ALU, loads/stores
  (0x3a90, 0x362c, 0x46a0), branches (0x1f20 `bgtz t9` / 0x1b20 `blez t9` encodings appear in
  the exits), register moves (0x29c4: home -> temp, `mfc1` for FPU homes, `lw off(gp)` for
  scratchpad homes; 0x4504 for the memory case).
- Cycle charge: `addiu t9, t9, -n` (0x2739xxxx) emitted per straight-line run from the records'
  cumulative counts (0x42a4, 0x4340, 0x44dc).

### Event scheduler and interrupts (0x953c)
- `t9` (also spilled at gp+0x1b0) = cycles until the next event; gp+0x1ac = time of the next event.
- Event list: doubly linked, sorted, head at gp+0x1b8; node = {next, prev, time, callback}.
  The scheduler pops every due event, calls it, recomputes t9.
- Then interrupts: SR at gp+0x130, Cause at gp+0x134. If IEc and (Cause.IP & SR.IM) != 0: if the
  instruction at the guest PC is a COP2 command (`op>>25 == 0x25`), it is executed first through
  the GTE table at 0xd45c4 (the hardware quirk the BIOS handler relies on), then exception 0.

### What this means for us
- POPS's speed is not "perfect HLE": it runs the real BIOS and real game code through a
  straightforward dynarec whose wins are (1) a static register map with 23 of 31 guest registers in
  host registers (11 GPR + 12 FPR), (2) direct block chaining, (3) per-site backpatched memory
  helpers with RAM at a fixed host address, (4) no per-store SMC check (FlushCache + per-game
  checksums instead), (5) an event-driven cycle counter in a register.
- Our recompiler already has (1)-(3) ahead of time (fixed guest registers, direct calls, fast
  lanes). Items where POPS is ahead or different: FPU registers as extra homes; the FlushCache
  trigger as THE invalidation point; event countdown in a register checked only at block exits.

---

## Part B. POPS per-game configuration (verified 2026-11-18, pops_01g)

### Storage
- 33 words in the context: gp+0x6ac = **hack bitmask** (config word -1), gp+0x6b0..0x72c = 32
  **tuning words** (config words 0..31). Defaults (0x24770): bitmask 0, tuning words all 0xFFFFFFFF
  (`memset(gp+0x6b0, 0xff, 128)`); a few words then get computed defaults (below).

### Sources, in order (loader 0x24770, called from 0x1b124 / 0x1b468)
1. **Built-in database** inside the module: table at 0xEF0B0, 12-byte entries
   `{key, count, pairs_ptr}`, terminated by key 0; 820 entries. Pairs are `{int index, u32 value}`:
   index -1 -> gp+0x6ac, index i >= 0 -> gp+0x6b0+4i. Several keys share one pairs list.
   - Key = `BE32(id[0..4]) ^ (strtol(id+4, 16) * 0x1001)`, id = normalised disc ID ("SLES02895",
     produced by 0x1b61c from the PSISOIMG field at +0x400).
   - Checked: SCUS94154 (Crash 2) -> key 0xC75DD407 -> {word4 = 0xFFFFFFFD, word15 = 0x11EDC};
     SLES02895 (Sheep Dog) -> 0x51C53DC6, no entry (defaults). Script:
     `python -I scripts/pops_compat_db.py fw661/pops/pops_01g.prx <IDs...>`.
2. **Disc ID prefix "PSRM"** (0xF18E8, compared with 4 chars; which discs carry it is not known):
   word4 = -2, word1 = 0xE8, word2 = 12, word11 = 0x10004, word3 = 12.
3. **PSISOIMG header** (loaded at host 0x09E80000): +0x420 = config version, +0x424 = required
   version, +0x428 = 4-byte bitmask + 128-byte tuning block.
   - +0x424 > 0x06060000 -> load fails with 0x80000002 (needs a newer POPS).
   - +0x420 > 0x06060000 -> the header's 132 bytes REPLACE the built-in values.
   So a converted EBOOT can carry its own config; that is how Sony shipped per-game fixes without
   touching the emulator.
4. **Japanese IDs** (id[2] == 'P': SLPS/SCPS/SLPM): 0x16120 bytes from module 0x3DB00 are copied
   over the embedded BIOS at +0x18000 (the region-specific part of the ROM; inferred).
5. Derived: word10 (gp+0x6d8, checksum length for SMC): 2 unless word0 has bit 2; with bit 2 a
   negative value becomes 0xFFFF (whole block). word0 == -1 -> 2.

### What the words control (by reader address; subsystem map in Part D)
| Word | gp+ | Readers | Use |
|---|---|---|---|
| bitmask | 0x6ac | ~50 sites: compiler (0x3c30, 0x5afc-0x5c88, 0x7560), I/O (0x87e4, 0x9b20, 0xa328), SPU/CD (0xaebc, 0xcbe4, 0xf3f8), GPU (0x114dc-0x15fa0), 0x1a5ec, 0x2aee4 | one bit per hack; each site tests its own bit with `ext rX,rY,bit,1` (bits 3,4,6,9,10,11,12,16,17,18,19,20,24,25,27,28,29 seen) |
| 0 | 0x6b0 | 0x4e20, 0x5978, 0x19f20, 0x24928 | CPU/SMC policy: bit 0 = full flush instead of per-block invalidate; bit 2 = enable entry checksums; -1 = blocks get flag 3 |
| 1 | 0x6b4 | 0x11fa8 (halfword) | GPU timing (0xE8 for PSRM) |
| 4 | 0x6c0 | 0xb41c-0xc938, 0x1af2c | SPU/CD-side flag mask (negative values clear bits: 0xFFFFFFE7, 0xFFFFFFFD...) |
| 10 | 0x6d8 | 0x4b64 | SMC checksum length in words |
| 11 | 0x6dc | 0x12730-0x127c4 (halfwords) | GPU: two 16-bit parameters (0x10004 = 1 and 4) |
| 12 | 0x6e0 | 0x59d8 | compiler scan limit (block end address) |
| 13 | 0x6e4 | 0x597c | one guest PC whose block always gets flags 0x4007 (e.g. 0x801EB9D8 in one entry) |
| 14 | 0x6e8 | 0x12cb4, 0x1b6ec | GPU / frame |
| 15 | 0x6ec | 0xc584 | SPU/CD value (Crash 2: 0x11EDC) |
| 16 | 0x6f0 | 0x8500, 0xb7bc, 0xba94, 0xc87c | I/O + SPU/CD |
| 17 | 0x6f4 | 0x51d8, 0x60a0 | compiler: a guest PC (bit 0 = flag); compiling it emits a translation-cache flush request (`sh rX,0x1c0(gp)`) |
| 19 | 0x6fc | 0x19f10 | instruction-fetch penalty shift, copied to gp+0x1C2 (Part G §5.2) |
| 22-28 | 0x708-0x720 | GPU 0x12670-0x15010, 0x1a0b8 | GPU parameters |

Meaning of the individual bits: inferred from the subsystem only; naming each one needs the
function around its reader (not done, not needed: we do not run POPS).

### Lesson for us
Sony's answer to "every game" is the same as ours (R7): one general emulator plus a database of
per-game exceptions, keyed by disc ID, overridable from the converted image. Our `patches.ini`
plays the role of the 0xEF0B0 table and the PSISOIMG +0x428 block.

---

## Part C. POPS threads, memory reservation, system imports (verified 2026-11-18, pops_01g)

### Threads (sceKernelCreateThread call sites)
| Name | Entry | Prio | Stack | Attr | Created at | Role |
|---|---|---|---|---|---|---|
| popsmain | 0x16080 | 32 | 0xBED900 (12.4 MB) | 0x80104000 (USER, VFPU, no stack fill) | 0x1604c | the emulator: sets its own `sp = 0x09BF0000`, then runs the dynarec |
| cdworker | 0xDA3C | 22 | 0x1800 | 0x100000 | 0x1b4e0 | disc reads (higher priority than the emulator) |
| mcworker | 0x1AA90 | 27 | 0x1000 | 0x102000 | 0x1a9a0 | memory card saves (`sceIo*` on the VMC; pspvmc imports) |
- Priority 17 is set at 0x1a938 after `sceKernelDelayThread(1000)` (which thread: the caller's).
- The 12.4 MB "stack" of popsmain is the memory reservation: thread stacks come from the top of the
  user partition, so it covers about 0x09412700-0x0A000000 (inferred from the 24 MB user partition
  ending at 0x0A000000), which holds every fixed host address the emulator uses: code cache
  0x09540000, JIT-side C stack below 0x09800000, PS1 RAM 0x09800000, C stack top 0x09BF0000, block
  tables 0x09C00000/0x09E00000, PSISOIMG header 0x09E80000. POPS therefore needs only the standard
  24 MB user partition: no 64 MB, no extended memory (works on PSP-1000).
- Two host stacks: normal C code runs on 0x09BF0000 downwards; inside translated code `sp` is the
  PS1 RAM base 0x09800000, so C helpers called from the JIT push below it (0x097B0000-0x09800000,
  which is why the code cache stops at 0x097B0000).
- Startup (0x16000): two SysMemUserForUser calls with 0x06060010 and 0x00030306 (inferred: compiled
  SDK version 6.60 and compiler version), `sceDisplaySetHoldMode`, thread creation, then a
  scePopsMan call (0x3d40c).

### Interrupts and timers
- `sceKernelRegisterSubIntrHandler(30, ...)` + `sceKernelEnableSubIntr(30, ...)` at 0x3cf5c/0x3cf70:
  PSP interrupt 30 = vblank. sceKernelSetAlarm / CancelAlarm at 0xa6c0-0xaaac (SPU/CD area).
- Frame pacing: sceDisplayWaitVblankStart(CB), sceDisplayGetVcount, sceKernelGetSystemTimeLow.

### Imports by library (count)
ThreadManForUser 15, IoFileMgrForUser 14, sceGe_user 8 (ListEnQueue, UpdateStallAddr, ListSync,
DrawSync, Save/RestoreContext, EdramGetSize, EdramSetAddrTranslation), sceDisplay 5, sceCtrl 5,
UtilsForUser 5 (D-cache only: no I-cache call, the code uses `cache` ops itself), InterruptManager 3,
scePower 3 (suspend callback), sceDmac 1 (sceDmacMemcpy), sceMeAudio 29 (exported by popsman: disc
reads, GE list/stall, ME audio), scePaf 35 + sceLibFont_HV 11 + sceVshCommonUtil 6 + sceUtility 5
(the in-game menu), pspvmc 2 (virtual memory card), sceImpose 2, sceRtc 2, sceOpenPSID 1.
No sceAudio, no sceSas, no sceMpeg: sound goes through popsman/ME, video (MDEC) is emulated by
POPS itself.

---

## Part D. POPS I/O map (verified 2026-11-18, pops_01g)

Registration: `0x1a050(addr, size, read, write)` fills the 8-byte slots
`gp + (addr - 0x1F800000)` (so 0x1F801xxx -> gp+0x1xxx) for `size/8` slots; a null handler
means the default (read 0x8a54, write 0x8aa4). Resolved by `scripts/pops_iomap.py`
(`?` = argument computed in a loop, not resolved statically; 0 = default handler).

| PS1 range | Read | Write | Subsystem | Registered at |
|---|---|---|---|---|
| 0x1F801000-0x1F801FFF (all) | 0x88bc | 0x89a0 | catch-all / memory control (0x89a0 also catches the cache-control path that flushes the code cache) | 0x1a0f0 |
| 0x1F801000-0x1F80103F, 0x1F801060 | default | default | memory control (ignored) | 0x1a108, 0x1a120 |
| 0x1F801040-0x1F80105F | 0x9f30 | 0xa06c, 0xa0e8 | SIO0 (pads, memory cards) / SIO1 | 0x1a500-0x1a550 |
| 0x1F801070-0x1F801077 | 0x9850 | 0x98c4 | interrupt controller (I_STAT, I_MASK) | 0x1a300 |
| 0x1F801080-0x1F8010EF | 0x9158 | 0x92a4 | DMA channels (per-channel loop) | 0x1a1a0-0x1a268 |
| 0x1F8010F0-0x1F8010F7 | default | 0x91bc | DPCR / DICR | 0x1a1d8 |
| 0x1F801100-0x1F80112F | 0x9be0 | (loop) | root counters (timers 0-2) | 0x1a438, 0x1a44c |
| 0x1F801800-0x1F80180F | 0xd088 | 0xd1b0 | CD-ROM controller | 0x1aef8 |
| 0x1F801810-0x1F801817 (inferred from the GPU handlers) | 0x12fbc | 0x127d8 | GPU (GP0/GPUREAD, GP1/GPUSTAT) | 0x1babc |
| 0x1F801820-0x1F801827 | 0xf6dc | 0xf70c | MDEC | 0x1b6b8 |
| 0x1F801C00-0x1F801E5F | 0x85f4 | 0x7f00 (0x1C00-0x1DFF only; 0x1E00+ default) | SPU | 0x19ff0, 0x1a00c |

Code regions by subsystem (from the handler addresses; boundaries approximate):
SPU front end 0x7f00-0x88bc · I/O catch-all 0x88bc-0x8b00 · DMA 0x9158-0x9300 · scheduler and
exceptions 0x94c4-0x9800 · IRQ 0x9850 · timers 0x9be0 · SIO 0x9f30-0xa100 · SPU/CD/XA engine
0xa100-0xd000 · CD-ROM 0xd088-0xf000 · MDEC 0xf6dc-0x11000 · GPU 0x11000-0x16000 · popsmain
0x16080 · menu, config, threads, I/O registration 0x19f00-0x1b700 · libc and import stubs
0x3cf00-0x3d430 · data from 0xd4000 (compiler tables 0xd40c8/0xd40e8/0xd4150, GTE table 0xd45c4,
config DB 0xef0b0), embedded BIOS 0x53c20-0xd3c20 (512 KB).

---

## Part E. POPS (pops_01g, fw 6.61): PS1 GPU emulation and its mapping onto the PSP GE

All addresses are module offsets of pops_01g (load base 0) unless they are clearly PSP physical or
virtual addresses (0x04xxxxxx eDRAM, 0x44xxxxxx uncached eDRAM, 0x49xxxxxx uncached RAM).
`gp = 0x10000`, so `gp+0x3630` = 0x13630, and so on. "Inferred" marks a deduction that is not proven
by the code read so far; each one says what would confirm it. POPS is reference only: this
describes mechanisms, it is not code to copy.

### 0. One-paragraph model

POPS is a **high-level GPU translator**. Every GP0 packet, from a CPU write to GP0 or from DMA2,
is decoded on the CPU and turned into a few GE words appended to **one long GE display list**. The
list lives in uncached RAM at 0x49a00000–0x49b7b800 (about 1.5 MB). The GE is enqueued once per
frame and kept **stalled**. Its stall address is advanced through popsman (`sceMeAudio_E7F06E2B` /
`sceMeAudio_7014C540`) after each DMA batch, never per primitive.

The PS1 VRAM is a real 1024×512 5551 framebuffer at PSP eDRAM 0x04000000 (stride 2048 B). PS1
primitives render straight into it with hardware rasterisation and these mappings:
- **Transform mode**, not through mode, with s16 vertices. A static world/view/projection set gives
  a +0.5 pixel offset and puts the PS1 drawing offset into the world matrix translation.
- PS1 semi-transparency → four GE blend presets.
- PS1 mask bit → the 5551 alpha bit, used as a 1-bit stencil.
- STP texels → two passes split by alpha test.
- 4/8-bit textures → a TRX-copied texture cache with a narrower stride. 15-bit textures are sampled
  directly from the VRAM.

The display is a bilinear-filtered sprite blit of the PS1 display area into a 5650 PSP framebuffer.
24-bit display uses GE TRX + T8 channel passes ORed together (LOGICOP). Pacing is slaved to the
44.1 kHz audio sample counter that the Media Engine advances.

---

### 1. GPU I/O entry points

#### 1.1 Registration (init 0x1b9c4)

| Call (site) | Meaning |
|---|---|
| `0x1a1f0(2, 0x12c74)` (0x1ba90..) | DMA channel 2 handler = 0x12c74. Channel struct 28 B at gp+0x1e8+ch*28: handler at +0x10 (gp+0x1f8 for ch0), default 0x8b1c, channel number at +0x1a. 0x1a1f0 also registers the MADR/BCR/CHCR I/O slots (read 0x9158, write 0x92a4). |
| `0x1a050(0x1f801810, 8, read=0x12fbc, write=0x127d8)` | GP0 (0x1810) and GP1 (0x1814) share one 8-byte slot pair. Handler table gp+0x1000 + (addr & 0xff8): +0 read, +4 write. Null handlers default to 0x8a54 (read) and 0x8aa4 (write). |

Handler ABI:
- Read: `v0 = read(a0=addr, a1=kind)`.
- Write: `write(a0=addr, a1=value, a2=kind)`.
- The guest cycle counter is live in t9 and saved at gp+0x1b0.

#### 1.2 GPU context fields (all in the PSP scratchpad)

| Field | Type | Meaning |
|---|---|---|
| gp+0x3500.. | u32[] | GP0 FIFO (packet words collected from CPU writes) |
| gp+0x35bc | u32 | GPUREAD latch |
| gp+0x35c0 | u32 | GetVcount value at the last present |
| gp+0x35c4 | u32 | time base for the GPUSTAT bit 31 toggle and the busy-poll skip |
| gp+0x35c8 | u32 | display start (x + y*1024) at the last present |
| gp+0x35cc | i32 | current GE list id |
| gp+0x35d0 | event | 60 Hz sub-tick event (§4.4) |
| gp+0x35e0 | i32 | sub-tick counter |
| gp+0x35e4 | event | vblank / active-display event (§4.4) |
| gp+0x35f8 | event | "GPU busy" event (GPUSTAT bits 26/28 return to ready when it fires) |
| gp+0x3608 / 0x360c | u32 | audio counter / emulated time at the last pacing check (§4.5) |
| gp+0x3610 / 0x3612 | u16 | GP1(05) display start x, y |
| gp+0x3614 / 0x3616 | u16 | GP1(06) H range x1, x2 |
| gp+0x3618 / 0x361a | u16 | GP1(07) V range y1, y2 |
| gp+0x361c / 0x361e | u16 | derived display width, height in VRAM pixels (height 240/288, doubled when interlaced) |
| gp+0x3620 / 0x3622 | i16 | E3 draw area x1, y1 |
| gp+0x3624 / 0x3626 | i16 | E4 draw area x2, y2 |
| gp+0x3628 / 0x362a | i16 | E5 draw offset x, y (x already wrapped, §3.6) |
| gp+0x362c | u32 | GE list write pointer (uncached, 0x49a00000..) |
| gp+0x3630 | u32 | GPUSTAT image (bits 11/12 = E6 mask bits, 23 = display disable, 25/26/28/29-30 as on PS1) |
| gp+0x363c..0x3646 | u16 | VRAM transfer x, y, w, h, current column, current row |
| gp+0x364c | u32 | raw E2 (texture window) |
| gp+0x3650 / 0x3651 | u8 | texture window offset x, y (in 8-px units) |
| gp+0x3652 / 0x3653 | u8 | texture window wrap size x, y (lowest set bit of mask, §3.3) |
| gp+0x3654 | u16 | E1 texpage bits 0-10; bit 13 kept; bit 14 = blend preset must be re-emitted; bit 15 = texture must be re-bound |
| gp+0x3656 | u8 | last TPSM / texture format emitted (0xFF = none) |
| gp+0x3657 | u8 | GP0 FIFO state: 0 = command, 8 = polyline continuation, 9 = CPU→VRAM data |
| gp+0x3658 | i8 | list state: >0 pending stall update, 0 synced, <0 = list closed, re-enqueue before the next packet |
| gp+0x3659 | u8 | GPUREAD source: 0-7 info index, 16 = VRAM→CPU pending, 17 = transferring, 0xFF idle |
| gp+0x365a | u8 | draw area is NOT inside the display area → drawing may be render-to-texture → invalidate the texture cache |
| gp+0x365b | u8 | draw area overlaps the display area |
| gp+0x365c | u8 | cost shift (from E1 bit 10 and gp+0x365d) |
| gp+0x365d | u8 | interlace field parity / double-buffer toggle |
| gp+0x365e | u8 | double buffering on |
| gp+0x365f | u8 | display range dirty (=2) |
| gp+0x3660 | u8 | display dirty (=2): something drew into the displayed area |
| gp+0x3663 | u8 | screen mode (index into the display-scaling tables; from 0x25344, user setting, inferred) |
| gp+0x3664 | u8 | eDRAM 4 MB (slim) |
| gp+0x3668 | u32 | display mode word: bit 11 = PAL, 12 = 24-bit, 13 = interlace (GP1(08) bits 3/4/5 at byte 0x3669) |
| gp+0x366b / 0x366c | u8 | GP0 words still needed / collected |
| gp+0x6ac | u32 | per-game option flags (database). Bit 0 = never invalidate the texture cache on offscreen draws; 3, 6, 11, 15, 29 alter present/copy paths; bit 4 = call 0x125f0 after block DMA; bit 24 = fill x not 16-aligned |
| gp+0x6dc | u32 | 60 Hz sub-tick divisor (low 16) and multiplier (bits 16-30) |
| gp+0x6e8 | u32 | GPU cost scale: low byte = right shift, >>8 = left shift |
| gp+0x708 | i32 | advance of the vblank IRQ before the visible-area end (§4.4) |
| gp+0x70c / 0x710 / 0x718 | u32 | extra cost for a texture bind / per GPUREAD / shift for VRAM copy cost |
| gp+0x714 | u32 | texture-window helper (TEXOFF values) |

#### 1.3 GP0/GP1 write handler 0x127d8

```
write(addr, val):
  if (addr & 4) == 0:                       # GP0
     GPUSTAT &= ~(1<<26 | 1<<28)            # "busy"
     if state == 9:                         # CPU->VRAM data words
        collect up to min(w*h, 96) pixels (≤48 words) then run executor on them
     elif state == 8:                       # polyline
        if (val & 0xf000f000) == 0x50005000: terminate
     else:
        n = LEN[val>>26]  (table 0xd5338; cmd 0x02 special = 2)
        collect n extra words in gp+0x3500
     when packet complete:
        cyc = exec(gp+0x3500, bytes)        # 0x133d0
        if cyc: schedule(gp+0x35f8, cyc)    # 0x945c; GPU stays busy for cyc cycles
        else:   GPUSTAT |= 1<<26 | 1<<28
        if state==9 chunk and gp+0x3658 > 0: listptr += 4; 7014C540(listId, listptr)
  else: GP1 via jump table 0xd5378 (0..16)
```

Extra-word table 0xd5338 (by `cmd>>2` for polygon groups):

| Command | Extra words |
|---|---|
| 0x20 | 3 |
| 0x24 | 6 |
| 0x28 | 4 |
| 0x2c | 8 |
| 0x30 | 5 |
| 0x34 | 8 |
| 0x38 | 7 |
| 0x3c | 11 |
| lines | 2 or 3 |
| rects | 2 / 3 / 1 / 2 |
| 0x80-0x9f | 3 |
| 0xa0-0xdf | 2 |
| others | 0 |

GP1 commands:

| GP1 | Handler | Effect |
|---|---|---|
| 00 reset | 0x129b4 | Close the open list: FINISH/END, `UpdateStallAddr(id, 0)`. Then EnQueue the static reset list (module 0xd5008) and the eDRAM draw-state sub 0x041b9300. GPUSTAT = 0x1c800000; H = 0x200/0xc00; V = 0x10/0x100; texpage = 0xc000; list ptr = 0x49a00000. `sceGeDrawSync(0)`. `EnQueue(0x49a00000, stall=0x49a00000)` → id. |
| 01 reset FIFO | 0x12aac | FIFO state cleared |
| 02 ack IRQ | — | no-op |
| 03 display enable | 0x12aec | GPUSTAT bit 23, display dirty |
| 04 DMA direction | 0x12b30 | GPUSTAT bits 29-30; bit 25 = (dir != 0) |
| 05 display start | 0x12b4c | x, y saved; overlap flag 0x365b recomputed against the draw area; dirty |
| 06 / 07 | 0x12be8 / 0x12c20 | H / V range, dirty if changed |
| 08 mode | 0x12c48 | byte 0x3669; dirty if (old ^ new) & 0x5f |
| 09-0F | — | no-op |
| 10 info | 0x12c68 | GPUREAD source = index & 0xf |

#### 1.4 Read handler 0x12fbc

GPUSTAT read:
- Costs 1 cycle.
- **Busy-poll skip:** if fewer than 30 cycles have passed since the previous GPUSTAT read, emulated
  time is fast-forwarded to the next 2048-cycle boundary from gp+0x35c4. A tight `while(!(GPUSTAT &
  ready))` loop therefore burns no host time.
- The returned value is assembled from the GPUSTAT image:
  - bits 17-22 from the display mode;
  - bit 16, bit 14 and bits 0-10 from texpage;
  - **bit 31 = ((now − gp+0x35c4) >> 11) & 1**, a synthetic odd/even line toggle every 2048 cycles.

GPUREAD:
- Costs gp+0x710 cycles.
- `0x130bc(gp+0x35bc, 4)` returns the next word of a VRAM→CPU transfer, or the info value. The info
  table at 0xd53bc gives index 2 = texture window, 3/4 = draw area, 5 = offset.

#### 1.5 DMA2 0x12c74(MADR, BCR, CHCR)

**Linked list (CHCR & 0x400): no copy, in place.**

```
budget = 2153 cycles per slice
p = MADR
loop:
   hdr = RAM[p]; n = hdr>>24
   cyc += exec(p+4, n*4) scaled by gp+0x6e8      # packets parsed straight from PS1 RAM
   cost += n + 7                                  # DMA bus cost per node
   next = hdr & 0xffffff
   end if next & 0x800000, or next == p, or next is a 2-node loop
   if cost > budget:
       E7F06E2B(listptr)                          # let the GE run what is queued
       0x8bb8(2, cost, budget)                    # DMA completion timing
       adjust gp+0x1b0
       schedule continuation event gp+0x220, callback 0x8cac
       return next | 0x80000000                   # new MADR
at end: GPUSTAT |= 1<<28; schedule(gp+0x35f8, cyc)
```

The stall is advanced **once per slice or per transfer**, never per primitive.

**Block, to GPU (CHCR & 1):**
- A partial FIFO packet is merged when the total is under 192 B.
- `exec(RAM block)`, then E7F06E2B, then the busy event is scheduled.
- Option bit 4 calls 0x125f0, which forces a present check.

**Block, from GPU (VRAM→CPU):**
- First zeroes the dynarec block table (0x09C00000 + offset) for the destination range, because
  code may be overwritten.
- Then calls `0x130bc(dst, bytes)` (§5).

#### 1.6 GP0 executor 0x133d0(ptr, bytes)

Prologue checks:
- If listptr > 0x49b7b800 → **wrap** (0x15f2c): write FINISH/END, `E7F06E2B(0)`, then the restart
  path below.
- If gp+0x3658 < 0 → **restart** (0x15f04): `sceGeListSync(id, 0)`, then
  `id = sceGeListEnQueue(0x49a00000, stall=0x49a00000, -1, 0)` and listptr = 0x49a00000.

It then dispatches on the FIFO state or `cmd>>5` through the table at 0xd53dc. It returns the
accumulated cost (s7), in cycles.

| Index | Handler | Command class |
|---|---|---|
| 0 | 0x13468 | misc: 0x01 clear cache, 0x02 fill, NOP |
| 1 | 0x13704 | polygons 0x20-0x3f |
| 2 | 0x144bc | lines 0x40-0x5f |
| 3 | 0x14760 | rectangles 0x60-0x7f |
| 4 | 0x14fd4 | VRAM→VRAM 0x80 |
| 5 | 0x154a4 | CPU→VRAM 0xa0 header |
| 6 | 0x15508 | VRAM→CPU 0xc0 header |
| 7 | 0x15554 | environment E1-E6 |
| 8 | 0x157c4 | polyline continuation (re-enters the line code at 0x145f0 with the stored last vertex) |
| 9 | 0x157d8 | CPU→VRAM data |

---

### 2. Primitive → GE

#### 2.1 The display list

| Item | Value | Evidence |
|---|---|---|
| Buffer | 0x49a00000–0x49b7b800 (uncached RAM, ≈1.48 MB) | GP1(00) 0x129b4; wrap test in 0x133d0 |
| Lists | One open list, `sceGeListEnQueue(base, stall=base)`; id at gp+0x35cc | 0x1b990, 0x15f1c, 0x1251c |
| Stall advance | `sceMeAudio_E7F06E2B(stall)` after DMA slices and blocks; `sceMeAudio_7014C540(listId, stall) → id` after CPU-side syncs (VRAM→CPU, CPU→VRAM slow paths, packet-by-packet CPU writes when gp+0x3658 > 0). `sceGeListUpdateStallAddr` is only used in GP1(00) and the init close (0x1b950) with stall 0. | call sites above |
| Per frame | At present the list is closed: stall released with `E7F06E2B(0)`, gp+0x3658 = −1, listptr = 0x49a00000 (0x117c4-0x117e8, 0x118f0). The next GP0 packet calls `ListSync` (waiting for the previous frame's GE work) and re-EnQueues at 0x49a00000. One GE list per PS1 frame, so effectively a single buffer and no double buffering of the list. | |
| Double buffering of the list | None. The CPU waits for the GE lazily at the first packet of the next frame. | |

Inferred: the frame tail must end the list with FINISH/END for the ListSync to return. The words
are written by the present code (0x11dcc-0x122c4), but they were not isolated. To confirm, decode
the tail words written at 0x120ac-0x12210.

#### 2.2 eDRAM subroutines

These are built at init in eDRAM, reached by `OFFSETADDR 0x041b9000` (0x13041b90) + relative CALL.

| eDRAM | Size | Content |
|---|---|---|
| 0x041b9000 | 0x40 | TRX copy kick, 16- and 32-bit variants (CALL 0x00, 0x08, 0xc4 = kick with source = VRAM) |
| 0x041b9040 | | clear: 16 sprites of 64×512 covering the VRAM with the MATAMBIENT color (used by GP0 0x02 fill under scissor) |
| 0x041b9080 + 16·mode | | blend presets (§3.4) |
| 0x041b90c0 | | 256×256 TRX |
| 0x041b9100 + 16·mode | | mask-bit stencil presets (§3.5) |
| 0x041b9200 | 100 B, module 0xd5150 | display blit state: MATAMBIENT = brightness grey; FBW 512 / FBP 0x1bc000; DITH0-3; TBW0 1024; TEXWRAP clamp; TPSM 5551; FBFORMAT 5650; DITHEN; TSIZE 512×512; bilinear; TEXFUNC 0x104 (replace, RGBA); TEXEN; VTYPE through + tex16 + pos16; VADDR 0x041bbc00; blend/atest/stencil/fog off; TEXFLUSH; TEXSYNC; RET |
| 0x041b92c0 | | screen border clear |
| 0x041b9300 | 76 B | PS1 draw state: the PS1 dither matrix exactly; nearest filter; repeat wrap; STENCILEN 1; ATESTEN 1; FBP 0, FBW 1024, FBFORMAT 5551; TEXFUNC 0x10100 = modulate, RGBA, ×2 color doubling (PS1 texel·color/128) |
| 0x041b9400 / 0x041b94c0 | | 24-bit setup and 10 × 32-B strip templates (§4.2) |
| 0x041ad800 | 1 KB | identity CLUT 8888: entry i = i·0x01010101 |
| 0x041a0000 | | staging rows (24-bit strips; overlapping VRAM copies use y = 832 of a 1024-wide frame = 0x041a0000) |
| 0x041bba00 | | 24-bit channel-pass vertices: 10 strips × 3 channels × 20 B |
| 0x041bbc00 | | display blit vertices |
| 0x041bbd00 | | full-VRAM strip vertices 16 × (64i, 0)-(64i+64, 512) |
| 0x041bc000 | | PSP framebuffer (fat), 512 × 272 5650, ending exactly at 0x04200000 |

RAM templates (64 B each) are copied at init to uncached RAM:
- 0x49b7f800: 16 polygon variants from table 0xd52bc.
- 0x49b7fe00: 8 from 0xd5318.
- 0x49b7fd00: 4 from 0xd52fc.
- 0x49b7fc40: 3 from 0xd530c.
- 0x49b7fc00: 24 B from 0xd4a38.

#### 2.3 How a polygon is emitted (0x13704)

```
cost += 30
if textured and packet texpage != gp+0x3654:  # or bit 14 set
    OFFSETADDR 0x041b9000; CALL 0x80 + 16*blendmode
    rebind texture (3.1/3.2): TBP0/TBW0/TPSM/TSIZE/TEXSCALE/TEXOFF/TEXFLUSH/TEXSYNC, cost += gp+0x70c
    if CLUT changed: CBP=VRAM+clut*32, LOADCLUT
vertices: x,y = s11 sign-extended
          signed area via mult/msub
          reject if max(dx, 2*dy) >= 1024          # PS1 limits 1023×511
flat color: MATAMBIENT word = 0x55000000 | rgb  (raw texture, cmd bit 24: 0x55808080)
emit:  ORIGIN (0x14000000)
       CALL  (template 0x49b7f800 + 64*variant) encoded RELATIVE to ORIGIN
       second CALL/JUMP word into the 0x49b7fe00 set (STP/mask pass)
       inline vertex data (VADDR in the template = ORIGIN + 0xc)
       -> 76 bytes per triangle in the list
```

**Relative CALL trick.** The CALL word is `(0x53b7f800 | variant<<6 | flags<<2) − listptr`. With
listptr ≈ 0x49axxxxx the top byte comes out as 0x0a (CALL) and the low 24 bits are the offset of
the template from ORIGIN. One subtraction yields a ready CALL word, and the same holds for 0x51b7fe00
→ 0x08 (JUMP).

The variant index comes from cmd bits 25-28:
- gouraud,
- quad,
- textured,
- semi-transparent.

Template meaning (decoded from 0xd4a50..):

| VTYPE | Vertex layout | Use |
|---|---|---|
| 0x100 | s16 x, y, z | flat untextured |
| 0x101 | u8 u, v + s16 x, y, z (8 B: `u | v<<8 | x<<16`, `y | z<<16`) | flat textured |
| 0x11c | 8888 color + s16 xyz | gouraud untextured |
| 0x11d | u8 uv + 8888 + s16 xyz | gouraud textured |

All are **transform mode**. Untextured templates `JUMP` back to the list at +0x24 or +0x38;
textured ones enable DITHEN/TEXEN/FOGEN, set SOP and RET. Gouraud untextured uses ATEST always and
DITHEN.

Sprites/rects (0x14760) use the same scheme with templates 0xd4cb0/0xd4cd0 (untextured) and
0xd4cf0/0xd4d20 (textured). Size presets 1×1, 8×8 and 16×16 are taken from the command.

**Transform setup that replaces vertex arithmetic** (static reset list 0xd5008):
- OFFSETX 1536, OFFSETY 1792.
- Viewport scale 512/−256, center 2048.
- Projection [1/512, −1/256, translate −1, 1].
- View diag(0.5, 0.5, 1).
- World diag(65536, 65536, 0), translation (2·offx+1, 2·offy+1, 7).

Net effect: an s16 vertex lands on screen at x + offx + 0.5, y + offy + 0.5. The +0.5 gives the
PS1 top-left fill convention. The drawing offset is handled by the GE: changing E5 rewrites 3
words (§3.6), not every vertex. z is multiplied by 0, so depth is unused (TEXSCALE 0.5, TEXOFF
−0.998 for u8 coordinates).

**Cost model (cycles charged to the guest, returned to the busy event).**
- Polygon: 30 + area term. The area term is the bbox inside the draw area, halved when untextured
  (cmd bit 26), with a semi-transparent/mask surcharge (cmd bit 25 or GPUSTAT bit 11), then shifted
  right by (gp+0x365c + 1). Code 0x13a98-0x13b40.
- Fill: (w·h) >> (gp+0x365c + 4).
- VRAM copy: w·h >> gp+0x718.
- Everything is finally scaled by gp+0x6e8 (DMA path).

**Fill 0x02 (0x134e0):**
- x &= 0x3f0 (unless option bit 24); zero size is skipped.
- Emit SCISSOR1/2 = rect, MATAMBIENT = color & 0xf8f8f8, OFFSETADDR 0x041b9000, CALL 0x40
  (full-VRAM sprites clipped by the scissor), then restore the scissor from E3/E4. That is 28 B.
- Invalidates overlapping texture-cache groups; sets display dirty if it hits the display.

**Clear cache 0x01:** CBP 0x1bc000 + LOADCLUT 1 (a dummy load that flushes the CLUT cache), and
texpage bit 15 is set.

#### 2.4 Lines (0x144bc) and polylines

Same emitter with line templates. The polyline state (8) keeps the last vertex in gp+0x3640/3644
and re-enters 0x145f0 for each new vertex until a word matches `(w & 0xf000f000) == 0x50005000`.

Not decoded in detail: the line template set and whether lines are GE LINES or thin sprites.
Confirm with `pops_gelist.py` on the templates referenced from 0x144bc.

---

### 3. Textures, blending, mask, dithering, clipping

#### 3.1 15-bit textures

Sampled **directly from PS1 VRAM** (0x14078): `TBP0 = 0x04000000 | y<<11 | x<<1`, TBW 1024, TPSM
5551. Stride 2048 B is legal for 16-bit, so nothing is copied.

#### 3.2 4/8-bit textures: the texture cache

The GE cannot sample T4/T8 with a 2048-byte stride, so CLUT pages are **copied with GE TRX into a
cache with a 512-byte stride**: 1024 px for T4, 512 px for T8.

| Item | Value |
|---|---|
| Buffer | 0x04360000 in eDRAM when `sceMeAudio_2AC64C3F() == 0x400000` (4 MB eDRAM, PSP-2000/3000). Otherwise `memalign(1024, 0xa0000)` in RAM (PSP-1000). Init 0x1b9c4. |
| Descriptors | gp+0x3400 (y 0-255) and gp+0x3480 (y 256-511): 16 × 8 B `{u32 ptr, u8 valid@+4, s8 column_in_group@+5, u16 TRX dst pos@+6}` |
| Grouping | PS1 VRAM columns of 64 halfwords grouped {0-2}, {3-5}, {6-8}, {9-11}, {12-15}. Each group = 256 rows × 512 B = 0x20000. 5 groups = 0xa0000 = the buffer size. |
| Miss (0x13fc8) | Mark the group's entries valid (bytes at +4, +12, +20, +28). Emit TRXDBP/TRXDBW = buffer ptr and stride, TRXSPOS from the descriptor, then `CALL 0xc4` (TRX kick with source = PS1 VRAM). Then TBP0/TBW0 point into the buffer. |
| CLUT | Normal path: `CBP = VRAM + clut*32` directly in PS1 VRAM, LOADCLUT. Second path (0x14034): the CLUT is first copied by TRX to 0x041a0000+ and loaded from there (condition not established; probably a CLUT not usable in place). |
| Invalidation | Any write into a group's VRAM rect clears its `valid` bytes and sets texpage bit 15 (forces a rebind before the next textured primitive): fills, VRAM copies, CPU→VRAM, polygons/rects when gp+0x365a = 1 (drawing outside the display area = render-to-texture, unless option bit 0). At 24-bit present, entries whose buffer is below 0x04200000 are dropped because the staging overlaps them (0x11c6c). |

Open: two descriptor sets (y halves) × 0xa0000 would need 1.25 MB, but only 0xa0000 is allocated.
Inferred: both halves share the 5 group slots, with `valid` deciding the occupant. Confirm by
reading the descriptor fill loop in 0x1b9c4 (ptr values for gp+0x3480).

#### 3.3 Texture window (E2, 0x155a0)

```
mask_x = E2 bits 0-4, mask_y = 5-9, off_x = 10-14, off_y = 15-19   (8-px units)
size_x = lowest_set_bit(mask_x | 0x20); size_y likewise
gp+0x3652/53 = size (-> TSIZE via clz), gp+0x3650/51 = offset (-> TEXOFF/ TEXSCALE)
texpage |= 0x8000   (rebind)
```

This is an **approximation**: the GE repeat wrap is used with a power-of-two size equal to the
lowest set mask bit, plus a texture offset. It is exact for the common cases of a single
contiguous power-of-two window.

#### 3.4 Semi-transparency → GE blend (presets at 0x041b9080 + 16·mode)

| PS1 mode | Formula | GE |
|---|---|---|
| 0 | B/2 + F/2 | FIX A = 0x7f, FIX B = 0x7f, ADD |
| 1 | B + F | FIX 0xff / 0xff, ADD |
| 2 | B − F | REVERSE SUBTRACT (dst − src) |
| 3 | B + F/4 | dst + 0.25·src |

Re-emitted lazily: the E1 or texpage change sets bit 14, and the next textured or semi-transparent
primitive emits `OFFSETADDR 0x041b9000; CALL 0x80+16·mode` (0x140a8).

**STP bit (textured semi-transparent), two passes** (templates 0xd4d54/0xd4d64):
- Pass 1: ATEST alpha != 0 (texels with STP = 1), blend on, SOP INCR (sets mask).
- Pass 2: VADDR rewound, ATEST alpha == 0, CTEST color != 0 (drops 0x0000 = transparent texels),
  BLENDEN 0, SOP ZERO.

The "raw texture" variant (cmd bit 24) uses TEXFUNC 0x103 with LOGICOPEN.

#### 3.5 Mask bit (E6) → stencil (0x15700, presets 0x041b9100 + 16·mode)

The 5551 framebuffer alpha bit is the PS1 bit 15 and serves as a 1-bit stencil.

| E6 set / check | GE |
|---|---|
| 0 / 0 | STEST ALWAYS, SOP ZERO |
| 1 / 0 | ALWAYS, SOP INCR |
| 0 / 1 | NOTEQUAL, ZERO |
| 1 / 1 | NOTEQUAL, INCR |

E6 also writes GPUSTAT bits 11-12. These bits route VRAM copies and CPU→VRAM to the CPU slow path
whenever set-mask is on, because TRX cannot OR bit 15.

#### 3.6 Draw area, offset, dithering

| Command | Mapping |
|---|---|
| E3 / E4 | `SCISSOR1/2 = 0xd4/0xd5 << 24 | x | y<<10`. The PS1 bit layout (x 10 bits, y 9 bits at bit 10) is the GE scissor layout, so the packet word is inserted as is (`ins v1,s0,0,19`). Then the overlap flags 0x365a/0x365b are recomputed. |
| E5 | x = s11 (bits 0-10), y = s11 (bits 11-21). If draw_x1 − x ≥ 1024 then x += 2048. Emits `WMS 9` (0x3a000009) plus two `WORLDD` words holding (2x+1) and (2y+1) as floats. The float→command trick: `ror(float_bits(2x+1) + 59, 8)` yields a 0x3b command byte (low byte 0 + 0x3b) carrying the top 24 bits of the float. Costs 3 words. |
| Dithering | The PS1 matrix is in DITH0-3 (0x041b9300); DITHEN is set per template (gouraud and textured). Function 0x1b6ec (called at init) builds the dither setup; not decoded. Whether E1 bit 9 toggles dithering is not established (search the E1 path and templates for DITHEN 0). |

---

### 4. Display

#### 4.1 Present: `0x115b4`, called from the vblank event 0x11410, the 60 Hz ticker 0x12710, 0x125f0 and 0x15fa4

```
present():
  if a 24-bit/menu override is active: skip (gp+0x3667 == 8 etc.)
  if mode.interlace changed: 0x1b7c8 (sceMeAudio_69C4BCCB)          # inferred: tells popsman the field mode
  idle-frame counter gp+0x366a (option bit 15): 60 frames with nothing drawn → 0x1af14(0)
  r = 0x30c24()   # pause/HOME menu state machine; codes 0x0f,0x44,47..59,0x57,74..77 → 0x2c8a4 (UI)
  if display enabled:
     write frame tail at listptr:
        OFFSETADDR 0x041b9200; CALL 0xc0 → display blit state (2.2)
        slim double buffer: patch FBP/FBW at uncached 0x441b9204/08:
          FBW 704 (0x9d0402c0), FBP 0x04269a00 or 0x042c4080 (toggle gp+0x365d bit 1,
          370,304 B = 263 rows × 704 × 2 apart)
        15-bit: sprites from VRAM (display start, width/height from 0x361c/0x361e) to the FB,
                optional deinterlace TRX (option bit 11, interlaced: copy the other field's rows)
        24-bit: §4.2
  invalidate texture-cache groups (loop 0x11784)
  texpage |= 0xc000; listptr = 0x49a00000; gp+0x3658 = -1     # list closed, restart on next packet
  re-emit PS1 state for the next frame: WMS 9 + WORLDD x,y (offset), SCISSOR1/2, OFFSETADDR 0x041b9300 + CALL
  do { v = sceDisplayGetVcount(); if v - (last+1) < 0: 0x3728c(); sceDisplayWaitVblankStartCB() } while v < last+1
  sceMeAudio_E7F06E2B(0)          # release the stall: the GE finishes the frame
  gp+0x35c0 = v; save audio counter / emulated time for pacing (§4.5)
```

The PSP framebuffer is **never re-pointed by pops_01g** for game frames. The only
`sceDisplaySetFrameBuf` call is 0x34fc8 (512 wide, 5650, at 0x04000000, after copying 0x44000 B
from RAM 0x09410000), which belongs to a menu/screenshot function. Inferred: game frame display
addresses (fat 0x041bc000, slim 0x04269a00/0x042c4080) are set by popsman. Confirm in popsman.prx
(sceDisplaySetFrameBuf call sites).

Scaling to 480×272:
- Per-mode tables are relocated module data at 0x24 (9 B per H-resolution entry; byte 7 =
  width/16) and 0x30 + 6·screen-mode (vertical scale bytes).
- Visible height = min(V2 − V1, 280), doubled when interlaced.
- Vertical destination = ((Vrange − 16 NTSC / 25 PAL) · (300 + tbl[4]) >> 8) + tbl[1]
  (0x11e44-0x11e68).
- The sprites are drawn through mode with bilinear filtering (TEXFILTER 0x101) and dithering into
  5650. The user's screen mode (gp+0x3663, from 0x25344) selects the table row.

The exact list of supported modes (original / normal / full / zoom) is inferred from the table
shape; confirm by dumping module 0x24 and 0x30.

#### 4.2 24-bit display

Fast path, GE only, used when display y + h < 512:
1. For each 64-output-pixel strip (192 B = 48 words of the 24-bit line), patch `TRXSPOS(x/2 +
   48·i, y)` into the 32-B templates at uncached 0x441b94c0 + 32·i. The strip count is width/64,
   and RET (0x0b000000) closes the last one.
2. Each template: `TRXSPOS; TRXKICK 32-bit` to staging 0x041a0000 (stride 48 words); TEXFLUSH;
   TEXSYNC; LOGICOPEN 0; PRIM sprites; LOGICOPEN 1; PRIM sprites.
3. The staging is sampled as **T8 with the identity CLUT** (0x041ad800, 8888) three times. Each
   pass uses a channel-mask vertex color (0xf / 0xf0 / 0xf00 in 4444 vertex colors from the
   0x041bba00 table) with u shifted by one byte, and the results are **ORed** (LOGICOP OR) into an
   8888 image at **0x04100000, width 640**.
4. The present blit then binds `TBP0 0x04100000` (TBW 640, TPSM 8888) and scales it like the 15-bit
   path.

Slow path (0x12334), when the area wraps the VRAM:
- Sync the list (7014C540).
- The CPU unpacks with unaligned `lwl/lwr` from cached VRAM 0x04000000, 4 pixels per VFPU quad
  (`mtv` ×3 + `lv.s` → `sv.q ...,wb`), into uncached 0x44100000 at stride 640×4.
- It zero-fills with `cache 0x19` + `sv.q` of a zero row.
- Then `TBW0 640 (0xa8040280)`, `TPSM 8888 (0xc3000003)`.

#### 4.3 GP1 display state

GP1(05/06/07/08) only store values and set dirty flags. Everything is consumed at present time.
The display enable bit (GPUSTAT 23) gates the blit.

#### 4.4 Vblank and frame timing (events on node gp+0x35e4)

| Event | Code | Period |
|---|---|---|
| Vblank start | 0x11410: `0x9940(1)` (vblank signal/IRQ 0), `0x96e4(1)`; present if the display start changed (or always, depending on PAL / option bits 3 and 29) | next = visible part, callback 0x1265c |
| Visible area | 0x1265c: `0x9940(0)`; next = (V2 − V1) × **2152.550 cycles** (NTSC) / **2167.603** (PAL), minus gp+0x708 | callback 0x15fe4; if that is ≤ 0, call 0x15f54 directly |
| Blanking | from 0x11410: (525 + k − 2·(V2 − V1)) half-lines × **1076.275** (NTSC) / **1083.802** (PAL) cycles, where k = 0x6467 >> (8·interlace) & (129 − 2·PAL) → NTSC 1/0, PAL 103/100 (non-interlaced / interlaced) | — |

Example, NTSC 240 lines: 240·2152.55 + 46·1076.275 = 566,121 cycles/frame = 59.83 Hz at
33.8688 MHz.

60 Hz sub-ticker (event gp+0x35d0, 0x12710):
- Period 564480 / N cycles (564480 = 33868800/60 exactly), N = gp+0x6dc low 16, counted down in
  gp+0x35e0.
- Each tick calls the pacing check 0x11520.
- With option bit 3, the last tick presents. The frame rate is then independent of the game's
  V-range.

#### 4.5 Frame pacing against the audio clock (0x11520)

```
a = *(u32*)0x49f40294          # sample counter in uncached RAM, advanced by the ME audio loop (inferred)
ahead = (now_emulated - gp+0x360c) - (a - gp+0x3608) * 768     # 768 PS1 cycles per 44.1 kHz sample
if ahead >= 3387:                                               # ~0.1 ms
    if 24-bit display (gp+0x3668 bit 12) and ahead < (a-delta)*768/2: return          # movies get slack
    sceKernelDelayThread((ahead * 1741 >> 16) >> (elapsed > 0x84e10 ? 1 : 0))   # µs ≈ 0.0266 per cycle
```

768 × 44100 = 33,868,800, so the sample counter is a PS1-cycle-exact clock. The emulator sleeps
when it runs ahead of audio output, which keeps audio gap-free. The vsync wait in present() only
limits presents to one per PSP vblank. That the counter belongs to the ME is inferred; confirm in
popsman (who writes 0x09f40294).

---

### 5. CPU access to VRAM and coherency

| Path | Mechanism |
|---|---|
| VRAM→CPU, fast (0x130bc) | When dst is 16-aligned, w % 8 == 0, the rect is in bounds and one DMA covers w·h·2: emit TRXDBW (dst hi \| w), TRXDBP, TRXSPOS(x, y), TRXSIZE, `OFFSETADDR 0x041b9000`, `CALL 0xc4` → GE copies VRAM into PS1 RAM. `7014C540` advances the stall. Then the **dcache is invalidated** over dst: `cache 0x1b` on unaligned edges, `cache 0x19` per line, or `sceKernelDcacheInvalidateRange` above 128 KB. |
| VRAM→CPU, slow | After the gp+0x3658 sync: halfword reads from uncached `0x44000000 \| y<<11 \| x<<1`. |
| CPU→VRAM, fast (0x157d8) | The pixel words stay in PS1 RAM (or the FIFO); the GE pulls them. `cache 0x1a` writeback per 64-B line over the source (or `sceKernelDcacheWritebackRange` ≥ 128 KB). Then TRXSBP = src & 0xffffff, TRXSBW = (src >> 24 & 0xf) << 16 \| w, TRXSPOS(src & 0xe >> 1, 0) (source 16-B alignment absorbed into x), TRXDPOS(x, y), TRXSIZE, `CALL 0x08` (16-bit kick). Then texture-cache invalidation. |
| CPU→VRAM, slow (0x15d54 / 0x15e70) | Used when set-mask is on (GPUSTAT 11) or x + w > 1024. Sync the list (7014C540; gp+0x3658 = 0), then the CPU writes halfwords to uncached `0x44000000 \| y<<11 \| x<<1` with bit 15 ORed. |
| VRAM→VRAM (0x14fd4) | Fast: no mask, x + w ≤ 1024 → TRXSPOS/TRXDPOS/TRXSIZE + `CALL 0x00`. If source and destination overlap: bounce through staging (TRXDPOS y = 832 → 0x041a0000) and copy back. 1×1 copies also patch the texture cache copy directly (0x15284). Slow (mask on, or wrap): list sync, then a CPU loop over cached 0x04000000 honoring direction for overlap, ORing bit 15, then `sceKernelDcacheWritebackInvalidateRange(0x04000000, 1 MB)`. |
| Option bit 6 | An alternative copy path at 0x15478 (not decoded). |

CPU→VRAM data cost: half the pixel count (`(w·h + 1) >> 1`), plus the cache writeback.

Every CPU touch of VRAM is preceded by a stall advance plus an implicit wait: `7014C540` returns the
id after the GE has consumed up to the stall (inferred from its use before CPU reads). Confirm in
popsman (implementation of NID 7014C540).

`sceGeDrawSync` appears only in GP1(00) reset and init. Normal operation never waits for the whole
GE.

---

### 6. Speed tricks (what makes POPS fast)

1. **Packets parsed in place.** DMA2 linked lists are executed directly from PS1 RAM, with no copy
   into a FIFO. Partial-FIFO merging only happens below 192 B.
2. **Precompiled GE templates plus relative CALL.** A triangle costs 3 control words plus inline
   vertices (76 B). All state lives in 64-B RAM templates and eDRAM subroutines, chosen by one
   subtract (`template − listptr` gives a CALL word).
3. **Lazy state.** Blend preset and texture bind are re-emitted only when texpage bits 14/15 are
   dirty. The CLUT is reloaded only when it changes. The texture cache is valid until a write
   overlaps it.
4. **The GE does the math.** The drawing offset and the half-pixel offset are in the world matrix
   (E5 = 3 words); scissor = PS1 bits verbatim; dither matrix = PS1 matrix; color doubling =
   TEXFUNC ×2; mask bit = stencil.
5. **Stall advanced per batch.** Once per DMA slice of 2153 cycles, per block or per sync. There is
   one list per frame and no per-primitive kernel call.
6. **Busy-poll skip** on GPUSTAT (<30 cycles between reads → jump to the 2048-cycle boundary) and
   the synthetic bit 31.
7. **GE TRX for all bulk VRAM traffic**: VRAM↔RAM, VRAM↔VRAM, CPU→VRAM from PS1 RAM, texture cache
   fill and 24-bit unpacking. The CPU only touches VRAM in mask/wrap cases.
8. **24-bit video without CPU per-pixel work**: TRX + 3 T8 passes with LOGICOP OR. The VFPU
   fallback is for the wrap case only.
9. **Pacing on the audio clock** (768 cycles/sample) with sceKernelDelayThread, plus a per-game cost
   scale (gp+0x6e8) and option bits from the compatibility database.
10. **Not found:** no primitive merging or reordering, no frame skipping of GPU work (every packet
    is drawn), and no VFPU in the primitive path (VFPU appears only in the 24-bit slow unpack).

---

### 7. Relevance for psxrt (facts, not prescriptions)

- One continuously stalled list, advanced per batch, is enough for full speed on a PSP-1000-class
  CPU. POPS never builds per-primitive kernel calls.
- Transform mode with a fixed matrix removes per-vertex offset and half-pixel math. It costs 3 GE
  words per E5.
- On slim, the texture cache and double framebuffers live in the extra 2 MB of eDRAM
  (0x04200000–0x043fffff, detected by sceMeAudio_2AC64C3F() == 0x400000).
- Mask-bit-set copies and wrapping transfers are the only CPU VRAM loops.

### 8. Open items (each with the cheapest confirmation)

| Item | How to confirm |
|---|---|
| Frame-tail FINISH/END words and exact 15-bit blit vertices | Decode 0x11dcc-0x122c4 word by word |
| Line primitive templates (GE lines vs sprites) | `pops_gelist.py` on the templates used at 0x144bc |
| Second CLUT path condition (0x14034) | Branch into 0x14034 inside 0x13704 |
| Texture cache descriptor sharing between the two y halves | Descriptor init loop in 0x1b9c4 |
| Fog enable in textured templates (constant ≈0.973 factor?) | FOGEND/FOGSLOPE use in templates and in 0x041b9300 |
| Who sets the PSP display framebuffer for game frames; who writes the audio counter 0x09f40294; what 7014C540 waits for | popsman.prx |
| 0x1b6ec (dither setup), 0x3728c and 0x34350 (called in vblank wait / present), 0x30c24 menu codes | Read those functions |

---

## Part F. POPS (pops_01g, fw 6.61): SPU, Media Engine mixer, CD-ROM, disc I/O, threads, memory card, pad

All addresses are pops_01g module offsets (load base 0) unless they are host absolutes (0x09xxxxxx,
0x49xxxxxx = uncached alias). `gp` = 0x00010000 (PSP scratchpad, emulator context). "PS1 now" =
`gp[0x1ac] - gp[0x1b0]` (next-event time minus cycles left). Handlers charge guest cycles by
decrementing `gp+0x1b0` (offset 432). Events: `0x945c(node, delay)` schedules, `0x9668(node)` cancels;
node = {0 next, 4 prev, 8 time, 12 callback}. `0x96e4(mask)` raises a PS1 interrupt (4 = CD IRQ2,
128 = SIO IRQ7, 512 = SPU IRQ9). Allegrex cache ops seen: 0x18 create-dirty-exclusive, 0x19
hit-invalidate D, 0x1a hit-writeback D, 0x1b hit-writeback-invalidate D.

Global addresses written as `D+x` are formed by `lui/addiu` pairs that land inside the text image
when read raw (e.g. 0xDE50 is in the middle of code). They are segment-relative relocations to the data
segment (va 0xfed00); inferred, confirm with the PRX relocation table. Code pointers built the same way
(thread entries) are text-relative and verified against the disassembly.

---------------------------------------------------------------------------------------------------
### 0. Ten-line summary

1. The SPU is fully emulated on the **Media Engine**. The mixer is the function at module offset 0
   (0x0–0x19dc, no CPU callers). The ME calls it once per 44.1 kHz output sample, and it returns
   `L | R<<16`.
2. CPU→ME handoff: the CPU writes SPU regs into an **uncached register shadow at 0x49F40000**. Under a
   **Peterson lock** (bytes 668–671 of the shadow: 669 CPU flag, 670 ME flag, 671 turn), it ORs
   dirty/KON/KOFF masks. The ME snapshots and clears the masks under the same lock, once per sample.
3. SPU RAM is 512 KB at host 0x09F402C0. The ME reads ADPCM through the cached alias and then does
   `cache 0x19`. The CPU writes it uncached (FIFO) or via memcpy/sceDmacMemcpy (DMA4).
4. The voice engine is psx-spx-exact in structure:
   - 4-tap Gaussian (Sony table, 512 entries), 28-sample ADPCM blocks, 5 filters;
   - full ADSR state machine, PMON, noise LFSR;
   - capture buffers (0x000/0x400/0x800/0xC00);
   - IRQ address checked on every block/reverb access;
   - reverb at 22.05 kHz alternating L/R.
5. **Emulated time is slaved to the ME sample counter** (shadow+660):
   - Each SPU write ≥768 cycles after the previous one waits until the ME has produced a new
     sample (768 = 33868800/44100).
   - `0x11520` sleeps the main thread when the PS1 clock leads audio by ≥3387 cycles.
6. CD-ROM is a register-exact controller (0x1800–0x1803) with a 31-entry command table at 0xd4810.
   - Responses are delivered by events: first response ~16384 cycles, second via a second node.
   - Seek model: linear below 7000 sectors, sqrt above, ×3/×8 multipliers, plus LCG jitter.
   - Sector period: 0x72400 = 468,992 cycles (1x), 0x3B200 (2x).
7. Disc image path: PSISOIMG 16-sector blocks (0x9300 B), **LZRC range-coder** decompressor 0xe02c.
   - Index at host 0x09E84000 (32 B per block). 17-entry LRU block cache at 0x09492600.
   - Read via the bounce buffer 0x09480000 with sceMeAudio_30BE34E4(fd, buf, offset, size).
   - **Read-ahead of the next block** on every hit, served by thread `cdworker` (prio 22) through
     event flag `cdread` (bit1 = request, bit2 = done).
8. XA-ADPCM: the CPU submits whole raw sectors (pointer ring of 8 at shadow+608) to the ME. The ME
   decodes the whole sector and resamples 37.8/18.9 kHz → 44.1 kHz with a 7-phase × 5-tap polyphase
   filter, then applies the ATV matrix.
   - CD-DA tracks are **ATRAC3-like 384-byte frames** (XOR-scrambled per track). `cdworker` decodes them
     on the CPU (0x3b300) into a PCM ring that reuses the block-cache memory, and they reach the ME as
     2352-B sectors.
9. Threads:

   | Thread / object | Entry | Priority | Details |
   |---|---|---|---|
   | `popsmain` | 0x16080 | 32 | own sp = 0x09BF0000; dynarec + all hardware except the SPU mix |
   | `cdworker` | 0xda3c | 22 | |
   | `mcworker` | 0x1aa90 | 27 | |
   | sema `mcWriteBack` | | | |
   | 1-s alarm | 0x1a9d0 | | |
   | `powerCB` / `msCB` callbacks | | | |
   | ME | | | runs only the mixer |

10. Memory card: two 128 KB images in RAM (D+0xDE50, stride 0x20088), emulated byte-by-byte on SIO0.
    - Persisted 1 s after the last write by `mcworker` through sceUtilitySavedata as SCEVMC0/1.VMP,
      with "\0PMV" header and pspvmc signature calls.
    - Pad: sceCtrlPeekBufferNegative (active-low like the PS1) is mapped to a digital pad (ID 0x41).
      The analog stick can drive the D-pad.

---------------------------------------------------------------------------------------------------
### 1. Host memory map (audio/disc)

| Host address | Size | Contents | Evidence |
|---|---|---|---|
| 0x09480000 | ≤0x12600 | bounce buffer for compressed PSISOIMG reads (512-aligned) | da3c: `lui s4,0x948`; d.c58 memcpy tail |
| 0x09492600 | 17 × 0x9300 = 0x9C300 | block cache, 17 buffers of 16 sectors × 2352 B. In CD-DA mode the same area is the PCM ring (0x93000 = 16 slots) | 1b2f0 init loop (`ori t0,0x2600`, step 0x9300, 17 nodes); ddac `0x49492600` |
| 0x09E80000 | 0x4000+ | PSISOIMG header copy (from popsman, inferred). TOC at +0x800 (10-B Q entries; first/last track BCD at +0x807/+0x811; track MSF at +0x81B+10·n). Audio track table at +0xBE0+16·t: {+0 file offset, +4 size, +12 scramble key} | d880 (3040/3044/3052 + 16·track), c078 |
| 0x09E84000 | 32 B per 16-sector block | block index (see §5.2) | d630, daf4 (`ori 0x4000`, `sll 5`) |
| 0x09F40000 / 0x49F40000 | 0x2C0 | SPU register shadow + CPU↔ME shared fields | SPU write 0x7f00, mixer prologue |
| 0x09F402C0 | 0x80000 | SPU RAM 512 KB | mixer/DMA4 |
| 0x09FF0000 | ~6044 B | ME private mixer state (`ra` base inside the mixer) | init 0x19dd8 |
| 0x09FF8000 | – | ME stack (sp passed to sceMeAudio_DE630CD2) | 0x1a02c |

#### 1.1 Shared fields at 0x09F40000 (offset = PS1 address − 0x1F801C00 for regs)

| Off | Writer | Meaning |
|---|---|---|
| 0x000–0x17F | CPU | voice regs (16 B/voice: volL, volR, pitch, start, ADSR lo/hi, ENVX, repeat) |
| +12+16v | ME | ENVX (current envelope) |
| +14+16v | ME/CPU | repeat address; the ME writes it on loop-start blocks |
| 384 (0x180) | CPU | main volume L/R |
| 388/390 | CPU | reverb output volume |
| 392 / 396 | CPU | KON / KOFF (raw) |
| 400 / 404 / 408 | CPU | PMON / NON / EON |
| 412 | ME | ENDX |
| 418 | CPU | reverb work-area start |
| 420 | CPU | IRQ address |
| 426 | CPU | SPUCNT |
| 430 | ME | SPUSTAT |
| 432 / 434 | CPU | CD volume L/R |
| 448–511 | CPU | reverb registers |
| 512+4v | ME | current volume L\|R (0x1E00 area) |
| 608+4i | CPU | XA/CD-DA sector-pointer ring, 8 slots |
| 640 / 644 | CPU | KON pending / KOFF pending masks |
| 648 | CPU | dirty mask: bits 0–23 = voice v changed, bit31 = reverb regs changed |
| 652–655 | CPU | CD ATV matrix (L→L, L→R, R→R, R→L), applied >>7 |
| 656 / 657 | CPU / ME | ring write index / ring read index (bytes) |
| 658 | CPU↔ME | IRQ-ack request (CPU sets; ME clears its IRQ latch) |
| 659 | CPU/ME | disc-audio mode: 1 = CD-DA, 2 = XA, −1 off, −2 reset handshake (ME clears) |
| 660 | ME | sample counter (+1 per output sample) — the audio clock |
| 668–671 | both | Peterson lock: 669 CPU wants, 670 ME wants, 671 turn |
| 672 | CPU | repeat-address-written-by-CPU mask |

---------------------------------------------------------------------------------------------------
### 2. SPU

#### 2.1 Install (0x19f8c) and init (0x19dd8)
- Handlers:
  - Read 0x85f4 for 0x1F801C00–0x1E5F.
  - Write 0x7f00 for 0x1C00–0x1DFF. Writes to 0x1E00+ go to the default handler, i.e. ignored.
  - DMA4 0x8698 is installed via `0x1a1f0(4, cb)`. DMA regs 0x1080+16·ch use read 0x9158 / write 0x92a4;
    the per-channel struct is at gp+0x1E8+28·ch (gp+500 is the +0xC callback field of ch0).
  - Event nodes:

    | Node | Callback | Purpose |
    |---|---|---|
    | gp+848 | 0x8580 | SPU IRQ poll |
    | gp+864 | 0x8898 | deferred DMA4 completion |

- Init 0x19dd8:
  - Zeroes the ME state 0x09FF0000..+6044; noise state 6030 = 0x80, 6028 = 1.
  - Builds the Gaussian table (2 KB at ME+0) from 128 entries at 0xd3c20, mirrored.
  - ADPCM filters from 0xd4020: f0 = {0, 60, 115, 98, 122}, f1 = {0, 0, 52, 55, 60}.
  - XA polyphase table from 0xd402c to ME+2048.
  - Constants 2128 = 32767, 2132 = 24576.
  - Zeroes shadow + SPU RAM (0x09F40000..0x09FC02C0).
  - Per-game config: gp+0x700 → 6036/6037 (voice-specific hack); gp+0x6AC bit 8 → 6031 (capture-index
    reset), bit 7 → 6038 (IRQ re-latch quirk).
  - Sets 648 = 0x80000000 (force reverb recompute), then sceKernelDcacheWritebackInvalidateAll.

#### 2.2 Write handler 0x7f00 (CPU side)
```
cost: 1 cycle (gp+432 -= 1)
byte write: odd addr -> value<<8 as halfword (other byte 0); word write = two halfword writes
off = addr - 0x1F801C00
if off == 0x1AE (SPUSTAT): ignore
if (off & 0xE) == 6 and off < 0x180 and value < 512: value = 514   // start addr never in capture area
shadow_uncached[off] = value
if off not in {0x1A4, 0x1A6, 0x1A8, 0x1AE}:                       // audio-clock throttle
    now = gp[0x1ac] - gp[0x1b0]
    if now - gp[888] >= 768 and shadow[660] == gp[892]:
        do sceKernelDelayThread(22) while shadow[660] == gp[892]
    gp[888] = now; gp[892] = shadow[660]
lock_cpu():  sb 1 -> 669; sb 1 -> 671; spin while lh(670) == 0x0101   // ME wants and ME's turn
  voice reg      -> 648 |= 1 << (off>>4); repeat (off&0xF == 14) -> 672 |= bit
  KON 0x188/18A  -> 640 |= v; 644 &= ~v      (only when v != 0)
  KOFF 0x18C/18E -> 644 |= v
  reverb regs / 0x1A2 -> 648 |= 0x80000000
unlock: sb 0 -> 669
0x1A8 data FIFO -> gp+776[32 halfwords], count gp+843
0x1A6 transfer addr -> gp+844 = v*4 (halfword units); IRQ check against shadow[420]
0x1A4 IRQ addr -> IRQ check
0x1AA SPUCNT -> 0x824c:
    gp+840 = value; if bit15 == 0 shadow stores value & 0x3F
    (bits4-5) == 1 (manual write): flush FIFO to SPU RAM uncached, wrap 18 bits, IRQ check -> gp+842 = 64
    bit6 (IRQ enable) set   -> schedule poll node gp+848 every 2153 cycles (if not linked)
    bit6 clear              -> cancel poll; shadow[658] = 1 (ack to ME); gp+842 = 0
```
- PMON/NON/EON are stored in the shadow only. The ME reads them every sample.

#### 2.3 Read handler 0x85f4
- Cost 10 cycles, +7 for a word.
- SPUSTAT lh/lhu = `(shadow[430] & 0xF7F) | gp842 | ((bit5) << 7)`.
- Everything else is a raw shadow read. So ENVX, ENDX and current volumes come from what the ME last
  wrote.

#### 2.4 SPU IRQ poll 0x8580 (event, period 2153 cycles ≈ 15.7 µs ≈ 0.69 samples)
- If shadow[658] == 0 and `((SPUSTAT | gp842) & SPUCNT-shadow) & 0x40`: raise IRQ9 (`0x96e4(512)`) and
  set 658 = 1. Otherwise reschedule.
- The ME latches IRQ hits in ME+6039 and mirrors them into SPUSTAT bit6.

#### 2.5 DMA4 0x8698(madr, bytes, dir)
- RAM = 0x09800000 | (madr & 0x1FFFFF). Size is clipped at the 512 KB end. gp+844 = end address.
  IRQ check over the range.
- Config gp+1794 > 0 → completion is deferred by 4 cycles/word through node gp+864 (madr | 0x80000000).
  Otherwise it completes at once. The return value depends on config gp+1708 bit 9.
- Write: writeback the source lines (or DcacheWritebackRange when ≥128 KB). Then Kernel memcpy (≤4096 B)
  or **sceDmacMemcpy** to the uncached SPU RAM.
- Read: invalidate the destination, zero the dynarec block-table entries of the destination
  (+0x400000 = 0x09C00000 table), then copy.

#### 2.6 The ME mixer (offset 0x0–0x19dc), one call per output sample
- Registration at 0x1a02c–0x1a038:
  ```
  sceMeAudio_C93C56F8((word@D+0x60000+17720 << 14) + 0x8000)   // meaning unknown
  sceMeAudio_DE630CD2(func = module_base + 0, sp = 0x09FF8000)
  ```
  popsman's ME loop (popsman+0x2F88, from earlier popsman notes, unverified) waits on the 44.1 kHz audio FIFO and calls it.
- Registers: gp/a0 = 0x49F40000 (uncached shadow), s8 = 0x09F40000 (cached), ra = 0x09FF0000 (ME state).

```
mixer():
  cache 0x19 on the shadow lines holding 400.. and 640..         // drop stale copies
  sh 1 -> 670 ; spin while (word@668 & 0xFF00FFFF) == 0x100       // CPU wants && turn == CPU
  shadow[660]++                                                   // audio clock
  dirty=648; rep=672; kon=640; koff=644 ; zero all four
  sb 0 -> 670                                                     // ME critical section = snapshot only
  if !(SPUCNT & 0x8000): reset voices + disc ring (0x1968); return 0
  if shadow[658]: ME+6039 = 0; shadow[658] = 0                    // IRQ ack
  noise step (0x11c-0x184): LFSR ME+6026, timer ME+6028, mask ME+6030;
      feedback = b15 ^ b12 ^ b11 ^ b10 ^ 1; timer reload 16384 >> shift(SPUCNT 10-13),
      step bits 8-9 via rotating mask                       (approximation of psx-spx; inferred)
  for v in 0..23 (state ME+2136+116*v), masks rotated 1 bit per voice:
      if dirty: reload vol/ADSR/pitch/repeat; ADSR word changed -> state = (state & ~3) - 4
      if KON:  addr = start & 0xFFFE; env = 0; ctr = 0xFFFE4000; state = 5; ENDX &= ~bit
      if KOFF and state < 24: state = 24 (release)
      step = pitch (clamp 0x3FFF); if PMON: step = step*(prev_voice_out + 0x8000) >> 15
      ctr += step; if ctr >= 0: decode next 16-B block (below), ctr -= 0x1C000
      s = NON ? noise : gauss4(hist, (ctr>>4)&0xFF)              // table ME+0, 256x4 halfwords
      s = s*env >> 15 ; L += s*volL >> 15 ; R += s*volR >> 15 ; if EON: revL/R += ...
      ADSR tick (0x1418)
  disc audio (0x288-0x424) if SPUCNT bit14? and shadow[659] > 0
  capture: CD L/R -> SPU RAM 0x000/0x400, voice1 -> 0x800, voice3 -> 0xC00 (index ME+5096, 9-bit)
  reverb (0x428-0x924), main volume with sweep (0x964-0xc6c, state ME+4920)
  shadow[430] = SPUCNT&0x3F | irq<<6 | (capidx>>8)<<11 ; shadow[412] = ENDX
  return clamp16(L) | clamp16(R) << 16
```

- Per-voice state (116 B at 0x09FF0858 + 116·v):

  | Off | Field | Off | Field |
  |---|---|---|---|
  | 0 / 10 | vol L / R current (+2..+8, +12..+18 sweep params) | 40 | pitch counter s32 (start 0xFFFE4000 = −28<<12) |
  | 20 | cached vol reg word | 44 | current address (8-B units) |
  | 24 | cached ADSR word | 46 | repeat-written flag |
  | 28 | ADSR state | 47 | last block flags |
  | 29 | exp flag | 48 | pitch (≤0x3FFF) |
  | 30 | target | 50 | repeat address |
  | 32 | step counter | 52–57 | 3 history samples |
  | 34 | step | 58–113 | 28 decoded samples |
  | 36 | counter reload | 114 | stop: 1 = ended without loop, −1 = envelope dead (silent) |
  | 38 | envelope level | | |

- ADPCM block decode (0x1278–0x138c):
  ```
  read 16 B at SPU RAM (cached alias), then cache 0x19 on the line
  hdr = shift | filter<<4 ; filter >= 5 -> 0
  s = (nibble<<12 >> shift) + ((old*f0 - older*f1) >> 6), clamp s16
  (flags & 6) == 6 and no CPU repeat write -> repeat = cur addr, also stored to the shadow repeat reg (+14+16v)
  flags & 1 (end): ENDX |= bit; addr = repeat; if !(flags & 2): voice[114] = 1, ENVX reg = 0
  IRQ: if (irq_addr - block_addr) < 16 -> ME+6039 = 1     (quirk 6038/6034 re-latches IRQ addr)
  ```
- ADSR (0x1418), per sample:
  - The counter decrements. At 0, apply the step (exp: level·step>>15, else level+step), clamp
    0..32767, and on crossing the target dispatch through jump table 0xd4074[state−4]:

    | State | Handler | Action |
    |---|---|---|
    | 4/5 | 0x14cc/0x14c8 | attack setup → 8 lin / 9 exp |
    | 8 | 0x158c | → decay 12; exp factor (0xFFFFC000>>shift)&0x7FF8; target (SL+1)·0x800−1 |
    | 9/18 | 0x153c | exp attack/increase knee at 0x6000 |
    | 12 | 0x15dc | → sustain 16 + (dir·2 + mode): 16 lin+, 17 lin−, 18 exp+, 19 exp− |
    | 16 | 0x1670 | hold |
    | 17/19/24 | 0x16d8 | reached 0 → voice[114] = −1 (silent) |
    | 20 | 0x1680 | release setup → 24 |

  - Rate: counter = 1 << max(0, shift−11); step = (7−s) or (−8+s) << max(0, 11−shift) (psx-spx
    formula).
- Reverb (0x428–0x924):
  - When dirty bit31 is set, pointers are recomputed from regs 448–511 relative to the work start (418).
  - Odd/even sample alternates the L/R side: each side runs at 22.05 kHz, state ME+4964+64·parity.
  - Same-side/diff-side reflection, 4 combs, 2 all-pass, IIR via ME+4948.
  - The input downsampler is a **short FIR (coefficients 15901, 418, 4839, 22962)**, not psx-spx's
    39-tap one.
  - IRQ check against every reverb access. Output × reverb out volume (388/390).

#### 2.7 XA / CD-DA in the mixer (0x288–0x424 decode, 0xcac–0xfec XA)
- Ring:
  - pending = (wr − rd) & 7 (shadow 656/657).
  - Sector pointer = shadow[608+4·rd]. The ME does `cache 0x19` over 2352 B before reading.
- CD-DA (mode 1): one 16-bit stereo frame per output sample, so 1:1 at 44.1 kHz. rd++ after 588
  samples.
- XA (mode 2):
  - Coding byte = sector+19: bit0 stereo, bit2 18.9 kHz.
  - The whole sector (18 sound groups × 128 B from sector+24) is decoded at once into ME+5112.., using
    the same filter tables. Then rd++.
  - Resample with phase += 60 (37.8 k) or +30 (18.9 k), wrap at 70.
  - 7-phase × 5-tap polyphase table at ME+2048 (copied from rodata 0xd402c).
- Then:
  - ATV matrix (652–655, >>7, clamp).
  - Capture buffers 0x000/0x400.
  - × CD volume (432/434).
  - Into the output if SPUCNT bit0 (CD audio enable). Into the reverb input if SPUCNT bit2.
- CPU side, submit 0x83e8(sector, mode):
  ```
  while ring full (pending == 7) and SPU on: sceKernelDelayThreadCB(100); 0x25190()
  if shadow[659] == -2: wait for ME to clear it
  writeback sector lines (mode 2: sector+16, 2312 B; else 2352 B)
  ring[wr] = sector ; wr++ ; old = shadow[659] ; shadow[659] = mode
  if old <= 0 and mode > 0: 0x11520()             // resync audio clock
  ```
- 0x8538 = pending count. 0x8550 = pending−1 when mode > 0.

---------------------------------------------------------------------------------------------------
### 3. Audio clock = emulation clock (0x11520, 8 call sites, incl. 0x83e8/0xd880/0xd9f4)
```
lag = (now - gp[13836]) - (shadow[660] - gp[13832]) * 768      // cycles PS1 ahead of audio
if lag < 3387: return                                            // 3387 = 100 us
el  = now - gp[13836] - ...; shift = (el > 0x84E10) ? 1 : 0     // 544272 cyc ~ 16 ms
if (gp[13928] & 0x1000: gp+0x3668 bit 12 = 24-bit display) and lag < (samples*768)/2: return
sceKernelDelayThread(((lag * 1741) >> 16) >> shift)            // ~0.9 * lag in microseconds
```
- Together with the 768-cycle throttle in the SPU write handler, the dynarec main thread can never run
  more than ~100 µs ahead of the ME's 44.1 kHz output. **Audio is the master clock.**
- This is the inferred reason POPS has no audio buffering glitches.

---------------------------------------------------------------------------------------------------
### 4. CD-ROM controller

#### 4.1 Install (0x1adb0) and state
- 0x1adb0:
  - Cancels nodes gp+14408/14364/14336/14392/14424 and zeroes gp+14336..+184.
  - Node A (14336) and node B (14364) callback = 0xC268. Node C (14408) = read event 0xC5EC.
    The play event is 0xCA50.
  - 14436 = 0xCE00, 14492 = 0x20 (mode), 14458 = 12, 14460 = 2352, 14493 = 2 (stat: motor on),
    14496 = GetSystemTimeLow (LCG seed), 14462 = −1.
  - Calls 0xd9f4 (stop disc audio), copies ATV 14520 → shadow 652.
  - Installs DMA3 = 0xCE18 and regs 0x1800–0x180F read 0xD088 / write 0xD1B0.

| gp+ | Field | gp+ | Field |
|---|---|---|---|
| 14524 | index/status (bit5 resp, bit6 data, bit7 busy) | 14492 | mode |
| 14525 / 14526 | IE / IF | 14493 | stat |
| 14527 | request reg | 14494 | filter file/channel |
| 14440 | response FIFO (cnt 14452, rd 14473) | 14500 | seek deadline |
| 14454 | parameter FIFO (4 deep, cnt 14453) | 14504 | current LBA |
| 14462 | deferred command (−1 none) | 14508 | Setloc target |
| 14466/14467 | sector buffer bank wr/rd; ptrs 14484+4·bank | 14512 | XA active / LBA |
| 14458 / 14460 | data position / end | 14520–14523 | ATV staging |
| 14468/14469 | error flag/code | 14472 | Setloc pending |
| 14470 | busy (GetID) | 14516 | last subheader/Q ptr |

#### 4.2 Register access
- Read 0xD088:
  - reg0 = status: bit6 only while the data buffer is ready, bit2 while XA is active, bits 3/4 from
    the parameter count.
  - reg1 = response FIFO.
  - reg2 = data byte; reaching the end clears bit6.
  - reg3 = IE (index 0/2) or IF|0xE0.
- Write 0xD1B0, jump table 0xd4924[reg + 4·index]:
  - 0.x → index.
  - **1.0 command (0xd228):**
    - Ignored while busy. ≥31 → 0.
    - Deferred into 14462 if IF ≠ 0, a response is queued (14362), or the read event is due within
      0xC672 = 50,802 cycles.
    - Otherwise it runs at once: `0xae5c(cmd)`.
  - 2.0 = parameter.
  - 3.0 = request: BFRD swaps the bank. mode & 0x20 → pos 12 / end 2352; mode & 0x30 == 0 →
    pos 24 / end 2072.
  - 2.1 = IE (raises an IRQ if IE & IF).
  - 3.1 = IF ack:
    - bit6 resets the parameters; bit5 → 0xd9f4 and XA off.
    - When IF reaches 0, it delivers the queued node B/A response, or runs the deferred command.
  - 2.2, 3.2, 1.3, 2.3 = ATV bytes. 3.3 bit5 = apply ATV → shadow 652 (+0x11520 unless muted).

#### 4.3 Command processor 0xae5c
- Parameter-count table 0xd47d4. Too few parameters → error 0x20.
- Sets busy (14524 bit7). Config gp+1708 bits 14/16/21/23 = per-game hacks.
- Jump table 0xd4810:

| Cmd | Handler | Cmd | Handler | Cmd | Handler |
|---|---|---|---|---|---|
| 01 Getstat | af54 | 0B Mute | b580 | 15/16 SeekL/P | bc84 |
| 02 Setloc | aff0 | 0C Demute | b5a0 | 19 Test | bd28 (sub-table 0xd488c, 38 entries) |
| 03 Play | b0ec | 0D Setfilter | b5bc | 1A GetID | bdc0 |
| 04/05 Fwd/Back | b280 | 0E Setmode | b5e0 | 1B ReadS | be48 |
| 06 ReadN | be48 | 0F Getparam | b738 | 1C Reset | bfd0 |
| 07 MotorOn | b2b0 | 10 GetlocL | b76c | 1E ReadTOC | c018 |
| 08 Stop | b334 | 11 GetlocP | b8e8 | 00,17,18,1D,≥1F | bffc → error 0x40 |
| 09 Pause | b3c0 | 12 SetSession | bab4 | | |
| 0A Init | b4c8 | 13 GetTN / 14 GetTD | bafc / bbb8 | | |

- Response helpers:

  | Helper | Role |
  |---|---|
  | 0xc3a4(type, delay) | first response through node A, usually 16384 cycles; errors → INT5 |
  | 0xc418 | second response through node B |
  | 0xc358 | immediate second response |
  | 0xc268 | delivers into the FIFO when IF == 0, IRQ when IE & IF |

- Utilities: 0xade8 BCD MSF → LBA−150; 0xc078 LBA → track (TOC); 0xc128 GetlocP MSF.

#### 4.4 Timing (PS1 cycles at 33.8688 MHz)
| Event | Cycles |
|---|---|
| First response (most commands) | 16384 (0x8000 for some) |
| Sector period 1x / 2x | 0x72400 = 468,992 (72.2/s) / 0x3B200 = 242,176 |
| ReadN without new Setloc | +0x52B000 (≈160 ms) unless config 1728 bit6 |
| Seek d = \|target − cur\| < 7000 | (d·500 + 10000)/3, ×3 unless config 1728 bit1 |
| Seek d ≥ 7000 | sqrt(d−6000)·6000 + 1,000,000, ×8 unless config bit1 |
| Seek minimum | 0x4A6B3 = 305,843, or 0x129ACC = 1,219,276 if config 1728 bit5 clear (unless bit0) |
| Seek extra | cap 0x121680 if 14477 bit4; ≥ remaining node 14424; jitter (128 − (rand&0x7F))·32 with LCG ×0x41C64E6D+12345; ×24 if target == gp+1772; 1 if 14479 |
| Play start | 0x72400 + seek |
| Pause | max(remaining, 22656, 761,497 when reading / 388,940 when playing) ×6 unless config 1728 bit2 |
| Init | 0x800000 + (rand&0xFFFF)·33; 0x204CC00 (1 s) if the motor is off |
| MotorOn / Stop | 0x8000 / 0xD00000 (motor on) |
| GetID | second after 0x8000: stat, 00, 00, 02 (sic), "SCEI" |
| Setmode speed change | 10000, or 1 s (0x204CC00) per config 1728 bit7 |

#### 4.5 Read event 0xC5EC and play event 0xCA50
```
read_event:
  stat = (stat & ~0x40) | 0x20
  sec = 0xd5cc(lba, wait=1)            // pointer into a cached 2352-B raw sector
  if !sec: INT5 error 4
  if mode&0x40 (XA) and (submode & 0x44) == 0x44 and (!(mode&8) or filter match):
      0x83e8(sec, 2)                   // to the ME, not to the CPU buffer
      next XA sector interval 14471 = 4 << (double + mono + 18.9k)
  else: copy into bank, INT1 via node B
  period = 1x/2x; XA pacing variants 0x6E400/0x37200/0x52B00/0x675C0 chosen from ring fill (0x8538 vs gp+1776)
  run deferred command
play_event (CD-DA):
  past end (gp+1840) -> INT4; track change via TOC; report INT1 (GetlocP-style) when mode&4
  if compressed audio track: 0xd880(track, lba) once (start CPU decode), then 0xd5cc(lba,1)
  0x83e8(sector, 1)
  next period: ring pending < 4 -> 0x37200 (225,792, catch up); >= 7 -> 0x89CFF (564,479); else normal
```

#### 4.6 DMA3 0xCE18(madr, bytes, chcr)
- Only when CHCR bit28 is set. Destination RAM 0x09800000 | (madr & 0x1FFFFF).
- Transfers >24 B first **invalidate the dynarec block table** for the destination range (0x09C00000
  mirror, +0x400000). This happens only for 64 KB pages whose bit is set in the code-page bitmap
  gp+472, using VFPU `sv.q` of a zero vector, cache-line aligned.
- Copies from the current bank at the current position with VFPU lv.s ×16 / sv.q ×4 per 64 B, using
  `cache 0x18` (create dirty exclusive: no line fill) on each destination line.
- Clipped at end 14460. At the end it clears status bit6 and request bit7. Overrun bytes are filled
  with the last byte (sceKernelMemset).
- **Zero guest cycles** are charged. The transfer is instantaneous.

---------------------------------------------------------------------------------------------------
### 5. Disc image path (PSISOIMG on Memory Stick)

#### 5.1 Open (0x1b2f0, called from main init 0x16114)
- Initialises 17 cache nodes: gp+15616+16·k = {next, prev, base LBA, buffer = 0x09492600 + 0x9300·k}.
  The circular LRU head is gp+15888. Request slot gp+15892 = −1.
- 0x36cf4(fd): reads the 40-B PBP header (magic "\0PBP") through **sceMeAudio_30BE34E4(fd, dst,
  offset, size)**, then reads the ICON0 section (meaning inferred).
- Reads 1 KB at gp[15928] (PSAR offset) and compares with "PSTITLEIMG000000" (16 B) or "PSISOIMG0000"
  (12 B).
  - Multi-disc: up to 5 disc offsets → gp+15908+4·i, count → gp+15940.
  - Single disc → 0x1b004.
  - sceMeAudio_528266FA(1 KB) processes the header (decrypt; inferred).
- Creates event flag `cdread` (attr 0x200, init 0) → gp+15932.
- Creates thread `cdworker` (entry 0xda3c, priority 22, stack 6144, attr 0x100000) and starts it.
- fd = gp+15936. The fd is opened by popsman; the open site is not in pops (inferred).

#### 5.2 Block index (host 0x09E84000 + 32·(lba>>4))
| Off | Read by | Meaning |
|---|---|---|
| +0 | daf4 | byte offset in the image; low 9 bits = start inside the 512-aligned read |
| +6 (u16) | d654 | sector layout: 0 = raw 2352 sectors; ≠0 = sync/header regenerated on access (00 FF×10 00 + BCD MSF of lba+150); 2 = sector order permuted (index = bitrev4(15−i)) |
| +8 (16 B) | dbf8 | digest passed to sceMeAudio_F6637A72 (integrity check, inferred) |
| +24 (s32) | db08 | size: >0 LZRC-compressed length; 0 = all-zero block; <0 = stored, read by sceMeAudio_3771229C(fd, dst, offset) |

#### 5.3 Sector lookup 0xd5cc(lba, wait) (data mode, gp[15942] == 0)
```
node = find(lba - node.base < 16)                    // linear walk of the 17-entry LRU list
if node: move to front; fix header per +6 type; 0xd41c(lba+16)  // read-ahead
         return node.buf + (lba & 15) * 2352
if !wait: return 0
loop: 0xdefc()                                      // wait for in-flight request (event bit 2)
      0xd41c(lba & ~15)                             // request
      while not cached: 0x344bc(1)                  // service vblank/ctrl/power while blocked
```
- 0xd41c(lba): ignores LBAs beyond gp+1840 (disc end). Walks forward past already-cached blocks to the
  first missing one, stores it in gp+15892, then `sceKernelSetEventFlag(cdread, 1)`.
  - In CD-DA mode it instead checks the PCM-ring slot table.

#### 5.4 Thread `cdworker` (0xda3c)
```
for (;;) {
  sceKernelWaitEventFlagCB(cdread, 1, OR|CLEAR(33), 0, 0)
  if (gp[15942]) { cdda_decode(); continue; }       // §5.5
  lba = gp[15892]; if cached: continue
  disable interrupts (mtic 0); victim = lru_tail; victim.base = 0x80000000; enable
  e = index[lba>>4]
  if e.size > 0:
     off = e.off & ~0x1FF ; skew = e.off & 0x1FF
     if off == gp[15900]: reuse 512-B tail already in bounce buffer (memcpy)
     sceMeAudio_30BE34E4(fd, 0x09480000, off, size)   // MS read; tail start kept in gp[15900]
     every >= 13231 ME samples (0.3 s), if no XA pending and savedata idle:
         sceMeAudio_F6637A72(buf, &e.digest) ; fail -> D+0x351F14 = -1
     r = lzrc_decompress(cap=0x9300, src=0x09480000+skew, dst=victim.buf)   // 0xe02c
     ok = (r == 0x92FF)
  elif e.size == 0: zero 0x9300 B
  else: sceMeAudio_3771229C(fd, victim.buf, off)
  if ok: victim.base = lba & ~15; move to front
  gp[15892] = -1; sceKernelSetEventFlag(cdread, 2)
}
```
- **0xe02c is LZRC**, Sony's adaptive binary range coder plus LZ, as used in PSISOIMG.
  - 2656-B probability model on the stack.
  - Probabilities are updated as p −= p>>3 / p += (256−p)>>3 style (`addiu 31`, `srl 3`).
  - Range = (range>>8)·p. The code is primed from big-endian bytes 1..4. Header byte 0 bits 3–4 set
    the initial probability (128 − 16·n), bits 0–2 the literal context shift. Bit7 set = stored block
    (0xe760 path).
  - Other callers (0x1c60c, 0x27a24.., 0x31dd8..) decompress UI/asset data.
- **scePopsMan_0090B2C8 is not decompression.** It is called with 0x80000004 on fatal paths (0x1ace0,
  0x16078, 0x161a0, 0x1b2e8, 0x250b0, 0x319a0): an error/exit report (inferred).

#### 5.5 CD-DA (compressed audio tracks)
- Start 0xd880(track, lba):
  - Calls 0x11520 and sets shadow[659] = −1.
  - If track ≥ 2 and the track table size (0x09E80BE4+16·t) ≠ 0:
    - Invalidates all 17 cache nodes (base = 0x80000000) and sets gp[15942] = 1, gp[15944] = track.
    - frame = ((lba − track_start)·588 − 1162) >> 10.
    - gp[15968] = file_off + frame·384; gp[15972] = ror(key, frame).
    - Sets gp[15948] = lba−1 and gp[15952] = lba, then raises the event flag.
- Stop 0xd9f4: clears 15942/15944, waits for idle, calls 0x11520, sets shadow[659] = −1.
- Decode loop in `cdworker` (0xdcb4–0xdef8):
  ```
  read up to 0xDC80 B of frames via 0xdf4c:
      sceMeAudio_30BE34E4(fd, 0x09480000, gp[15968], n)
      descramble each 384-B frame:
          a = key; w' = w ^ a; a = key + w*0x075BCD15; key = ror(key,1) per frame
  0x3b300(frame, out=gp+15976, 0) -> must consume 384 B;
      1024 stereo samples as two s32 arrays (ptrs at gp+15976/15980)
  clamp to s16, pack L|R<<16 into uncached ring 0x49492600 + gp[15960] (4096 B per frame)
  every 0x9300 B completes a slot: slot LBA table gp+15608+16*k = gp[15952]; gp[15952] += 16
  ring wraps at 0x93000 (16 slots); throttled by gp[15948] (consumer LBA) - 32
  ```
- 0x3b300 (0x18c8 bytes, with 0x3cbc8) is a **software audio decoder on the main CPU**, in cdworker.
  - Its frame geometry (384-B frames → 1024 samples) matches ATRAC3 at 132 kbps (inferred; confirm
    from the 0x3b300 tables).
  - The ME only receives the decoded 2352-B sectors through the ring (§2.7).
  - **Data and CD-DA share the 0x09492600 buffer:** starting CD-DA flushes the data cache.

---------------------------------------------------------------------------------------------------
### 6. Threads, callbacks, synchronisation

| Object | Created at | Entry / handler | Priority | Stack / attr | Role |
|---|---|---|---|---|---|
| thread `popsmain` | 0x1604c | 0x16080 | 32 | size 0x00BED900, attr 0x80104000; sets its own sp = 0x09BF0000 | dynarec + all PS1 hardware except mixing; GE/vblank in 0x115b4 |
| thread `cdworker` | 0x1b4e0 | 0xda3c | 22 | 6144, 0x100000 | block decompression, read-ahead, CD-DA decode |
| thread `mcworker` | 0x1a9a0 | 0x1aa90 | 27 | 4096, 0x102000 | memory-card load and write-back |
| event flag `cdread` | 0x1b4b8 | – | – | attr 0x200 | bit1 request (main→cdworker), bit2 done |
| sema `mcWriteBack` | 0x1a970 | – | – | init 0, max 1; id at D+0x4DF60 | wakes mcworker |
| alarm | 0xa6d8 (SIO MC write) | 0x1a9d0 | – | 1,000,000 µs; id D+0x4DF64, cancelled and re-armed per write | debounce: signal sema 1 s after the last card write if savedata is idle |
| callback `powerCB` | 0x160b8 | 0x34388 | – | scePowerRegisterCallback(−1) | suspend/resume |
| callback `msCB` | 0x160dc | 0x34468 | – | – | Memory Stick events (inferred) |
| ME | sceMeAudio_DE630CD2 | module+0 | – | sp 0x09FF8000 | SPU mixer only |

- 0x1a908 waits (DelayThread 1000) until the alarm id is < 0, i.e. the write-back has finished, then
  calls `sceKernelChangeThreadPriority(0, 17)` (inferred: exit/suspend flush).
- Busy-waits on the main thread:
  - SPU write throttle (DelayThread 22).
  - XA ring full (DelayThreadCB 100).
  - Audio-clock sleep 0x11520.
  - Blocking disc miss (0x344bc service loop).

---------------------------------------------------------------------------------------------------
### 7. SIO: pad and memory card

#### 7.1 Ports
- 0x1a494 installs:
  - 0x1040 and 0x1050: read 0x9F30, write 0xA06C.
  - 0x1048 and 0x1058: write 0xA0E8, default read.
- Port struct = gp+688 + 44·port (port = address bit4), and is also an event node:

  | Offset | Field |
  |---|---|
  | +12 | callback |
  | +16 | STAT |
  | +18 | CTRL |
  | +20 | byte index |
  | +21 | RX byte |
  | +22 | TX byte |
  | +24 | byte transfer delay |
  | +26 | ACK delay |
  | +28 | transfer-done callback 0x9E64 |
  | +32 | device handler |
  | +36/+40 | poll timestamps |

- Init: STAT = 5, SIO1 STAT = 0x195, IRQ bit +23 = 1.
- Data write 0xA06C: TX = byte; STAT &= ~5; schedule the node after `+24` cycles. +24 is computed at
  CTRL/MODE/BAUD write (0xa1d4) as (BAUD·(char-length + 5)) << (mode-factor).
- Transfer done 0x9E64:
  - On byte 0, select the device by TX byte:

    | TX byte | Device | Handler | ACK delay |
    |---|---|---|---|
    | 0x01 | pad | 0xA250 | 67 cycles |
    | 0x81 | memory card | 0xA508 | 270 cycles |
    | other | none | 0x1A574 (returns 0x1FF) | – |

  - rx = handler(index, port, tx). STAT |= 7. If the handler returns bit8 (no ACK), the device becomes
    "none". Otherwise schedule the ACK callback 0xA220 → IRQ7 if CTRL bit12. index++.
- Read 0x9F30, **poll acceleration**:
  - A data read within 34 cycles of the pending event fires the event immediately.
  - A STAT read within 80 cycles of the previous STAT read fires the pending transfer immediately.
  - Both refund the skipped cycles from the time budget (gp+432/484).
- CTRL write 0xA0E8: bit6 reset → STAT = 5, index 0, cancel; bit4 ACK → STAT &= 0xFDC7; JOYn select
  edges reset the index.

#### 7.2 Pad (handler 0xA250, state gp+15360 + 48·port)
- Init 0x1a57c:
  - sceCtrlSetSamplingMode(1) (analog), sceCtrlSetIdleCancelThreshold(64, 64).
  - sceCtrlSetSamplingCycle(8341 µs ≈ 120 Hz if config gp+1708 bit20, else 0 = vblank).
  - Per port: buttons = 0xFFFFFFFF, +33 = 1 (connected), +34 = 0x41 (ID: digital pad), +35 = 2
    (halfwords), +36 = 0x41, +44 = 0x5A.
- Byte 0 (TX 0x01):
  - Mapping mode from 0x2c718(port) (<0 = unplugged).
  - **sceCtrlPeekBufferNegative**(buf, 1 + bit20). Negative = active-low, the same polarity as the PS1.
    With 2 buffers the results are ANDed.
  - HOLD switch (0x20000) clear → no input (0xFFFF).
  - Mode 0/default: remap table 0x2c3e4.
  - Analog stick (Lx = buf+8, Ly = buf+9) → D-pad with thresholds <64 / ≥193 (mode 1: also a 96..160
    dead band). Modes 2/3 map L/R/triangle combos to L2/R2 (masks 0xC00, 0x300).
  - The result is stored at +0 and the reply is 0xFF.
- Byte 1 → ID 0x41; byte 2 → 0x5A; bytes 3.. → button bytes (+0, +1), with bit8 on the last one.

#### 7.3 Memory card (handler 0xA508, image D+0xDE50 + 0x20088·port)
- Handler:
  - Port < 2 and flag bit0 (inserted) → per-index jump table 0xd4734 (6 states: command R/W/ID…).
    Otherwise 0x1FF (no card).
  - Writes set flag bit1 (dirty). The alarm is cancelled and re-armed for 1 s (0xa6c0–0xa6d8).
- `mcworker` 0x1aa90:
  - Load: memset both images (0x40110 B), then for each card load through the savedata utility
    (0x37204/0x379f0/0x37478…, sceUtilitySavedata*).
  - Validate: 0x1aa1c checks "POPS"/".MEM"-style tags.
  - VMP check: 0x37814 requires magic "\0PMV" and calls **pspvmc_0820B291**(buf, &hdrsize); hdrsize
    must be 128.
  - Write-back: 0x3748c calls **pspvmc_38A87A12** (VMP signature, inferred).
  - Files are SCEVMC0.VMP / SCEVMC1.VMP (also MEMCARD1/2.DAT strings at 0xf192c) under
    "ms0:/PSP/SAVEDATA/%s".
  - Loop: WaitSema(mcWriteBack). For each card with flag bit1, clear the bit with interrupts off; if
    state == 3, write it (0x3748c).
  - Fatal failure → scePopsMan_0090B2C8(0x80000004).

---------------------------------------------------------------------------------------------------
### 8. Lessons for psxrt (inferred design guidance)
1. Make the audio output clock the master. Throttle the guest against the sample counter: a 100 µs lead
   budget, plus 1-sample granularity on SPU register writes. POPS never buffers more than one sample of
   SPU state.
2. ME handoff: shadow registers written uncached, plus *masks* (dirty/KON/KOFF) under a two-flag
   Peterson lock. The ME critical section is ~10 instructions per sample.
3. XA goes to the mixer as raw sectors (8-deep pointer ring). XA is never decoded on the CPU.
4. The disc path is a 16-sector-block LRU with one-block read-ahead on a priority-22 worker thread.
   Blocking misses still stall the emulator. 17 × 37 KB ≈ 625 KB of cache.
5. SIO/CD STAT polling acceleration (fire the event early when the guest polls) is a cheap,
   general idle-loop speedup.

Not examined: popsman.prx internals (the ME loop, how fd/headers are prepared), 0x3b300 decoder tables,
0x115b4 vblank/GE loop, the MC jump table 0xd4734 states in detail, 0xe8f8 (MDEC-side user of
events).

---

## Part G. POPS (pops_01g, fw 6.61): GTE, MDEC, interrupts/COP0, timers, DMA, BIOS hooks, cycle model

Addresses are module offsets (load base 0) unless marked otherwise. "gp" = 0x00010000 (the PSP scratchpad).
Tags: **[V]** read in the disassembly (instructions cited). **[I]** inferred, with what would confirm it.
Helper used: `D:\Temp\PSX2PSP-Reborn\scripts\popsdis.py` (`START END` dumps lines, `-w VA N` words,
`-h VA N` hex of the module image; file offset = va + 0xC0 for both segments).

Correction to the brief: the GTE op table at 0xd45c4 is not in the data segment. Segment 0 runs from va 0
to 0xFECD4 and holds text, BIOS (0x53C20) and rodata. All the tables below (0xd4150, 0xd42fc, 0xd435c,
0xd4498, 0xd45c4, 0xd46c4, 0xd499c, 0xd49b8, 0xef0b0) are rodata in segment 0. Their pointers are
module-relative.

---

### 0. Context map (gp-relative, the part these subsystems use)

| gp+ | Contents | Evidence |
|---|---|---|
| 0x000-0x07F | GTE data regs 0-31, 4 bytes each, in canonical form (see 1.4) | RTPS 0x106ac: `lh 128(gp)` = R11, `lw 148(gp)` = TRX, `sw 100(gp)` = MAC1 |
| 0x080-0x0FF | GTE control regs 32-63. The FLAG slot 0xFC is not live (FLAG lives in VFPU S330). 0xFC doubles as the `ra` save slot of the memory helpers | `sw ra,252(gp)` at 0x1afc; `lv.s S330,252(gp)` at 0x1c574 (state load) |
| 0x100-0x13F | COP0 r0-r15: r8 BadVaddr @0x120, r12 SR @0x130, r13 Cause @0x134, r14 EPC @0x138, r15 PRId @0x13C = **1** | 0x5920, 0x94c4, 0x1a2a4 (`sw v1(=1),316(gp)`) |
| 0x1A0 | guest PC | 0x2650 |
| 0x1AC | absolute time of the head event | 0x953c |
| 0x1B0 | saved t9 (cycles left to the head event) | everywhere |
| 0x1B4 | host address to resume at after the scheduler | 0x1a6c, 0x9510 |
| 0x1B8/0x1BC | event-list sentinel {next, prev} | 0x1a284 |
| 0x1C0 (h) | request: >0 = flush the translation cache, <0 = full emulator reset | 0x95a0, 0x9648, 0x9658 |
| 0x1C2 (b) | instruction-fetch penalty shift (cycle model, 5.3) | 0x19f28 |
| 0x1C3/0x1C4/0x1C8 | I_STAT poll back-off state | 0x9850 |
| 0x1CC / 0x1D0 | code-cache write pointer for RAM / BIOS code (init 0x09540000 / 0x09B80000) | 0x19f34 |
| 0x1D4 | cached host entry of the block at 0x80000080 | 0x6724, 0x94f8 |
| 0x1D8 | bitmap of 64 KB RAM pages that hold compiled code | 0x5a50, 0x7e60 |
| 0x1E0 | cache-control register 0xFFFE0130 | 0x8998, 0x8a38 |
| 0x1E4 | cycle-credit pool (I/O busy-wait accelerators; paid back at vblank) [I] | 0x15f78, 0xa1ac |
| 0x1E8 + 28*ch | DMA channel ch0-6 (event node + handler) | 0x1a1f0 |
| 0x2AC/0x2AE | DMA pending-channel mask / round-robin index | 0x8d88 |
| 0x400-0x41B | MDEC IDCT/YUV float constants (copied from 0xd499c) | 0x1b6c8 |
| 0x420-0x61F | MDEC dequant float tables, luma/chroma interleaved | 0xf648 |
| 0x620-0x64B | MDEC state; event node at 0x63C (callback 0x648 = 0xe8f8) | 0xf54c-0xf768 |
| 0x64C + 32*n | root counters 0-2 | 0x1a3a8 |
| 0x6AC, 0x6B0-0x72F | per-game flags word and config words cfg[0..31] (section 5.5) | 0x24770 |
| 0x1000-0x1FFF | I/O handler table, {read, write} per 8 bytes of 0x1F801000-0x1F801FFF | 0x1a050 |
| 0x2000-0x2FFF | I/O register shadow: I_STAT 0x2070, I_MASK 0x2074, DMA 0x2080+16ch, DPCR 0x20F0, DICR 0x20F4 | 0x96e4, 0x8b1c |
| 0x3000-0x33FF | PS1 scratchpad | brief |
| 0x35E4 | display/vblank event node | 0x11410, 0x15f54 |
| 0x3610/0x3612, 0x3618/0x361A | display start X/Y, vertical display range Y1/Y2 | 0x114b4, 0x11430 |
| 0x3662 (b) | half-frame counter (+1 at vblank start and at display start; bit 0 = in vblank) | 0x1141c, 0x126c8 |
| 0x3668 | display-mode word: GP1(08) mode << 8: bit 11 = PAL, bit 12 = 24-bit, bit 13 = interlaced; byte 0 set to 1 when an MDEC decode command arrives | 0x11438, 0xf584 |

PSP memory used by these subsystems (absolute):

| Range | Use | Evidence |
|---|---|---|
| 0x09540000-0x097AFFFF | translation cache for RAM code (flushed at 0x097B0004) | 0x5944, 0x7ed4 |
| 0x097C0004-0x097E0000 | GTE reciprocal (UNR) table, 32768 words | 0x1a314 |
| 0x09800000 | PS1 RAM. `sp` = 0x09800000 inside generated code | 0x1a00, 0x10a10 |
| 0x09B80000- | translation cache for BIOS code | 0x19f1c |
| 0x09C00000 / 0x09E00000 | block tables for RAM / BIOS | 0x2650 |
| 0x09E80000 | PSISOIMG header buffer. +0x420/+0x424/+0x428 = config, +0x12B0 = anti-LibCrypt word | 0x1b110, 0x6f60 |
| 0x09F40000-0x09FC02BF | SPU register shadow (0x2C0 B) + SPU RAM (512 KB from 0x09F402C0), cleared at init (Part F §1) | 0x19ea8 |
| 0x041B0000 (eDRAM) | compiler's per-instruction analysis records (16 B each) | 0x59dc |

---

### 1. GTE (COP2)

#### 1.1 Where the ops live

- **Compiled code calls a native Allegrex routine per op.** It runs `jal handler` from the generated code;
  the emitter is 0x6ff0, its per-op switch is the table 0xd4498 indexed by funct-1, and the call is emitted
  by 0x308c. Simple setup is inlined, e.g. the `sf` shift is put in a0. Before some calls, cached guest
  registers are spilled with `0x2de8(code, n)`.
- **The table at 0xd45c4 (64 entries, funct&0x3F) is used only on the interrupt path** (0x95e8, 1.6) and
  always holds the variant that computes FLAG. Invalid functs go to 0x1a3a0 = `jr ra; nop`, so they do
  nothing.

| funct | op | 0xd45c4 (IRQ path) | compiled: FLAG needed / FLAG dead | cost (0xd46c4) | psx-spx |
|---|---|---|---|---|---|
| 01 | RTPS | 0x106ac | 0x106ac / **0x10924** | 15 | 15 |
| 06 | NCLIP | 0x10b34 | 0x10b34 | 8 | 8 |
| 0C | OP | 0x9828 → 0x112f8 | 0x112f8 (a0=sf*12 inline) | 6 | 6 |
| 10 | DPCS | 0xf76c | 0xf76c / **0xf820** | 8 | 8 |
| 11 | INTPL | 0x1101c | 0x1101c | 8 | 8 |
| 12 | MVMVA | 0x9748 → 0x102ec (fields decoded at run time) | fields decoded at compile time, then 0x102f0 / **0x10510** | 8 | 8 |
| 13 | NCDS | 0xfa68 | same | 19 | 19 |
| 14 | CDP | 0xf898 | same | 13 | 13 |
| 16 | NCDT | 0xfba8 | same | 44 | 44 |
| 1B | NCCS | 0xfdcc | same | 17 | 17 |
| 1C | CC | 0xf994 | same | 11 | 11 |
| 1E | NCS | 0x100a0 | same | 14 | 14 |
| 20 | NCT | 0x10184 | same | **31** | 30 |
| 28 | SQR | 0x97b0 → 0x10b78 | 0x10b78 (a0=sf*12) | 5 | 5 |
| 29 | DCPL | 0x10e88 | same | **13** | 8 |
| 2A | DPCT | 0x11178 | same | **8** | 17 |
| 2D | AVSZ3 | 0x10bf0 | same | 5 | 5 |
| 2E | AVSZ4 | 0x10c38 | same | 6 | 6 |
| 30 | RTPT | 0x10b14 | 0x10b14 / **0x10b24** | 23 | 23 |
| 3D | GPF | 0x97d8 → 0x10c88 | 0x10c88 (a0=sf*12) | 5 | 5 |
| 3E | GPL | 0x9800 → 0x10d7c | 0x10d7c | 5 | 5 |
| 3F | NCCT | 0xfee0 | same | 39 | 39 |
| other | — | 0x1a3a0 (nop) | — | 1 | — |

- The cost byte table at 0xd46c4 is indexed by funct; invalid functs read 1. Three entries differ from
  psx-spx (NCT, DCPL, DPCT, in bold).
- **MVMVA (0x72fc).** The compiled path decodes mx/v/cv/sf/lm from the instruction word at compile time and
  emits constants:
  - matrix offset: mx 0 → 0x80 RT, 1 → 0xA0 LLM, 2 → 0xC0 LCM, 3 → the sentinel 1 (garbage matrix);
  - vector pointer: v 0/1/2/3 → gp+0, +8, +16, +0x24 (IR);
  - translation offset: cv 0 → 0x94 TR, 1 → 0xB4 BK, 2 → 0xD4 FC, 3 → none;
  - lm: low clamp 0 or -0x8000.

#### 1.2 VFPU use

- **FLAG lives in the VFPU register S330.**
  - Handlers write it with `mtv at,S330.s`, e.g. 0x108c4 in RTPS and 0x10b68 in NCLIP.
  - The lighting and colour ops build it as float bit-values: `vcmp` + `vcmovt` select constants from
    C400/C410, `vfad` sums them, and `vf2id.s S330,S330,16` converts back (e.g. 0x10160-0x10180).
  - **Persistent VFPU allocation.** R403 is used as a zero vector all over POPS (`sv.q R403` clears,
    0x19f50). C400/C410/C430 hold the GTE saturation and flag constants.
  - MDEC overwrites these and restores them at exit. It saves S330 to the stack at 0xe9b0 and reloads it at
    0xf31c, then rebuilds R403 = 0 and R400 = 2*R401 (0xf320-0xf328).
- **Integer handlers (mult/madd, min/max):** RTPS/RTPT, NCLIP, AVSZ3/4, SQR and the projection divide.
- **VFPU single-precision float handlers:** NCS/NCT/NCDS/NCDT/NCCS/NCCT/CC/CDP/DPCS/DPCT/INTPL/DCPL. They
  use `vs2i`/`vi2f` to bring the 16-bit matrices to float, `vtfm3`/`vhtfm4` for the matrix products,
  `vmax`/`vmin` to saturate, and `vi2uc` to pack RGB+CODE. The colour FIFO is shifted with
  `lv.q C720,80(gp)` / `svl.q C720,88(gp)`.
- **FIFO shifts.** The SXY FIFO (gp+0x30) and the SZ FIFO (gp+0x40) are shifted with `lv.q` + `svl.q`
  pairs, e.g. 0x106ac-0x106bc.

#### 1.3 Accuracy choices

- **Divide: bit-exact UNR.**
  - At init, 0x1a314 computes for every normalised divisor d in 0x8000..0xFFFF:
    - `u = (0x40100 + i)/(2*i + 0x200)` with `i = min(((d+0x40)>>7)-0x100, 0xFF)`;
    - `t = (0x80 - d*u) & 0x1FFFF00 >> 8`;
    - `r = (t*u + 0x80) >> 8`.
  - This is psx-spx's UNR recurrence, with the table entry recomputed instead of looked up. It stores
    `r << 14` at `0x097E0000 - 4*(d-0x8000)`.
  - RTPS (0x107a8-0x107e8):
    - `z = clz(SZ3)-14`; `HI:LO = 0x80000000` (`multu 0x8000,gp`);
    - `maddu (H<<z), table[0x09800000 - (SZ3<<z)]`; `q = min(HI, 0x1FFFF)`.
    - Overflow test: `(H>>1) < SZ3`, else q = 0x1FFFF and flag bit 17 (0x10904).
    - This equals psx-spx `(n*d + 0x8000) >> 16` exactly.
- **RTPS/RTPT MAC1-3.** The handler takes `mflo` of `mult/madd` (the low 32 bits of the sum), then `sra 12`,
  then adds TRx. HW accumulates 44 bits; POPS truncates the product sum to 32 bits [V 0x1070c].
  - The 44-bit MAC overflow flags (bits 30-25) are never set in RTPS/RTPT.
  - Bits produced: 24/23/22 (IR1-3 saturation), 18 (SZ3), 17 (divide), 16/15 (MAC0 overflow, only on the
    IR0 slow path 0x108d0), 14/13 (SX2/SY2), 12 (IR0).
- **Lighting and colour ops are float** (24-bit mantissa). They are not bit-exact once intermediate products
  exceed 2^24. Flags come from float compares (IR and colour saturation). MAC 44-bit overflow bits do not
  appear to be produced [I: confirm by reading 0xf898-0x10184 fully].
- **FLAG.**
  - Computed per op as raw bits. **Bit 31 is computed lazily on CFC2 read**: the emitter 0x4504 for offset
    0xFC emits `mfv rt,S330`, `and tmp,rt,0x7F87E000`, `sltu tmp,zero,tmp`, `ins rt,tmp,31,1` (0x45a0-0x4608).
  - **Writes to FLAG (CTC2 r31) are dropped:** register class 3, and the emitter 0x46a0 returns at once for
    class 3.
  - **Dead-flag elimination.** The emitter 0x6ff0 scans forward in the block. If no CFC2 of r63 into a
    non-zero register comes before the next FLAG-writing GTE op, it calls the no-flag variant (RTPS, RTPT,
    MVMVA, DPCS only). At the block end it assumes FLAG is live.
- **Register file in canonical form.** The class table at 0xd42fc (64 bytes, by register 0-63) drives the
  writes (emitter 0x46a0), so reads are a plain `lw` (emitter 0x4504).
  - class 1 = signed 16: VZ0-2, IR0-3, RT33, L33, LB3, DQA, ZSF3, ZSF4. Stored with `seh`.
  - class 2 = unsigned 16: OTZ, SZ0-3, H. Stored with `sh`; the upper half stays 0.
  - class 3 = read-only or special: ORGB, LZCR, FLAG.
  - class 0 = 32 bit.
- **Special registers.**
  - SXYP (r15): a write emits the FIFO push (`lw 0x34,0x38; sw 0x30,0x34`, then store 0x38, 0x48ac); a read
    returns SXY2 (0x4540).
  - IRGB (r28): a write stores, then `jal 0x113e4` expands it to IR1-3; a read returns ORGB.
  - ORGB (r29): a read emits `jal 0x113a4` (computed from IR1-3 on demand) + `move rt,a0`.
  - **H (r58) reads back sign-extended** (MFC2/CFC2 emitter 0x6e84), the HW quirk.
  - LZCS (r30) write computes **LZCR = clz(max(v, -v))** (0x4ad4: `subu a1,zero,v; max a1,a1,v; clz a1,a1`).
    This is off by one for v = -2^k (e.g. v = -1 gives 31; HW gives 32) [V decode; would be confirmed by a
    one-off test].

#### 1.4 MFC2/CFC2/MTC2/CTC2/LWC2/SWC2 in generated code

**Analysis records** (pass 0x5154; opcode switch 0xd4150 indexed by op-1, or by 0x40|funct-1 for SPECIAL):

| Instruction | record type | base cost a1 | Notes |
|---|---|---|---|
| MFC2/CFC2, rt≠0 | 6 | 3 | rec[2]=rt, rec[14]=rd (+32 for CFC2); flag 0x40 |
| MFC2/CFC2, rt=0 | 19 | 1 | effectively a nop |
| MTC2/CTC2 | 10 | 3 | 0x5420 |
| COP2 command | 18 | table 0xd46c4 | record flag 0x4000, rec[2]=-1 (0x5450) |
| LWC2 | 8 | 8 | 0x54c0 |
| SWC2 | 4 | 4 | 0x54d4 |

**Emitters** (switch 0xd435c by type):
- **LWC2 (0x6ea8):** a guest load (`0x3a90`) with opcode lh/lw/lhu chosen by the register class (0x6ecc:
  class 1 → `lh`, class 2 → `lhu`, else `lw`). The value is then stored into gp+4*reg by the store emitter
  0x46a0 (canonicalisation and special-register side effects).
- **SWC2 (0x6e2c):** loads gp+4*reg into a temporary (0x4504, the same specials as MFC2), then emits the
  guest store (`0x3a90` with `sw`).
- **MFC2/CFC2 (0x6e6c):** `lw rt, 4*reg(gp)`; the specials are listed in 1.3.
- **MTC2/CTC2 (0x6f18 → 0x7d6c → 0x46a0):** a class-specific store. The same emitter serves MTC0 (2.2) and
  carries the anti-LibCrypt hook (4.3).
- No separate GTE pipeline state exists at run time: the op runs at once and the latency is only charged
  (1.5).

#### 1.5 Cycle cost of GTE ops

- **Each op costs `gp+0xB42 + table[funct]`.**
- **Overlap model in the compiler (0x5b54-0x5c88).** The GTE is treated as concurrent with the CPU.
  - Starting at the next instruction, the compiler subtracts the cost of each following instruction from
    the op's latency.
  - It stops at the first MFC2/CFC2 with rd≠0 (record type 0x12 with rec[2]≠0), the first SWC2 (0x3A), or
    the first record of a type that must wait.
  - The remainder (minimum 1) is what gets charged.
  - If a branch ends the block first, the remainder is halved (`sra v1,1`, 0x5c14).
- **The same mechanism covers MULT/DIV.** MULT and MULTU cost 12, DIV and DIVU 35 (0x52ac: `1+11`, 0x52a8:
  `24+11`). They are hidden until MFHI/MFLO.
- **cfg flags bit 28 turns the overlap model off**: every instruction then costs `1 + gp+0xB42`
  (`ext t2,gp+0x6ac,28,1; movn v1,1`, 0x5c94).

#### 1.6 The "execute pending GTE op before the interrupt" rule (0x95e8-0x9630)

When the scheduler decides to take an interrupt:
```
s1 = PC
if PC in RAM (bits 23..28 == 0):
    loop: w = RAM[PC & 0x1FFFFF]
          if (w >> 25) != 0x25: break          // not a COP2 command
          gp+0x1B0 -= 1                         // 1 cycle charged per op
          PC += 4
          call table_0xd45c4[w & 0x3F](a0 = w)  // flag-computing variant
EPC (gp+0x138) = PC                             // after the executed GTE ops
take exception 0x94c4(ExcCode 0)
```
Every consecutive GTE command is executed, so the BIOS handler's own "EPC points at a GTE op" check never
fires. For a PC in BIOS, EPC is the PC unchanged.

---

### 2. Interrupts and COP0

#### 2.1 I_STAT / I_MASK (0x1F801070/74)

- Registered at 0x1a2e0: read handler 0x9850, write handler 0x98c4. Storage: I_STAT gp+0x2070, I_MASK
  gp+0x2074.
- **Raising an IRQ (0x96e4, a0 = bit mask):**
  ```
  if (I_STAT | a0) != I_STAT:
      I_STAT |= a0
      if a0 & I_MASK:
          Cause.bit10 = 1                       // IP2
          if (SR.IEc ? SR : 0) & Cause & 0xFF00:
              time_head -= t9; t9 = 0           // gp+0x1AC -= gp+0x1B0; gp+0x1B0 = 0
  ```
  t9 = 0 forces the next block exit into the scheduler.
- **IRQ sources found:**

  | Bit | Source | Raised at | Note |
  |---|---|---|---|
  | 0x1 | VBLANK | 0x114ac | |
  | 0x4 | CD-ROM | 0xc328, 0xd32c | |
  | 0x8 | DMA | 0x8ba8 | |
  | 0x10/0x20/0x40 | timers | 0x9b08 | byte +0x1C of each counter |
  | 0x80 | SIO | 0xa23c | `lbu 23(a0) << 7` |
  | from 0x85e4 | SPU | 0x85e4 | the mask is `v0` [I] |

- **Writes (0x98c4):**
  - offset 0 (I_STAT): `I_STAT = value & I_STAT`, i.e. acknowledge by writing 0.
  - offset 4 (I_MASK): store.
  - Then Cause bit 10 is set to `(I_STAT & I_MASK) != 0` if that value changed, and the same t9 = 0 forcing
    as above applies.
- **Read acceleration (0x9850).**
  - Reads return gp+0x2000 + (addr & 0xFFC).
  - If this read comes less than 40 cycles after the previous I_STAT/I_MASK read and t9 > 0, the handler
    burns `n = min(2*prev, t9, 80)` cycles (t9 -= n). The sequence starts at 4 and runs 8, 16, 32, 64, 80, 80...
  - Busy-wait polls on I_STAT therefore fast-forward to the next event.

#### 2.2 COP0

- **Registers** are at gp+0x100+4*r (section 0).
- **Reset (0x1a2a4):** SR = 0x00400000 (BEV), PRId = 1, LZCR (gp+0x7C) = 32, PC = 0xBFC00000.
- **MTC0 SR** (emitter 0x46a0, offset 0x130): `SR = v & 0xF27DFF3F`, so bits 6-7, 17 (SwC), 23, 24, 26, 27
  are dropped. The emitter then adds `sw t9,0x1B0(gp); jal 0x96ac; lw t9,0x1B0(gp)`. 0x96ac re-checks
  `(SR.IEc?SR:0) & Cause & 0xFF00` and forces t9 = 0.
- **MTC0 Cause** (offset 0x134): only bits 8-9 are written (`lb 0x135; ins; sb`), followed by the same
  re-check.
- **RFE** (emitter 0x6db0, inline): `lw a1,0x130(gp); srl a2,a1,2; ins a1,a2,0,4; sw a1,0x130(gp)`, then
  `jal 0x96ac`. KUo/IEo are kept.
- **Exception entry 0x94c4(a0 = ExcCode):**
  ```
  Cause = (Cause & ~0x7C) | (ExcCode << 2)         // ins t0,a0,2,4 (bits 2-5)
  SR    = (SR & ~0x3F) | ((SR << 2) & 0x3F)        // push KU/IE stack
  if SR.BEV (bit 22): PC = 0xBFC00000, host = blocktable[0x09E00000]   // RESET vector, not 0xBFC00180 [V 0x952c]
  else              : PC = 0x80000080, host = gp+0x1D4 (cached) or compile(0x58c0)
  gp+0x1B4 = host
  ```
  - The caller sets EPC (and BadVaddr). Cause.BD is never set.
  - Callers: the interrupt path (0x9638, code 0), syscall/break, and the compiler's address error at
    0x5920 (BadVaddr = PC, code 4).
- **When interrupts are taken.** The check is in the scheduler 0x953c, at its end (0x95a0-0x95e8), after
  due events:
  - `lh gp+0x1C0`: <0 → reset (0x24b58); >0 → flush cache (0x7e60).
  - Then if `(SR.IEc?SR:0) & Cause & 0xFF00`, take the interrupt (1.6).
  - **Latency:** the scheduler runs only when t9 <= 0 at a block exit or branch edge. The emitter 0x6768
    emits `blez t9, stub` at every direct exit (stub → `jal 0x1a80`), and conditional edges use
    `bgtz t9` + `0x1a68`. Raising an enabled IRQ zeroes t9, so the interrupt is taken at the end of the
    current block.

---

### 3. Scheduler, root counters, vblank, DMA

#### 3.1 Event list (0x945c insert, 0x9668 remove, 0x953c run)

- **Node** = {+0 next, +4 prev, +8 absolute time, +0xC callback}, sorted by time. Head time is in
  gp+0x1AC, and t9 = head time − now. **now = gp+0x1AC − gp+0x1B0**, wrapping 32-bit.
- **Init (0x1a284):** gp+0x1AC = gp+0x1B0 = 0x0204CC00 = 33,868,800 (one second).
- **Insert(node, delay):** time = now + delay, sorted insertion. If the node becomes the head and t9 > 0, it
  sets t9 = delay and head time = time.
- **Run (0x953c):**
  - `elapsed = gp+0x1AC − gp+0x1B0`;
  - while `head.time − elapsed <= 1`: unlink, set gp+0x1B0 = 0, call the callback, then
    `elapsed −= gp+0x1B0`;
  - finally gp+0x1AC = head.time and t9 = head.time − elapsed.
  - Then the interrupt check (2.2). It returns gp+0x1B4 (the resume address).

#### 3.2 Root counters 0x1F801100-0x1F80112F

- **Init (0x1a3a8).** Three structs at gp+0x64C + 32n:

  | Offset | Field |
  |---|---|
  | +0..+0xC | event node; callback 0x9ad0 |
  | +0x10 | target; 0 is stored as 0x10000; bit 31 = paused |
  | +0x14 | base time |
  | +0x18 | mode (bits 0-9 as written, bit 10 IRQ, bit 11 reached target, bit 12 reached 0xFFFF) |
  | +0x1C (b) | I_STAT bit = 0x10 << n |
  | +0x1D (b) | clock shift |

- **I/O registration:** 0x1F801100+16n (count, mode) read 0x9be0, write 0x9c60; +8 (target) write 0x9c60.
- **Count (0x9b6c):**
  ```
  c = (now - base) >> shift
  if c >= target: mode |= 0x800; if mode & 8 (reset at target): c %= target; base = now - (c << shift)
  elif c >> 16:   mode |= 0x1800; c &= 0xFFFF; rebase
  ```
- **Clock sources** (mode write, 0x9e18-0x9e5c):

  | Counter | mode bit | shift | effective clock |
  |---|---|---|---|
  | 0 | bit 8 | 3 | **sysclk/8 stands in for the dot clock** (approximation: the real dot clock depends on the horizontal resolution) |
  | 1 | bit 8 | 11 | **sysclk/2048 stands in for hblank** (approximation: the real line is about 2152.5 cycles NTSC and 2167.6 PAL) |
  | 2 | bit 9 | 3 | sysclk/8, correct |

- **Writes:**
  - count (+0): resets the counter to 0; the written value is ignored [V 0x9ce0].
  - mode (+4): stores bits 0-9, resets the count, recomputes the shift.
  - target (+8): store (0 → 0x10000), then reschedule.
- **Reads:** mode reads return the value and clear bits 10-12 (`andi 0x3FF`, 0x9c54). Count reads are
  sign-extended for `lh`.
- **Sync modes:** only counter 1 with sync enable is modelled. It pauses (target bit 31) when
  `(mode & 6) ^ (in_vblank << 1) == 6`, with in_vblank from gp+0x3662 bit 0 (0x9d0c). Counter 0 hblank
  sync and the counter 2 stop modes are not emulated [I: 0x9db4-0x9e10 only special-cases counter 1].
- **IRQ (callback 0x9ad0).** If mode & 0x30 (IRQ on target or overflow):
  - toggle mode (bit 7) flips bit 10;
  - one-shot (bit 6 clear) clears 0x30;
  - raises the IRQ at +0x1C;
  - reschedules at `(next_limit − count) << shift`, with next_limit = target when mode & 0x18, else
    0x10000.
  - If target > 16384 and cfg flags bit 12 is clear, it also calls the audio pacing 0x11520.

#### 3.3 VBLANK and the frame period (0x11410, 0x1265c, 0x15f54)

The frame is two events on the node gp+0x35E4:

1. **Vblank start (0x11410).**
   - Increments gp+0x3662, runs counter-1 sync (0x9940(1)), raises **IRQ 0 at once**.
   - Schedules display start after `round((H − 2*(Y2−Y1)) * halfline)`.
   - H = 526 NTSC progressive, 525 NTSC interlaced, 628 PAL progressive, 625 PAL interlaced.
   - halfline = 1076.27 (float 0x448688CE) NTSC or 1083.80 (0x448779A7) PAL.
   - Then presents (0x115b4) if the display address changed (gp+0x3610/12 vs gp+0x35C8).
2. **Display start (0x1265c).**
   - Increments gp+0x3662 and runs 0x9940(0).
   - Schedules vblank after `round((Y2−Y1) * line) − cfg[22]`, with line = 2152.55 (0x450688CE) NTSC or
     2167.60 (0x450779A7) PAL.
   - If cfg[22] > 0, an intermediate callback 0x15fe4 fires cfg[22] cycles early, presents, and schedules
     the vblank for the rest.
   - 0x15f54 also pays back the cycle-credit pool: t9 −= gp+0x1E4.

Resulting frame lengths:

| Mode | lines | cycles/frame | rate |
|---|---|---|---|
| NTSC progressive | 263 | 566,121 | 59.83 Hz |
| NTSC interlaced | 262.5 | 565,044 | 59.94 Hz |
| PAL progressive | 314 | 680,626 | 49.76 Hz |
| PAL interlaced | 312.5 | 677,375 | 50.00 Hz |

The vblank length depends on the game's vertical display range. **There is no hblank event or IRQ.**

Other timing pieces:
- **Pacing to the audio clock (0x11520).** It compares emulated cycles since a mark with
  `samples*768` (768 = 33,868,800/44,100), where samples is the ME counter at 0x49F40294. If emulated time
  is ahead by more than 3387 cycles, it calls `sceKernelDelayThread((excess*1741>>16) >> k)` [V].
- **60 Hz tick (0x12710).** Period 0x89D00 = 564,480 cycles divided by cfg[11] & 0xFFFF; it calls 0x11520.

#### 3.4 DMA 0x1F801080-0x1F8010F4

- **Channel struct** (gp+0x1E8 + 28*ch): event node, +0x10 start handler, +0x14 stall cycles, +0x18 mode
  bits, +0x1A channel number. Registered with `0x1a1f0(ch, handler)`; completion callback 0x8b1c.

| ch | handler | registered at |
|---|---|---|
| 0 MDEC in | 0xf54c | 0x1b688 |
| 1 MDEC out | 0xf654 | 0x1b698 |
| 2 GPU | 0x12c74 | 0x1baa0 |
| 3 CD-ROM | 0xce18 | 0x1aed8 |
| 4 SPU | 0x8698 | 0x19fd0 |
| 6 OTC | 0x9364 | 0x1a134 (set directly) |
| 5 | none registered | — |

- **Registers:** MADR/BCR read 0x9158 (refunds 4 cycles: `gp+0x1B0 += 4`); CHCR write 0x92a4; DPCR/DICR
  write 0x91bc.
- **DPCR** is reset to 0x77777777 (0x1a194).
- **CHCR write (word only):** store, then:
  - bit 24 set → start 0x8e4c;
  - bit 24 cleared → cancel the pending event; for ch2 also call 0x125f0; start the next pending channel.
- **Start (0x8e4c).**
  - Requires DPCR enable and CHCR bit 24. Priority against active channels uses the DPCR priority fields;
    blocked channels go to the gp+0x2AC pending mask.
  - Byte count from BCR by sync mode: mode 0 → BCR.lo (0 → 0x10000); mode 1 → blk*cnt; mode 2 → 0.
  - Calls `handler(MADR & 0xFFFFFC, bytes, CHCR)`. The return value v0 means:
    - **>0** = duration in PS1 cycles. MADR advances (step by CHCR bit 1).
      - Sync mode 0 without chopping: **the CPU is stalled for the whole transfer + 3 cycles** (t9 −= ...)
        and the channel completes synchronously (0x90b0-0x90d4).
      - With chopping: stretched by the CHCR window fields (0x907c).
      - Modes 1 and 2: a completion event after v0 cycles; other channels' events are shifted (0x8bb8).
        [I: exact stall split]
    - **0** = pending: completion comes later from the device (MDEC, GPU).
    - **<0** = pending with MADR = v0 & 0xFFFFFC.
- **Completion (0x8b1c):**
  - t9 −= struct+0x14 (stall); CHCR bit 24 cleared; BCR = 0.
  - **If DICR bit 23 and DICR bit (16+ch):** DICR |= (0x01000000<<ch) | 0x80000000 and IRQ 3 is raised.
  - Otherwise **the flag is not stored at all** [V 0x8b78].
  - Then the next pending channel starts (0x8d88).
- **DICR write:** flags 24-30 are cleared by writing 1, bits 0-23 are written directly, and bit 31 is
  recomputed as `bit23 && any(flag 24-30)`. The per-channel enable and bit 15 are ignored in bit 31.
- **OTC (0x9364):** only CHCR == 0x11000002. Writes `n = min(BCR, MADR)` entries backwards: each entry =
  addr−4, the last = 0x00FFFFFF. Over 8192 entries it goes through the uncached alias. **Duration = n
  cycles.**
- **MDEC durations:** section 4.

---

### 4. MDEC (0x1F801820/24; decoder 0xe8f8-0xf54c)

**Registration (0x1b6a0):** read 0xf6dc, write 0xf70c. Constants are copied to gp+0x400 (28 bytes from
0xd499c), and gp+0x648 = 0xe8f8.

**Command and control:**
- **Write 0x1820 (0xf70c):** gp+0x620 = command. Nothing runs until DMA0.
- **Write 0x1824:** if bit 31 (reset), cancel the MDEC event, zero gp+0x620..0x64B, re-register (0x1b678).
  Other bits are ignored.
- **Status read (0xf6dc):**
  - bit 31 is always set;
  - bits 23-26 are command bits 25-28;
  - bit 29 (busy) = remaining parameter bytes >= 5;
  - bits 0-15 = remaining bytes / 4.
- **DMA0 handler (0xf54c), command = gp+0x620 >> 29:**
  - **1 (decode):** remaining = (cmd & 0xFFFF)*4 bytes. The input pointer is set straight into PS1 RAM
    (0x09800000 | MADR), so **the input is not copied** (lazy). Byte 0 of gp+0x3668 is set to 1. If DMA1
    is already armed, it decodes now.
  - **2 (quant tables):** reads 128 bytes. Each byte q[i] becomes the float
    `q*scale[i&63] (×8 for DC) * 2^-43 (*1.772 for the first 64 = luma)` at gp+0x420+8*(i&63)+4*(i>>6).
    scale = 0xd49b8, 64 AAN prescale factors ×2^15 in zigzag order.
  - **3 (IDCT matrix) and anything else: ignored.** The IDCT is fixed.
  - Returns 0 (pending).
- **DMA1 handler (0xf654):** output pointer = 0x49800000 | MADR (the **uncached** alias of PS1 RAM); bytes
  wanted = a1. If input is available:
  - with cfg flags bit 17: schedule the decode after `4*bytes` (24-bit) or `6*bytes` (15-bit) cycles;
  - otherwise decode now.

**Decoder (0xe8f8):**
1. **RLE:** skip 0xFE00 padding. q_scale = bits 10-15 of the first halfword; DC = 10-bit signed value.
2. **Dequant:**
   - DC: `float(DC<<?)*qt[0]` + 128.0 (or 132.0 in 15-bit mode, for rounding: `0x10C0|bit27 << 18` as float
     into $f2, 0xe99c).
   - AC: `vmul` (level*q_scale) * qt[k], then **a computed jump into a table of 63 two-instruction stubs**
     (0xea7c-0xec70). Each stub does `vmul.s S_xy` at the zigzag position, so the whole 8×8 block is held
     in the VFPU matrices M000-M300.
3. **IDCT: VFPU float AAN.** `vbfy1`/`vbfy2` butterflies, `vscl` with 1.082392, 1.414214, 1.847759,
   2.613126 (gp+0x400), rows then columns. Not bit-exact with the PS1 integer IDCT.
4. **YUV→RGB:**
   - Cr/Cb are upsampled 2× horizontally by `vpfxt x,x,y,y`.
   - Constants 0.791196 (=1.402/1.772), 0.509486 (≈0.714/1.402), 0.193962 (≈0.344/1.772), with the luma
     pre-scaled by 1.772.
   - `vf2in ×2^23` then `vi2uc` saturate to 0..255 and pack RGBA.
5. **Output:**
   - **24-bit** (cmd bit 27 = 0): `lwl`/`lwr` repack drops the alpha (0xf27c).
   - **15-bit** (bit 27 = 1): `ins` builds 5:5:5 + bit 15 from cmd bit 25 (0xf440).
   - Only bit 27 is tested: the 4-bit and 8-bit mono modes are not implemented (8-bit takes the 15-bit
     path), and **signed output (bit 26) is ignored** [I].
   - Output goes to PS1 RAM through the uncached pointer. **There is no VRAM shortcut in MDEC**; the
     24-bit unpack to 0x04100000 is on the GPU side.
6. **End:**
   - Leftover input under 128 bytes is copied to gp+0x380 and **DMA0 completes** (0x8b1c(gp+0x1E8)).
   - When the output request is satisfied: without cfg flags bit 17, the **DMA1 completion is scheduled after
     4 cycles/byte (24-bit) or 6 cycles/byte (15-bit)**, minus 3072 when over 6144. That is **3072 cycles per
     16×16 macroblock**: about 11,025 MB/s, about 27 ms for a 320×240 frame. With bit 17 it completes at once
     (the delay came earlier).
   - 0x8e00/0x8bb8 shift the other pending channel events by `(words+cycles)*33/32`.
7. **Timing of the decode itself:** it runs synchronously on the main CPU inside the DMA handler. Its host
   cost is not charged to t9.

---

### 5. Cycle model

#### 5.1 Charging

- The compiler accumulates per-instruction costs over a segment and emits **`addiu t9,t9,-N` at segment
  entry** (0x44dc; also in branch delay slots via 0x4340/0x44a8). The total per segment is precharged.
- t9 is tested at every block exit (section 2.2).
- The scheduler and I/O handlers see t9 through gp+0x1B0.

#### 5.2 Per-instruction cost

The record cost is `rec[15] = gp+0xB42 + a1`.

**Base a1** (from the 0xd4150 switch):

| Instruction | a1 |
|---|---|
| ALU/shift/branch/jump/store | 1 |
| load (lb..lwr) | 5 |
| MULT/MULTU | 12 |
| DIV/DIVU | 35 |
| MFC2/CFC2/MTC2/CTC2 | 3 (1 when rt = 0) |
| LWC2 | 8 |
| SWC2 | 4 |
| GTE command | table 0xd46c4 |

**gp+0xB42 = extra per instruction by fetch region** (0x5994-0x59c4): 0 for KUSEG/KSEG0 RAM (cached), **5
for KSEG1 RAM (uncached)**, **2 for BIOS ROM**.

**Load latency overlap** (0x5ac4-0x5c88, only when gp+0xB42 == 0):
- The load's 5 is reduced by the costs of following instructions until one reads the loaded register or is
  another memory access. The next instruction (the load delay slot) always counts.
- The minimum is 1; at a block-ending branch the remainder is halved.
- GTE and MULT/DIV use the same model (1.5).

**Run-time region adjustments in the memory helpers** (0x1a90-0x28cc; generic helper, backpatched to
region-specific ones):

| Access | lb/lbu | lh/lhu | lw | Notes |
|---|---|---|---|---|
| RAM | 0 | 0 | 0 | |
| scratchpad | +4 refund | +4 refund | +4 refund | net 1 cycle |
| BIOS | 0 | −1 | −3 | net 5/6/8 |
| I/O | per handler | per handler | per handler | DMA reg reads refund 4 |
| unmapped/expansion reads | | | | return 0, or 0xFF/0xFFFF/−1 at 0x1F802030/40 |
| BIOS writes | | | | dropped, cost (1<<size)+15 (0x8a08) |

- Stores to RAM and scratchpad cost only their base 1 (write buffer). Address-only helper 0x28d0: BIOS
  costs 3 net.
- **I-fetch penalty** (0x5cac-0x5d30, only when gp+0xB42 == 0): at each block end, and at each branch
  whose segment is over 1024 instructions or is a forward exit, the compiler adds
  `(instructions since the last penalty point) >> gp+0x1C2`.
  - Backward branches (loops) inside a block of 1024 instructions or fewer (4 KB = the PS1 I-cache) are
    exempt.
  - The first segment's penalty is stored in gp+0xB44 and charged at block entry.
  - **gp+0x1C2 = cfg[19]** if ≥ 0, else bit 1 of cfg[0]. **The default is 1, so straight-line code runs at
    about 1.5 cycles per instruction.**
  - cfg[19] per game: 0 (+1 per instruction), 1, 2, 3, 4, 5, 10 or 16 (off).
- **Interrupt-path GTE:** 1 cycle per op (1.6).
- **Not charged:** compile time, the SMC checksum prologue (0x4b44: the first cfg[10] words of a block, 2 by
  default, compared at entry; on mismatch recompile through 0x2878), MDEC host time.
- **Cache-control hook:** a write of 0x804 to 0xFFFE0130 (the BIOS FlushCache sequence) costs 32 cycles and
  **flushes the whole translation cache** (0x8a30-0x8a44).

#### 5.3 Clock scaling and overclock

There is no CPU clock multiplier. The CPU-time knobs are the per-game config: cfg[19] fetch penalty, flags
bit 28 overlap off, cfg[0] compile flags. The device-time knobs are flags bit 17 (MDEC delay), cfg[22]
(present lead) and cfg[11] (tick divider). The time base stays 33.8688 MHz, with the 60/50 Hz constants in
3.3.

#### 5.4 Busy-wait accelerators affecting time

I_STAT polling (2.1), the SIO wait shortcuts (0x9fb0, 0xa1ac, using the credit pool gp+0x1E4), and pacing
to the audio clock (3.3).

#### 5.5 Per-game configuration (loader 0x24770)

- **Defaults:**
  - `memset(gp+0x6B0, 0xFF, 0x80)`: cfg[0..31] = −1;
  - gp+0x6AC (flags) = 0; cfg[16] = cfg[23] = cfg[24] = 0.
- **Built-in table at 0xef0b0** (12-byte entries {key, count, ptr} → `count × {index, value}`;
  index −1 = flags). Key = `BE32(ID[0..3]) ^ (n<<12) + n` with `n = strtol(ID+4, 0, 16)`.
  - It has **820 entries**.
  - Most-set entries: cfg[4] 322 times, flags 362, cfg[14] 209, cfg[2] 217, cfg[19] 198, cfg[0] 189,
    cfg[1] 183, cfg[11] 150.
- **ID "PSRM"** (string at 0xf18e8): extra defaults cfg[4] = −2, cfg[1] = 232, cfg[2] = cfg[3] = 12,
  cfg[11] = 0x10004.
- **EBOOT override:** if the PSISOIMG word at +0x420 > 0x06060000, the flags word = +0x428 and
  cfg[0..31] = +0x42C..+0x4AB (0x248f4). The word at +0x424 must be ≤ 0x06060000 or the load fails
  (0x80000002).
- **Post-processing:** cfg[5]++. cfg[10] = 2 by default, 0xFFFF if cfg[0] bit 2 and cfg[10] < 0 (the SMC
  checksum length).
- **Uses found in these subsystems:**

  | Word | Effect |
  |---|---|
  | cfg[0] | compile flags gp+0xB40; −1 → 3; 0x4007 for the PC in cfg[13] |
  | cfg[12] | block-length limit address |
  | cfg[13] | special-compile PC |
  | cfg[17] | PC that requests a translation-cache flush when reached; bit 0 = also force t9 = 0 (0x6094) |
  | cfg[19] | fetch-penalty shift |
  | cfg[22] | present lead before vblank |
  | cfg[11] | tick divider |
  | flags bit 12 | timer pacing |
  | flags bit 17 | MDEC delay |
  | flags bit 28 | no overlap model |
  | flags bit 3, bit 29 | present conditions |

  Other words belong to GPU/CD/SPU.

---

### 6. BIOS: LLE, and every hook found

The BIOS (embedded "System ROM Version 4.5 05/25/00 J", module 0x53C20) runs as normal code through the
dynarec. Its blocks go to the 0x09B80000 cache and the 0x09E00000 table, with gp+0xB42 = 2.

**There is no A0/B0/C0 dispatch interception and no CD, memory-card or pad HLE.** Those are emulated at the
hardware-register level (SIO 0x1F801040 state machine 0xa508-0xa888 handling 'R'/'W' card commands; CD-ROM
0x1F801800 in other code). The searches covered `lui 0xbfc0`, li 0xA0/0xB0/0xC0, and PC compares in the
compiler; the only PC constants compared are 0x80000080 and the cfg PCs.

| # | Hook | Trigger | Replacement / effect | Evidence |
|---|---|---|---|---|
| 1 | Japanese shell | disc ID[2] == 'P' (SLPS/SCPS...) at config load | `memcpy(BIOS+0x18000, module 0x3DB00, 0x16120)`. It replaces the 4.5 compressed shell at 0xBFC18000-0xBFC2E11F with an **uncompressed v1.0 shell** ("System ROM Version 1.0", "Licensed by…", "Not PS Disk", "Shell Opened !!") | 0x247cc-0x247f8 |
| 2 | Anti-LibCrypt | MTC0 to cop0 r3 (BPC) whose previous instruction is AND/OR, source register not a known constant, and PSISOIMG+0x12B0 (0x09E812B0) ∉ {0, 0x72D0EE59} | compiled as `li tmp, word ^ 0x72D0EE59; sw tmp, 0x10C(gp)`: the BPC write gets the image's key | 0x6f18-0x6fd0 |
| 3 | FlushCache | write 0x804 to 0xFFFE0130 | flush the whole translation cache (0x7e60), cost 32 cycles | 0x8a30 |
| 4 | SR.IsC stores | stores while SR bit 16 (IsC) is set | compiled as record type 19 (not a RAM store) [I: the type-19 emitter 0x7a88 was not read] | 0x5498 |
| 5 | Exception with BEV=1 | any exception while SR.BEV | jumps to **0xBFC00000** (reset), not 0xBFC00180 | 0x94e4, 0x952c |
| 6 | BIOS ROM writes | stores to 0x1FC00000-0x1FC7FFFF | dropped, cost (1<<size)+15 | 0x8a08 |
| 7 | cfg PC hooks | PC == cfg[13] (special compile flags) or cfg[17] (flush/sync); RAM addresses in the table | per game | 0x597c, 0x51d8, 0x6094 |

Init writes into the BIOS copy: only hook 1.

---

### 7. What this means for psxrt (short)

- **Bit-exact divide is cheap.** POPS uses a 128 KB UNR table (32K words) and one `maddu`. psxrt can reuse
  the same layout.
- **FLAG liveness is worth having.** POPS computes FLAG only when a CFC2 can see it, and computes bit 31
  only on read. Its lighting ops are float: psxrt's integer GTE is already more exact; keep it.
- **POPS deviations to avoid copying:** LZCR off by one for −2^k; FLAG writes dropped; NCT/DCPL/DPCT costs
  that differ from psx-spx; dot clock = sysclk/8; hblank = 2048 cycles; BEV exception to reset; DICR flag
  not latched when its IRQ is disabled; MDEC 4/8-bit and signed modes and the IDCT table ignored.
- **Timing references worth matching:**
  - frame = (263|262.5|314|312.5) × (2152.55|2167.6) cycles;
  - MDEC 3072 cycles per macroblock;
  - OTC 1 cycle per word;
  - burst DMA stalls the CPU for the transfer + 3;
  - load 5 with overlap, MULT 12, DIV 35;
  - I-fetch +1/2 per instruction outside 4 KB loops.
