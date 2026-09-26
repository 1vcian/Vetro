# ADR 0024 — JIT a regioni: codice compatto, flag pigri, SIMD, runtime condiviso

- Stato: accettata (M4, mantenimento, 2026-09-26). Estende l'ADR 0012 e
  l'ADR 0013.

## Contesto
Android chiede centinaia di MIPS sostenuti nel browser. Prima di questo
lavoro l'avvio del kernel guest (284,8 M istruzioni con l'initramfs di oggi,
di cui circa la metà tempo saltato nelle WFI) durava 1,91 s col JIT in V8;
a regime, nel guest, `sha256sum` andava a ~575 MIPS, `gzip` a ~360 e un
ciclo di shell (ash, sistema e utente mescolati) a ~109.

Le misure (profilo di V8, `sample` di macOS, contatori; numeri in
`docs/progress/M4.md`) hanno mostrato dove va il tempo:
- **compilazione**: nel thread principale di V8 la validazione dei moduli e
  la compilazione pigra di Liftoff al primo uso di ogni funzione prendevano
  il 25% dell'avvio (il 37% con le prime regioni, più grandi). Il costo è
  proporzionale ai byte: ~1,1 KB di WASM per blocco, 150-370 byte per istruzione
  nelle istruzioni di memoria (percorso lento in linea). TurboFan, sui
  thread di fondo, ha un costo più che lineare nella dimensione delle
  funzioni e nel numero di valori vivi (allocazione dei registri);
- **uscite verso l'interprete**: nel codice utente di BusyBox (musl) ~590
  mila uscite su poche istruzioni SIMD (LDR/STR/LDP/STP di registri Q, DUP,
  INS, UMOV, MOVI: `memcpy`/`memset`), con frammentazione delle regioni e
  interpretazione a freddo dopo ognuna; nel kernel ~110 mila uscite su
  MRS/MSR DAIF, MSR DAIFSet/DAIFClr, ELR/SPSR/ESR/FAR;
- **dispatch**: a regime ~17 istruzioni per chiamata di regione; il ciclo del
  dispatcher costa poco, costano prologo e coda della regione (registri,
  flag) e le chiamate per ogni accesso alla memoria.

## Decisione

### Regioni al posto dei blocchi
- L'unità è la **regione**: i blocchi base di una pagina raggiungibili
  dall'ingresso con salti diretti, presi e non presi, anche all'indietro, al
  più 64 istruzioni. Nella funzione: un `loop` con un `br_table`
  sull'indice del prossimo blocco base; il blocco successivo in memoria si
  raggiunge senza salti. Ogni blocco base controlla prima di iniziare che i
  suoi passi stiano nel limite, altrimenti esce al suo inizio: il numero di
  istruzioni resta esatto nei cicli, e quindi interrupt, tempo e punti di
  arresto del replay (ADR 0019) non cambiano.
- **Ingressi multipli.** Ogni blocco base con passi è un ingresso (indice
  in 6 bit nella voce della cache dei salti, scritto in `JitState::entry` dal
  dispatcher): un `pc` già dentro una regione compilata non ne fa tradurre
  un'altra, che duplicherebbe il codice.
- **64 istruzioni, niente ritorni delle chiamate.** Regioni da 256
  istruzioni danno +10% a regime ma +40% di CPU di compilazione (TurboFan);
  seguire anche l'indirizzo di ritorno dopo BL/BLR porta i byte da 31 a 53
  MB e l'avvio da 2,0 a 4,1 s. Il limite si rivaluta quando la
  compilazione costerà meno (per esempio con un secondo livello).

### Codice compatto
- **Runtime condiviso.** I percorsi lenti, le coppie, gli accessi Q e non
  allineati, il calcolo di NZCV, la fine della regione e la copia dei
  registri SIMD stanno in un modulo di runtime compilato una volta
  (`translate::runtime`, `Engine::runtime`): le regioni lo importano come
  `rt.*` (in V8 una chiamata diretta fra istanze). Definiti in ogni modulo
  costavano 1,4 KB e 20 compilazioni pigre per modulo.
- Il **percorso veloce della TLB** degli accessi allineati resta in linea
  (+10% a regime rispetto alla chiamata, a costo di qualche byte); il resto
  è una chiamata.
- **Uscite corte**: per FAULT `pc` e `steps` li ha già salvati il percorso
  lento; `NEXT` e `done = 0` sono il valore iniziale delle locali; la coda
  scrive `pc`/`steps` senza chiamate per `NEXT` e chiama `rt.finish` per
  gli altri codici (STOP dopo uno store: `pc` salvato + 4).
- Forme dirette per ADD/SUB senza flag, MOV, UBFM/SBFM (LSL, LSR, ASR,
  UBFX, SBFX, UBFIZ, SBFIZ), indirizzi con offset; l'allineamento di SP si
  controlla una volta per blocco base finché SP non cambia in modo da
  poterlo perdere.
- Risultato: 66 byte per istruzione nel campione di istruzioni tipiche del
  kernel del test `codice_compatto` (~130 prima), 10,1 MB di moduli per
  l'avvio contro 14,9 MB.

### Flag pigri
- ADDS/SUBS/CMP/CMN/ANDS/TST lasciano tipo, operandi e risultato (in locali
  dentro la regione, in `JitState` fra una regione e l'altra: `fk`, `fa`,
  `fb`, `fr`). B.cond, CSEL e CCMP calcolano la condizione dagli operandi se
  il tipo è noto nel blocco base (`cmp; b.ne` diventa un confronto);
  altrimenti `rt.nzcv`. L'host ricava NZCV con `state::lazy_nzcv` (la stessa
  formula) quando ricopia lo stato nella `Cpu`.

