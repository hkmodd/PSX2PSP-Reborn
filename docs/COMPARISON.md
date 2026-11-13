# Confronto: PSX2PSP-Reborn vs PSXPackager, PSX2PSP-Enhanced, pop-fe

Stato al 2026-11-12. Fonti:
- i repository pubblici dei tre progetti (sorgenti, README, release);
- il codice di questo workspace, con i test che dimostrano ogni ✓ nostro.

Legenda:
- ✓ supportato;
- ◐ parziale;
- ✗ assente;
- *test* = coperto da un test automatico in questo repo.
- ? = non verificato nelle fonti lette: da controllare prima di usarlo come argomento.

## Matrice delle funzionalità

| Funzionalità | **Reborn** | PSXPackager | PSX2PSP-Enhanced | pop-fe |
|---|---|---|---|---|
| Linguaggio / runtime | Rust nativo, binario unico | C# / .NET (WPF) | Python + `popstation.dll` | Python |
| Windows / Linux / macOS | ✓ / ✓ / ✓ | ✓ / ? / ? | ✓ / ✗ / ✗ | ✓ / ✓ / ? |
| BIN/CUE → EBOOT single-disc | ✓ *test bit-exact* | ✓ | ✓ | ✓ |
| Multi-disco PSTITLEIMG (2–5) | ✓ *test, costanti popstation* | ✓ (m3u) | ◐ fino a 4 | ✓ |
| Estrazione EBOOT → BIN/CUE | ✓ *test bit-exact* | ✓ | ? | ? |
| **BASE.PBP + DATA.PSP inclusi** | ✓ incorporati nel binario | ✓ | ✓ | ✓ |
| Patch PPF 1/2/3 | ✓ *test* | ? | ? | ✓ |
| Patch PAL→NTSC | ✓ preset `patches.ini` (6 regole) + PPF per titolo, *test end-to-end* | ✗ | ✓ `patches.ini` | ◐ |
| Artwork da qualsiasi immagine (auto-fit) | ✓ | ? | ? | ? |
| **Anteprima XMB animata (onda) Game/Save/Boot** | ✓ shader WGSL, *test GPU* | ◐ onda, senza le 3 schermate | ✗ | ✗ |
| **ICON1.PMF generato** | ✓ **muxer PSMF nativo**, *test vs sample Sony* | ✗ | ✗ (solo GIF) | ✗ |
| **ICON1 animato in anteprima** | ✓ *test GPU* | ✗ | ✗ | ✗ |
| SND0.AT3 da file audio/video | ✓ | ✗ | ✓ (at3tool) | ✓ (atracdenc) |
| SND0 da YouTube | ✓ yt-dlp | ✗ | ✓ | ✓ |
| **Taglio dei secondi (inizio/fine)** | ✓ qualsiasi intervallo | ✗ | ? | ◐ (taglio fisso 59 s) |
| **YouTube → ICON1 + SND0 dallo stesso clip** | ✓ | ✗ | ✗ | ✗ |
| **Solo colonna sonora, staccata dal video** | ✓ modalità `SND0 only` | ✗ | ✓ | ✓ |
| **Budget XMB 500 KB condiviso, bitrate auto-adattato** | ✓ ICON1 adattato a ciò che resta | ✗ | ✗ | ✗ |
| Download dei tool con un clic | ✓ ffmpeg, yt-dlp, atracdenc | n/a | ? | ? |
| Formati in ingresso CHD / ZIP / 7z / RAR | ✗ | ✓ | ✗ | ✓ CHD |
| Database dei giochi (titolo e copertina automatici) | ◐ titolo e save ID (10 109 seriali), copertine no | ✓ | ✗ | ✓ Redump |
| Patch LibCrypt | ✗ | ✗ | ✗ | ✓ |
| Target PS3 / PS Classic / Vita bubble | ✗ | ✗ | ✗ | ✓ |
| Compressione parallela in streaming | ✓ rayon | ? | ? | ? |
| Annullamento, log e UI mai bloccata | ✓ | ? | ? | ? |

## Dove siamo gli unici

1. **ICON1.PMF dal nulla.** Il muxer PSMF è nativo: pack MPEG-PS da 2048 B, tabella AU 0xBF per
   GOP, PTS/DTS ed estensione P-STD.
   - Sul sample Sony (605 frame) il re-mux è bit-identico nel video, nelle tabelle AU e nei PTS.
   - Nessuno degli altri tre genera ICON1: si fermano al passthrough o a una GIF.
2. **Clip editor unico.** Da un URL o da un file si scelgono inizio e fine, poi si ottiene:
   - ICON1 e SND0 insieme;
   - oppure solo ICON1;
   - oppure solo la colonna sonora, con ATRAC3 a 66, 105 o 132 kbps.

   Il budget di 500 KB della XMB viene diviso automaticamente: SND0 è CBR, quindi si fa per
   primo, e il video riceve il resto con un bitrate ricalcolato a ogni tentativo.
3. **Anteprima WYSIWYG animata.** L'onda XMB, i layer e l'ICON1 che scorre nello slot ICON0 sono
   resi su GPU, con le tre schermate Game, Save e Boot.
4. **Tutto compreso.** Il donor (`BASE.PBP` e `DATA.PSP`) è incorporato nel binario. I tool
   esterni si installano con un clic in `<exe>/tools`: FFmpeg GPL con libx264, yt-dlp e
   atracdenc, tutti dalle release ufficiali.

## Lacune ancora aperte

| Lacuna | Chi ce l'ha | Costo stimato |
|---|---|---|
| Input CHD e archivi | PSXPackager, pop-fe | medio: decoder CHD (zstd/lzma/flac) e lettura degli archivi |
| Database dei giochi e copertine automatiche | PSXPackager (`gameInfo.db`), pop-fe (Redump) | medio: va ricostruito su una fonte con licenza compatibile |
| Patch LibCrypt | pop-fe | basso–medio: tabella dei settori e chiavi per titolo |
| Set di patch PAL→NTSC pronto (`patches.ini`) | PSX2PSP-Enhanced | basso: import del formato ini nel pattern patcher |
| Target PS3 / PS Classic | pop-fe | alto, ed è fuori dallo scope di AGENTS.md |

## Non ancora verificato

- **Hardware reale.** Nessun EBOOT è stato ancora avviato su PSP o Vita, inclusi gli ICON1
  generati. La struttura è validata contro i sample Sony, non ancora contro il firmware.
- **atracdenc 0.2.3 e yt-dlp.** Sono provisionati dal codice, ma non scaricati né eseguiti su
  questa macchina: il download richiede il consenso dell'Host.
