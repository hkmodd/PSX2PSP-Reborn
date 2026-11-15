# S24 — Un gioco 2D (Exoddus) e il disco nelle scene CPU-bound (piano)

**Stato:** passi 1, 2 e 3 fatti. Passo 3: Exoddus in PPSSPP, 105 overlay tradotti, `native: ST.OVL at 8009FA40`, passi d'interprete in gioco 9.943/s → 0. Rayman e SOTN non sono disponibili:
Abe's Exoddus (SLES-01480, PAL) è il gioco 2D di S24.

## Da dove si parte: il log PSP dell'EBOOT 3c9afc0 (Sheep Dog)

`psplogs/psxrt_3c9afc0.log`, 344 s su PSP reale. L'interprete fa 0 passi in 341 secondi su 344:
sulla PSP gira tutto tradotto.

| scena | secondi | velocità | note |
|---|---|---|---|
| boot/FMV | 53 | 93.3% | mdec ~210 ms/s |
| WHEEL | 37 | 81.5% | 22 s sotto il 97%; f 3411-4049 al 63-68% con draw 240-418 ms/s, sleep 0 |
| INTRO | 11 | 92.5% | |
| LEV01 | 236 | 98.4% | 41 s sotto il 97% |

LEV01 f 8160-8858: il gioco disegna 50-52 fps al 87-92%, con sleep 0. Lì l'host è il limite
(CPU-bound). Per secondo: io ~230 ms (dma ch2 182, di cui poligoni 110), update ~100 (cd 30-42,
spu 31-35, host 21-27), present ~10. Il resto è codice tradotto. Senza `psxrt.prof` sulla card
mancano i contatori di I-cache (S23 passo 0b).

**Il read-ahead del disco non serve mai.** Ogni secondo di LEV01 dà `inline 0 ahead 8 (waited 8)`:
il thread CPU attende ogni blocco. Il worker aveva priorità 0x30, sotto il thread CPU (0x20), e
girava solo mentre quello dormiva. Nelle scene CPU-bound lo sleep è 0, quindi il worker parte
solo quando il thread CPU si blocca ad aspettarlo. Il risultato è lettura e inflate in linea, più
due cambi di thread. In LEV01 cd costa in media 57 ms/s, e in 50 secondi su 236 supera i 100.

## Passo 1 — Pacing PAL (fatto)

Un gioco PAL a 50 Hz sulla PSP a 60 Hz: 6 vblank PSP ogni 5 frame PS1 (`native.c`,
`vs_step`, `frame_us3`). Su Exoddus in PPSSPP: velocità 99-100%, 49-50 vbl/s. Il fixed check
di Sheep Dog (NTSC, `--60hz`) deve restare uguale.

## Passo 2 — Worker del disco sopra il thread CPU (fatto, aspetta il log PSP)

`disc.c`: `PREFETCH_PRIORITY` passa da 0x30 a 0x1F, sopra il thread CPU e sotto audio (0x12) e
callback (0x11). La lettura del file blocca il worker nel driver, e intanto il frame va avanti.
Solo l'inflate prende la CPU, e lo prendeva già prima, in linea. Il log divide il tempo:
`ms read X+Y inflate X+Y` (thread CPU + worker).

**Criterio sul log PSP.** In LEV01 `waited` scende sotto metà di `ahead`, e la colonna cd di
`update` scende sotto i 15 ms/s. Se `read` del worker è piccolo e `inflate` domina, la leva
successiva è tenere i blocchi dei file di streaming non compressi.

## Passo 3 — Overlay tradotti (S24 vero)

Exoddus carica il codice dei livelli da file `.OVL` senza header PS-X EXE: 115 file, 7.2 MB.
Oggi quel codice gira nell'interprete.

- `psx2psp-recomp::overlay::locate_all` trova l'indirizzo di carico di ogni file. Ogni `jal` e
  ogni puntatore a word allineata nel file votano `t - o` per ogni prologo `o`. È forte con 8 voti
  e 3 volte il secondo. I deboli (≥3 voti) prendono uno slot già fissato da un forte.
  Su Exoddus 106 file su 116 sono collocati: slot 1 0x8009FA40 (livelli, 150-254 KB) e
  slot 2 0x800DDA40 (5-18 KB, vtable C++). Il codice host totale è ≈5.5 MB e ci sta.
- `native_code::build(exes, overlays)`: gli overlay vanno nei gruppi come gli eseguibili. Se
  il codice non ci sta, si toglie prima il gruppo di soli overlay più grande. Gli overlay non fanno
  mai fallire la build. Il tetto è MAX_IMAGES (256, come il runtime).
- `convert_native` passa i file non-EXE da 0x800 a 0x1F0000 byte. Il log dice quanti overlay
  sono tradotti.

Uno slot sbagliato non costa nulla: il runtime prende una traduzione solo se la RAM contiene
esattamente il file (hash). **Criterio:** Exoddus in PPSSPP, il log mostra `native: XX.OVL at
8009FA40` e i passi dell'interprete in gioco scendono di almeno 10 volte rispetto a oggi.
Sheep Dog: fixed check PASS.
