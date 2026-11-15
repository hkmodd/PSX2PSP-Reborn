# S20d — Codice riscritto dal gioco: tabella di patch dal traduttore (piano, non codice)

**Stato:** proposta, in attesa di approvazione (GOAL.md §4 punti 2 e 5: tocca `native.c` oltre 50 righe).

## Problema

Sheep Dog, a ogni caricamento di livello, riscrive gli immediati di ~110 istruzioni del proprio
codice 3D: 46 `addi`, 28 `slti`, 36 `slti` in delay slot (WHEEL: 0x80033C00-0x8003D400,
0x8004A400-0x8004CC00; LEV02: 0x80037C00-0x80041400, 0x8005CC00-0x8005F400).
Esempi: `2308D96C`→`2308FD26`, `2B012694`→`2B0103DA`.

- **S19 / S20a:** la traduzione vecchia continua a girare con gli immediati vecchi. È veloce
  (100% PPSSPP), ma **non è corretta**: il gioco usa costanti che non ha scritto.
- **S1 (c3283cb):** il blocco cambiato andava all'interprete per sempre. È corretto, ma il 3D
  scende al 9-12% su PSP.
- **La mia patch per-parola (mai committata):** cercava a runtime dove fosse finita ogni parola
  nel codice host (scansione di ≤10 parole per trovare il delay slot). Crash in PPSSPP a
  `0x8010A2C0` al primo avvio. Era una congettura, non un dato.

## Idea (psp-web-recomp, MIT, patch 0010 «cache code written at run time»)

Chi sa con certezza dove finisce ogni istruzione guest è il **traduttore**, non il runtime.
`emit.rs` copia ogni istruzione «simple» (ALU e immediati: `simple()`) in **una** parola host,
oppure in due se è un delay slot duplicato sui due rami. Il traduttore può quindi emettere
una tabella esatta:

    guest pc → [indirizzo host 1, (indirizzo host 2)]   solo per le parole simple

Al flush della I-cache il runtime confronta le parole del blocco cambiato con quelle tradotte.
Per ogni parola cambiata:

- vecchia e nuova sono simple **della stessa forma** (stessi registri e opcode, cambia solo
  l'immediato, o lo shift amount): scrive la parola nuova (con `addi`→`addiu`) in ogni
  indirizzo host della tabella. **Resta nativo.**
- altrimenti: il blocco va all'interprete, come in S1. È la rete di sicurezza: resta lento ma
  corretto, e il log dice quale parola lo ha mandato lì.

psp-web-recomp fa un passo in più: le varianti di codice scritte a runtime vengono registrate
e tradotte AOT alla conversione successiva, indicizzate per (indirizzo, hash). Lo tengo come
secondo livello per i giochi che riscrivono più degli immediati: qui non serve.

## Dove sta la tabella

Non in RAM: guest 682 KB → ~170k parole, una tabella completa costerebbe 0,7-1,3 MB.
Va nel file della traduzione dentro l'EBOOT, divisa per blocco da 1 KB, e il runtime la legge
**solo per i blocchi che cambiano**. Succede ai caricamenti, dove un flush costa già 10-18 ms.

## Passi e verifica (Regola 4: offline prima)

1. **`emit.rs`:** `Code` registra (indice host, guest pc) per ogni parola simple, compresi gli
   slot spostati e duplicati. Gli indici seguono `append` come già fanno `fix` e `defs`.
   `Translation` espone `patch: BTreeMap<u32, Vec<u32>>`. Per ora non viene serializzata:
   l'EBOOT resta identico.
2. **Test Rust con dati veri:** gli exe di Sheep Dog (exes/) più i dump RAM del sim dopo il
   caricamento di WHEEL e LEV02, che contengono le ~110 parole riscritte.
   - Traduco l'exe originale (T) e l'exe con il codice riscritto (T′).
   - Applico la tabella di T alle parole cambiate: deve dare **esattamente** le parole di T′,
     bit per bit.
   - Test sintetico su tutte le parole simple dell'immagine: cambio l'immediato di ognuna,
     ritraduco e confronto, delay slot compresi.
   - Se un caso non torna, si vede qui in 30 secondi, non come crash su PSP.
3. **Serializzazione nell'EBOOT** e lettura lazy nel runtime (`native.c` `cache_flushed`):
   parola uguale in forma → patch; altrimenti blocco DIRTY. Stimo ~80 righe di `native.c`.
4. **Fixed-check** (`scripts/fixed_check.ps1`): le 3 prove verdi, fps ≥ 95% di 58,9.
5. **Test PSP:** il 3D deve restare ≥ S20a e il log deve dire «N parole patchate, 0 blocchi
   all'interprete» a ogni livello.

## Criterio di chiusura

- Test 2 verde con dati veri: 0 differenze su WHEEL e LEV02.
- Fixed-check verde.
- PSP: 3D ≥ S20a, 0 KB all'interprete per Sheep Dog.

## Non chiudere se

- Qualche parola riscritta da Sheep Dog finisce all'interprete.
- Il 3D su PSP scende sotto S20a.

## Perché è una fix e non una patch (RECOMP_VISION §6)

Il codice che si modifica da solo per cambiare costanti è comune nei giochi PS1 (tabelle di
livello, parametri di rendering). La tabella non sa nulla di Sheep Dog.
