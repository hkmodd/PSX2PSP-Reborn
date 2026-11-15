# S21 — Video FMV su PSP: dove va il tempo (piano, non codice)

**Stato:** approvato dall'Host il 2026-11-15 («SEI TU che scegli COSA è GIUSTO
architetturalmente fare / PROCEDI»), dopo il log PSP di S20d (`baseline_psp/README.md`).
Si parte dal passo 2 (timer): è l'unica perdita con causa provata.

**Passo 2, scelta architetturale.** Il controllo si fa nel runtime (`core/pollscan.c`), non nel
traduttore. Il valore di GetRCnt arriva al confronto attraverso un `jr ra`: il traduttore vedrebbe
solo `lhu` e ritorno, il runtime ha `ra` e i registri. Il codice letto è quello in RAM, quindi vale
anche per il codice interpretato e per ogni gioco. Il salto atterra dove il confronto cambia
(il timeout esce in orario), mai oltre il prossimo evento, il target o il giro del contatore.
Test sul codice vero di SLES in `cargo test -p psx2psp-sim`.

**Passo 2, il primo giro in PPSSPP: 0 salti lunghi.** Nel codice tradotto `ra` è un indirizzo
host (il `jal` tradotto scrive il ritorno host, `emit.rs`), quindi `poll_bound` leggeva codice PS1
a 0x096EBF5C. `guest_ra` in native.c lo riporta al pc guest. Cerca nella tabella la parola la cui
traduzione inizia lì, preceduta di 8 byte da una call, e ricorda l'ultima risposta. Per
STUB_GRET usa `gret`. La ricerca legge tutto il testo (~1 ms per un livello), quindi le risposte
si ricordano in 16 voci associative. Con una cache indirizzata per ra, due siti caldi
(0x0963E318, 0x09630718) cadevano nello stesso slot. Ogni lettura rifaceva la ricerca: timer
2000 ms/s nel passaggio LEV05 → LOADER, velocità 6%. Ora le ricerche sono 0-15 al secondo
(`ra scans` nel log).

**Passo 2 in PPSSPP, intro FMV, 30 s:** letture timer 19 874 → ~7 500/s (60 → 13-17 ms/s), io
327 → 275 ms/s, chiamate host 46k → 33k/s, ~600 salti al secondo, tutti lunghi. Velocità 100% e
fps del video 14,1, invariati. Fixed check verde (EBOOT 2026-11-15T17:26:49): WHEEL, LEV01
a 58,7 fps, livello 1 a 59,0 fps (100% di 58,9), timer 6 ms/s. Manca il log PSP: il bersaglio
FMV ≥ 85% si misura lì.

