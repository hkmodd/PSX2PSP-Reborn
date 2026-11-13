# popsfs: full-screen PS1 games on PSP (ARK-4 / ARK-5)

POPS, the PSP's PS1 emulator, scales a fixed 240 lines (NTSC) or 288 lines (PAL) to the screen.
A game that shows fewer lines, such as a 256-line PAL game, keeps an empty band at the bottom,
whatever the screen mode.

This kernel plugin patches the display routine of POPS as it starts. The height comes from the
vertical display range the game actually programs, so the picture fills the current screen-mode box
in every mode. The patch matches two code shapes, which cover `pops_01g` to `pops_09g` of firmware
6.61. If neither shape matches, the plugin writes nothing and POPS runs unchanged.

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

The plugin uses the pspdev toolchain (pspsdk), for example through its Docker image:

```
docker run --rm -v "$PWD":/src -w /src pspdev/pspdev make
```
