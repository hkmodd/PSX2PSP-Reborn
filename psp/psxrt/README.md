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
line per second, goes to `psxrt.log` next to the EBOOT. HOME exits.

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

## DATA.PSAR (version 4)

Little endian, written by `write_native_psar` in `crates/psx2psp-engine`; offsets are from the
start of DATA.PSAR:

| Offset | Size | Field |
|---|---|---|
| 0x00 | 8 | `PSXRTDAT` |
| 0x08 | 4 | version (4) |
| 0x0C | 4 | header length (0xD0) |
| 0x10 | 16 | disc ID, NUL padded (`SLES02895`) |
| 0x20 | 128 | title, UTF-8, NUL padded |
| 0xA0 | 4 | raw sectors of the disc image (2352 bytes each; sector 0 is LBA 150) |
| 0xA4 | 4 | sectors per block (16) |
| 0xA8 | 4 | offset of the block index (0xD0) |
| 0xAC | 4 | blocks = ceil(sectors / 16) |
| 0xB0 | 4 | offset of the code directory (0: no translated code) |
| 0xB4 | 4 | entries in the code directory |
| 0xB8 | 4 | host address of the code range |
| 0xBC | 4 | size of the code range |
| 0xC0 | 4 | flags: bit 0 PAL60 (a PAL picture at 60 vblanks a second: patch database `pal60` with the 60 Hz switch) |
| 0xC4 | 4 | CPU shift: translated loops charge their PS1 cycles >> this (at most 8; 0 = R3000A cost). The converter writes 0; a `psxrt.cpu` file beside the EBOOT (decimal) overrides it for measurements |
| 0xC8 | 8 | zero |

The index holds blocks + 1 offsets: block i is `index[i]..index[i+1]`. A block is raw deflate,
or stored as is when deflate would not make it smaller (its length is then the raw length; only
the last block may be shorter than 16 sectors).

The blocks are followed by each translated executable's code and dispatch-table slice (one
word per text word: a host address, or MISS), stored the same way, then the directory, 0x40
bytes per executable (`CodeEntry` in `disc.h`):

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
| 0x38 | 8 | zero |

## Build

```
make                                   # pspdev toolchain, e.g. WSL with /usr/local/pspdev/bin on PATH
cp psxrt.prx ../../resources/psxrt.prx # the prebuilt module the converter embeds (tracked)
```

Only `resources/psxrt.prx` is tracked; the build outputs here are ignored. The converter embeds
the PRX at compile time, so rebuild `psx2psp` after replacing it.
