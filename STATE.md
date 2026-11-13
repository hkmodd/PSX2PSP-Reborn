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
- **2026-11-12 · GUI senza donor** (commit 36d64fb)
  - La GUI usa sempre il donor incorporato. `--donor` resta solo come override nella CLI.
- **2026-11-12 · selezione dischi** (`engine::resolve_discs`, non committato)
  - BIN e CUE selezionati insieme contano come un disco solo.
  - Un BIN da solo passa al CUE accanto che lo referenzia.
  - Un `.m3u` si espande nei suoi dischi, in ordine di playlist.
  - I duplicati spariscono e l'ordine è naturale (Disc 2 prima di Disc 10).
  - Vale per drop, finestra Add disc, argomenti da riga di comando e CLI `convert`.
- **2026-11-12 · processi annullabili** (`media::proc`, non committato)
  - Ogni tool gira in un Job Object Windows con KILL_ON_JOB_CLOSE (process group su Unix).
    Annulla uccide yt-dlp, il suo FFmpeg e l'atracdenc in ≤50 ms, e lo stesso succede se l'app
    esce o va in crash.
  - stderr è letto in streaming: la barra segue il download e l'encode H.264.
  - Watchdog: yt-dlp viene fermato dopo 90 s senza output.
  - yt-dlp riceve `--js-runtimes` (deno/node/bun/quickjs auto-rilevati). Prima mancava: YouTube
    offriva meno formati e quelli DASH erano throttled, ed è la causa del download "bloccato".
  - Le sezioni preferiscono HLS ≤360p (solo i segmenti necessari).
  - Nella GUI: pulsante "Stopping…", toast "Clip cancelled" invece di "failed".
  - Test: media 15, engine 5. Clippy pulito.
