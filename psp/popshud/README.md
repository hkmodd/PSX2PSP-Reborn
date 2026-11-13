# popshud

Measurement overlay for PS1 games under POPS on a PSP with ARK-4 / ARK-5 (firmware 6.61).
A diagnostic tool for tuning per-game settings, not part of the release.

```
FPS 30 EMU 100% CPU  85%
```

Refreshed every second in the bottom left corner of the picture:

| Field | Meaning | Source |
|---|---|---|
| FPS | frames the game showed | changes of the GP1(05h) display start, sampled at each vblank |
| EMU | emulated time over real time (100% = full speed) | POPS's emulated cycle count at the last emulated vblank, at 33.8688 MHz |
| CPU | PSP CPU time outside the idle thread | `sceKernelReferSystemStatus` idle clock |

How to read it:
- EMU under 100 with CPU at 100: the PSP cannot keep up (slow motion). Cheaper CPU accounting
  (the POPS cycle shift) or a lighter game path is the fix.
- EMU at 100 and FPS under 60: the game itself, in emulated time, draws at that rate (frame cap
  or emulated CPU too slow for the work per frame).
- CPU well under 100: headroom left on the PSP.

`---` means a value that could not be measured (counter reset, unknown clock unit). FPS counts
buffer flips; a game that draws into a single buffer shows 0.

## Install

Copy `popshud.prx` to `ms0:/SEPLUGINS/` and add to `ms0:/SEPLUGINS/PLUGINS.TXT`:

```
ps1, ms0:/SEPLUGINS/popshud.prx, on
```

It works next to `popsfs` and with any EBOOT. Set it to `off` to remove the overlay.

Each start appends a few lines to `ms0:/SEPLUGINS/popshud.log`:

```
start 4b4b4b4b 4b4b4b4b 4b4b0000 ...   how each function was found: 4b K kernel lib, 55 U user lib, 2d - missing
pops modgp text size coregp             the pops module; coregp 0 = core state not found, FPS/EMU show dashes
stats fps emu cpu <n>                   the first three one-second windows
fb ret addr stride format               the frame buffer the line is drawn into
```

No `start` line means the module did not run; no `pops` line means the start-module
handler never saw POPS.

## How it works

Like popsfs, the module imports systemctrl only: the kernel NIDs of 6.61 differ from the
pspsdk stubs, so the Sony functions are looked up at run time with `sctrlHENFindFunction`,
in the kernel library first and then in the user library of the same module.

The start-module handler of systemctrl catches the `pops` module. POPS's emulation core does
not run with the module gp: its entry loads `$gp` with the address of the core's state block
(`lui gp, 0x49F4` at text+0xc in pops_01g..09g), and the handler takes the address from that
instruction. A kernel thread wakes at each vblank, reads two words of the block (+0x360C
emulated cycles, +0x3610 display start; same offsets in all six) and draws the line into the
frame buffer on screen. POPS itself is not modified.

POPS redraws the whole picture with the GE right after every vblank, so a line drawn at the
vblank is erased before the screen shows it. The thread draws 12 ms after the vblank, at the
bottom of the screen, which the PSP scans 15-16 ms after the vblank. When the game runs below
full speed (EMU under 100) POPS's picture can land in between, and the line flickers.

## Build and test

```
make                      # pspdev toolchain
cl /W4 /WX /D_CRT_SECURE_NO_WARNINGS test_hud.c hud.c && test_hud .   # PC test of hud.c
```

`test_hud` checks the measurements on known windows and the text drawing in all four PSP pixel
formats, and writes `hud_<format>.rgba` (480x272 RGBA) for a look.
