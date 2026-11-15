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

## Still to record (S20c proper)

Three 60 s benches (`psxrt.bench` holding `60` beside the EBOOT) on one build: the intro FMV from
boot, WHEEL 3D, LEV01 from a snapshot of that same EBOOT.
