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
- **2026-11-12 · donor incorporato**
  - `resources/BASE.PBP` e `resources/DATA.PSP` sono nel binario via `include_bytes!`;
    `--donor` resta come override.
  - Questo supera la voce "Donor … mai inclusi nel repo" scritta sopra.
- **2026-11-12 · multi-disco con semantica popstation**
  - Header per disco con p1 = p2 = 0.
  - STARTDAT puntato solo dal PSTITLEIMG (+0x10, +0x284).
  - Costanti `unk18`/`data3` di popstation usate quando il donor è single-disc.
- **2026-11-12 · muxer PSMF nativo (ICON1.PMF)**
  - Pack da 2048 B, tabella AU 0xBF per GOP, PTS/DTS, P-STD.
  - Re-mux del sample Sony bit-identico nel video, nelle tabelle AU e nei PTS.
  - Durata calcolata dai frame, perché il `last` nell'header Sony non è coerente.
- **2026-11-12 · pipeline clip**
  - Sorgente: URL (yt-dlp) o file locale, con intervallo inizio/fine.
  - Modalità ICON1+SND0, solo ICON1, solo SND0 (66/105/132 kbps).
  - Budget 500 KiB: SND0 per primo (66 kbps in Both), ICON1 riceve il resto, minimo 48 KiB,
    bitrate x264 rifatto a ogni tentativo.
  - CLI `psx2psp clip` e `convert --clip`.
- **2026-11-12 · provisioning tool**
  - `psx2psp tools [install]` e pulsante GUI "Install missing".
  - FFmpeg GPL (BtbN), yt-dlp latest, atracdenc 0.2.3, in `<exe>/tools`, via curl + tar di
    sistema.
- **2026-11-12 · GUI media**
  - Card Media con sorgente, Probe, From/To, modalità, kbps, Make, file pronti .pmf/.at3,
    riga del budget e stato dei tool.
  - ICON1 animato nello slot ICON0 dell'anteprima XMB (texture riscritta in place).
  - Job generici nel worker (`JobEvent<T>`); drop di video/audio/.pmf/.at3.
- **2026-11-12 · confronto competitivo**
  - [docs/COMPARISON.md](docs/COMPARISON.md): matrice contro PSXPackager, PSX2PSP-Enhanced e
    pop-fe, con lacune aperte.
- **2026-11-12 · test (aggiornato)**
  - Suite: app 5, core 11 + roundtrip 3, engine 2, media 13, patching 11 = 45 test.
  - `cargo clippy --workspace --all-targets` pulito.
  - Smoke GUI con ICON1: circa 342–355 fps, frame peggiore 5–10 ms a regime.

## IN CORSO
- Nessun task attivo. Clip, PSMF, provisioning e donor incorporato sono pronti per il commit.

## BLOCCO
- **Test su hardware reale:** gli EBOOT generati non sono ancora stati provati su PSP o PS Vita
  (DoD §7.1–7.2 di AGENTS.md). Si sblocca con un dispositivo dell'Host e un donor `BASE.PBP`.
- **ATRAC3 e yt-dlp mai eseguiti:** il provisioning è nel codice, ma atracdenc 0.2.3 e yt-dlp
  non sono ancora stati scaricati su questa macchina. Si sblocca con il consenso dell'Host al
  download, poi `psx2psp tools install` e un clip da URL reale.

## PROSSIMO
- Smoke test su hardware: un gioco single-disc (BIN/CUE con tracce audio) passato per
  `psx2psp convert --clip <video> --start … --end …`, poi avvio su PSP con CFW e su Vita
  (Adrenaline), con verifica di boot, ICON1/SND0 nella XMB, audio CDDA e salvataggi.

## DECISIONI
- **Shader WGSL in `iced_wgpu` invece di Vello.** Un unico pass full-screen compone onda e layer
  a costo O(pixel), senza una dipendenza in più.
- **Nessun player GStreamer per ICON1.** È una dipendenza runtime pesante e non portabile, e il
  binario deve restare standalone (AGENTS §1.1). L'anteprima decodifica i frame con ffmpeg (fino
  a 900) e li carica come texture; ICON1.PMF è generato dal muxer PSMF nativo.
- **Donor incorporato** (sostituisce "Donor obbligatorio"). DATA.PSP e il trailer PGD non si
  possono generare, ma i concorrenti li distribuiscono nei loro repo: li includiamo nel binario,
  con override via `--donor`.
- **Tool esterni scaricati dalle release ufficiali, non ridistribuiti.** Il binario resta piccolo
  e le licenze GPL (FFmpeg/x264) restano separate.
- **SND0 prima di ICON1 nel budget.** ATRAC3 è CBR e la sua dimensione è prevedibile; il video
  H.264 si adatta con retry di bitrate.
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
- **Semantica di p1/p2 per i singoli dischi nel multi-disco** (p1 = p2 = 0, come popstation e
  PSX2PSP v1.4.2): sostituisce la variante PSXPackager. Non è ancora confermata su hardware; si
  verifica insieme al test di cambio disco.
- **ICON1 x264 Main@2.1 144×80 a 29.97 fps** accettato dal firmware PSP: la struttura combacia con
  il sample Sony, ma il player della XMB non è ancora stato provato.
- **Display a circa 360 Hz con vsync**, da cui i 357 fps: l'animazione continua rende al refresh
  del monitor mentre la finestra è visibile.
