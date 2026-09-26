# ADR 0026 — JIT: FP/SIMD nelle regioni, orologio, concatenamento in modalità utente

- Stato: accettata (M4, 2026-09-26). Estende l'ADR 0012, 0013 e 0024.

## Contesto
ART, bionic, Skia e SwiftShader usano molto la virgola mobile e SIMD.
Dopo l'ADR 0024 le regioni traducevano solo i load/store dei registri V,
DUP/INS/UMOV/SMOV e MOVI: ogni altra istruzione SIMD/FP chiudeva la
regione e la eseguiva l'interprete.

Il profilo con i contatori per classe (sotto) su `tests/linux/c/fpsimd.c`
(conversioni, prodotti di matrici float e double, memcpy/strlen NEON, TBL,
EXT, ADDV/UMAXV, FPCR/FPSR) mostrava:
- il 76% delle istruzioni ancora all'interprete (31 dei 41 milioni);
- il JIT (21 MIPS) non più veloce dell'interprete (20 MIPS).

Inoltre:
- nell'avvio del kernel le letture di CNTVCT uscivano dalle regioni
  (17 mila);
- in modalità utente ogni regione era una chiamata a wasmtime (27 milioni
  per `gzip`), e le esclusive restavano all'interprete (6% delle uscite di
  `awk`).

La semantica FP dell'interprete (`vetro_cpu::simd::fp`) è software, bit per
bit come l'Arm. Il WASM ha l'IEEE 754 con arrotondamento al pari più
vicino, ma:
- non ha i flag;
- non fissa i bit dei NaN;
- non ha la FMA;
- non guarda la minuscolità prima dell'arrotondamento, come fa l'Arm per
  UFC.

## Decisione

### Misura: contatori per classe
`vetro_jit::profile` conta per classe le istruzioni che l'interprete esegue
col JIT attivo, distinguendo quelle che il JIT saprebbe tradurre (codice
freddo) da quelle che mancano. Le classi SIMD/FP sono fini: classe, opcode,
U, Q, dimensione. Si attiva con `VETRO_JIT_PROFILE=1` in `vetro run` e in
`vetro boot` (`JitConfig::profile`, `SysJitConfig::profile`). Conta anche
le istruzioni eseguite da `env.simd`.

### `env.simd`: l'interprete dentro la regione
Ogni istruzione SIMD/FP senza memoria (intera, FP, crittografica) si
traduce. Quelle senza una forma in linea chiamano `rt.simd` → `env.simd`
(`vetro_jit::helper`): l'interprete (`vetro_cpu::simd::exec_dp`) la esegue
sui registri V, FPCR e FPSR di `JitState`, senza uscire dalla regione.
- La semantica è quella dell'interprete per costruzione.
- Registri generali e NZCV restano nelle variabili della regione: chi chiama
  passa il registro generale letto e NZCV, e riceve il registro scritto o
  il nuovo NZCV (`helper::io`).
- `JitState` porta FPCR e FPSR (offset 424 e 428, prima riempimento).
- vetro-wasm esporta `vetro_jit_simd` (ABI 11).
- Una CPU d'appoggio per thread; si copiano i 512 byte dei registri V.

### SIMD intero in linea con WASM SIMD
Solo le forme esatte per costruzione, con la semantica di
`vetro_cpu::simd::int` (con Q = 0 la metà alta è zero):
- logiche, BSL/BIT/BIF;
- somme e differenze, confronti, min/max, [SU]ABD/[SU]ABA, MUL/MLA/MLS,
  URHADD;
- ADDP, [SU]MAXP/[SU]MINP;
- ABS/NEG, CNT, NOT, REV, [SU]ADDLP/[SU]ADALP, XTN;
- riduzioni (ADDV, [SU]MAXV, [SU]MINV, [SU]ADDLV);
- shift per immediato (SHL, [SU]SHR, [SU]SRA, [SU]SHLL, SHRN);
- ZIP/UZP/TRN, EXT, TBL/TBX (con `swizzle`: indice - 16k per la tabella k).

Le saturanti [SU]Q{ADD,SUB} a 8 e 16 bit, SQXTN e SQXTUN usano le
saturanti del WASM e scrivono FPSR.QC se una corsia differisce dal
risultato modulare o dal troncamento: c'è saturazione esattamente quando
differiscono.

