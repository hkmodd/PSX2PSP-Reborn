# popsfs: full-screen PS1 games on PSP (ARK-4 / ARK-5)

> **Status: version 2 works on a real PSP (ARK-5, 2026-11-13)**, tested with MediEvil and Abe's
> Exoddus. Version 1 failed on the same games with vertical streaks and a split picture. It removed
> the POPS code that adds the game's display start row to the texture base, so any game that
> draws its picture from a row other than 0 sampled the wrong rows. Version 2 keeps that code.
> It was checked offline against the real `pops_01g`..`pops_09g` code (4500 cases, see below).

POPS, the PSP's PS1 emulator, scales a fixed 240 lines (NTSC) or 288 lines (PAL) to the screen.
A game that shows fewer lines, such as a 256-line PAL game, keeps an empty band at the bottom,
whatever the screen mode. POPS also moves the picture down when the game's display range starts
below the usual line.

This kernel plugin patches the display routine of POPS as it starts:

- the source height becomes the vertical display range the game programs (GP1(07h) Y2 - Y1),
  clamped to 64..240 (NTSC) or 64..288 (PAL);
- the first displayed line goes to the top of the current screen-mode box;
- the vertical scale makes those lines fill the box down to its last line.

The patch matches two code shapes, which cover `pops_01g` to `pops_09g` of firmware 6.61. If
neither shape matches, the plugin writes nothing and POPS runs unchanged.

A game whose content is shorter than its display range keeps its own black band. Abe's Exoddus,
for example, plays 240-line FMVs inside a taller PAL display.

## Install

1. Copy `popsfs.prx` to `ms0:/SEPLUGINS/`.
2. Add this line to `ms0:/SEPLUGINS/PLUGINS.TXT`:

   ```
   ps1, ms0:/SEPLUGINS/popsfs.prx, on
   ```

3. Start a PS1 game. The screen-mode setting in the POPS HOME menu still picks the box; its
   full-screen mode fills all 272 lines.

To remove the plugin, set the line to `off` or delete it.

## Build

The plugin uses the pspdev toolchain (pspsdk). From this folder, in a shell where `psp-gcc` is
on the `PATH` (for example a WSL or Linux install of pspdev):

```
make clean all
```

## How it was checked

An Allegrex interpreter runs the stock and the patched display routine of each `pops_0Xg.prx`.
It covers the 15-bit path (normal games) and the 24-bit path (FMV), from where they rejoin the
routine through the sprite table and the scissor. From the GE state (texture base, wrap mode,
sprite v range, scissor) it maps every visible screen row back to the VRAM row it shows. The
cases are 5 screen modes, NTSC and PAL, interlaced, six display start positions (including ones
that cross VRAM row 512) and eight display ranges. Every case must:

- cover the whole box;
- sample only rows the game displays;
- show the row expected at each screen line, within one row;
- give stock output for the stock geometry, on the LCD and on TV out.
