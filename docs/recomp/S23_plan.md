# S23 — Codice tradotto e I-cache della PSP (piano, non codice)

**Stato:** passo 0 e passo 1 fatti offline; aspettano il log PSP. Il log PSP dell'EBOOT e43b795
è arrivato: FMV 73 → 80%. S21 resta aperto (passo 2c, pacing a debito).
S22 (SPU sul ME) non parte da qui: spu.c e sceSas sono di S1.

## Da dove si parte: i numeri del log PSP di S21 passo 2

Il criterio scritto in GOAL («livello 1 ≥ 80% su PSP reale») è **già raggiunto**.
`baseline_psp/s21p2_3f6b45d.txt` dà LEV01 98% (worst 87%), e S20d dava 97% (worst 91%).
Le scene sotto bersaglio sono le due con il 3D pieno:

| scena | velocità | worst | interp | io | update | present | sleep | resto (ms/s) |
|---|---|---|---|---|---|---|---|---|
| INTRO | 82% | 71% | 7 | 320 | 114 | 22 | 47 | **490** |
| WHEEL | 85% | 62% | 22 | 239 | 138 | 29 | 249 | **323** |
| LEV01 | 98% | 87% | 8 | 146 | 176 | 23 | 286 | **361** |

`resto` = 1000 meno le parti misurate (`draw`, `mdec` e `spu` stanno dentro `io`). È il tempo nel
codice tradotto, più il poco che nessun contatore copre. In INTRO è metà del secondo, e lo sleep
è 47 ms: lì la CPU è il limite, e il codice tradotto è la parte più grossa.

**Come il traduttore dispone il codice oggi (`emit.rs`).** Il passo principale scorre `d.code` in
ordine di indirizzo guest. Ogni accesso alla memoria emette 6 parole + `then` nel flusso
principale e ~13 parole + `then` + `undo` di percorso lento (scratchpad, I/O) come stub. Hanno
uno stub anche il poll del budget, le chiamate host e i salti lontani. Gli stub sono scritti
(`flush`) **alla fine di ogni tratto lineare**, cioè subito dopo il salto che lo chiude. Il
codice caldo di due tratti consecutivi è quindi separato dal codice freddo dei loro percorsi
lenti, che non gira quasi mai. La I-cache dell'Allegrex è di 16 KB a linee da 64 byte. Ogni
linea che porta uno stub è una linea di codice caldo in meno.

L'ipotesi di GOAL («la ragione principale per cui la PSP è 1.5-3.5x più lenta di PPSSPP») non è
provata. La PSP ha 333 MHz contro un host PC, e un miss della D-cache costa 216 ns (log PSP).
Anche gli accessi alla RAM PS1 (D-cache) stanno nel `resto`. Il passo 0 separa i due prima di
toccare il layout.

## Passo 0 — Misurare, senza toccare il codice tradotto

**0a, `psplog.py`.** Una colonna `code` = 1000 − (interp + io + update + present + sleep).
Lo strumento cambia, il runtime no. Si ripassano i log PSP e PPSSPP già salvati. Il rapporto PSP
contro PPSSPP per scena dice quanto del gap sta nel codice tradotto.

**0b, contatori hardware.** Il profiler dell'Allegrex (registri a `0xBC400000`, come
`pspDebugProfilerRegs` del pspsdk) conta cicli, miss di I-cache e D-cache, writeback, load e
store non in cache, e stalli. Vuole il modo kernel, e psxrt è un modulo utente. ARK-4, che è sulla
card dell'Host, esporta `kuKernelCall` (libreria `KUBridge` di systemctrl): chiama una funzione
con k1 = 0 e livello kernel.
- Il runtime ne usa una da 10 righe: abilita il profiler, legge i registri, li azzera.
- Ogni secondo il log ha una riga `-- cpu: cycles M i_miss K d_miss K wb K uncached K`.
- La misura è opt-in: parte solo se c'è un file `psxrt.prof` accanto all'EBOOT.
- Sotto emulatore la misura è saltata (`kemulator:`). Se l'import non è risolto (un'altra CFW),
  il valore di ritorno lo dice e il runtime spegne la misura.
- Prima del codice si verificano offline il NID di `kuKernelCall` e la mappa dei registri, dai
  sorgenti di ARK-4 e del pspsdk (D-D5): nessuna prova a tentativi sulla PSP.

