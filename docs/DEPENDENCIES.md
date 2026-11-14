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
| ATRAC3 encoder (`psx2psp-media::atrac3`) | atracdenc, at3tool | QMF → 4 bands, MDCT 256, rate–distortion bit allocation (VLC or CLC per unit), joint stereo at 66 kbps. Written from the format, not ported: no atracdenc / FFmpeg code (both LGPL); constants noted with their origin. Default backend; `PSX2PSP_ATRAC3=<path>` still selects an external encoder. |
| Audio decode + resampler (`psx2psp-media::audio`) | FFmpeg, for local audio files | symphonia (MP3, AAC, ALAC, FLAC, Vorbis, PCM/ADPCM in WAV/AIFF/Ogg/MP4) and a Kaiser-sinc polyphase resampler to 44.1 kHz. What symphonia cannot open (Opus, WebM, YouTube audio) falls back to FFmpeg. |

ATRAC3 validation, 2026-11-14 (`tests/atrac3_eval.rs`): SNR in dB after FFmpeg's ATRAC3 decoder,
same input and bitrate, native vs atracdenc 0.2. Both are rendered with inverted polarity by
FFmpeg (inaudible), and both have the same 1162-sample delay.

| Input | 66 kbps | 105 kbps | 132 kbps |
|---|---|---|---|
| Music, metal (20 s) | 13.5 vs 11.3 | 17.3 vs 14.5 | 19.8 vs 16.4 |
| Music, pop (20 s) | 17.0 vs 13.2 | 19.7 vs 16.1 | 22.0 vs 18.2 |
| Pink noise | 10.4 vs 9.3 | 9.4 vs 8.1 | 10.6 vs 9.6 |
| 1 kHz sine | 34.6 vs 34.5 | 34.6 vs 34.5 | 34.6 vs 34.5 |

The sine sits at the format's ceiling (mantissas ≤ ±31, no tonal components written). Encoding
runs at ~25× real time in release. The real-PSP test of the native output is still pending.
| Self-updater (SHA256SUMS check, atomic swap with rollback) | | On ureq + rustls |
| XMB preview: wave shader, Game / Save / Boot screens | | iced + wgpu |

## External programs

| Tool | Used for | Verdict |
|---|---|---|
| **atracdenc** (LGPL-2.1) | PCM → ATRAC3 for SND0.AT3 | **Rewritten (2026-11-14): native ATRAC3 encoder, above.** It was the only tool with Windows builds only, so SND0 on Linux and macOS needed a hand-built atracdenc. No longer downloaded; still usable through `PSX2PSP_ATRAC3`. |
| **FFmpeg**: audio decode | any audio → 44.1 kHz stereo PCM for the encoder | **Rewritten for local files (2026-11-14):** symphonia + native resampler, above, so a local audio file → SND0 needs no external tool. FFmpeg stays the fallback (Opus, WebM, YouTube audio) and decodes SND0.AT3 back for the preview. |
| **FFmpeg**: video decode + scale | any video → frames for ICON1 | **Keep.** Arbitrary input codecs (H.264, VP9, AV1, HEVC…) are FFmpeg's job; a native decoder set is years of work for no user-visible gain. |
| **libx264** (inside FFmpeg) | H.264 encode for ICON1.PMF | **Keep for now.** A native H.264 encoder (I + P, CAVLC, 144×80) is feasible, but FFmpeg is still needed to decode the input, so it removes no tool; x264's rate control at a 500 KB budget is hard to beat. Revisit only if ICON1 must work without FFmpeg. |
| **yt-dlp** | YouTube clip download | **Keep.** It tracks YouTube's weekly changes; a fork would be broken within a month. |
| **curl, tar** (from the OS) | downloading and unpacking the tools above | **Keep.** Shipped with Windows 10+, macOS and every desktop Linux. ureq is already linked, but unpacking `.tar.xz` natively would need an LZMA2 decoder for a one-time download. |

## Rust crates (direct dependencies)

Transitive crate count, normal edges: core 15, patching 16, media 63, engine 97, app 271
(the GUI stack is most of it; symphonia's decoders took media from 39 to 63).

| Crate | Used for | Verdict |
|---|---|---|
| miniz_oxide `=0.9.1` | the deflate streams the PSP inflates; built-in resources | **Keep, pinned.** Measured against zlib-rs on 14,934 MediEvil blocks: zlib-rs is ~4× faster, but never smaller (level 1 +11%, 5 and 6 equal, 9 +0.4%), and its streams have never been run on a PSP whose kernel inflate is not provably stream-agnostic. Pinned exactly, called directly (no flate2 feature unification can swap it), golden test `psar::tests::deflate_streams_are_pinned`. |
| rayon | parallel block compression | Keep: a work-stealing pool is not ours to reinvent. |
| thiserror | error enums | Keep (compile-time only, no runtime code). |
| image (png, jpeg, bmp, gif, webp) | artwork import and resizing | Keep: decoders for five formats. |
| iced 0.14 + wgpu | GUI, shaders | Keep: the GUI framework AGENTS.md chose. |
| rfd | native file dialogs | Keep: OS dialogs through each platform's API. |
| cpal, symphonia (mp3) | soundtrack preview playback | Keep. |
| symphonia (mp3, aac, alac, flac, vorbis, pcm, adpcm; wav, aiff, ogg, isomp4) | local audio → PCM for the native SND0 encoder | Keep: a set of audio decoders, MPL-2.0 used unmodified. |
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
