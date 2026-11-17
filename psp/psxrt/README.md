# psxrt

The native runtime of PSX2PSP's recomp backend (`docs/recomp/PLAN.md`): a user-mode PSP module
that the converter puts in DATA.PSP of an EBOOT built with `psx2psp convert --native`. It holds
no game code: the PS1 disc and, from S3b on, the translated code come from DATA.PSAR.

## Slice S3

The runtime boots the disc the way a PS1 does, on the shared core in `core/` (the same C files
the PC simulator `crates/psx2psp-sim` builds, so a fix lands on both):

| Part | Where |
|---|---|
| PS1 RAM 2 MB + scratchpad + context + dispatch table at 0x09800000-0x09C00FFF | reserved first, `PSP_SMEM_Addr` (`docs/recomp/S0.md`); if refused, a filler block takes the memory below it |
| VRAM, SPU RAM, MDEC buffers | ordinary `PSP_SMEM_Low` blocks |
| Disc | `disc.c`: DATA.PSAR of its own EBOOT (argv[0]), two inflated blocks cached |
| Boot | `psx_boot`: HLE BIOS reads SYSTEM.CNF and loads the EXE |
| CPU | translated code from DATA.PSAR (`native.c`, `trampolines.S`); the core's interpreter between translations, or for everything when the EBOOT has no code |
| Display | the PS1 display area of VRAM is drawn by the GE as a 5551 texture scaled to 480x272; 24-bit modes (MDEC movies) are converted to an 8888 texture first |
| Pad | PSP buttons map 1:1 to the PS1 pad, L/R to L1/R1 |

The overlay (debug screen over the picture) shows a status line (frame, PC, speed as % of the
PS1's 33.8688 MHz, fps) and the last log lines, which L+R+SELECT toggles (off at start: the CPU
draws them, which costs time every frame). Every line, plus a status
line per second, goes to `psxrt.log` next to the EBOOT. HOME exits. A `psxrt.watch` file beside
the EBOOT (up to 8 PS1 RAM addresses, hex) adds a line per second with each word's last value
and its sum over the game's display flips, to follow a per-frame variable such as a time step.

The heap is kept at 256 KB (`PSP_HEAP_SIZE_KB`) so it is taken from the bottom of the user
partition and cannot grow over 0x09800000.

## Native mode (S3b)

The converter translates every PS-X EXE on the disc (`crates/psx2psp-recomp`) and stores the code
in DATA.PSAR, each executable linked at its own host address below 0x09800000; executables loaded
at the same address share their host range. The runtime reserves that range and fills the
dispatch table (one word per PS1 RAM word) with MISS.

* When the disc or DMA writes RAM (`Psx.ram_loaded`), the entries it covers become DIRTY. On a
  miss at such an address the runtime hashes the text of each executable that covers it (its
  first 2 KB first) and, on a match, inflates that executable's code and table slice (evicting
  whichever shared its range). No match: the interpreter runs it. A MISS entry inside an
  executable whose slice is installed is code the translator did not find: interpreted, no
  hashing.
* Floating code (code the game runs wherever it loads it, as Crash 2's native GOOL pieces and
  Spider-Man's CD.WAD modules) is units translated once (`floating.h`): their head (guest words,
  sites, a map of the words they translate, a sparse index) is read below the code range, their
  bodies (host code, table, relocations) stay in DATA.PSAR. On a miss no executable serves, the
  runtime looks up the two words at the miss and up to 7 words back in the index and, when RAM
  holds a whole unit around them that translates the missed word, reads its body into an arena
  slot (at the start of the code range), relocates it to that guest and host address, writes its
  sites from RAM and installs its table entries. RAM may differ from the unit only at its sites:
  the immediates and shift amounts its translation copies, and the j/jal targets it holds in a
  lui/ori pair, which is what a game that links the code where it loads it rewrites. A placement
  is the unit with the site words it was placed with; a rewrite of one drops it. A
  placement whose words RAM still holds is reinstalled after a disc load; one whose words are
  gone gives its slot back when the arena is full. A live one is never moved: when the arena has
  no room the interpreter runs the code.
