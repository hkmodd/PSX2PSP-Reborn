# AGENTS.MD - PSX2PSP-Reborn: North Star (GUI SOTA 2026 Edition)

**Versione:** 3.0 (Finale - Adversarial-Hardened)
**Data:** 12 Novembre 2026
**Stato:** Documento di riferimento definitivo per l'agente costruttore autonomo.
**Missione:** Sviluppare da zero un'applicazione nativa, performante e visivamente all'avanguardia in Rust che sostituisca PSX2PSP v1.4.2 e superi i limiti dei tool esistenti. L'obiettivo è creare l'esperienza definitiva di authoring per EBOOT.PBP su PSP e PS Vita, con un'interfaccia utente fluida, animata e con anteprima in tempo reale.

---

## 1. Principi Ingegneristici e Strategia

Questi principi sono non negoziabili e guidano ogni decisione architetturale.

1.  **Performance e Natura Nativa:** Il risultato finale deve essere un binario Rust standalone, privo di runtime esterni. L'uso di memoria e CPU deve essere ottimizzato, con particolare attenzione all'I/O asincrono per file di grandi dimensioni e al rendering GPU-accelerato per la UI.
2.  **Correttezza del Formato:** La generazione di `EBOOT.PBP` deve essere bit-perfect e conforme alle specifiche del formato PBP, `PSISOIMG` e `PSTITLEIMG`, garantendo la piena compatibilità con POPS su PSP e PS Vita.
3.  **Architettura Modulare e Testabile:** Il codice deve essere organizzato in crate Rust indipendenti e testabili. La logica di conversione, il parsing dei formati e l'interfaccia utente devono essere disaccoppiati per facilitare la manutenzione e i test.
4.  **Multi-Piattaforma Reale:** Supporto nativo per Windows, macOS e Linux, sia su architetture `x86_64` che `aarch64`.
5.  **UI/UX SOTA 2026 (Non Negoziabile):** L'interfaccia utente non è un accessorio, ma il cuore dell'esperienza. Deve essere **fluida, animata, reattiva e visivamente stupefacente**. Deve includere un sistema di **anteprima in tempo reale** (Game, Save, Boot) che mostri esattamente come apparirà l'EBOOT.PBP sulla PSP, con rendering GPU-accelerato e zero scatti.
6.  **Differenziazione Competitiva:** PSX2PSP-Reborn sarà l'unico tool **olistico, nativo e performante** che integra patching PAL→NTSC, creazione di `SND0.AT3` da YouTube e `ICON1.PMF` da video, il tutto con una GUI SOTA.

---

## 2. Architettura del Sistema

L'architettura segue il modello "Clean Architecture" (Esagonale), con un forte accento sulla pipeline di rendering della GUI.

### 2.1. Strati Architetturali

*   **Core (Dominio):** Entità e regole di business, senza dipendenze esterne.
    *   **`psx2psp-core`**: Strutture per `PbpImage`, `IsoTrack`, `SfoData`, `PbpHeader`. Parsing e generazione di PBP, `PSISOIMG`, `PSTITLEIMG`.
    *   **`psx2psp-patching`**: Applicazione di patch (PAL→NTSC) a livello di settori raw (MODE2/2352) dell'immagine ISO/BIN.
    *   **`psx2psp-media`**: Trait per la transcodifica di media (audio in ATRAC3, video in PMF).

*   **Adapter (Infrastruttura):** Implementazioni concrete.
    *   **`adapter-ffmpeg`**: Orchestra la conversione audio/video tramite `ffmpeg-sidecar` (o `ez-ffmpeg`).
    *   **`adapter-at3tool`**: (Opzionale) Wrapper per `at3tool` di Sony per massima compatibilità.
    *   **`adapter-yt-dlp`**: (Opzionale) Integrazione con `yt-dlp` per il download audio.

