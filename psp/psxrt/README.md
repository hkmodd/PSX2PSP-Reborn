# psxrt

The native runtime of PSX2PSP's recomp backend (`docs/recomp/PLAN.md`): a user-mode PSP module
that the converter puts in DATA.PSP of an EBOOT built with `psx2psp convert --native`. It
contains no game code; later slices add the translated code (in DATA.PSAR) and the PS1
hardware it talks to.

## Slice S1

S1 checks, on a real PSP, that the memory layout chosen in `docs/recomp/S0.md` is available
to a user-mode game, and that the converter's data reaches the runtime:

| Check | How |
|---|---|
| DATA.PSAR | opens its own EBOOT (argv[0]) and reads the converter's header: disc ID and title |
| PS1 RAM + dispatch table at 0x09800000-0x09BFFFFF (4 MB) | `sceKernelAllocPartitionMemory(2, PSP_SMEM_Addr)`; if refused, a filler block takes the free memory below 0x09800000 and the block is allocated from the bottom |
| Scratchpad 0x00010000-0x00013FFF | writes, flushes the data cache and reads back the first and last word, then restores them |

Each line ends in green (OK) or red; the last line says `S1 PASS` or `S1 FAIL`. HOME exits.
The scratchpad test runs last: if user mode may not touch the scratchpad, the access faults and
the screen stops on its `testing` line.

The heap is kept at 256 KB (`PSP_HEAP_SIZE_KB`) so it is taken from the bottom of the user
partition and cannot grow over 0x09800000.

## DATA.PSAR header (version 1)

Little endian, written by `native_psar_header` in `crates/psx2psp-engine`:

| Offset | Size | Field |
|---|---|---|
| 0x00 | 8 | `PSXRTDAT` |
| 0x08 | 4 | version (1) |
| 0x0C | 4 | header length (0xA0) |
| 0x10 | 16 | disc ID, NUL padded (`SLES02895`) |
| 0x20 | 128 | title, UTF-8, NUL padded |

## Build

```
make                                   # pspdev toolchain, e.g. WSL with /usr/local/pspdev/bin on PATH
cp psxrt.prx ../../resources/psxrt.prx # the prebuilt module the converter embeds (tracked)
```

Only `resources/psxrt.prx` is tracked; the build outputs here are ignored.
