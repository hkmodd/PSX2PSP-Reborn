# S21 — Video FMV su PSP: dove va il tempo (piano, non codice)

**Stato:** piano scritto il 2026-11-15 in attesa del test PSP di S20d. Da approvare dall'Host
(GOAL §5 punto 2). Non si apre finché S20 non è chiusa sulla PSP.

## La premessa del GOAL non regge più

GOAL.MD dice «S21: port dell'IDCT e della conversione colore su VFPU». **È già fatto (S15).** Il
log PSP S20a lo conferma: `mdec: VFPU IDCT and colour in use, 0 of 49152 channels differ`.
Un macroblocco costa 13,0 us di IDCT, 27,7 us di IDCT più colore e 6,5 us di packing 24 bit.
L'obiettivo resta lo stesso: intro ≥ 60% su PSP. Con S19 è all'85%, con S20a al 73%. Il criterio
va quindi alzato (sotto). Il lavoro da fare è diverso.

## Dove va il tempo: intro FMV su PSP (ms per secondo reale, `tools/psplog.py`)

| build | speed | worst | io | di cui mdec | dma ch1/ch2/ch3 | vlc | timer | update | present | sleep |
|-------|-------|-------|----|-------------|-----------------|-----|-------|--------|---------|-------|
| S19   | 85%   | 75%   | 350 | 156 | 222 / 73 / 29 | 0  | 12-18 | 333 | 115 | 220 |
| S20a  | 73%   | 68%   | 498 | 177 | 205 / 77 / 34 | 55-82 | 59-67 | 266 | 87 | 114 |

- **ch1 (MDEC → RAM) contiene la decodifica:** `mdec_dma_block` chiama `macroblock()`. Quindi
  `mdec` e ch1 si sovrappongono: ch1 meno mdec dà circa 30 ms di copia.
- **La regressione S19 → S20a (−12 punti) è già letta in `baseline_psp/README.md`.**
  - `vlc` 55-82 ms: è la ricetta C che sostituisce il `DecDCTvlc` del gioco (−67 ms in `update`).
    In S20a contava due volte le chiamate a `update()`. 4950d4b lo corregge: il prossimo log PSP
    darà il costo del decoder da solo e dirà se la ricetta resta.
  - Le letture del timer sono 15 000 al secondo per tutta l'intro, contro 4-5 000 in S19
    (+37 ms). Poi DMA +36 e MDEC +21.
  - È il primo bersaglio: costa 12 punti e non richiede VFPU.
- **`update` (`psx_update`) 266-333 ms** è la voce più grande dopo io, e non è scomposta.
  Contiene `spu_run` (in parte già misurato come spu 83), `cd_update` (lettura dal Memory Stick e
  inflate dei blocchi EBOOT, decodifica XA-ADPCM), `dma_update`, timer, sio. Senza una
  scomposizione ogni intervento sarebbe una congettura.

## Passi (offline prima, Regola 4)

1. **Scomposizione di `update`.** Contatori come quelli di io (n/ms) per cd (lettura,
   inflate, XA), spu_run, dma_update e timers_update, campionati con `PROF_EVERY`. Una riga in
   più nel log e una colonna in `psplog.py`. Circa 30 righe in main.c e native.c, fuori da
   native.c/ge.c/mdec.c oltre le 50. Verifica: fixed-check verde, e la somma delle parti in
   PPSSPP deve dare `update` ±5%.
2. **Regressione vlc/timer (S19 → S20a).**
   - Ricetta VLC: con il log PSP di 4950d4b (decoder da solo) si confronta con i 67 ms del codice
     guest di S19. Se non vince, si toglie: non serve altro.
   - Timer: individuo offline chi legge il timer 15 000 volte al secondo (un poll del gioco in
     attesa del frame MDEC? native ha già i «poll skips») e perché S19 smetteva dopo l'inizio.
   - Fix generale solo con la causa provata. Bersaglio: FMV ≥ S19 (85%) prima di ogni altra
     cosa.
3. **Kernel VFPU dell'MDEC.** 2,2 us per blocco 8×8 di IDCT sono ~720 cicli a 333 MHz, molto
   sopra le due moltiplicazioni 8×8 per VFPU. Il colore costa 19 cicli per pixel, il packing 8.
   - Sospetti, da misurare sul device: conversione s32 → float fuori dalla VFPU, `vf_coef`
     azzerato a ogni macroblocco, accessi non in cache.
   - Test di equivalenza già esistente (`vf_check`: 49 152 canali, 512 blocchi RLE): deve restare
     a 0 differenze.
   - Stima: mdec 177 → ~90 ms/s.
4. **Solo se 1 lo indica:** spostare il lavoro di `cd_update` (inflate e XA) fuori dal thread
   della CPU. Il Media Engine è di S22 (SPU): qui al massimo un thread kernel, da decidere col
   numero in mano.

## Test

- Ogni passo: fixed-check verde (fps ≥ 95% di 58,9). Per i passi 1-3, `vf_check` a 0 differenze.
- PSP: `psplog.py BASE LOG` contro S19 e S20a, scena `SLES_028.95/fmv`.

## Criterio di chiusura

- Intro FMV su PSP **≥ 90%** di media e worst 10% ≥ 80%. Il vecchio criterio (≥ 60%) è già
  superato da S19.
- Fixed-check verde, `vf_check` a 0 differenze, commit.

## Non chiudere se

- Migliora solo PPSSPP.
- WHEEL o LEV01 scendono sotto S20a sulla PSP.

## Cosa NON so ancora (D-D5)

- Se il Media Engine ha la VFPU: non verificato, non serve per S21.
- Quanto costa l'inflate del kernel per blocco EBOOT durante lo streaming: lo dice il passo 1.