- **2026-11-12 · preset PAL→NTSC** (non committato)
  - `patching::pal_to_ntsc()`: le 6 regole di `patches.ini` di PSX2PSP 1.4.2 (Y Fix #1–2,
    Graphic Fix #1–4), verificate sul file originale. Match solo a offset allineati a 4 byte
    (istruzioni MIPS): non può scattare dentro dati compressi a offset dispari.
  - Applicato in streaming dopo l'eventuale PPF, con EDC/ECC rigenerati; il BIN sorgente resta
    intatto. Il log riporta i settori patchati, oppure "0 → serve una PPF specifica".
  - GUI: toggle per disco, attivo di default sugli ID PAL. CLI: `--pal-ntsc`.
    (Superato: ora è opt-in e vincolato al video-mode, vedi sotto.)
  - Test: regole e allineamento (patching), convert → extract end-to-end (engine).
- **2026-11-12 · database giochi** (non committato)
  - `engine::gamedb`: 10 109 seriali dal `gameInfo.db` di PSX2PSP 1.4.2, in
    `resources/gamedb.tsv.z` (133 KB). Tag regione rimossi, come `hideGameRegion=1`.
  - All'aggiunta del disco il titolo si compila da solo (DB, poi etichetta del volume) e segue
    il disco 1 finché l'utente non lo modifica. CLI: titolo di default dal DB, `info` lo mostra.
  - DISC_ID di PARAM.SFO = save ID del DB: i dischi 2+ e le compilation condividono i salvataggi
    del disco 1, come PSX2PSP.
  - Test: suite a 53, clippy pulito.
- **2026-11-12 · fix SND0/atracdenc + audit** (non committato)
  - Bug segnalato dall'Host ("output is not RIFF/ATRAC3"). Causa 1: atracdenc sceglie il
    container dall'estensione in modo case-sensitive, quindi `SND0.AT3` produceva EA3/OMA.
    Ora l'output temporaneo è `snd0.at3`.
  - Causa 2, latente: `--bitrate 66/132` dava frame da 272/424 B, dimensioni non Sony, che la
    PSP non riproduce. Ora 66/105/132 kbps passano come 64/100/128, cioè frame da 192/304/384 B.
    Il block_align viene verificato sull'output.
  - Budget SND0: overhead esplicito (512 B + 3 frame) al posto del 2%, che sforava con
    budget piccoli (trovato dal test reale).
  - Gli AT3 importati dall'utente con frame non standard vengono rifiutati con un messaggio chiaro.
  - Audit: bound check sui PTS/DTS in psmf, PSMF 0×0 rifiutato, cut del PREGAP verificato,
    commit atomico senza remove_file.
  - Test: suite a 54, incluso make_snd0 reale con atracdenc a 66/105/132. Clippy pulito.
    Verificato su release: decodifica ffmpeg `atrac3` a 66 144 bps.
- **2026-11-12 · anteprima audio SND0 sincronizzata con ICON1** (non committato)
  - `media::clip::snd0_pcm`: decodifica con ffmpeg i byte stessi di SND0.AT3 (quelli che
    finiscono nell'EBOOT, non la sorgente). Se ffmpeg non riesce a decodificarli, l'AT3 è rifiutato.
  - `app::audio`: stream cpal su un thread dedicato, resampling lineare al rate del device,
    loop, mute. Si ricostruisce se il device cambia.
  - Il clock audio (compensato della latenza di output) pilota i frame di ICON1: lip-sync
    garantito. Senza SND0 il movie usa il timer del widget.
  - Comportamento XMB: suona solo nella schermata Game e riparte da 0 a ogni ingresso,
    a ogni nuovo clip e a ogni nuovo ICON1. Pulsante "Sound on/off" nell'header.
  - Test: suite a 56. Round-trip reale 66/105/132 kbps (durata e RMS del segnale decodificato),
    clock sul device reale (WASAPI a 48 kHz), interpolazione e loop. Clippy pulito.
    Build offline: tutte le dipendenze di cpal erano già in cache.
- **2026-11-12 · PAL→NTSC per gioco + compressione 5** (non committato)
  - Causa del blocco di Sheep Dog 'n' Wolf (SLES-02895) prima del menu lingue. Lo scan del BIN
    trova solo **Y Fix #2**, nel `PutDispEnv` della libgpu PsyQ, in tutti i 26 eseguibili
    (SLES, LOADER, FEND, LEVxx…). Nessuna Graphic Fix scatta, quindi il gioco resta a 50 Hz con il
    registro GP1(07h) (vertical display range) azzerato: configurazione incoerente.
  - `PatternPatch::hits`: dry run per regola, senza scrivere. `pal_to_ntsc_applies`: il preset
    si applica solo se scatta almeno un fix del video-mode. Il log elenca ogni regola con il
    numero di hit, oppure "NOT applied" con il motivo.
  - GUI: PAL→NTSC non è più attivo di default sugli ID PAL (opt-in per disco).
  - `DEFAULT_LEVEL` passa da 9 a 5, come PSXPackager. Misura su 108 MiB di Sheep Dog:
    1 → 57,3%, 5 → 55,4%, 9 → 55,3%. Il tempo di deflate raddoppia da 5 a 9; l'inflate resta
    a 279–294 ms a ogni livello. Etichette dello slider: "recommended" e "slower, ~same size".
  - Verifica reale: convert `--pal-ntsc` di Sheep Dog → "NOT applied — Y Fix #2 ×26" in 1,8 s.
    L'extract ha lo stesso SHA-1 del BIN sorgente. Suite a 58, clippy pulito.
- **2026-11-12 · Fix per titolo: Sheep Dog 'n' Wolf** (non committato)
  - RE di LEV00.EXE (capstone). La fisica usa un delta-time misurato: root counter 1 in modo
    hblank, `dt = hblank × 4096 / 15625` (PAL) o `/ 15734` (NTSC, via GetVideoMode). Poi `dt`
    viene limitato a 0xAA (41,5 ms). Un frame da 3 vblank (60 ms) avanza il mondo di soli 41,5 ms:
    il gioco va al 69%. È il "rallentamento casuale" quando POPS resta indietro.
  - Il main loop chiude ogni frame con `VSync(2)`, cioè max 25 fps a 50 Hz.
  - Entrambi i siti sono presenti una volta in 23 dei 26 EXE (LEV00–17, FEND, INTRO, SCENE,
    WHEEL, ENDING), con lo stesso pattern.
  - Nuovo `engine::fixes`: per ID, config POPS + patch di codice di default + unlock fps opt-in.
    - SLES-02895: config pop-fe (The_Ho, 272 B, SHA-256 3c1619fa…), clamp portato a 0x100
      (62,5 ms), unlock `VSync(2)→VSync(0)` (fino a 50 fps).
    - SLUS-01369: solo la config.
    - Un file `<image>.pspconfig` accanto al disco ha la precedenza (come pop-fe).
    - Dry run con il conteggio per regola nel log; le regole a 0 hit vengono scartate.
  - Core: `DiscSpec.pops_config` scritto a PSISOIMG+0x420 (pop-fe `encode_psiso`, PSP), max
    0x3E0 byte, validato.
  - CLI `--no-title-fixes`, `--fps-unlock`. GUI: riga "Known fixes for this game" con
    "Game fixes" (on) e "Unlock fps" (off), visibile solo per gli ID noti.
  - Verifica reale: clamp ×23, cap ×23, 46 settori patchati, config a 0x10420. L'extract
    differisce dal BIN solo in quei 46 settori. Suite a 61, clippy pulito. Non testato su PSP.
- **2026-11-12 · Sheep Dog 'n' Wolf a 60 Hz nativi** (non committato)
  - Che cosa fanno i fix di PSX2PSP. "Graphic Fix #1–4" sono `lui/lw` di variabili a indirizzi
    fissi (0x8005DA30…): valgono solo per i giochi linkati con la libreria a quegli indirizzi. Su
    Sheep Dog non scatta nessuno. "Y Fix" azzera l'offset +0x10/+0x13 in `PutDispEnv`: rimedio
    per un gioco che resta a 256 righe PAL dentro un display NTSC.
  - RE: il gioco è un'unica build PAL/NTSC. SLES_028.95 (0x80100010) scrive il bit 0 del byte
    flag a 0x8001008C, un blocco che sopravvive ai caricamenti degli EXE. Tutti i 26 EXE lo
    rileggono e scelgono:
    - 256 o 240 righe;
    - offset del display e posizioni dell'HUD;
    - `SetVideoMode` e quindi, via `GetVideoMode`, il divisore hblank del timer della fisica.
  - Patch `addiu a1,zero,1 → 0`: un'istruzione, una sola occorrenza sul disco. Il gioco gira sul
    proprio ramo NTSC (quello della release USA), con il centraggio fatto dal gioco stesso:
    nessun Y Fix.
  - Con la patch applicata viene impostato anche il flag NTSC della config POPS (pop-fe
    `force_ntsc`: 0x0B/0x8F |= 0x10).
  - Il ramo con dt costante (0x88/0xA3) è codice morto in retail: il flag viene scritto solo
    a 0. L'unlock fps quindi non accelera nessuno stato.
  - CLI `--60hz`, GUI toggle "60 Hz" (opt-in) nella riga dei fix noti.
  - Verifica reale: clamp ×23, cap ×23, 60 Hz ×1, 47 settori; extract = sorgente salvo quei 47
    settori; config 0x0B = 0x8F = 0x12. Suite a 62, clippy pulito. Non testato su PSP.
- **2026-11-12 · Riga patch autoesplicativa (GUI)** (non committato)
  - Tooltip su ogni switch: Game fixes elenca le patch e la config POPS, Unlock fps e 60 Hz
    mostrano cosa cambiano.
  - Riga "Result: 60 Hz (NTSC) · frame cap off · game fixes on" calcolata dagli switch.
  - Bottone "★ All on", visibile finché manca un fix.
  - Il PAL→NTSC generico è nascosto (e non inviato all'engine) quando il gioco ha il suo
    switch 60 Hz.
  - Fix dell'etichetta "60 Hz" che andava a capo.
  - Build debug e clippy puliti, suite a 62. Il binario release non è stato aggiornato (GUI
    aperta) e il risultato non è stato verificato a schermo.

- **2026-11-12 · Bisect su PSP + database patch** (non committato)
  - Bisect dell'Host su PSP reale: T1 (tutto) e T3 (config + clamp 0x100) si bloccano a 1/3
    del caricamento dopo le cutscene; T4 (solo config) funziona. Colpevole: la patch del clamp.
  - Bug Schermo Intero (sotto tagliato, sporcizia in alto) con il 60 Hz: sommavamo la patch
    NTSC del gioco e il bit `force_ntsc` di pop-fe (0x0B/0x8F |= 0x10). Ricerca (codice pop-fe,
    commit c993260): pop-fe lo usa solo per giochi PAL lasciati a 50 Hz, mai insieme a una patch
    di gioco. Rimosso: con le patch 60hz la config resta com'è.
  - `resources/patches.ini` + `engine::patchdb` sostituiscono `engine::fixes`. Formato INI
    `[game ID…]` / `[patch ID…|*]` (name, kind fix|fps|60hz|other, default, search/replace con
    `??`, align, note). File utente fuso sopra (portable accanto all'exe, altrimenti
    `%APPDATA%\PSX2PSP-Reborn\patches.ini`); un file utente rotto è segnalato e ignorato.
  - Clamp: default off, portato da 0x100 a 0xFF (ipotesi: 256 non sta in un byte).
  - Engine: dry run multi-patch in una sola lettura, `scan_patches`, `load_ppf`; log per patch
    (applicata ×N, NOT found, disponibile ma off, scelta sconosciuta).
  - CLI: `--patch NAME`, `--no-patch NAME`, comando `patches <disc>` (elenco con hit). Verifica
    sul BIN reale: clamp ×23, cap ×23, 60 Hz ×1.
  - GUI: "Patches for this game", un toggle per voce (config compresa) con tipo, "off by
    default", origine e nota nel tooltip; le scelte vincono sugli switch, uno switch azzera le
    scelte del suo tipo. Release GUI e CLI ricompilate.
  - Suite a 65, clippy pulito.
- **2026-11-12 · Causa vera del blocco a 1/3: CRC del loader Infogrames** (non committato)
  - PSP: T7 (fps), T8 (fps + 60 Hz), T9 (clamp 0xFF) si bloccano tutti come T1/T3; solo T4
    (config, nessun EXE toccato) va. Fattore comune: qualunque patch di codice. L'ipotesi
    "clamp a 8 bit" era sbagliata.
  - RE di LOADER.EXE (0x8011EBC8): legge l'header da 0x800 byte, confronta hdr+0x100 con il
    CRC di hdr[0..0x100], carica il corpo, confronta hdr+0x104 con il CRC del corpo. CRC
    (0x8011F0AC): riflesso 0xEDB88320, init 0xFFFFFFFF, senza XOR finale (= ~zlib.crc32).
    Tre errori → reset (jal 0x80112CB0) e ciclo infinito: è il blocco della barra.
    La stessa routine è in SLES_028.95 (0x80117AF0) e CREDITS.EXE (0x800277F8): tre loader,
    un solo schema. Formula verificata su tutti i 26 EXE originali.
  - Fix generico: `patching::exe_crc` (`loader_crc`, `SectorWrites`, `plan`) +
    `core::DiscImage::iso_files` (walk ISO9660). Dopo tutte le patch l'engine ricalcola le due
    parole di ogni PS-X EXE modificato, solo se l'originale rispetta lo schema (falso positivo
    2^-64). Scritte da un `ImagePatch` in coda alla catena, con EDC/ECC rigenerati. Log:
    "loader CRC updated in K patched executable(s)".
  - Test `patched_executables_keep_a_valid_loader_crc` (disco ISO sintetico, EXE con CRC valido
    + EXE con parole fasulle che non va toccato). Suite a 68, clippy pulito, CLI release
    ricompilata.
  - Verifica indipendente (script Python, walk ISO proprio) sull'estratto di T10: 26 EXE,
    0 falliscono il check del loader, EDC validi; 24 modificati, CREDITS e LOADER intatti.

- **2026-11-12 · T10 funziona su PSP reale** (conferma dell'Host): config + clamp 0xFF + fps +
  60 Hz con CRC ricalcolati. Il blocco a 1/3 è risolto.
  - Osservazione dell'Host: lo slow motion (all'inizio della corsa, nelle cacce) è sparito. Gli
    fps arrivano a 60 solo in alcuni punti della mappa; altrove scendono (a occhio ~40, con
    alternanza percepita tra 60 e 30). Non dà fastidio, ma si nota.
- **2026-11-12 · default per gioco verificato** (non committato)
  - `[game] default = fix fps 60hz` in patches.ini (`Game::switches`,
    `PatchDb::default_switches`, `Switches::RECOMMENDED` = solo fix se il campo manca).
  - SLES-02895: clamp senza `default = off`, tutti e 4 gli switch attivi di default.
  - GUI: `Disc::seed` riparte dai default del gioco quando cambia l'ID (probe o ID scritto a
    mano). CLI: senza flag valgono i default; nuovi `--no-fps-unlock`, `--50hz`; `patches`
    mostra lo stato effettivo `[on]/[off]`.
  - Suite a 68, clippy pulito, release (GUI e CLI) ricompilata. `D:\Temp\…\bisect` cancellata.
- **Config POPS di pop-fe per tutti i giochi (2026-11-13).** `tools/popfe_import.py` legge
  `gamedb.py` di pop-fe come dati (ast, mai eseguito) e i blob `pspconfigs/` via `git cat-file`,
  e genera `resources/popfe_configs.ini` (1,6 MB): 1796 config per 1833 codici, commit
  3be61d7a1050. Compilato sotto `patches.ini` (`PatchDb::builtin`: le nostre [game] vincono,
  poi il file utente sopra a tutto). 13 codici saltati: pop-fe punta a file che non ha (refusi
  come SLPS01149 → SLPS-01449). Verifica indipendente: ogni codice confrontato byte per byte con
  una lettura separata file per file, 0 differenze. Sheep Raider .bin/.BIN = il nostro di base.
  Suite a 69 verde.
- **pop-fe parte 2: LibCrypt, fix PPF per dump, Terracon (2026-11-13).** Il file generato ora è
  `resources/popfe.ini` (1,96 MB, sostituisce `popfe_configs.ini`): 1938 [game] per 1979 codici
  (1796 config, 225 sezioni LibCrypt = 230 codici) e 128 patch PPF (84.773 B: 54 fix + 74 crack
  krHACKen opt-in).
  - Formato INI: [game] `libcrypt` (magic word) e `pops_id`, risolti campo per campo (un [game]
    utente con la sola config non nasconde il LibCrypt di pop-fe); [patch] `ppf` (byte PPF in
    hex, esclusivo con search/replace) e `md5` (MD5 dei settori raw della traccia 1, da INDEX 01
    alla traccia successiva, come pop-fe).
  - Motore: catena PPF del DB → PPF utente → PAL→NTSC → pattern → CRC exe. Una patch con md5 di
    un altro dump o con blocco di validazione PPF non corrispondente è saltata e loggata. Ogni
    lookup passa da `pops_id` (convert, `scan_patches`, CLI, GUI). MD5 streaming in
    `engine::md5` (vettori RFC 1321).
  - Core: `DiscSpec::subchannels` → tabella a fine indice, puntatore/conteggio a PSISOIMG
    0x12D4/0x12D8 (come pop-fe su PSP, che non scrive il magic word: lo fa solo nel path PS3);
    `read_discs` la rilegge e la valida. `psar::libcrypt_subchannels(magic)` = 34 voci da 12 B.
  - CLI: `info <disc>` stampa l'MD5 della traccia 1; `info <pbp>` la tabella subchannel;
    `patches` mostra "built as" e il magic word.
  - Saltati (31, dichiarati dal generatore): 13 config mancanti in pop-fe, TEST01040 (PPF non
    presente), SLPS01940 (fix xdelta, non PPF), 16 crack LibCrypt in zip esterni a pop-fe.
  - Verifica indipendente (`scratchpad\lcverify.py`): 230 magic word = gamedb.py, 128 PPF
    byte-identiche ai blob git di pop-fe; due EBOOT da CLI (SCES-01492 0xD16A, SCES-01695
    0x89EA) con tabella identica a quella ricavata a mano (MSF via divmod); controllo negativo
    (gioco senza LibCrypt) con puntatore 0. Suite a 71 verde, clippy pulito.
  - Sheep Raider in pop-fe: solo la config POPS (= nostra base). Niente ppf_fixes, LibCrypt o
    widescreen per il gioco; grep "widescreen" su tutto pop-fe *.py vuoto.
  - Checkout sparse di pop-fe in `D:\Temp\PSX2PSP-Reborn\popfe` (~5 MB), tenuto per rigenerare.

## IN CORSO
- **Sheep Dog a 60 fps costanti: analisi.** Ipotesi di lavoro (da misurare, non presunta): con
  VSync(0) ogni frame che supera 16,7 ms resta a schermo per 2 refresh, quindi la cadenza
  alterna 60 e 30 nelle scene pesanti. Se POPS emula alla velocità del PS1, la causa è il carico
  del gioco: l'originale era pensato per 25 fps (40 ms di budget).
  - Misura proposta: Beetle PSX HW (`mednafen_psx_hw_libretro.dll`, già nel RetroArch di EmuVR)
    sull'EBOOT, con FPS interni a schermo e scaling della frequenza CPU, per separare il collo
    di bottiglia CPU da quello GPU.
  - 2026-11-12, aggiornamento: l'Host riporta che in DuckStation con CPU al 1000% il gioco andava
    a circa 140 fps, quindi il limite è la CPU emulata. Ricerca web: non risulta alcuna leva
    nota di POPS per accelerare la CPU emulata, né nei config né nei plugin (thread GBAtemp "POPS
    on PSP/Vita config research", wiki overclock). Su PSP gli fps sono quindi circa quelli di un
    PS1 vero senza cap. Restano due strade:
    - (a) dimezzare il lavoro CPU del gioco per frame: profilo PC in una scena pesante tramite lo
      stub GDB di DuckStation al 100%, poi RE delle funzioni calde (draw distance, LOD, AI);
    - (b) 30 fps fissi a 60 Hz (`--no-fps-unlock`), con cadenza regolare.
    - (c) richiesta dell'Host: RE di POPS per trovare il budget di cicli della CPU emulata. I file
      in `D:\Temp\PSX2PSP-Reborn` (POPS.ELF, POPS.PAK, IOPRP252.IMG) sono il POPS della **PS2**:
      ELF R5900 con moduli IOP (padman, hdd, pfs, iopcd). Non è il target. Serve il POPS PSP
      (`flash0:/kd/pops_0Xg.prx` + `popsman.prx`, cifrati ~PSP) + pspdecrypt; sui dischi non c'è.
      Ghidra 12.2 headless è pronto (Java 25).
  - 2026-11-13, RE di POPS PSP 6.61 (aggiornamento Sony 6.61 + pspdecrypt, scaricati con il
    consenso dell'Host; `pops_01g.prx`, base 0, gp 0x10000). POPS è un dynarec: `$t9` è un
    budget di cicli decrescente (spill gp+0x1b0), il codice ricompilato emette
    `addiu t9,t9,-N` (emitter 0x44dc) e controlla `blez/bgtz t9`. Frame = 564480 cicli a
    33,8688 MHz, limiter sui vblank reali (0x1252c).
    - Costo per blocco (0x5a30–0x5d58): N = (istruzioni >> shift) + stalli. Lo shift è il
      byte gp+0x1c2, scritto a 0x19f28: word 19 del config se ≥ 0, altrimenti bit 1 della
      word 0. Il loader del config (0x24770) applica prima la tabella interna Sony per gioco
      (0xEF0B0, 820 voci; la word 19 è impostata per 198 giochi, valore più comune 3; Sheep Dog
      non c'è), poi copia il blob PSISOIMG+0x428/0x42C se la versione a +0x420 è > 0x06060000.
      Quindi word 19 = blob 0x58 (mirror 0xDC). Il config di The_Ho la lascia a -1 → shift 1.
    - Verifica indipendente (subagente, disassembly proprio): stessa semantica e stesso
      mapping (buf+0x478 = blob+0x58). Lo shift 1 → 3 dà 1/8 invece di 1/2 ciclo per
      istruzione, cioè ~4× meno, con stalli invariati; non si applica al codice in KSEG1/BIOS.
      La stessa catena è in tutte le 7 varianti `pops_0Xg.prx`.
    - Applicato: word 19 = 3 nel config di SLES-02895/SLUS-01369 (patches.ini) + assert nel
      test `title_fixes_patch_code_and_embed_pops_config`.
    - Non dimostrabile offline: il guadagno reale. Se l'host (Allegrex 333 MHz) non regge più
      istruzioni emulate per frame, il tempo emulato rallenta (rischio slow motion). Da misurare
      su PSP con T12 (`D:\Temp\PSX2PSP-Reborn\T12\EBOOT.PBP`, 352,9 MB, default del gioco;
      decodifica indipendente del config nell'EBOOT: copia attiva, word 19 = 3, mirror = 3).
      Suite a 68, release GUI e CLI ricompilate.
    - Esito atteso su PSP: più scene a 60 fps. Se compare slow motion, l'host non regge: si
      torna a shift 2. Se non cambia nulla, il frame è dominato dagli stalli o dagli eventi GPU.
    - **Esito T12 su PSP (2026-11-13):** rallentamenti risolti, hub incluso; non ancora 60 fissi
      ovunque. Il clock non aiuta: POPS chiama già scePower 333/333/166 durante l'emulazione.
      Secondo leva trovata: bit 28 della word -1 (gp+0x6ac) forza a 1 lo stallo simulato
      (0x5c88), usato da Sony per 8 giochi. Build di prova, patches.ini rimasto a shift 3:
      T13 = shift 16 (`D:\Temp\PSX2PSP-Reborn\T13\EBOOT.PBP`), T14 = shift 16 + bit 28
      (`T14\EBOOT.PBP`), 352,9 MB ciascuno, config decodificato e verificato.
  - **Barra di caricamento disallineata con il 60 Hz (2026-11-13), causa dimostrata:** le `.EBS`
    PAL sono due immagini MDEC 512×256 (decodificate con ffmpeg mdec da un STR finto). Nella
    LEV00.EBS la cornice bianca è alle righe 236–247, "LOADING..." alle righe 16–30 (copiato dalla
    2ª immagine nelle righe 0–31). LOADER in NTSC: display h = 0xF0 da VRAM y = 0 (righe 0–239)
    e barra a y = 0xE1 invece di 0xEF (LoadImage, nessun clip). Risultato: cornice tagliata, barra
    11 righe sopra la cornice. Il codice NTSC del gioco sposta tutto di −14/−16 righe perché le
    immagini USA sono impaginate a 240 righe; le PAL no. I livelli rifanno il proprio
    SetVideoMode (0x8001c784, tail-call) e copiano la striscia "Press X" a 0xD0 (NTSC) / 0xE0.
    ~~Fix scelto: disp.y = 0x10 + barra PAL~~ superato dal RE di POPS qui sotto (l'Host: la barra
    è giusta, è l'immagine a essere allungata e tagliata sotto).
  - **Geometria verticale di POPS, RE 2026-11-13 (tutte e 6 le varianti pops_0Xg, identiche):**
    - GP1(05h) → gp+0x3610/0x3612, GP1(06h) → 0x3614/0x3616, GP1(07h) → 0x3618/0x361A (Y1/Y2),
      GP1(08h) → byte 0x3669 (bit 11 della word 0x3668 = PAL). Unico writer: il comando GPU.
    - Altezza sorgente fissa: 0xF0 righe di VRAM dal display start se NTSC, 0x120 se PAL
      (pops_01g 0x11e0c/0x11e20). Y2−Y1 serve solo a clip e timing vblank, mai alla scala.
    - Scala = 0x12C + tabella per modalità schermo (data+0x30, indice gp+0x3663 = scelta nel
      menu POPS). NTSC/PAL per modo: 256/213, 288/240, 384/320, 290/241, 256/213. Rapporto 1,2 in
      ogni modo: 240 righe NTSC occupano lo spazio di 288 PAL.
    - Config: word 1 (gp+0x6b4) clip inferiore, word 2/3 (0x6b8/0x6bc) clip superiore 15/24 bit.
      Tagliano, non riscalano. Nessun bit del config cambia l'altezza sorgente.
    - `force_ntsc` di pop-fe (0x0B/0x8F |= 0x10) = bit 28 della word −1: in 6.61 si legge solo a
      0x5c94 (stallo simulato = 1), nessun load a byte di 0x6af. Non tocca il video. T14 lo
      contiene già (è il "bit 28").
    - Conseguenza universale: ogni conversione PAL→60 Hz (preset generico o switch del gioco)
      mostra il contenuto impaginato a 256 righe con righe 1,2× più alte del PAL e perde 16 righe
      in fondo. È la resa fisica di 256 righe su un display a 240, non un bug di POPS. Il 3D e
      l'HUD del ramo NTSC del gioco non ne soffrono; le bitmap/FMV solo-PAL sì.
    - Il "bug Schermo Intero (sotto tagliato, sporcizia in alto)" del 12/11 è con ogni
      probabilità questo stesso taglio (righe di VRAM sopra l'immagine), non `force_ntsc`.
- `D:\Temp\PSX2PSP-Reborn\exes\`: 26 EXE estratti (15 MB) per il RE; da cancellare a RE chiuso.
- `D:\Temp\PSX2PSP-Reborn\fw661\pops\`: i `pops_0Xg.prx` + `popsman.prx` decifrati (~8 MB),
  tenuti per il RE; aggiornamento e dump completo del firmware cancellati.

## BLOCCO
- **Sheep Dog 'n' Wolf (SLES-02895):** il caricamento funziona su PSP (T10). Per i 60 fps
  costanti manca il dato su dove va il tempo del frame.
  - Rischio aperto: i salvataggi fatti con la versione PAL non patchata potrebbero riportare il
    flag a PAL se la memory card salva quel byte (non verificato).
- **SND0 su hardware:** RIFF ATRAC3 senza chunk `fact` (layout di atracdenc), ancora da provare
  sulla PSP reale.

## PROSSIMO
- Sheep Dog: schermate di caricamento a 50 Hz (LOADER e fase di caricamento dei livelli
  rileggono il flag PAL/NTSC: lì si forza PAL, il gameplay resta 60 Hz). Unico modo di avere
  l'immagine PAL identica senza ricodificare le .EBS. Patch come dati in patches.ini, verificata
  offline, poi EBOOT di prova. In attesa del via dell'Host.
- GUI patch, seconda metà: hit per patch (scan in background), form "+ Add patch" →
  `append_user`, rimozione delle patch utente, Apri/Ricarica patches.ini.
- Poi import di patches.ini di PSX2PSP, i restanti fix dell'audit (proc.rs, cap STARTDAT,
  extract sicuro, add_discs fuori dal thread UI), la modalità batch.

## BACKLOG PARITÀ (nulla di questo è fatto; ordinato per valore)
1. Batch mode: N giochi → N EBOOT in coda (ce l'hanno PSX2PSP e PSXPackager).
2. Impostazioni persistenti: cartella di output, livello di compressione, ultime cartelle.
3. Patch utente: import di `patches.ini` e cartella di PPF abbinate per ID gioco.
4. ~~LibCrypt~~ fatto 2026-11-13 (tabella subchannel + crack opt-in). Resta: prova su PSP reale
   di un titolo LibCrypt.
5. Input CHD / ZIP / 7z (PSXPackager, pop-fe).
6. Copertine e ICON0 automatici (pop-fe; serve rete, quindi consenso).
7. Template grafici in stile Custom pics (custodia CD, badge disco).
8. Encoder ATRAC3 nativo, senza atracdenc.
9. Codici widescreen GameShark convertiti in patch statiche dell'eseguibile (nessuno lo fa).
10. Target PS Vita bubble e PS3.

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
- **Deflate raw a livello 1–9 (default 5) con fallback "stored" per blocco.** Gli EBOOT del tutto non
  compressi risultano non avviabili.
- **Parole casuali di PSTITLEIMG da xorshift con seed sul titolo.** Così le build sono
  riproducibili.
- **PAL→NTSC = preset generico di PSX2PSP + PPF per titolo.** Le 6 regole generiche coprono i
  giochi che usano quelle sequenze di codice; per gli altri serve una PPF specifica.
- **Fix per titolo = conoscenza verificata, non euristica.** Ogni patch nasce dal RE, ha
  il pattern esatto e il conteggio atteso, e finisce nel log. La config POPS della community è
  attiva di default; l'unlock fps è opt-in perché aumenta il carico su POPS.
- **60 Hz per titolo = il ramo NTSC del gioco, non la patch generica.** Se un gioco ha il
  proprio switch PAL/NTSC si gira quello: il centraggio Y resta quello dell'autore. Il preset
  generico PAL→NTSC resta per i giochi dove scattano le Graphic Fix.
- **Clamp 0xFF (62,3 ms), attivo di default su SLES-02895.** Copre il frame da 3 vblank. Il
  blocco di 0x100 era il CRC del loader, non il valore; T10 su PSP l'ha confermato.
- **fps e 60 Hz attivi di default solo per i giochi verificati su hardware** (`[game] default`).
  Per gli altri parte solo "Game fixes": un unlock non provato può sovraccaricare POPS.
- **Gli EXE patchati ricevono il CRC del loader ricalcolato, riconosciuto e mai presunto.** Si
  riscrivono le parole +0x100/+0x104 solo se quelle originali rispettano lo schema: è generico
  per ogni gioco con lo stesso loader e non tocca chi non lo usa.
- **Patch 60 Hz di gioco senza `force_ntsc` di POPS.** Motivazione originale (sposta l'immagine
  a Schermo Intero) smentita dal RE 6.61: il bit è solo lo stallo simulato. Resta tolto perché
  non serve al video.
- **Nessuna leva universale in POPS per il 256→240.** L'altezza sorgente NTSC è 240 righe fisse
  e la sceglie il bit PAL di GP1(08h), cioè il gioco. Il rimedio è per titolo (dati in
  patches.ini): restare a 50 Hz nelle schermate solo-PAL, o scegliere quali 240 righe mostrare.
- **LibCrypt via tabella subchannel, non crack.** POPS risponde alle letture SubQ dalla tabella
  in PSISOIMG: il disco resta originale. Le crack krHACKen sono importate ma `other` (opt-in).
- **Database patch in INI, non codice.** È modificabile dall'utente e dalla GUI; quello
  incorporato fa da base, il file utente vince per nome.
- **PAL→NTSC opt-in, dry run e gate sul video-mode.** I Y Fix senza Graphic Fix rompono il
  setup video (caso Sheep Dog). Meglio non patchare che patchare a metà.
- **Compressione di default 5, non 9.** Il 9 non rallenta la PSP (l'inflate non dipende dal
  livello) ma raddoppia il tempo di build per lo 0,1% di spazio.
- **cpal per l'audio dell'anteprima, non rodio.** Basta un'uscita senza mixer: meno dipendenze.
  Su Linux serve ALSA (libasound2-dev in build).
- **Audio master clock.** Il video segue la posizione audio, non viceversa: è lo standard per
  l'A/V sync. L'anteprima suona l'AT3 decodificato, non la sorgente: si sente quello che va
  sulla PSP.
- **PIC0 a 310×180.** È la dimensione reale nella XMB; il 480×272 indicato in AGENTS.md vale per
  PIC1 e boot.

## ASSUNZIONI
- **SND0 sulla XMB:** va in loop e riparte quando l'icona riceve il focus. ICON1 e SND0 hanno
  loop indipendenti. Da verificare su PSP reale.
- **Il decoder ATRAC3 di ffmpeg è un proxy del firmware PSP.** Il bitstream è validato, ma
  resta un rischio sul container (manca il chunk `fact`, che at3tool scrive): serve una prova
  su hardware.
- **Coordinate XMB dell'anteprima** (ICON0 a (20,96), PIC0 a (160,46)): sono stimate e da
  verificare contro uno screenshot reale della PSP, alla prima occasione.
- **Semantica di p1/p2 per i singoli dischi nel multi-disco** (p1 = p2 = 0, come popstation e
  PSX2PSP v1.4.2): sostituisce la variante PSXPackager. Non è ancora confermata su hardware; si
  verifica insieme al test di cambio disco.
- **ICON1 x264 Main@2.1 144×80 a 29.97 fps** accettato dal firmware PSP: la struttura combacia con
  il sample Sony, ma il player della XMB non è ancora stato provato.
- **Display a circa 360 Hz con vsync**, da cui i 357 fps: l'animazione continua rende al refresh
  del monitor mentre la finestra è visibile.