### SIMD, DAIF e registri delle eccezioni nelle regioni
- LDR/STR di registri B/H/S/D/Q, LDP/STP di S/D/Q, DUP, INS, UMOV/SMOV,
  MOVI/MVNI/ORR/BIC immediati. I registri V stanno in `JitState` e ci
  arrivano solo quando serve: la prima regione della corsa che li usa chiama
  `rt.vsync` → `env.vsync` (l'host copia `Cpu::v`, `v_valid` = 1), e chi
  ricopia lo stato nella `Cpu` riporta i registri se `v_valid`. Nessun costo
  per le corse senza SIMD.
- Un accesso Q è due accessi da 8 con le regole dell'accesso intero di
  `SysMem::access`: a cavallo di pagina FAULT (l'interprete traduce tutte le
  pagine prima di scrivere: niente scritture parziali); allineato a 8 ma non
  a 16, in modalità sistema, le metà vanno all'host marcate come non
  allineate (`SIZE_PART_OF_MISALIGNED`: SCTLR_EL1.A, memoria Device).
- **TLB dei non allineati**: una seconda TLB software, riempita solo dopo un
  accesso non allineato riuscito (quindi memoria Normal e SCTLR_EL1.A a 0),
  per gli accessi non allineati che restano nella pagina. Svuotata come
  l'altra.
- In modalità sistema le istruzioni SIMD si traducono solo se CPACR_EL1.FPEN
  le permette all'EL (parametro `fp` della regione); i parametri della
  regione (EL, TBI, SPSel, FP) entrano anche nel contesto della cache dei
  salti (prima solo EL: una voce poteva valere con SPSel diverso).
- A EL1: MRS/MSR DAIF, ELR_EL1, SPSR_EL1, MRS ESR_EL1, FAR_EL1, MSR
  DAIFSet/DAIFClr. Un MSR che smaschera (un bit di DAIF da 1 a 0) esce con
  il nuovo codice `YIELD` dopo l'istruzione: `SysJit::run` torna al
  chiamante, che ricontrolla gli interrupt prima dell'istruzione
  successiva, come l'interprete.

### Host
- Il codice freddo (nessuna variante, sotto la soglia) non traduce il
  `pc` con la MMU: basta contarlo.
- Soglia di default 64 ingressi (16 prima): meno codice tiepido compilato;
  misurato 16/32/64/128, 64 il migliore nell'avvio.

### Non fatto, e perché
- **Concatenamento diretto fra moduli** (tail call): il dispatcher è già
  WebAssembly e non torna al JS; il suo ciclo è una piccola parte del costo
  per regione (prologo, coda e chiamate contano di più). `return_call`
  esiste in V8 e wasmtime, ma il guadagno atteso è piccolo rispetto al
  rischio di un altro ABI: resta come possibilità, da misurare.
- **Compilazione in un Worker o asincrona**: in V8 la compilazione pigra di
  Liftoff avviene comunque al primo uso nel thread principale;
  `WebAssembly.compile` asincrono sposterebbe solo la validazione (~8% del
  thread principale) e richiede che il ciclo di esecuzione ceda al ciclo
  degli eventi. Il determinismo non è un ostacolo: interprete e JIT danno
  lo stesso stato architetturale (stesse istruzioni, stesso log, verificato
  dai test di parità e dall'avvio), quindi finché un modulo non è pronto il
  codice gira nell'interprete e il risultato non dipende da quando la
  compilazione finisce; cambierebbero solo i contatori del JIT. Da fare con
  un secondo livello (regioni calde ricompilate più grandi) quando conviene.
- **Modalità utente**: `JitCpu` chiama ancora il motore per ogni regione
  (niente dispatcher) e ogni accesso passa da `UserMemory`: il prossimo
  guadagno lì è il concatenamento, non il traduttore.

## Verifica
- `vetro-jit-native/tests/parity.rs` e `sys_parity.rs` (programmi casuali in
  modalità utente e sistema, ora con SIMD, DAIF, ELR/SPSR, UMA casuale): un
  errore introdotto apposta viene trovato in ognuno dei punti nuovi (flag
  pigri nell'host, MOVI, metà di un Q, Q allineato a 8 su memoria Device,
  TLB dei non allineati riempita da accessi allineati).
- Test mirati: Q a cavallo di pagina (modalità utente, memoria uguale
  all'interprete), accesso non allineato che sconfina da una pagina nella
  TLB dei non allineati a una non mappata (modalità sistema), YIELD dopo
  DAIFClr (e niente uscita per DAIFSet o per un bit già a zero), regioni con
  cicli e dimensione del codice (`codice_compatto`).
- Avvio del kernel col JIT su wasmtime e su V8 (`tools/wasm-boot.sh --jit`):
  stesse istruzioni e stesso log dell'interprete; replay e snapshot col JIT;
  tests/linux con `VETRO_JIT=1`; tests/diff e tests/isa con l'oracolo.

## Conseguenze
- ABI del JIT (`docs/specs/jit.md`): `JitState` a 976 byte (ingresso, DAIF,
  ELR/SPSR/ESR/FAR, registri V, flag pigri), area della modalità sistema da
  1024 con la TLB dei non allineati, voce della cache dei salti con
  l'ingresso, codice `YIELD`, import `rt.*`, `env.vsync`,
  `Engine::runtime`, `Host::vsync`. vetro-wasm passa all'ABI 10
  (`vetro_jit.runtime`, `vetro_jit_vsync`, `yields`).
- `vetro-cpu` esporta `simd::{CopyOp, MovImmOp}` (solo i nomi dei tipi).
