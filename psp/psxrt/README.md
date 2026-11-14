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
| Boot | `psx_boot`: HLE BIOS reads SYSTEM.CNF and loads the EXE; the CPU core interprets it |
| Display | the PS1 display area of VRAM is drawn by the GE as a 5551 texture scaled to 480x272 |
| Pad | PSP buttons map 1:1 to the PS1 pad, L/R to L1/R1 |

The overlay (debug screen over the picture) shows a status line (frame, PC, speed as % of the
PS1's 33.8688 MHz, fps) and the last log lines; L+R+SELECT toggles it. Every line, plus a status
line per second, goes to `psxrt.log` next to the EBOOT. HOME exits.

The heap is kept at 256 KB (`PSP_HEAP_SIZE_KB`) so it is taken from the bottom of the user
partition and cannot grow over 0x09800000.

## DATA.PSAR (version 2)

Little endian, written by `write_native_psar` in `crates/psx2psp-engine`; offsets are from the
start of DATA.PSAR:

| Offset | Size | Field |
|---|---|---|
| 0x00 | 8 | `PSXRTDAT` |
| 0x08 | 4 | version (2) |
| 0x0C | 4 | header length (0xB0) |
| 0x10 | 16 | disc ID, NUL padded (`SLES02895`) |
| 0x20 | 128 | title, UTF-8, NUL padded |
| 0xA0 | 4 | raw sectors of the disc image (2352 bytes each; sector 0 is LBA 150) |
| 0xA4 | 4 | sectors per block (16) |
| 0xA8 | 4 | offset of the block index (0xB0) |
| 0xAC | 4 | blocks = ceil(sectors / 16) |

The index holds blocks + 1 offsets: block i is `index[i]..index[i+1]`. A block is raw deflate,
or stored as is when deflate would not make it smaller (its length is then the raw length; only
the last block may be shorter than 16 sectors).

## Build

```
make                                   # pspdev toolchain, e.g. WSL with /usr/local/pspdev/bin on PATH
cp psxrt.prx ../../resources/psxrt.prx # the prebuilt module the converter embeds (tracked)
```

Only `resources/psxrt.prx` is tracked; the build outputs here are ignored. The converter embeds
the PRX at compile time, so rebuild `psx2psp` after replacing it.