### Virgola mobile: percorsi veloci nel runtime
FMOV, FABS, FNEG e FCSEL (scalari) e FABS/FNEG vettoriali sono in linea:
sono bit, anche sui NaN.

Le altre operazioni hanno una funzione del runtime `rt.fp<k>`. La regione
la chiama con la parola dell'istruzione; la funzione legge i registri da
`JitState` e calcola col WASM. Scrive il risultato solo se è sicuramente
quello dell'interprete, cioè se valgono tutte queste condizioni:
- FPCR = 0;
- nessun NaN in ingresso o in uscita;
- nessun infinito da trabocco;
- per prodotti, quozienti, FMA e conversioni che restringono, un
  risultato normale e maggiore del più piccolo normale (l'Arm segnala UFC
  guardando la minuscolità prima dell'arrotondamento);
- IXC già a 1 in FPSR (flag cumulativo), oppure un risultato esatto
  verificato in modo esatto:
  - TwoSum per le somme;
  - in singola, prodotto, quoziente e radice ricontrollati in doppia;
  - conversioni: il valore arrotondato uguale all'ingresso.

Se una condizione manca, la funzione chiama `env.simd`.

Sono coperte:
- scalari: FADD/FSUB/FMUL/FDIV/FMAX/FMIN/FMAXNM/FMINNM/FNMUL, FSQRT,
  FMADD e varianti, FCMP/FCMPE, FCVT fra S e D, FRINT[NPMZAIX],
  SCVTF/UCVTF, FCVT[NPMZA][SU];
- vettoriali: le stesse binarie, FMLA/FMLS e FMUL/FMLA/FMLS per elemento,
  FADDP, FABD, FCMEQ/FCMGE/FCMGT (anche con zero), FSQRT,
  FCVT[NPMZA][SU], SCVTF/UCVTF, FCVTL/FCVTN.

La FMA:
- in singola si calcola in doppia: il prodotto è esatto, la somma si
  arrotonda "a dispari" (TwoSum, poi il bit basso) e poi al pari in
  singola. È l'arrotondamento corretto della FMA (Boldo e Melquiond).
- in doppia è l'algoritmo "Emulation of FMA" di Boldo e Melquiond (IEEE TC
  2008):
  1. prodotto esatto di Dekker con lo spezzamento di Veltkamp;
  2. TwoSum;
  3. somma degli errori arrotondata a dispari;
  4. somma finale.

  Vale senza trabocchi né minuscoli, garantiti dagli esponenti: fattori in
  [2^-400, 2^400), addendo zero o in [2^-800, 2^800).