**Cancello.** Si moltiplica i_miss al secondo per il costo di un miss, misurato sulla PSP come
quello della D-cache (`ge.c` ring_bench). Se il risultato è sotto il 10% del `code` di INTRO, il
layout non è la leva. S23 si ferma qui con i numeri, e il prossimo front lo sceglie il contatore
che domina (d_miss: gli accessi alla RAM PS1 e al contesto).

## Passo 1 — Caldo e freddo separati, senza profilo (`emit.rs`)

Gli stub non si scrivono più dopo ogni tratto: si accumulano e si scrivono a blocchi. Un blocco
va alla fine di un tratto quando il codice dall'ultimo blocco supera metà di `FLUSH_WORDS`.
`far_flush` resta come rete, con il suo `j` sopra il blocco.
- Il branch dal caldo allo stub resta entro 16 bit (`FLUSH_WORDS` = 0x4000 parole, la portata è
  0x7FFF).
- Il ritorno dallo stub è già un `j`.
- `map`, `pat` e la tabella di dispatch non cambiano.
- La modifica sta solo in `emit.rs`, poche righe. `native.c`, `ge.c` e `mdec.c` non si toccano.

**Misura offline prima della PSP.** Un test stampa, per ogni eseguibile di Sheep Dog, le parole
del flusso caldo e le parole totali, prima e dopo. Senza questo numero non si consegna l'EBOOT.
Per un accesso lo stub è circa il doppio del percorso caldo. Quanto pesa sul totale dipende da
quanti load e store ha il codice, ed è quello che il test misura. Correttezza:
`cargo test -p psx2psp-recomp` e il fixed check PPSSPP.

**Fatto (offline).** Il codice freddo è il 66% delle parole host di ogni eseguibile (stub degli
accessi, chiamate host, poll, salti lunghi). Prima un blocco freddo ogni ~82 parole calde, ora uno
ogni ~2930 (11,7 KB, `cold words / cold blocks` nel test `real_executables_translate`). Rapporto
host/guest invariato (7,68 su LEV02). Test e oracolo verdi, fixed check verde a 59,1 fps. Il
profiler hardware (passo 0b) è nel runtime: `psxrt.prof` accanto all'EBOOT, via KUBridge di ARK-4
(`kubridge.S`), riga `-- cpu (k)` nel log.

## Passo 2 — Ordine per frequenza (solo se il passo 1 non basta)

**2a, statico e universale.** Il passo principale scrive per primi i tratti che stanno dentro un
ciclo (bersaglio di un back-edge) e le funzioni chiamate da molti siti, poi il resto. Un tratto
che cade nel successivo e non è più adiacente prende un `j`. La ricostruzione con i salti lunghi
(`Fail::Far`) copre i branch che escono dalla portata. Non serve nessun dato per gioco.

**2b, profilo.** `psx2psp-sim` conta le istruzioni eseguite per tratto con un'opzione
`--profile`, sul codice vero e offline. Il traduttore ordina con quei conteggi. Un profilo è un
dato per gioco: la domanda «fix vs patch» (GOAL §4.6) si fa all'Host prima di 2b.

## Test

Un EBOOT per cambio. S23 parte da un EBOOT di S21 già misurato sulla PSP, mai insieme a un passo
di S21 non ancora misurato. Il layout tocca tutte le scene, FMV compreso, e due cambi nello
stesso EBOOT confondono le due misure.
- Offline: test di recomp, parole calde e totali per eseguibile, fixed check (WHEEL, LEV01,
  livello 1 in PPSSPP).
- PSP: intro, WHEEL, LOADER, INTRO, LEV01, con `psxrt.prof` sulla card. Si confronta con
  `psplog.py` contro l'ultimo log PSP.

## Criterio di chiusura (proposto, cambia GOAL)

- INTRO ≥ 90% e WHEEL ≥ 90% su PSP reale, worst ≥ 80% in entrambe.
- `code` di INTRO giù di almeno il 20%, e i_miss/s giù nella stessa direzione.
- Nessuna scena sotto la baseline. Fixed check verde.

**Non chiudere se:** il guadagno è solo su alcuni secondi, o se i contatori non mostrano meno
miss (il guadagno verrebbe da altro).

**Fermarsi se:** il passo 0 dice che i miss di I-cache non contano (vedi il cancello), o se un
EBOOT di S23 va sotto la baseline.
