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
  PIC1/boot 480×272.
- **Clips**: take a YouTube link (or any yt-dlp URL) or a local video/audio file, and pick start
  and end seconds. From that span you get one of:
  - an animated `ICON1.PMF` plus its `SND0.AT3`;
  - the icon only;
  - the soundtrack only, detached from the video.

  `ICON1.PMF` is H.264 144×80 in a native PSMF muxer. Both share the XMB's 500 KB: SND0 is
  encoded first and the video bitrate is fitted to what is left. Ready-made `.pmf` and `.at3`
  files are accepted too.
- **All-inclusive**: the donor (`BASE.PBP` + `DATA.PSP`) is built into the binary. ffmpeg (with
  libx264), yt-dlp and atracdenc install with one click, or with `psx2psp tools install`.
- **GUI**: iced 0.14 + wgpu. The XMB wave is a WGSL shader.
  - The preview has Game, Save and Boot tabs, is WYSIWYG at 480×272 and plays ICON1 in the
    ICON0 slot.
  - Every job runs in the background with progress, cancel, a log and toasts.

How it compares with PSXPackager, PSX2PSP-Enhanced and pop-fe: [docs/COMPARISON.md](docs/COMPARISON.md).

## Requirements

| What                     | Why                                                                 |
|--------------------------|---------------------------------------------------------------------|
| Rust ≥ 1.88 (edition 2024) | build                                                             |
| ffmpeg with libx264 | ICON1 needs it; SND0 needs ffmpeg itself |
| atracdenc or at3tool | the ATRAC3 encoder for SND0 |
| yt-dlp | URL sources |

None of these are needed to build an EBOOT. They are looked up next to the executable, in
`<exe>/tools/` and on `PATH`. The **Install missing** button in the GUI fetches the official
release binaries into `<exe>/tools/`, and `psx2psp tools install` does the same from the CLI.
The download size is shown before it starts. The GUI never asks for a donor. The CLI still takes
`--donor` as an expert override of the built-in one.

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
- Dropping a video or audio file makes it the clip source, and its length is probed. A `.pmf`
  loads as ICON1 and an `.at3` as SND0.
- **Media** card:
  1. Paste a link or browse a file.
  2. Set **From** / **To** (seconds or `m:ss`).
  3. Pick `ICON1 + SND0`, `ICON1 only` or `SND0 only`, then **Make**.

  The result animates in the preview at once.
- To print the frame rate to stderr, set `PSX2PSP_FPS=1`. Every 2 s it prints a line of the
  form `xmb: <fps> fps, worst frame <ms> ms`.

## CLI

```text
psx2psp convert [options] <disc.cue|disc.bin>...
psx2psp extract <EBOOT.PBP> <out-dir>
psx2psp info    <EBOOT.PBP | disc.cue>
psx2psp clip    <video|audio|URL> [--start t] [--end t] [--mode both|icon1|snd0] [--snd0-kbps 66|105|132] [-o dir]
psx2psp tools   [install]
```

```bash
psx2psp convert --icon0 cover.jpg --pic1 bg.png --clip "https://youtu.be/<id>" --start 1:05 --end 1:35 "Disc 1.cue" "Disc 2.cue"
```

```bash
psx2psp clip intro.mp4 --start 0:05 --end 0:25 --mode icon1 -o out
```

| Option | Meaning |
|---|---|
| `--donor <pbp>` | custom PS1 Classic donor; the default is the built-in one |
| `--clip <src>` | file or URL → ICON1.PMF + SND0.AT3; takes `--start`, `--end`, `--mode` and `--snd0-kbps`. It cannot be combined with `--icon1` or `--snd0`. |
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
  psx2psp-media     artwork fitting · PSMF (ICON1) muxer/parser · clip pipeline · ffmpeg / yt-dlp /
                    atracdenc / at3tool adapters · tool provisioning
  psx2psp-engine    use cases: convert / extract / inspect / probe + `psx2psp` CLI
  psx2psp-app       iced GUI `psx2psp-reborn` + XMB WGSL shader
```

## Status

Project state, measured performance, decisions and open risks are tracked in
[STATE.md](STATE.md). The generated EBOOTs have **not yet been tested on real PSP or Vita
hardware**.
