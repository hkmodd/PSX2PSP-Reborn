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

### Who reads the timer 15 000 times a second (offline, 2026-11-15)

A count of timer reads by guest pc in the simulator (interpreter, intro FMV; the probe was
removed afterwards) gives one loop: SLES_028.95 0x80101738, the wait for the next streamed frame.

    SetRCnt(RCntCnt1, 0xFFFF, 0x2000); StartRCnt(RCntCnt1); t0 = GetRCnt(RCntCnt1)
    do { if (GetRCnt(RCntCnt1) - t0 > 10000) return 0;   /* hblank timeout */
         if (!--n) return 0; }                            /* n = 0x800000 */
    while (StGetNext(&addr, &head));                      /* 0x80103130: ring slot 1/2 -> 4 */

GetRCnt is 0x8010202C (`lhu` of the counter at 0x8010204C). It makes ~90% of the timer reads in
every FMV second.

- **The loop waits on the CD.** It spins until the next frame's sectors are in the ring. Every
  emulated microsecond the frame's decode does not use is spent here, one timer read per
  iteration.
- **Rates.**
  - PPSSPP, with the same native code: 22 000 reads per emulated second.
  - PSP S20a: 15 000 at 73%, the same rate.
  - PSP S19: 4-5 000 at 85%, because the guest's DecDCTvlc used the time that is now spent
    waiting.
- **The reads cost nothing on the PS1, but a lot on the PSP.** Each iteration costs ~4 us
  (59 ms / 15 000) and moves emulated time by at most one hblank tick.
  - `poll_skip` advances to the next tick and never further, so that loops counting tick edges
    stay exact.
  - This loop only compares a difference with 10 000, so it could advance to `next_event` in one
    step.
- **A general fix needs a rule that tells the two kinds of loop apart.** That is a `native.c`
  change, so it goes to the Host first.

## Still to record (S20c proper)

Three 60 s benches (`psxrt.bench` holding `60` beside the EBOOT) on one build: the intro FMV from
boot, WHEEL 3D, LEV01 from a snapshot of that same EBOOT.
