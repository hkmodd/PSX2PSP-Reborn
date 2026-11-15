# PSP baseline (GOAL.MD S20c)

Device logs (`psxrt.log` from `ms0:/PSP/GAME/Sheepnative`) and their reading by
`tools/psplog.py`. Speed decisions are taken here, not in PPSSPP.

    python -I tools/psplog.py baseline_psp/s20a_b066913.log            scenes of one run
    python -I tools/psplog.py baseline_psp/s19_98fae2f.log NEW.log     NEW against the S19 base

A scene is the executable running (`WHEEL`, `LEV01`...); seconds with MDEC work are `<exe>/fmv`.
`worst` is the mean of the slowest 10% of the scene's seconds: the heavy seconds are the problem,
not the mean (GOAL.MD §5).

| file | build | run |
|---|---|---|
| `s19_98fae2f.log` | S19, psxrt built 2026-11-15 00:54 UTC | boot, intro, WHEEL, save load, LOADER, INTRO, LEV01 (64 s) |
| `s20a_b066913.log` | S20a | boot, intro, WHEEL, save load, then switched off at WHEEL → LOADER |

## S20a against S19

| scene | S19 | S20a | |
|---|---|---|---|
| intro FMV | 85% (worst 75%) | 73% (worst 68%) | **−12**: VLC recipe 73 ms/s, timer 22→59, DMA 304→340 |
| WHEEL 3D (worst 10%) | 61% | 60% | equal; the means differ by the menu seconds each run spent |
| LEV01 | 91% (worst 74%) | — | not reached |

S20 is not closed (GOAL.MD S20: "test PSP reale conferma ≥ S19"): intro below S19 and the crash.

### Reading the intro FMV −12 (offline, from the two logs)

Per second of intro FMV, S20a against S19:

| part | ms/s | reading |
|---|---|---|
| update | 333 → 266 (−67) | the guest's DecDCTvlc code no longer runs |
| io_vlc | 0 → 73 | the C recipe, **plus the `update()` calls it makes up to each device event**: S20a counts those twice (also in `update`) |
| present | 115 → 87 | GE wait 110 → CPU 24-bit conversion ~50 + draw ~60 (aff4e90 picks the converter at frame 60) |
| io_timer | 22 → 59 | 15 000 timer reads/s for the whole intro (S19: 18 000 at its start, then 4-5 000) |
| io_dma | 304 → 340 | MDEC transfers |
| sleep | 220 → 114 | |

The VLC recipe looks like a wash on PSP: −67 in the guest code, +73 in the recipe. The 73 ms
still include device updates, so the C decoder's own cost is not known yet. The rest of the
loss is the timer polling (+37), DMA (+36) and MDEC (+21). These come with S20's other changes
(24-bit conversion choice, MDEC timing at boot), and the logs cannot tell them apart.

The next psxrt counts the update time only in `update`, so `io_vlc` becomes the decoder alone.
The next PSP run then answers whether the recipe should stay.

## Still to record (S20c proper)

Three 60 s benches (`psxrt.bench` holding `60` beside the EBOOT) on one build: the intro FMV from
boot, WHEEL 3D, LEV01 from a snapshot of that same EBOOT.
