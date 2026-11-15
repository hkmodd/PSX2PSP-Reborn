# S20d — Codice riscritto dal gioco: tabella di patch dal traduttore (piano, non codice)

**Stato:** passi 1-3 fatti (passo 3 approvato dall'Host il 2026-11-15: «Continua pure»); restano fixed-check e test PSP.

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

## Esito passi 1-2 (offline, 2026-11-15)

- `emit.rs`: `Translation::patch` e `emit::repatch(host, guest)`. Nessuna serializzazione:
  l'EBOOT è identico.
- `tests/patch.rs`, test sintetico: ogni parola simple di un programma con slot di `jal`,
  `j`, `jr ra`, `jr`, `beq`/`bne` e uno slot che è anche bersaglio di salto (due copie host).
  Immediati e shift cambiati: T + tabella == T′ bit per bit.
- Test con dati veri: LEV02.EXE con `sim/ram08100.bin` e `sim/ram05600.bin`.
  - 110 parole di codice riscritte, tutte patchabili (120 parole host), 0 all'interprete.
  - T + tabella == T′ bit per bit.
  - Le altre 241-243 parole cambiate nel testo non sono codice raggiunto (dati).
- WHEEL.EXE con `simw/ram03000.bin` (sim senza input, frame 3000): stesse 110 parole, tutte
  patchabili (120 parole host), T + tabella == T′ bit per bit. Le stesse funzioni 3D sono in
  entrambi gli exe.

## Esito passo 3 (serializzazione e runtime, 2026-11-15)

- **Verifica su tutti gli exe, prima del runtime.** `tests/patch.rs`
  `every_patchable_word_of_real_executables_patches`: per ciascuno dei 26 exe del disco cambio
  **tutte insieme** le parole patchabili (~1,3 milioni in totale), applico la tabella e confronto
  con una traduzione fresca. Risultato: 0 parole host diverse.
  - Il primo giro ha trovato una dipendenza vera: gli hook di libreria (`hooks.rs`) riconoscono
    le funzioni dal testo, immediati compresi. Una traduzione fresca del testo stravolto perdeva
    gli hook.
  - Il runtime invece tiene gli hook del testo tradotto. Il test ora fa lo stesso
    (`emit::translate_with`).
  - Se un gioco riscrivesse una funzione hookata, l'hook resterebbe vecchio. È un limite
    dichiarato in `native.c`.
- **Engine.** `native_code::patch_groups` produce un gruppo ogni 4 blocchi da 1 KB, con le
  parole di codice (scoperte o delay slot) che `emit::patchable` accetta.
  - Una parola copiata in modo semplice ha i suoi indirizzi host. Le altre sono «fissate» con la
    parola originale: se cambiano, il blocco va all'interprete.
  - I gruppi sono deflate. Directory nell'entry, a 0x38 e 0x3C (prima erano zeri).
- **Runtime (`native.c`).**
  - Ogni blocco ha due somme: delle parole e della loro forma (senza immediati e shift).
    Ora contano anche i delay slot: prima un `slti` riscritto in uno slot non spostava la somma.
  - Al flush, se cambia solo la somma delle parole, si legge il gruppo e si patcha. Prima si
    verifica tutto, poi si scrive (tutto o niente). Altrimenti il blocco va DIRTY, come in S1.
  - Un'immagine patchata viene riletta prima di essere reinstallata. Al load di uno snapshot gli
    immediati della RAM vengono riapplicati al codice riletto.
  - Il bit «la prima parola del blocco è un delay slot» si salva quando si calcolano le somme.
    Così un blocco mandato all'interprete non cambia le somme del blocco dopo (niente cascata).
- **Rotta PPSSPP WHEEL→LOADER→INTRO→LOADER→LEV01.** A ogni caricamento si patchano 120 parole.
  - All'interprete restano solo i blocchi caricati sopra (dati o un altro exe al posto del
    codice: «beyond immediates»), cioè il caso giusto.
  - Al flush 12, 26 KB di INTRO venivano rifiutati con «parola host diversa». La parola letta
    era 0x68xxxxxx: è il marcatore che il JIT di PPSSPP scrive all'inizio di ogni suo blocco
    (op 0x1A, che Allegrex non usa). Il codice era intatto. Lo ha dimostrato una sonda temporanea
    sulla memoria host: le scritture cadono durante WHEEL, all'inizio dei blocchi JIT.
  - Correzione: una parola host con op 0x1A vale come «forma del blocco». Su PSP non capita mai.

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
