# FMV: what costs what, and the plan for less emulation

## 1. One second of the intro FMV on the PSP (S19 build)

Second 1347 of `psp_real_s19.log`, intro FMV, 88% speed, 8 pictures shown:

| part | ms/s | what it is |
|---|---|---|
| `update` | 304 | the game's own code, translated. The VLC decoder is most of it (see 3) |
| `mdec` | 151 | `mdec.c`: run-length decoding, the IDCT on the VFPU, the colour conversion |
| DMA ch1 | 173 | the MDEC output to RAM. It includes the `mdec` time, because the decoding runs as the output is drained |
| DMA ch2 | 56 | the pictures sent to PS1 VRAM, then up to the GE: 177 uploads, 1087 kpx |
| DMA ch0 | 26 | the run-length codes sent to the MDEC |
| `present` | 120 | 103 of it is the wait for the GE (the 24-bit picture) |
| `sleep` | 239 | the game was ahead and waited |

## 2. libpress in Sheep Dog

libpress is linked only into `SLES_028.95` (the boot executable, loaded at 0x80100000), `LOADER.EXE` and `CREDITS.EXE`. The intro FMV plays from `SLES_028.95`. Its pieces there:

| address | function |
|---|---|
| 0x801111B0 | `DecDCTReset(mode)`: calls `MDEC_reset` |
| 0x801111DC | `DecDCTGetEnv`: copies 16 + 16 + 32 words of tables |
| 0x80111870 | `DecDCTvlc(bs, buf)`: see 3 |
| 0x801116D4 | the reference to the "MDEC_in_sync" string, inside the in-sync wait |

## 3. `DecDCTvlc`

This is the PsyQ assembly routine. It turns a BS v2/v3 bitstream into the MDEC's run-length codes, all in registers:

- `a0 == 0` resumes from a saved state (9 words at 0x8011ACE4). Otherwise it reads the BS header: the size, the quantiser, and the version (3 or more keeps the DC predictors t7/t8/t9 for Cr, Cb, Y).
- It uses two tables: a2 = 0x8011AD10 for the short codes, and a3 = 0x8012AD10 (64 KB) for the long ones.
- It stops when the output reaches the size `DecDCTvlcSize` set. In that case it saves its state and returns 1. Otherwise it fills the rest with 0xFE00 and returns 0.

The translation runs it as MIPS on the Allegrex, with every guest register kept in its slot and every table read a PS1 RAM access. A C version would keep the bit buffer and the tables in host registers and cache.

## 4. The plan, by the "less emulation" rule

1. **Measure.** Time the `DecDCTvlc` calls on the device: one hook that counts the calls and times each one. Only this says how much of the 304 ms is the decoder.
2. **`DecDCTvlc` as a recipe** (`hooks.rs` + `native.c`), found by its exact code with the table addresses masked, so every game that links this libpress gets it. It must give the same output, word for word: checked against the translation on the whole intro in PPSSPP.
3. **The MDEC without the MDEC.** Hook `DecDCTin` and `DecDCTout`. The run-length codes go straight into the VFPU IDCT and the colour conversion, with no DMA0/DMA1 register emulation and no word-by-word FIFO. The output is still written where the game asked for it.
4. **Straight to the GE.** When the game's next step is a `LoadImage` of that buffer into VRAM, the macroblocks go into the GE's copy of VRAM directly. This one is game-shaped, so it comes last and only behind a check that the buffer is the one loaded.

The 24-bit wait at `present` (103 ms/s) is a GE cost of its own. It is measured apart from the decoder.

## 5. Step 2 done: `DecDCTvlc` runs in C (`vlc.c`)

- **Found by signature.** `hooks.rs` matches the routine's 212 words, with the 14 `lui`/`addiu` words of its globals masked. It then checks that the globals are laid out as libpress lays them out (`save` = size+4, `end` = size+0x28, `tab2` = tab1+0x10000). The match is in three links of the disc, each at its own address: SLES_028.95 0x80111870, LOADER.EXE 0x8011DBA0, CREDITS.EXE 0x80021500. Any game that links this libpress gets the recipe.
- **Bit-exact.** A host harness runs the guest code on a small R3000 interpreter and `vlc.c` side by side, on 40 frames of the intro. Both give the same output words, the same saved state, the same a0/a1, and the same instruction and memory-access counts. This also holds when `vlc.c` is cut into slices.
- **Same PS1 time.** The recipe charges the cycles the guest code would have spent (ins + 2 × mem), so device timing does not move. It stops at a block's DC when an event is due, lets `update()` run, and goes back to the guest for an interrupt. On the re-entry it resumes where it stopped.
- **PPSSPP intro bench** (42 s): 15 calls/s, one per FMV frame. The FMV runs at 100% speed, 14-15 fps. The `vlc` ms in the io line also counts the `update()` calls made inside the recipe.
- **Next: step 1 on the device.** The `vlc` io kind is the device measurement. One PSP run gives the decoder's real share of the 304 ms of io.
