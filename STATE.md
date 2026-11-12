# STATE — Manifesto Operativo PSX2PSP-Reborn

> Fonte di verità del progetto. Si legge prima di agire e si aggiorna dopo. FATTO cresce solo in
> append.

## PROGETTO
- **Obiettivo:** authoring nativo di EBOOT.PBP (POPS) per PSP e PS Vita, con GUI SOTA e anteprima
  XMB live. Sostituisce PSX2PSP v1.4.2. Riferimento: [AGENTS.MD](AGENTS.MD) v3.0.
- **Stack:** Rust 2024 (MSRV 1.88). Workspace a 5 crate:
  - core
  - patching
  - media
  - engine (che include la CLI `psx2psp`)
  - app (GUI `psx2psp-reborn`, iced 0.14 + wgpu 27)
- **Target:** Windows, macOS, Linux; x86_64 e aarch64.

## FATTO
- **2026-11-12 · core**
  - PBP: header a 8 offset, reader random-access, writer con DATA.PSAR allineato a 0x10000.
  - PARAM.SFO: parse e write lossless, set di chiavi POPS.
  - CUE/BIN multi-file.
  - PSISOIMG: blocchi da 0x9300 con deflate raw parallelo (rayon) e scrittura in streaming.
  - PSTITLEIMG: da 2 a 5 dischi, ciascuno allineato a 0x8000.
  - TOC POPS e reader/estrattore (PBP → BIN+CUE).
  - Donor: DATA.PSP e trailer PGD letti a runtime, mai inclusi nel repo.
  - Layout documentato in [docs/FORMAT.md](docs/FORMAT.md).
- **2026-11-12 · patching**
  - Modello di settore MODE2/2352.
  - PPF v1, v2 e v3.
  - Pattern patcher applicato ai blocchi in memoria: i file sorgente restano intatti.
- **2026-11-12 · media**
  - Fitting dell'artwork: ICON0 144×80, PIC0 310×180, PIC1 e boot 480×272.
  - Adapter per ffmpeg, at3tool e atracdenc.
  - SND0.AT3 con budget di 500 KiB condiviso con ICON1.
- **2026-11-12 · engine**
  - Use case convert, extract, inspect e probe.
  - CLI `psx2psp`.
  - `work_dir` accanto all'output, invece di %TEMP%.
- **2026-11-12 · app**
  - GUI con layout a due pannelli.
  - Onda XMB in uno shader WGSL che compone PIC1, PIC0, ICON0 e boot in un unico pass, con
    crossfade tra le schermate.
  - Tab Game, Save e Boot.
  - Barra di progresso, annulla, log a scomparsa e toast.
  - Tutto l'I/O e la conversione girano su thread worker.
  - Drag & drop e argomenti CLI; le immagini sono assegnate allo slot in base al nome del file.
  - Import di un PBP esistente.
  - Contatore FPS opzionale con `PSX2PSP_FPS=1`.
- **2026-11-12 · test**
  - Suite: core 11 + roundtrip 3, patching 11, media 5, engine 1, app 2. Include un render GPU
    off-screen delle 3 schermate con readback dei pixel.
  - `cargo clippy --workspace` è pulito.
- **2026-11-12 · performance misurata** (release, RTX 4080):
  - XMB a circa 357 fps; il caso peggiore per frame va da 3.4 a 5.4 ms (target: 60 fps / 16.7 ms).
  - Un solo picco di 122 ms al ripristino da minimizzato.
  - Con la finestra minimizzata la GPU resta idle, perché winit sospende il redraw.

## IN CORSO
- Nessun task attivo. Il workspace è pronto per il primo commit.

## BLOCCO
- **Test su hardware reale:** gli EBOOT generati non sono ancora stati provati su PSP o PS Vita
  (DoD §7.1–7.2 di AGENTS.md). Si sblocca con un dispositivo dell'Host e un donor `BASE.PBP`.
- **ATRAC3:** su questa macchina manca un encoder (at3tool/atracdenc); ffmpeg è presente. Si
  sblocca mettendo uno dei due nel PATH o accanto all'eseguibile.

## PROSSIMO
- Smoke test su hardware: un gioco single-disc (BIN/CUE con tracce audio) passato per
  `psx2psp convert --donor BASE.PBP`, poi avvio su PSP con CFW e su Vita (Adrenaline), con
  verifica di boot, audio CDDA e salvataggi.

## DECISIONI
- **Shader WGSL in `iced_wgpu` invece di Vello.** Un unico pass full-screen compone onda e layer
  a costo O(pixel), senza una dipendenza in più.
- **Nessun player GStreamer per ICON1.** È una dipendenza runtime pesante e non portabile, e il
  binario deve restare standalone (AGENTS §1.1). ICON1.PMF viene passato così com'è.
- **Donor obbligatorio.** DATA.PSP e il trailer PGD sono blob Sony firmati o cifrati: non si
  possono generare né ridistribuire.
- **Deflate raw a livello 1–9 con fallback "stored" per blocco.** Gli EBOOT del tutto non
  compressi risultano non avviabili.
- **Parole casuali di PSTITLEIMG da xorshift con seed sul titolo.** Così le build sono
  riproducibili.
- **Patch PAL→NTSC come PPF più pattern per gioco.** Non esiste una patch universale: i patch
  set di PSX2PSP erano specifici per titolo.
- **PIC0 a 310×180.** È la dimensione reale nella XMB; il 480×272 indicato in AGENTS.md vale per
  PIC1 e boot.

## ASSUNZIONI
- **Coordinate XMB dell'anteprima** (ICON0 a (20,96), PIC0 a (160,46)): sono stimate e da
  verificare contro uno screenshot reale della PSP, alla prima occasione.
- **Semantica di p1/p2 per i singoli dischi nel multi-disco** (p1 = ISO arrotondata + 0x100000,
  p2 = 0): ricavata da PSXPackager e non confermata su hardware. Si verifica insieme al test di
  cambio disco.
- **Display a circa 360 Hz con vsync**, da cui i 357 fps: l'animazione continua rende al refresh
  del monitor mentre la finestra è visibile.