Un modulo di regioni importa solo le `rt.fp<k>` che usa (importarle tutte
costava ~1,5 KB per modulo, 1,5 MB nell'avvio in V8).

### Load/store SIMD
- LD1/ST1 di 1-4 registri, LD1R, LD1/ST1 di una corsia, LD2..LD4/ST2..ST4
  (strutture multiple, permutazioni con due livelli di `shuffle`).
- Accessi da 8 o 16 byte invece che per elemento: stessi byte. Se un
  accesso largo fallisce e quelli per elemento no (allineamento), l'uscita
  per fault lascia decidere all'interprete.
- I load leggono tutto prima di scrivere i registri.
- Gli store scrivono in ordine: rifatti dall'interprete dopo un fault,
  riscrivono gli stessi byte.

### CNTPCT/CNTVCT nelle regioni
`JitState` porta `time_base` (istruzioni della macchina all'inizio della
corsa del dispatcher), CNTVOFF e `time_ok`. La regione calcola
`counter(time_base + passi)` come `Machine::counter`: CNTPCT = s / 8 × 5 +
(s mod 8) × 5 / 8, CNTVCT = CNTPCT − CNTVOFF.
- La macchina dà l'orologio prima di ogni corsa (`SysJitDyn::set_time`);
  senza, la regione esce e legge l'interprete.
- A EL0 si traduce solo con CNTKCTL_EL1.EL0PCTEN/EL0VCTEN a 1. I due bit
  entrano nei parametri della regione e nel contesto della cache dei salti
  (`ctx = epoca << 7 | parametri`).

### Modalità utente: concatenamento ed esclusive
`JitCpu` usa la tabella del motore e il dispatcher della modalità sistema.
- Ogni spazio d'indirizzamento ha un contesto per la cache dei salti. Il
  contesto cambia quando le pagine dello spazio si invalidano: le voci di
  uno spazio restano buone quando lo scheduler ci torna.
- Le voci mancanti le scrive l'host (`Host::resolve`) se la regione è già
  compilata.
- LDXR/STXR e varianti si traducono anche in modalità utente, col monitor in
  `JitState` (`from_cpu`/`to_cpu` lo copiano).

### Secondo livello: no
Si è provato un secondo livello per le regioni calde:
- un contatore per regione in una variabile globale del modulo;
- all'n-esimo ingresso l'host ricompila la regione con fino a 256
  istruzioni, e la cache dei salti riparte.

Misure in V8 (kernel M3 + BusyBox, comandi nel guest al secondo giro, avvio
di `tests/boot`):

| | avvio | moduli | `awk` FP | ciclo di shell |
|---|---|---|---|---|
| senza secondo livello | 1,61-1,68 s | 10,1 MB | 270 MIPS | 220 MIPS |
| secondo livello a 2000 ingressi | 1,76-1,80 s | 11,0 MB | 269 MIPS | 231 MIPS |
| secondo livello a 200 ingressi | 1,98 s | 13,4 MB | 283 MIPS | 224 MIPS |
| tutte le regioni da 256 | 1,68 s | 12,0 MB | 318 MIPS | 245 MIPS |

Al più +5% a regime, per un avvio più lento del 6-18%: il profilo non lo
giustifica, e il codice non entra.
- Il guadagno delle regioni grandi viene dalle transizioni fra regioni
  tiepide, non dalle poche regioni calde.
- Regioni più grandi per tutti (+11-18% sul codice ramificato, +19% di byte
  compilati) restano una scelta da rifare con un carico Android vero.

## Verifica
- `tests/linux/c/fpsimd.c` (`tests/linux/tests/fpsimd.rs`) contro QEMU, con
  l'interprete e col JIT.
- `vetro-jit-native/tests/parity.rs`:
  - registri V, FPCR e FPSR casuali con valori speciali;
  - programmi per tre quarti SIMD/FP, anche load/store dei registri V;
  - coppie esclusive in modalità utente;
  - concatenamento dopo l'invalidazione di una pagina.
- `vetro-jit-native/tests/fp_limits.rs`:
  - ogni istruzione con un percorso veloce su 470 mila combinazioni di
    valori speciali, FPCR e FPSR;
  - FMA casuali;
  - il doppio arrotondamento che l'arrotondamento a dispari evita (in
    singola e in doppia);
  - il prodotto minuscolo arrotondato al più piccolo normale;
  - i percorsi veloci usati davvero (`helper::calls` non cresce).
- `sys_parity.rs`: MRS CNTPCT/CNTVCT, CNTKCTL casuale, orologio a volte
  assente.
- `tests/diff`: i programmi casuali SIMD/FP col JIT (come prima), più
  `random_fp_fast_paths_match_qemu` (semi `fpfast-`: valori normali, FPCR a
  zero, IXC a 1 metà delle volte) contro QEMU.
- Ogni percorso veloce ha un errore introdotto apposta che un test trova:
  - somma senza TwoSum, soglia del minuscolo, arrotondamento a dispari;
  - BIT come BIF, QC mai scritto, MAXP come MINP;
  - permutazioni di LD2..LD4/ST2..ST4, offset di LD1 multiplo, indice di
    LD1 di una corsia;
  - contatore senza l'indice dell'istruzione;
  - contesto non cambiato dopo un'invalidazione, monitor non ricopiato.
- Durante il lavoro il test che conta le chiamate a `env.simd` ha trovato un
  errore vero: IXC combinato con AND come 0x10 invece che come booleano, e
  il percorso veloce con IXC a 1 non scattava mai.

## Conseguenze
- vetro-wasm ABI 11 (`vetro_jit_simd`); `JitState` di 992 byte (FPCR,
  FPSR, orologio); `docs/specs/jit.md` e `docs/specs/wasm.md` aggiornate.
- `vetro-cpu` esporta:
  - `simd::exec_dp`;
  - i tipi `simd::MovKind` e `simd::Post`.
- `SysJitDyn` ha `set_time` e i metodi del profilo; `Machine` li usa.
- In modalità utente il limite è ora la memoria: ogni load/store è una
  chiamata all'host (`UserMemory`). Il passo successivo è una TLB software
  con la memoria del guest nella memoria del motore.
