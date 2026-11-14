# PSX2PSP Recomp — native backend

A second way to build an EBOOT.PBP: instead of packing the disc for Sony's POPS emulator, the
game's own PS1 code is translated ahead of time into PSP Allegrex code and run on a native
runtime. Everything around the code is shared with the POPS path: disc reading, game ID,
PARAM.SFO, ICON0/PIC0/PIC1/BOOT framing, ICON1.PMF, SND0.AT3 and the PBP writer.

Why: POPS emulates a PS1 at real speed and the PSP cannot emulate it much faster (Sheep Dog
'n' Wolf, 2026-11-14: heavy frames take 1.5-1.9 PS1 vblanks; the PSP's CPU is at 75-78% doing
it). Run natively on the 333 MHz Allegrex, the same game code takes a fraction of a frame.

## Model

Static recompilation is per game, like every project of its kind (N64Recomp, PSXRecomp,
psp-web-recomp): the tool is generic, each supported game has a **recipe**. The recipe is data
(function hints, jump tables, overlays, hooks), in the same spirit as `patches.ini`. POPS stays
the universal path for every other disc.

The game's code never ships with PSX2PSP. The translator runs on the user's disc at conversion
time; the runtime contains no game code.

Prior art is reused, not rediscovered. Licences decide how:
- RecompOne (MIT): code may be ported, keeping its copyright notice.
- psxrecomp (PolyForm Noncommercial 1.0.0 + its PERMISSIONS.md): code may be ported into files
  that keep that licence and are marked as such; the project stays free of charge. Everything
  else stays MIT OR Apache-2.0.
- ps1-recomp (GPL-3.0): reference only, no code copied.

No Sheep Dog recipe exists in any of the three (searched SLES-02895, SLUS-01142, "sheep").

### What comes from where (map of 2026-11-14)

| Component | Source | Licence | Notes |
|---|---|---|---|
| Function discovery | RecompOne `Analysis/FunctionDetector.cs`, `FunctionPipeline.cs` | MIT | Ours (S2 2/4) already matches S0; add their fixpoint passes (escaping branches, cross-image calls, constant pointers) when a game needs them |
| Jump tables | RecompOne `Analysis/JumpTableAnalyzer.cs` | MIT | Register-state tracking; a table always keeps a dispatch default |
| Untranslated code | psxrecomp three tiers (`dirty_ram_interp.c`, `psx_dispatch_impl`, call contract) as the model | NC | Reimplemented in C, not copied. Their tier 2 (DLL compiled at run time) is impossible on PSP |
| Overlays | RecompOne `Dispatcher.LoadByLba`, `PSMemory.TrackWrite` | MIT | An overlay's table becomes active on load; overlapping ones are dropped |
| Interrupt delivery | RecompOne `Hardware/Interrupts.cs` | MIT | Poll at back-edges; PSP vblank instead of a wall clock; SysEnqIntRP chains and IntrEnv handler |
| Recipe schema | RecompOne `Config/ConfigLoader.cs` keys | MIT | Same fields, written as INI like `patches.ini` |
| GTE | RecompOne `Hardware/Gte.cs` | MIT | Integer core 1:1 (22 commands, flags, UNR table); its float extras are the widescreen hook |
| GPU packets -> GE | RecompOne `sdk/LibGpu` (DrawOTag walk) + `Gpu/GpuCommands.cs`; psxrecomp `gpu_sw_renderer.c` for accuracy | MIT / NC (reference) | Packet parser and OT walk ported; the GE backend is ours |
| SPU, XA | RecompOne `Hardware/Spu.cs`, `XaAudio.cs` | MIT | Reverb cost on Allegrex to be measured |
| MDEC | RecompOne `Hardware/Mdec.cs`, `LibPress` | MIT | Media Engine not used at first |
| CD | RecompOne `sdk/LibCd*`, `Cdrom/CdController.cs` | MIT | libcd-level HLE preferred; controller for games that bypass it |
| BIOS HLE | RecompOne `Bios/BiosA.cs`, `BiosB.cs`, `BiosC.cs` | MIT | BIOS threads become PSP threads |
| PsyQ signatures | RecompOne `AutoConfigure/signatures/psyq.json`, `SdkMatcher.cs` | MIT | Runs in the converter, not on the PSP. ps1-recomp's Crash numbers: 61 of 174 matched functions were safe to replace |
| Mod hooks | RecompOne `Modding/HookManager.cs` (idea) | MIT | A recipe entry replaces or wraps a function; widescreen and frame-rate patches go there |

