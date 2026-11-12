# PSX2PSP Reborn

A native Rust tool that turns PS1 disc images (BIN/CUE, single- or multi-disc) into POPS
`EBOOT.PBP` files for PSP and PS Vita, previewed live on a GPU-rendered XMB.
It replaces PSX2PSP v1.4.2.

- **Formats**: PBP, PARAM.SFO, PSISOIMG (single disc) and PSTITLEIMG (2–5 discs). The block
  compressor is parallel and streams its output, so the disc image is never fully buffered.
  The byte layout is documented in [docs/FORMAT.md](docs/FORMAT.md).
- **Patching**: PPF patches and sector pattern patches are applied to in-memory blocks. The
  source files are never touched.
- **Media**: artwork in any image format is auto-fitted to ICON0 144×80, PIC0 310×180, and
  PIC1/boot 480×272. Any audio or video file can become `SND0.AT3`, which needs ffmpeg plus
  at3tool or atracdenc. `ICON1.PMF` is passed through unchanged.
- **GUI**: iced 0.14 + wgpu. The XMB wave is a WGSL shader. The preview has Game, Save and Boot
  tabs and is WYSIWYG at 480×272. Conversion runs in the background with progress, cancel, a
  log and toasts.

## Requirements

| What                     | Why                                                                 |
|--------------------------|---------------------------------------------------------------------|
| Rust ≥ 1.88 (edition 2024) | build                                                             |
| `BASE.PBP` (donor)        | any PS1 Classic EBOOT. It supplies the Sony DATA.PSP loader and the STARTDAT PGD trailer, which are not redistributable. |
| ffmpeg (optional)          | needed only for SND0 conversion                                    |
| at3tool or atracdenc (optional) | needed only for SND0 conversion: the ATRAC3 encoder           |

The external tools are looked up on `PATH` and next to the executable.

## Build and test

```bash
cargo build --release
```

```bash
cargo test --workspace
```

The binaries land in `target/release/`: `psx2psp-reborn` (GUI) and `psx2psp` (CLI). The GUI
test `xmb::tests::screens_composite_layers_correctly` renders the three screens off-screen on
the real GPU. On a machine without a GPU adapter it skips.

## GUI

```bash
cargo run --release -p psx2psp-app
```

- Drop `.cue`, `.bin`, `.img`, `.ppf` or `.pbp` files, or images, onto the window, or pass them
  as arguments (`psx2psp-reborn game.cue ICON0.png`). A `.ppf` attaches to the last disc. An
  image is routed by its file stem, ignoring case:
  `ICON0` → ICON0, `PIC0` → PIC0, `PIC1` → PIC1, `BOOT` or `STARTDAT` → boot screen.
- Dropping an existing `.pbp` imports its title, ID and artwork.
- To print the frame rate to stderr, set `PSX2PSP_FPS=1`. Every 2 s it prints a line of the
  form `xmb: <fps> fps, worst frame <ms> ms`.

## CLI

```text
psx2psp convert --donor BASE.PBP [options] <disc.cue|disc.bin>...
psx2psp extract <EBOOT.PBP> <out-dir>
psx2psp info    <EBOOT.PBP | disc.cue>
```

```bash
psx2psp convert --donor BASE.PBP --icon0 cover.jpg --pic1 bg.png --snd0 theme.mp3 "Disc 1.cue" "Disc 2.cue"
```

| Option | Meaning |
|---|---|
| `-o, --out <path>` | output file; the default is `./<DISC_ID>/EBOOT.PBP`, the folder layout the PSP expects under `PSP/GAME/` |
| `--title`, `--id` | XMB title and product code override; both are auto-detected from the disc by default |
| `--ppf <file>`, `--ppf2..5` | PPF patch for disc 1, or for disc N |
| `--icon0/--pic0/--pic1/--boot <img>` | artwork |
| `--icon1 <pmf>` | ready-made ICON1.PMF |
| `--snd0 <audio>`, `--snd0-start <s>` | background music and its start offset in seconds |
| `--level <1-9>` | deflate level (default 9) |

Temporary SND0 files are written to `.psx2psp-work-<pid>` next to the output and removed
afterwards.

## Layout

```text
crates/
  psx2psp-core      PBP · SFO · CUE/BIN · PSISOIMG/PSTITLEIMG writer+reader · TOC
  psx2psp-patching  sector model · PPF 1/2/3 · pattern patches
  psx2psp-media     artwork fitting · ffmpeg / at3tool / atracdenc adapters
  psx2psp-engine    use cases: convert / extract / inspect / probe + `psx2psp` CLI
  psx2psp-app       iced GUI `psx2psp-reborn` + XMB WGSL shader
```

## Status

Project state, measured performance, decisions and open risks are tracked in
[STATE.md](STATE.md). The generated EBOOTs have **not yet been tested on real PSP or Vita
hardware**.
