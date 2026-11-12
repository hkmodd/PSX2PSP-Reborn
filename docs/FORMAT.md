# EBOOT.PBP (POPS) — byte layout as written by PSX2PSP-Reborn

All integers are little-endian. Offsets are hexadecimal. Every layout below is the one the code
emits (`crates/psx2psp-core/src/{pbp,sfo,psar}.rs`), cross-checked against PSXPackager,
popstationr and pop-fe. Values marked *donor* are opaque Sony blobs copied at runtime from a
user-supplied PS1 Classic EBOOT (`--donor BASE.PBP`) and never shipped.

## 1. PBP container (`pbp.rs`)

| Off  | Size | Field                                   |
|------|------|-----------------------------------------|
| 0x00 | 4    | magic `\0PBP`                           |
| 0x04 | 4    | version `0x00010000`                    |
| 0x08 | 8×4  | section offsets, monotonically increasing |

The sections are, in order: `PARAM.SFO`, `ICON0.PNG`, `ICON1.PMF`, `PIC0.PNG`, `PIC1.PNG`,
`SND0.AT3`, `DATA.PSP` and `DATA.PSAR`. Section *i* spans `off[i]..off[i+1]`, and `DATA.PSAR`
runs to EOF. An absent section has a length of 0, so its offset equals the next one. The first
seven sections are packed back to back right after the 0x28-byte header. `DATA.PSAR` starts at
the next **0x10000** boundary (`PSAR_ALIGN`), and the gap is zero-filled.

## 2. PARAM.SFO (`sfo.rs`)

```text
0x00 "\0PSF"  0x04 version 0x0101  0x08 key_table_start  0x0C data_table_start  0x10 count
0x14 index[count] { u16 key_off, u16 fmt, u32 len, u32 max_len, u32 data_off }   (16 B each)
key table: NUL-terminated ASCII keys, sorted, padded to 4 · data table: each value max_len bytes
fmt: 0x0004 utf8-special (raw) · 0x0204 utf8 NUL-terminated · 0x0404 u32
```

POPS key set (`Sfo::pops`):

| Key            | Value                     | max_len |
|----------------|---------------------------|---------|
| BOOTABLE       | 1                         | 4       |
| CATEGORY       | `ME`                      | 4       |
| DISC_ID        | e.g. `SLUS00594`          | 16      |
| DISC_VERSION   | `1.00`                    | 8       |
| LICENSE        | Sony America licence text | 512     |
| PARENTAL_LEVEL | 1                         | 4       |
| PSP_SYSTEM_VER | `3.01`                    | 8       |
| REGION         | 0x8000                    | 4       |
| TITLE          | XMB title                 | 128     |

## 3. Artwork and media

| Section   | Format                             | Size                   |
|-----------|------------------------------------|------------------------|
| ICON0.PNG | PNG RGBA                           | 144×80                 |
| PIC0.PNG  | PNG RGBA (info overlay)            | 310×180                |
| PIC1.PNG  | PNG RGB(A) (background)            | 480×272                |
| boot      | PNG inside STARTDAT (see §5)       | 480×272                |
| ICON1.PMF | PSMF/H.264 (passthrough)           | —                      |
| SND0.AT3  | RIFF/WAVE ATRAC3 (132 kbps)        | ICON1 + SND0 ≤ 500 KiB |

PIC0 is 310×180, as in the PSP's own XMB. 480×272 is the full-screen size that PIC1 and boot use.

## 4. DATA.PSAR — single disc: `PSISOIMG0000`

Offsets are relative to the start of DATA.PSAR.

| Off      | Size  | Field                                                                 |
|----------|-------|-----------------------------------------------------------------------|
| 0x0000   | 12    | magic `PSISOIMG0000`                                                  |
| 0x000C   | 4     | **p1** = offset of STARTDAT (end of compressed data, padded to 16)    |
| 0x0400   | 11    | `_SLUS_00594` (ID split as `_XXXX_NNNNN`)                             |
| 0x0800   | ≤0x3FC| TOC (10-byte entries, §6)                                             |
| 0x0BFC   | 4     | 0x00100000 (start of block data)                                      |
| 0x1220   | 4     | **p2** = p1 + 0x50 + boot PNG length (start of the PGD trailer)        |
| 0x1228   | 4     | 0x7FF                                                                 |
| 0x122C   | 128   | title, NUL-padded                                                     |
| 0x12AC   | 4     | 3                                                                     |
| 0x4000   | N×32  | block index `{u32 off (relative to 0x100000), u32 len, 24×0}`          |
| 0x100000 | …     | blocks                                                                |