* Code the game's own stores rewrite is checked at each I-cache flush (FlushCache, end of a
  cache isolation), per 1 KB block of code words (translated words and their delay slots). In a
  block whose words changed, the patch table (below) says which: a word changed only in its
  immediate or shift amount keeps its translation, and the runtime writes the new immediate in
  every host word that copies it (`addi` as `addiu`). Any other changed word goes to the
  interpreter (DIRTY), with the translated words of the block and the block below that reach it
  without the runtime, by falling through or by a direct branch, jump or call (`coderun.c`,
  shared with the simulator); the rest of the block keeps its translation.
* Translated code enters the runtime through host calls (I/O, COP0, GTE, syscalls, resumes),
  dispatch misses and loop back-edges; the runtime interprets until an address with a
  translation and goes back there.
* Time: the PS1 cycle count advances with interpreted instructions and loops: each loop
  back-edge charges the loop's estimated PS1 cycles to a budget, and the runtime adds what was
  spent at its next entry, sleeping when the count gets ahead of the real clock: counted wait
  loops (PsyQ's `VSync` timeout) last as long as on the PS1. When the host falls behind, PS1 time
  slows down rather than jumping, so that devices never outrun the code serving them (a CD
  sector delivered while the guest could not run is a lost sector). A 500 us alarm makes the
  budget negative so that loops come back to take interrupts.

The log has a line per second with host calls, misses, interpreted instructions and executables
loaded.

## Snapshots, bench, recipes (S9)

Files beside the EBOOT drive the runtime for development (`docs/recomp/S9.md`):

| File | Effect |
|---|---|
| `psxrt.snap` | written by L+R+START (or a `psxrt.pad` press of those bits) at the next host call of the top-level code: RAM, devices, context, translation state. The next start resumes there instead of booting. Refused, and the disc boots, when it comes from another EBOOT or another translation (the code directory's hash); runtime-only changes keep it valid |
| `psxrt.bench` | seconds to measure after `BENCH_SKIP` (2); then one `-- bench` line (speed, vblanks, fps, ms/s by kind) and a stop |
| `psxrt.shots` | frame numbers to save as `shotNNNNN.bmp`; `rNNN` saves a RAM dump instead |
| `psxrt.pad` | `frame buttons frames` lines: scripted presses |
| `psxrt.spu` | `cpu`: the SPU mixes every voice; `both`: sceSasCore mixes its voices into a buffer nobody hears, beside the SPU, and the `-- sas` line compares the levels; anything else or no file: sceSasCore plays them (the default, S17) |

Recipes (`crates/psx2psp-recomp/src/hooks.rs`) are library code found by its exact words and
served by the runtime (`native.c`, "recipes") instead of its translation: libpad's
`_padChkRC2wait` and the /ACK wait of `_padSioRW2`, which spin on root counter 2, become one
host call that reads the same registers, gives the same results and lets PS1 time jump to the
expiry. The log's `hooks` counts them.

## sceSasCore (S17)

`sas.c` plays the SPU's ADPCM voices with sceSasCore, which the PSP runs on the Media Engine
(`docs/recomp/S17.md`). The core offers each voice at its key on, and a sustained voice once more
when it loops back. The voice is taken if the PSP's VAG player can play it: no pitch modulation,
noise or reverb on it, and a loop the player can follow. A taken voice still walks in the SPU
without being decoded, so ENDX, IRQs, envelope levels and volume sweeps stay exact. The voice
goes back to the SPU when the guest asks for something only the SPU does.

- The player reads a mirror of SPU RAM. Its flag bytes are rewritten for the VAG player.
- Pages the guest writes reach the mirror at the same sample of the stream.
- Commands carry the stream sample they belong to, and the audio thread applies them at the
  nearest 256-sample grain.
- If the sascore module does not load, the SPU mixes everything, as with `psxrt.spu` `cpu`.

The log's `-- sas` line counts, per second:

- voices taken, adopted, given back and refused (by reason);
- commands and lost commands;
- page copies;
- sceSasCore errors;
- grains and their time.

In `both` mode it also gives the RMS of both mixes.

## DATA.PSAR (version 9)

Little endian, written by `write_native_psar` in `crates/psx2psp-engine`; offsets are from the
start of DATA.PSAR:

| Offset | Size | Field |
|---|---|---|
| 0x00 | 8 | `PSXRTDAT` |
| 0x08 | 4 | version (6) |
| 0x0C | 4 | header length (0xE8) |
| 0x10 | 16 | disc ID, NUL padded (`SLES02895`) |
| 0x20 | 128 | title, UTF-8, NUL padded |
| 0xA0 | 4 | raw sectors of the disc image (2352 bytes each; sector 0 is LBA 150) |
| 0xA4 | 4 | sectors per block (16) |
| 0xA8 | 4 | offset of the block index of disc 1 (0xE8) |
| 0xAC | 4 | blocks = ceil(sectors / 16) |
| 0xB0 | 4 | offset of the code directory (0: no translated code) |
| 0xB4 | 4 | entries in the code directory |
| 0xB8 | 4 | host address of the code range |
| 0xBC | 4 | size of the code range |
| 0xC0 | 4 | flags: bit 0 PAL60 (a PAL picture at 60 vblanks a second: patch database `pal60` with the 60 Hz switch) |
| 0xC4 | 4 | CPU shift: translated loops charge their PS1 cycles >> this (at most 8; 0 = R3000A cost). The converter writes 0; a `psxrt.cpu` file beside the EBOOT (decimal) overrides it for measurements |
| 0xC8 | 4 | offset of the disc table (0: one disc) |
| 0xCC | 4 | discs in the table (0: one disc; else 2..5) |
| 0xD0 | 4 | offset of the floating code head (0: none) |
| 0xD4 | 4 | its stored length (equal to the raw length: stored as is) |
| 0xD8 | 4 | its raw length (0: no floating code) |
| 0xDC | 4 | host address the runtime loads the head at |
| 0xE0 | 4 | bytes from the code range's start where floating units are read and placed |
| 0xE4 | 4 | offset of the floating units' bodies (each deflated or stored, where the head says) |

A game of more than one disc has a disc table of 0x20 bytes per disc (disc 1 first): disc ID
(16, NUL padded), raw sectors, offset of its block index, blocks, zero. Each disc's blocks and
index are stored as disc 1's.

The index holds blocks + 1 offsets: block i is `index[i]..index[i+1]`. A block is raw deflate,
or stored as is when deflate would not make it smaller (its length is then the raw length; only
the last block may be shorter than 16 sectors).

The blocks are followed by each translated executable's code, dispatch-table slice (one word
per text word: a host address, or MISS) and patch table, stored the same way, then the
directory, 0x40 bytes per executable (`CodeEntry` in `disc.h`), then the floating code head
and its units' bodies (`floating.h`): code the game runs wherever it loads it, translated once
and moved where RAM holds its words.

| Offset | Size | Field |
|---|---|---|
| 0x00 | 4 | text address (PS1) |
| 0x04 | 4 | text size |
| 0x08 | 4 | FNV-1a 32 of the text, to recognise it in RAM |
| 0x0C | 4 | host address of its code |
| 0x10 | 4 | code length |
| 0x14 | 4 | offset of the code |
| 0x18 | 4 | stored length of the code (equal to the raw length: stored as is) |
| 0x1C | 4 | offset of the table slice |
| 0x20 | 4 | stored length of the table slice |
| 0x24 | 16 | file name on the disc, NUL padded |
| 0x34 | 4 | FNV-1a 32 of the first 2048 bytes of the text (all of it when shorter): checked before the whole text, so data loaded over the address costs 2 KB of hashing, not the text size |
| 0x38 | 4 | offset of the patch table (0: none) |
| 0x3C | 4 | groups in the patch table |

The patch table is one group per 4 KB of RAM from the 1 KB block of the text's first word: a
(offset from the table, stored length, raw length) triple per group, then the groups, each
deflated or stored. A group's words: 5 offsets (in words, from the group's start) where each
1 KB block's entries start and the last ones end, then one entry per code word: `index | n << 8`
(`index` the word in its block), then `n` host addresses of its copies when its rewrite could be
patched (an I-type ALU immediate or a constant shift), or for `n` 0 the word as translated (any
rewrite of it is a change). A group that would pass 32 KB has only `n` 0 entries.

## Build

```
make                                   # pspdev toolchain, e.g. WSL with /usr/local/pspdev/bin on PATH
cp psxrt.prx ../../resources/psxrt.prx # the prebuilt module the converter embeds (tracked)
```

Only `resources/psxrt.prx` is tracked; the build outputs here are ignored. The converter embeds
the PRX at compile time, so rebuild `psx2psp` after replacing it.