What we keep of our own, because the PSP needs it:
- Direct Allegrex emission (all three donors emit C, C# or C++ and need a compiler).
- The load-delay check of S2 2/4: none of the MIT/GPL donors models load delays; psxrecomp
  does. Sites the checker finds are emitted with a temporary; Sheep Dog has none.
- Dispatch miss: the runtime's interpreter runs the code, as in psxrecomp, instead of aborting
  (ps1-recomp, RecompOne). Each miss is logged with its PC so the recipe can add the function.

## Architecture

| Part | Where | What |
|---|---|---|
| Translator | `crates/psx2psp-recomp` (Rust) | R3000A decoder, control-flow discovery, recipe loader, Allegrex emitter (direct machine code, no C compiler on the user's PC), indirect-jump dispatch table, differential interpreter for tests |
| Runtime | `psp/psxrt` (C, pspsdk) | Prebuilt ELF embedded in the converter: BIOS HLE, PsyQ-level HLE where it pays (DrawOTag, LoadImage, CdRead, ...), GPU packets -> GE, GTE, SPU/XA -> sceAudio, MDEC, pad, memory card -> file, CD reads from DATA.PSAR |
| Packaging | `psx2psp-engine` | Output mode Native: PARAM.SFO category MG, DATA.PSP = runtime ELF, DATA.PSAR = translated code + compressed disc data. Same art and media pipeline as POPS |
| Recipes | `resources/recipes.ini` | Per-game data, user overrides merged on top like `patches.ini` |

Decisions taken before S0 (each re-checked against the S0 numbers):
- Direct Allegrex emission instead of C: R3000A and Allegrex share almost every encoding, the
  game code was already optimised by its compiler, and no toolchain is needed at conversion.
- PS1 RAM mapping, register use and load-delay handling follow what Sony's POPS does on the
  same CPU (S0 measures it in pops_01g), not a design of our own from scratch.
- Overlays: Sheep Dog's are 26 whole executables known in advance, all translated at
  conversion time (no runtime capture).

## Verification

1. Differential tests, offline: one MIPS interpreter in Rust runs the original R3000A code (with
   load delays) and the translated Allegrex code (interlocked) from the same state; registers
   and memory must match. Per instruction class, per function, then whole frames from traces.
2. Emulated PSP smoke tests of each built EBOOT (PPSSPP headless), if the Host allows the
   download.
3. The Host's real PSP at each milestone, with the same popshud-style counters on screen.

## Roadmap (one slice at a time, each one done before the next)

| Slice | Done when |
|---|---|
| S0 Feasibility | Measured: code discovery and indirect jumps, ISA surface and load-delay hazards, PsyQ/GPU/disc surface, POPS's own mapping of PS1 onto Allegrex. Go/no-go written in `S0.md` |
| S1 Native EBOOT shell | A runtime "hello" ELF packed by the converter as category MG with the shared art boots on a real PSP |
| S2 Translator core | Every EXE of the game translates; differential tests pass on all discovered functions |
| S3 Boot | SLES_028.95 + LOADER run on the runtime to the game's main loop (BIOS HLE, CD from PSAR), log on screen |
| S4 First picture | GPU packets drawn by the GE: the language wheel on screen |
| S5 Playable | Pad, GTE, level overlays: level 1 playable |
| S6 Sound and video | SPU, XA, MDEC FMV |
| S7 Release | Memory card, locked 60 fps measured on PSP, GUI mode "Native"; merge into master |

## Branches

`master` stays the releasable POPS build. Work happens on `recomp`; fixes to the POPS path land
on master first and are merged into `recomp`. `recomp` merges into master at S7, or earlier only
behind a mode that is off by default.
