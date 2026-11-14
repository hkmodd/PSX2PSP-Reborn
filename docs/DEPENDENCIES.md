# Dependencies: what is ours, what is borrowed, and why

Audit of 2026-11-14. The rule: a dependency is rewritten in Rust only when the rewrite removes a
concrete limit (a platform it does not run on, a licence, a step the user has to do by hand, a
format guarantee). "We wrote it ourselves" alone is not a reason: a hand-made TLS stack or JSON
parser would be worse code, not better.

## Already native (written for this project)

| Part | Replaces | Notes |
|---|---|---|
| PBP / PARAM.SFO / PSISOIMG / PSTITLEIMG writer and parser | popstation, PSX2PSP's C core | Multi-disc, extraction, strict parser |
| Parallel streaming PSAR compressor | popstation's serial zlib loop | Every core, 0x9300 blocks, never holds a disc in memory |
| CUE / BIN / M3U reader, TOC and CDDA layout | libcdio-style readers | |
| LibCrypt: SBI parser, subchannel table, embedded SBI set | pop-fe's Python + external files | |
| PAL→NTSC patch engine and patch database | PSX2PSP's built-in patcher | Rules as data (`patches.ini`) |
| ICON1.PMF muxer (PSMF + MPEG-2 PS, 2048-byte packs) | nothing: no other tool can mux PSMF | Byte-for-byte from Sony-authored PMFs |
| RIFF ATRAC3 detection, SND0 bitrate and size budget | | |
| Self-updater (SHA256SUMS check, atomic swap with rollback) | | On ureq + rustls |
| XMB preview: wave shader, Game / Save / Boot screens | | iced + wgpu |

## External programs

| Tool | Used for | Verdict |
|---|---|---|
| **atracdenc** (LGPL-2.1) | PCM → ATRAC3 for SND0.AT3 | **Rewrite: native ATRAC3 encoder.** The only tool with Windows builds only: on Linux and macOS SND0 needs a hand-built atracdenc (cmake). A Rust encoder makes SND0 work on all six targets out of the box. Format constants (QMF, MDCT, quantiser and VLC tables) from the bitstream; no atracdenc code (licence). Validation: SNR after FFmpeg's ATRAC3 decoder, then the XMB on a real PSP. |
| **FFmpeg**: audio decode | any audio → 44.1 kHz stereo PCM for the encoder | **Rewrite with the encoder** for local files: symphonia (already linked for the MP3 preview) decodes MP3/AAC/FLAC/Vorbis/WAV, plus a native resampler. Then a local audio file → SND0 needs no external tool. YouTube audio (Opus) still goes through FFmpeg. |
| **FFmpeg**: video decode + scale | any video → frames for ICON1 | **Keep.** Arbitrary input codecs (H.264, VP9, AV1, HEVC…) are FFmpeg's job; a native decoder set is years of work for no user-visible gain. |
| **libx264** (inside FFmpeg) | H.264 encode for ICON1.PMF | **Keep for now.** A native H.264 encoder (I + P, CAVLC, 144×80) is feasible, but FFmpeg is still needed to decode the input, so it removes no tool; x264's rate control at a 500 KB budget is hard to beat. Revisit only if ICON1 must work without FFmpeg. |
| **yt-dlp** | YouTube clip download | **Keep.** It tracks YouTube's weekly changes; a fork would be broken within a month. |
| **curl, tar** (from the OS) | downloading and unpacking the tools above | **Keep.** Shipped with Windows 10+, macOS and every desktop Linux. ureq is already linked, but unpacking `.tar.xz` natively would need an LZMA2 decoder for a one-time download. |

## Rust crates (direct dependencies)

Transitive crate count, normal edges: core 15, patching 16, media 39, engine 74, app 260
(the GUI stack is most of it).

| Crate | Used for | Verdict |
|---|---|---|
| miniz_oxide `=0.9.1` | the deflate streams the PSP inflates; built-in resources | **Keep, pinned.** Measured against zlib-rs on 14,934 MediEvil blocks: zlib-rs is ~4× faster, but never smaller (level 1 +11%, 5 and 6 equal, 9 +0.4%), and its streams have never been run on a PSP whose kernel inflate is not provably stream-agnostic. Pinned exactly, called directly (no flate2 feature unification can swap it), golden test `psar::tests::deflate_streams_are_pinned`. |
| rayon | parallel block compression | Keep: a work-stealing pool is not ours to reinvent. |
| thiserror | error enums | Keep (compile-time only, no runtime code). |
| image (png, jpeg, bmp, gif, webp) | artwork import and resizing | Keep: decoders for five formats. |
| iced 0.14 + wgpu | GUI, shaders | Keep: the GUI framework AGENTS.md chose. |
| rfd | native file dialogs | Keep: OS dialogs through each platform's API. |
| cpal, symphonia (mp3) | soundtrack preview playback | Keep; symphonia gains formats when the native SND0 path lands. |
| ureq + rustls (ring, Mozilla roots) | self-update over HTTPS | Keep: TLS and crypto must not be home-made. |
| sha2 | update integrity | Keep: same reason. |
| serde, serde_json | project files, update manifest | Keep. |
| windows-sys | job objects (tool process trees), shell integration | Keep: raw OS bindings. |
| tempfile, pollster, resvg | tests only | Keep. |

## Removed in this audit

- **flate2.** It was only a wrapper: the backend was picked by Cargo feature unification (miniz
  today, through usvg's `rust_backend`), so any crate enabling flate2's `zlib-rs` feature would
  have silently changed the bytes sent to the PSP. Core and engine now call miniz_oxide directly;
  the output is byte-identical (checked at levels 0–10 on every block of real game images, and
  by the golden test). flate2 itself stays in the graph under iced/usvg, where it no longer
  touches an EBOOT.