*   **Application (Casi d'Uso):** Orchestra le operazioni.
    *   **`ConvertIsoToPbpUseCase`**: Flusso completo (ISO → Patch → Media → PBP).
    *   **`ExtractPbpUseCase`**: Estrazione di ISO e CUE da PBP.

*   **Presentation (GUI):** Interfaccia utente SOTA.

### 2.2. Framework GUI: La Scelta per il 2026

Dopo un'analisi aggiornata al 2026, la scelta ricade su un'architettura ibrida che combina la solidità di `iced` con la potenza di rendering di `Vello` (o `wgpu` diretto) per l'anteprima.

*   **Scelta Primaria: `iced` (v0.13+ / v0.14 in sviluppo)**
    *   **Motivazione:** Architettura ELM (Model-View-Update) che si sposa con la nostra Clean Architecture. Supporto nativo per **shader personalizzati** tramite `iced_wgpu`, essenziale per animazioni come l'onda XMB[reference:0]. Gestione efficiente delle texture GPU tramite `iced::image::allocate()`, che evita blocchi del thread UI durante il caricamento di immagini di grandi dimensioni[reference:1]. È la scelta ideale per un'applicazione desktop "pesante" con UI complessa.
*   **Motore di Rendering per Anteprima: `Vello` (integrato tramite `iced_wgpu`)**
    *   **Motivazione:** Renderer 2D GPU-accelerato scritto in Rust, focalizzato sul calcolo GPU. Permette di disegnare scene 2D complesse (come l'overlay dell'anteprima EBOOT) con performance interattive, usando `wgpu` per l'accesso alla GPU[reference:2]. È la chiave per un'anteprima fluida e realistica.
*   **Alternativa per Prototipazione Veloce: `egui`**
    *   **Motivazione:** Immediate mode, compilazione rapida, ecosistema maturo per tool ed editor[reference:3]. Tuttavia, il suo modello di rendering può essere meno adatto per animazioni complesse e UI "curate" rispetto a `iced` + `Vello`. Da considerare solo se la velocità di sviluppo iniziale è la priorità assoluta.

---

## 3. Specifiche Tecniche e di Formato (Backend)

### 3.1. Formato PBP (EBOOT.PBP)
*   **Header (40 byte):** Magic `\0PBP`, versione, offset per `PARAM.SFO`, `ICON0.PNG`, `ICON1.PMF/PNG`, `PIC0.PNG`, `PIC1.PNG`, `SND0.AT3`, `DATA.PSP`, `DATA.PSAR`.
*   **File Incorporati:**
    *   **`PARAM.SFO`**: Metadati (Titolo, ID, Regione).
    *   **Immagini**: `ICON0` (144x80), `PIC0` (480x272 overlay), `PIC1` (480x272 background) - PNG 32-bit.
    *   **`SND0.AT3`**: Audio ATRAC3. Dimensione totale `ICON1.PMF` + `SND0.AT3` ≤ 500 KB.
    *   **`ICON1.PMF`**: Video H.264 in container PMF.

### 3.2. Struttura `PSISOIMG` e Multi-Disco (`PSTITLEIMG`)
*   **Single Disc:** `DATA.PSAR` contiene un blocco `PSISOIMG0000`. Immagine suddivisa in chunk da `0x9300` byte, ciascuno compresso con zlib.
*   **Multi-Disco:** `DATA.PSAR` inizia con header `PSTITLEIMG000000`, seguito da un blocco `PSISOIMG` per ogni disco (fino a 5). Struttura annidata da generare e parsare.

### 3.3. Patching PAL -> NTSC
*   **Metodo:** Modifica di offset specifici nei settori raw del disco (pattern colori, modalità video, Y-Fix).
*   **Implementazione:** Modulo `psx2psp-patching` che opera a livello di `Track` e `Sector`, con set di patch predefinite (`PAL4U`, `NTSCPATCH`). Applicare la patch al `.BIN` prima della conversione.

### 3.4. Conversione Media
*   **Audio (`SND0.AT3`):** Input qualsiasi formato (o YouTube) → Decodifica PCM → Codifica ATRAC3 via FFmpeg (`-c:a atrac3`) o `at3tool`.
*   **Video (`ICON1.PMF`):** Input `.mp4` → Ridimensionamento (480x272 o 144x80) → Ricodifica H.264 (profilo compatibile PSP) → Muxing in container PMF via FFmpeg.

---

## 4. Specifiche Tecniche della GUI SOTA 2026 (Frontend)

Questa sezione è il **cuore** del nuovo documento e definisce gli standard non negoziabili per l'interfaccia utente.

### 4.1. Architettura di Rendering e Performance

*   **Rendering GPU-Accelerato Obbligatorio:** Tutta la UI (widget, immagini, animazioni) deve essere renderizzata tramite `wgpu`. Nessun rendering software.
*   **Texture Asincrone:** Le immagini (`ICON0`, `PIC0`, `PIC1`) devono essere caricate, decodificate e caricate sulla GPU in thread worker separati, senza bloccare il thread di rendering della UI. `iced` fornisce API come `image::allocate()` per gestire questo flusso in modo efficiente[reference:4].
*   **Anteprima Video in Tempo Reale:** Per `ICON1.PMF`, integrare un player video basato su GStreamer (es. crate `iced_video_player`) per visualizzare l'anteprima del video direttamente nella UI prima della conversione[reference:5].
*   **Target di Performance:** L'UI deve mantenere **60 FPS costanti** durante le animazioni e lo scrolling. Il caricamento di un'immagine di anteprima (anche 480x272) non deve causare frame drop.

### 4.2. Design e Animazioni (Il "Feel" del 2026)

*   **Tema e Stile:** Design system moderno, scuro (dark mode di default), con accenti di colore ispirati al brand PlayStation (blu, rosso, verde, rosa). Angoli arrotondati, ombre morbide, spaziature generose.
*   **Animazioni Fluide:**
    *   Transizioni tra schermate (es. da "Setup" a "Preview") con easing naturale.
    *   Feedback visivo su ogni interazione (hover, click, focus).
    *   **Animazione dell'Onda XMB:** L'anteprima deve includere un'animazione fluida dell'onda di sfondo della XMB della PSP. Questo può essere realizzato tramite un **custom shader** in `iced_wgpu` (basato su WebGL/GLSL o WGSL) che simula il movimento ondulatorio[reference:6][reference:7].
*   **Anteprima Realistica (WYSIWYG):** La schermata di anteprima deve replicare fedelmente l'aspetto della XMB della PSP, con:
    *   Onda di sfondo animata.
    *   Icona del gioco (`ICON0.PNG`) in primo piano.
    *   Sfondo (`PIC1.PNG`) e overlay informativo (`PIC0.PNG`) posizionati correttamente.
    *   Toggle per visualizzare le schermate **Game**, **Save** e **Boot**, proprio come nel tool originale[reference:8].

### 4.3. Componenti UI Chiave

*   **Pannello di Controllo (Sinistra):** Form per metadati (Titolo, ID, Regione), selezione file (ISO/BIN/CUE), opzioni di patching (checkbox PAL→NTSC), campi per media personalizzati (audio, video, immagini) con pulsanti "Sfoglia" e "Anteprima".
*   **Area di Anteprima (Destra):** Canvas di rendering dedicato che mostra l'anteprima EBOOT in tempo reale. Deve essere ridimensionabile e mantenere le proporzioni corrette (16:9 per la PSP).
*   **Barra di Progresso e Log:** Barra di progresso reattiva e non bloccante durante la conversione, con un pannello di log a scomparsa che mostra i messaggi di FFmpeg e del core.
*   **Gestione Errori:** Messaggi di errore chiari, con suggerimenti su come risolverli, visualizzati in modo non invadente (es. toast notification).

---

## 5. Piano di Implementazione (Roadmap per l'Agente)

### Fase 1: Fondamenta e Prototipo UI (Settimane 1-3)
*   **Setup del Workspace:** Workspace Cargo con i crate: `psx2psp-core`, `psx2psp-patching`, `psx2psp-media`, `psx2psp-app` (GUI).
*   **Parsing PBP:** Implementare il parsing completo dell'header PBP e di `PARAM.SFO` nel crate `core`. Unit test con file PBP di esempio.
*   **Prototipo GUI:** Creare una finestra `iced` di base con il layout a due pannelli (Controlli | Anteprima). Implementare il caricamento asincrono di un'immagine `PIC1.PNG` di test e visualizzarla nell'area di anteprima. **Obiettivo: validare la pipeline di rendering GPU e il caricamento texture.**

### Fase 2: Motore di Conversione e Anteprima Base (Settimane 4-6)
*   **Generazione PBP (Single Disc):** Logica per creare un PBP da BIN/CUE. Iniziare con PBP non compresso per validare la struttura.
*   **Anteprima EBOOT (Statica):** Implementare la logica per visualizzare nell'area di anteprima l'`ICON0.PNG`, `PIC0.PNG` e `PIC1.PNG` sovrapposti correttamente, simulando la schermata "Game" della PSP. Aggiungere i toggle per le schermate "Save" e "Boot".
*   **Integrazione Media:** Implementare gli adapter per FFmpeg. Casi d'uso per convertire audio in `SND0.AT3` e video in `ICON1.PMF`. Aggiungere un player video di anteprima per `ICON1.PMF`.

### Fase 3: Funzionalità Avanzate e Animazioni (Settimane 7-9)
*   **Supporto Multi-Disco:** Logica per `PSTITLEIMG` + `PSISOIMG` multipli.
*   **Patching PAL->NTSC:** Implementare il modulo di patching e integrarlo nella UI con un checkbox.
*   **Animazione Onda XMB:** Implementare un custom shader (WGSL) in `iced_wgpu` per l'onda di sfondo animata nell'anteprima. **Questo è il punto di svolta per l'UX.**
*   **Integrazione YouTube:** (Opzionale) Modulo per il download di tracce audio.

### Fase 4: Rifinitura, Testing e Distribuzione (Settimane 10-12)
*   **UI/UX Polish:** Transizioni, feedback visivi, gestione errori con toast, log in tempo reale. Assicurarsi che tutti i target di performance (60 FPS) siano raggiunti.
*   **Test di Compatibilità:** Testare gli EBOOT.PBP generati su PSP reale e PS Vita.
*   **Test Multi-Piattaforma:** Compilare e testare su Windows, macOS, Linux (x86_64, aarch64).
*   **Ottimizzazione:** Profilare l'applicazione (specialmente I/O, compressione e rendering) e ottimizzare.
*   **Rilascio:** Binari precompilati per tutte le piattaforme.

---

## 6. Strategia di Reverse Engineering (Solo se Necessario)

*   **Codice Sorgente Originale:** Studiare il codice di `popstation` per dettagli implementativi non documentati (es. gestione TOC audio CDDA).
*   **Disassemblaggio di `PSX2PSP.exe`:** Usare Ghidra per analizzare parti specifiche del binario originale, specialmente per l'applicazione di patch o la gestione di casi limite.

---

## 7. Definizione di "Fatto" (Definition of Done)

Il progetto è completo quando:
1.  Un EBOOT.PBP generato da un'ISO PS1 (singolo disco) funziona correttamente su PSP e PS Vita.
2.  Un EBOOT.PBP multi-disco generato funziona correttamente (con cambio disco).
3.  La funzionalità di patching PAL→NTSC è integrata e funzionante su titoli di test.
4.  La conversione da qualsiasi formato audio a `SND0.AT3` e da video a `ICON1.PMF` produce file validi e riproducibili.
5.  L'applicazione è compilabile e funzionante su Windows, macOS e Linux.
6.  **La GUI è fluida, animata (inclusa l'onda XMB) e mostra un'anteprima realistica dell'EBOOT (Game, Save, Boot) in tempo reale, mantenendo 60 FPS costanti.**
7.  **Tutte le operazioni di I/O e conversione sono asincrone e non bloccano mai il thread di rendering della UI.**

Questo documento è la guida definitiva. Ogni deviazione deve essere giustificata da un'analisi tecnica approfondita. Procedi con la costruzione.