**Passo 1: `update` contiene il frame del frontend.** `psx_update` chiama `PsxHost.vblank` e lì
`host_vblank` fa `frame()` (log e `present`) e `native_vsync()` (l'attesa del vsync). Quindi
`update` include `present` e buona parte di `sleep`: i 266-333 ms della tabella sotto non sono
tutti lavoro dei device. La riga `-- update (ms):` lo separa in vbl (gpu_vblank, bios_vblank),
spu (spu_run), host (il frame del frontend), timers, dma, cd e sio (`Psx.us_upd`, psx.c).
`update()` in native.c misura ogni vblank e una chiamata su `PROF_EVERY` delle altre. Toglie le
letture del clock che le parti aggiungono. Una callback guest del vblank che rientra in `update`
resta nella parte vbl. `psplog.py` stampa le parti sotto ogni scena.

**Passo 2 sulla PSP (`baseline_psp/s21p2_3f6b45d.log`): FMV 74 → 73%, niente.** Il polling
risparmiato diventa sleep (timer 70 → 41, sleep 106 → 174 ms/s). Quello sleep non è l'attesa del
vsync (0-2 al secondo, ~0 ms): è `sync_time`, che dormiva appena PS1 era 2 ms avanti dentro un
frame. Intanto la decodifica salta 16 vblank PSP al secondo (43 vblank PS1 + 16 = 59,9).
Correzione (passo 2b):
- dentro un frame PS1 può stare avanti fino a un frame (`AHEAD_MAX`);
- `native_vsync` tratta un frame arrivato prima del suo vblank PSP come puntuale;
- `native_vsync_early` lo fa aspettare quel vblank prima del present, così il suo swap non
  sostituisce quello del frame precedente.

Primo tentativo senza le ultime due: PPSSPP al 196%, 100 vblank PS1 al secondo. Ora PPSSPP intro
100%, 13,8 fps, sleep tutto nell'attesa del vsync (535 ms), 0-2 frame in ritardo al secondo.
Fixed check verde, 59,1 fps. Tetto atteso sulla PSP: 73% × 1000 / (1000 − 174) ≈ 88%.

**Passo 3, il kernel VFPU: i tre sospetti cadono leggendo il codice.**
- La conversione s32 → float è già nella VFPU (`vi2f.q`).
- L'azzeramento di `vf_coef` è di 96 `sv.q` a macroblocco, ~1% del suo costo.
- I buffer (`vf_*`, `Mdec.out`, `mdec_in` da `palloc`) sono tutti in memoria con cache.

Anche un quarto sospetto cade: il writeback della cache da ~480 KB a ogni upload video.
`sceKernelDcacheWritebackRange` (uOFW, `sysmem/start.S`) sopra i 64 KB scorre i tag della cache
(128 set), non le righe del range: ~3000 cicli.

Il log PSP di step 2 divide però l'FMV per canale DMA (ms al secondo per kparole):
- ch0 (input MDEC) 25 ms per 80 kparole, ~100 cicli a parola: passava parola per parola da
  `mdec_write`;
- ch1 204 (decodifica 176, copia 28);
- ch2 (upload in VRAM) 75 ms;
- ch3 35.

Correzioni con guadagno certo:
- `mdec_dma_in` copia a blocchi i dati di un comando di decodifica (PPSSPP: ch0 1 ms per 156
  kparole);
- il colore scrive 2 `sv.q` per riga al posto di 8 `sv.s`;
- la riga `ge` del log dà ora il tempo degli upload (`upload N (kpx, ms)`). Così il prossimo log
  PSP divide i 75 ms del ch2 fra copia in VRAM e invio al GE.

Il resto di un macroblocco sulla PSP (log di step 2: IDCT 13,3 µs, colore 14,5, packing 5,7, RLE ~16) è
lavoro di istruzioni, senza uno spreco provato. Il prossimo intervento si sceglie con le parti di
`update` del log 2b.

**Passi 2b + 3 sulla PSP (`baseline_psp/s21p3_e43b795.log`): FMV 73 → 80%, worst 67 → 76%.**
- ch0 25 → 4 ms/s. ch2 80 ms per 737 kparole, di cui upload al GE solo 12: ~68 ms sono la
  scrittura dei pixel nella VRAM emulata.
- WHEEL 85 → 75% e LEV01 98 → 91% sono un altro mix di scene, non una regressione: sugli stessi
  secondi (WHEEL fps < 45: 64,3 → 63,8%; menu: 100 → 98,9%).
- **La perdita dell'FMV è nel pacing, non nel lavoro.** Ogni secondo di FMV: ~16 frame in ritardo
  (in media 6,5 ms), 13 vblank "saltati" (`skipped`), 20 attese del vsync per ~110 ms. Un frame
  in ritardo di 2+ vblank spostava lo slot del successivo al vblank appena passato: il debito si
  perdeva (13 × 16,7 = 217 ms), mentre i frame leggeri dopo dormivano.

**Passo 2c, pacing a debito.** `native_vsync` tiene il debito fino a `OWE_MAX` = 2 vblank (39,4 ms
con il margine, dentro `BEHIND_MAX`); oltre, PS1 rallenta come prima. Un frame finito dopo il suo
vblank PSP non viene presentato (`native_vsync_early` → 0, il GE prosegue con `ge_run`): il suo
tempo va al recupero, al massimo `DROP_MAX` = 2 di fila. Il log conta i frame non mostrati
(`dropped`). Stima: i ~110 ms di sleep e i ~2 ms di present per frame saltato coprono gran parte
dei 217 ms, FMV ~88-90%.

**Passo 4, lettura anticipata del disco (`disc.c`).** Nel log e43b795 `upd_cd` vale in media
81 ms/s con picchi di 184 ms/s. È il tempo di seek, lettura e inflate di un blocco da 16 settori,
tutto sul thread della CPU. Dopo un blocco servito, un thread `psxrt disc` legge e decomprime il
successivo in un terzo slot della cache, con il suo fd e il suo `z_stream`. La sua priorità
(0x30) è sotto quella del thread della CPU (0x20), quindi gira solo mentre quello dorme:
`sceKernelDelayThread` del vsync e `sceGuSync`. Non c'è spin. Se la CPU chiede il blocco in volo,
aspetta il semaforo. Se il worker fallisce, il blocco si ricarica inline, con il messaggio nel
log. La riga `-- disc blocks: inline N ahead N (waited N)` dice quanti blocchi sono usciti dal
frame.
- Effetto atteso: FMV e caricamenti, dove c'è sleep e streaming. In INTRO lo sleep è 47 ms/s e il
  guadagno sarà minore.
- La memoria cresce di ~75 KB in partizione (slot + zbuf) e ~40 KB di heap zlib.

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
   - **Causa trovata (834ec8a):** è il ciclo d'attesa del frame in streaming, SLES 0x80101738
     (GetRCnt + StGetNext).
   - **Regola, verificata offline sul codice di SLES (2026-11-15).** Un salto di più tick è
     sicuro solo se:
     - il valore letto finisce in `subu` e poi in `slt`/`sltu` (un limite), non in `beq`/`bne`.
       Un'attesa di un valore preciso salterebbe il valore e aspetterebbe un giro intero del
       contatore;
     - il salto si ferma al prossimo target o giro del contatore letto. I cicli di timeout del
       timer 2 (0x8010FC90, 0x80110838) contano i giri dal bit 0x200 di 0x1124, che si azzera alla
       lettura. `timers_next` programma target e giro solo con l'IRQ attivo, quindi oggi
       `next_event` non basta come limite.
   - Il controllo si fa nel traduttore, statico: il valore va da `lhu` del timer a
     `subu`/`slt`/`sltu`. Così il runtime riceve un flag per sito e non deve indovinare. È la
     forma dell'idle-loop di psp-web-recomp.
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