The raw BIN (MODE2/2352 with audio tracks inline) is cut into **0x9300-byte blocks** (16 raw
sectors), and the last block is zero-padded. Each block is raw deflate (no zlib header), at
level 1–9 (default 9). If deflate does not shrink a block, the block is stored verbatim, and
`len == 0x9300` means stored. The index can hold at most (0x100000 − 0x4000) / 32 = 32 256
blocks, about 1.18 GB of raw image.

After the blocks, the writer pads with ASCII `'0'` to a 16-byte boundary and then appends the
STARTDAT (§5) at p1.

Patches (PPF or pattern) are applied to the in-memory blocks before compression. The source
files are never modified.

## 5. STARTDAT

```text
0x00 "STARTDAT"  0x08 u32 1  0x0C u32 1  0x10 u32 0x50 (header len)  0x14 u32 png_len  … 0x50
0x50 boot PNG (480×272)
     \0PGD trailer — donor (encrypted, copied verbatim)
```

## 6. TOC (POPS flavour, `build_toc`)

There are 10 bytes per entry: `ctrl, 0, point, amin, asec, afrm, 0, pmin, psec, pfrm` (BCD).

| point | meaning     | notes                                                         |
|-------|-------------|---------------------------------------------------------------|
| A0    | first track | `pmin` = first track number, `psec` = 0x20 (CD-XA, *not* BCD) |
| A1    | last track  | `pmin` = last track number                                    |
| A2    | lead-out    | MSF = total sectors + 150                                     |
| 01..  | track *n*   | a = relative MSF, p = absolute MSF (+150 frames lead-in)      |

`ctrl` is 0x41 for data tracks and 0x01 for audio tracks. When the image is read back,
`parse_toc` reverses the table and `toc_to_cue` emits a single-BIN CUE.

## 7. DATA.PSAR — multi-disc: `PSTITLEIMG000000` (2–5 discs)

| Off   | Size  | Field                                                                  |
|-------|-------|------------------------------------------------------------------------|
| 0x000 | 16    | magic `PSTITLEIMG000000`                                               |
| 0x010 | 4     | p1 = offset of the shared STARTDAT                                     |
| 0x018 | 16    | `unk18` — donor if the donor is multi-disc, else 0                     |
| 0x200 | 5×4   | offset of each disc's PSISOIMG (0 = absent)                            |
| 0x214 | 12×4  | pseudo-random words < 0xFFFF (xorshift32 seeded from title → reproducible) |
| 0x264 | 11    | `_SLUS_00594` of disc 1                                                |
| 0x284 | 4     | p1 + 0x50 + boot PNG length (PGD trailer)                              |
| 0x28C | 128   | `data3` — donor if multi-disc, else 0                                  |
| 0x30C | 128   | title                                                                  |
| 0x38C | 4     | 7                                                                      |

Every disc is a complete PSISOIMG (§4), aligned to **0x8000** inside the PSAR. Per-disc header
values follow PSXPackager: `p1 = ceil(iso, 0x9300) + 0x100000` and `p2 = 0`. A single STARTDAT
follows the last disc. The writer rejects images that would exceed 4 GiB of PSAR.

## 8. Donor contents (`psar::Donor`)

| Item           | Source in the donor EBOOT                                        |
|----------------|------------------------------------------------------------------|
| `data_psp`     | DATA.PSP, trimmed to the real module size (`~PSP` +0x2C or ELF extent) |
| `boot_png`     | the PNG inside the donor's STARTDAT (default boot splash)        |
| `pgd`          | everything after that PNG (the `\0PGD` trailer)                   |
| `title_consts` | `unk18` / `data3` when the donor itself is a PSTITLEIMG           |
