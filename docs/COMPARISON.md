# PSX2PSP-Reborn vs PSXPackager, PSX2PSP-Enhanced and pop-fe

State as of 2026-11-14 (PSXPackager v1.8.0, pop-fe v1.31, PSX2PSP-Enhanced at its last commit).
Sources:
- the public repositories of the three projects (sources, READMEs, releases);
- the code of this workspace, with the tests behind each of our ✓.

Legend: ✓ supported · ◐ partial · ✗ absent · *test* = covered by an automated test in this repo ·
? = not verified in the sources read; check it before using it as an argument.

## Feature matrix

| Feature | **Reborn** | PSXPackager | PSX2PSP-Enhanced | pop-fe |
|---|---|---|---|---|
| Language / runtime | native Rust, single binary | C# / .NET (WPF) | Python + `popstation.dll` | Python |
| Windows / Linux / macOS | ✓ / ◐ / ◐ (portable code, not yet built or run there) | ✓ / ✓ CLI / ✓ CLI (x64, arm, arm64) | ✓ / ✗ / ✗ (32-bit DLL) | ✓ / ✓ / ? |
| BIN/CUE → EBOOT, single disc | ✓ *test, bit-exact* | ✓ | ✓ | ✓ |
| Multi-disc PSTITLEIMG (2–5) | ✓ *test*; disc swap played on a PSP (Abe's Exoddus) | ✓ (m3u) | ◐ up to 4 | ✓ |
| Compression level | 1–9, per-game minimum for games that hang below it; encoder pinned | 0–9 | ? | 1, fixed |
| Batch (N games → N EBOOTs) | ✗ | ✓ multi-threaded | ✓ | ? |
| EBOOT → BIN/CUE extraction | ✓ *test, bit-exact* | ✓ | ? | ? |
| **BASE.PBP + DATA.PSP included** | ✓ built into the binary | ✓ | ✓ | ✓ |
| PPF 1/2/3 patches | ✓ *test* | ? | ? | ✓ |
| PAL→NTSC | ✓ PSX2PSP's 6 generic rules behind one switch, gated on the video-mode fix, *test end-to-end*; per-game 60 Hz switches (60 Hz played on a PSP: Sheep, Dog 'n' Wolf) | ✗ | ✓ `patches.ini` | ◐ |
| Per-game POPS configs | ✓ ~1,800 games imported from pop-fe, user-overridable | ? | ? | ✓ |
| LibCrypt | ✓ subchannel table in PSISOIMG (230 codes), *test*; cracks opt-in; MediEvil plays on a PSP | ✗ | ✗ | ✓ |
| Per-dump PPF fixes (track-1 MD5) | ✓ *test* | ? | ? | ✓ |
| Code patches with loader CRC recomputed | ✓ *test* | ? | ? | ? |
| User patch database (INI) | ✓ portable or per-user `patches.ini` | ? | ✓ `patches.ini` | ? |
| Artwork from any image (auto-fit) | ✓ | ? | ? | ? |
| **Animated XMB preview (wave), Game/Save/Boot** | ✓ WGSL shader, *GPU test* | ◐ wave, without the 3 screens | ✗ | ✗ |
| **ICON1.PMF generated** | ✓ **native PSMF muxer**, *test vs Sony layout*; plays on the PSP XMB | ✗ | ✗ (GIF only) | ✗ |
| **Animated ICON1 in the preview** | ✓ *GPU test* | ✗ | ✗ | ✗ |
| SND0.AT3 from an audio/video file | ✓ plays on the PSP XMB (atracdenc) | ✗ | ✓ (at3tool) | ✓ (atracdenc) |
| CDDA tracks → ATRAC3, manuals (DOCUMENT.DAT), memory cards | ✗ | ✗ | ✗ | ✓ |
| SND0 from YouTube | ✓ yt-dlp | ✗ | ✓ | ✓ |
| **Start/end trimming** | ✓ any span | ✗ | ? | ◐ (fixed 59 s cut) |
| **YouTube → ICON1 + SND0 from the same clip** | ✓ | ✗ | ✗ | ✗ |
| **Soundtrack only, detached from the video** | ✓ `SND0 only` mode | ✗ | ✓ | ✓ |
| **Shared 500 KB XMB budget, bitrate fitted** | ✓ ICON1 fitted to what is left | ✗ | ✗ | ✗ |
| One-click tool download | ✓ ffmpeg, yt-dlp, atracdenc | n/a | ? | ? |
| CHD / ZIP / 7z / RAR input | ✗ | ✓ native CHD, 7z/rar/tar | ✗ | ✓ CHD |
| Game database (automatic title and cover) | ◐ title (10,109 serials), no covers | ✓ | ◐ covers and BGM fetched from several sites | ✓ Redump |
| PS3 / PS2 / PS Classic / PSIO / Vita bubble targets | ✗ | ✗ | ✗ | ✓ |
| LibCrypt, POPS configs or code patches | ✓ | ✗ | ◐ patches only | ✓ |
| Parallel streaming compression | ✓ rayon | ? | ? | ? |
| Cancel, log, UI never blocked | ✓ | ? | ? | ? |

## Where we are the only one

1. **ICON1.PMF from scratch.** The PSMF muxer is native: 2048-byte MPEG-PS packs, the 0xBF AU
   table per GOP, PTS/DTS and the P-STD extension.
   - On Sony's sample (605 frames) the re-mux is bit-identical in its video, AU tables and PTS.
   - None of the other three generates ICON1: they stop at passthrough or a GIF.
2. **A real clip editor.** From a URL or a file, pick start and end, then get:
   - ICON1 and SND0 together;
   - or ICON1 alone;
   - or the soundtrack alone, as ATRAC3 at 66, 105 or 132 kbps.

   The XMB's 500 KB budget is split automatically: SND0 is constant-rate, so it goes first, and
   the video gets the rest, with its bitrate recomputed on every attempt.
3. **Animated WYSIWYG preview.** The XMB wave, the layers and the ICON1 playing in the ICON0 slot
   are rendered on the GPU, with the three screens Game, Save and Boot.
4. **Everything included.** The donor (`BASE.PBP` and `DATA.PSP`) is built into the binary. The
   external tools install with one click into `<exe>/tools`: FFmpeg GPL with libx264, yt-dlp and
   atracdenc, all from their official releases.
5. **Verified per-game code patches.** Each patch comes from reverse engineering, with its exact
   pattern and expected hit count, and executables guarded by a loader CRC get it recomputed.

## Gaps still open

| Gap | Who has it | Estimated cost |
|---|---|---|
| Linux and macOS builds actually built and run | PSXPackager (CLI), pop-fe | low in code, blocked on CI (never ran: billing) and on hardware to test |
| SND0 without a hand-built encoder on Linux/macOS | nobody: atracdenc ships Windows builds only | medium–high: native ATRAC3 encoder (see `docs/DEPENDENCIES.md`) |
| CHD and archive input | PSXPackager, pop-fe | medium: a CHD decoder (zstd/lzma/flac) and archive reading |
| Game database with automatic covers | PSXPackager (`gameInfo.db`), pop-fe (Redump) | medium: needs a source with a compatible licence |
| Import of PSX2PSP's own `patches.ini` | PSX2PSP-Enhanced | low: map its format onto the pattern patcher |
| Batch mode (N games → N EBOOTs) | PSX2PSP, PSXPackager | low–medium |
| CDDA → ATRAC3, manuals, memory cards | pop-fe | medium each |
| PS3 / PS Classic targets | pop-fe | high, and outside this project's scope |

## Hardware status (real PSP, ARK)

- **Verified:**
  - a PAL title (Sheep, Dog 'n' Wolf) with its POPS config, code patches and the 60 Hz switch;
  - disc swapping in a multi-disc EBOOT (Oddworld: Abe's Exoddus);
  - a LibCrypt title (MediEvil, Italy), with its per-game minimum compression level;
  - the generated ICON1.PMF and SND0.AT3, playing on the XMB.
- **Still to confirm:**
  - a PS Vita;
  - the six generic PAL→NTSC rules on a title that has no per-game patch.
