# ADR 0013 — JIT in modalità sistema: blocchi per pagina fisica, concatenamento, TLB software

- Stato: accettata (M4, 2026-09-25). Estende l'ADR 0012.

## Contesto
L'ADR 0012 fissa il JIT a blocchi verso WebAssembly e la soglia di M4:
l'avvio del kernel guest sotto il JIT in Node (V8) non deve durare più
dello stesso avvio con l'interprete nativo. In modalità utente il JIT c'è
già (`JitCpu`). In modalità sistema servono in più:
- la MMU: il codice si trova per indirizzo virtuale, ma vale per una pagina
  fisica, e la traduzione cambia con i registri, con le TLBI e con l'EL;
- gli interrupt e i dispositivi: l'orologio (istruzioni eseguite) e i punti
  in cui arrivano gli interrupt devono restare quelli dell'interprete;
- le scritture fisiche da ogni parte (CPU, DMA dei dispositivi, caricamento
  delle immagini) sul codice tradotto;
- velocità: in V8 ogni passaggio fra JS e WebAssembly costa, e ogni accesso
  alla memoria che torna all'host costa ancora di più.

## Decisione

### Chi fa che cosa
- `vetro_jit::sys::SysJit` esegue **solo blocchi tradotti** e si ferma
  prima di qualunque cosa debba fare l'interprete (istruzione non tradotta,
  fault, accesso fuori dalla RAM, SVC). Non conosce la piattaforma: vede la
  memoria fisica attraverso il trait `SysPhys`.
- `Machine::run` alterna `SysJit::run` e `Cpu::step_system`. Chiama il JIT
  solo quando l'interprete, al passo successivo, non prenderebbe un
  interrupt (IRQ/SError non mascherati, PSTATE.IL, PC non allineato), e gli
  concede al più i passi fino alla prossima scadenza del timer (lì
  l'interprete aggiorna le linee di interrupt prima dell'istruzione).
- Dentro i blocchi niente cambia le linee di interrupt: i blocchi non
  toccano l'MMIO (un accesso fuori dalla RAM è un fault del JIT e lo rifà
  l'interprete) né i registri di sistema della piattaforma, e non cambiano
  DAIF. Quindi stesse istruzioni, stessi interrupt negli stessi punti, stesso
  log: è verificato byte per byte (sotto).

### Blocchi
- Chiave: (`pc`, EL, TCR_EL1.TBI0, TBI1, PSTATE.SP). Ogni chiave ha una
  variante per pagina fisica. A ogni ingresso dall'host si traduce `pc` per
  un fetch con i permessi di EL (la stessa traduzione dell'interprete, cache
  delle traduzioni recenti compresa) e si usa la variante di quella pagina.
- Il blocco finisce dopo un salto incondizionato, prima di SVC o di
  un'istruzione non tradotta, a fine pagina o dopo 64 istruzioni. I salti
  condizionati non lo chiudono: presi, escono dal blocco (uscita laterale).
- In più rispetto alla modalità utente si traducono: le esclusive (il
  monitor sta in `JitState`), DC ZVA (sempre dall'host, con le regole di
  `zero_block`), CRC32, MRS/MSR di TPIDR_EL0, TPIDRRO_EL0, TPIDR_EL1, SP_EL0,
  e MRS di TCR_EL1, DCZID_EL0, CurrentEL, con i permessi dell'EL del blocco.
  Restano all'interprete WFI, LDTR/STTR, le manutenzioni delle cache a EL0 e
  ogni altro registro di sistema. Con SP come base e SP non allineato il
  blocco esce e decide l'interprete (SCTLR_EL1.SA/SA0).
- Un blocco si traduce dopo 16 ingressi con l'interprete; i blocchi caldi si
  compilano a gruppi di 16 in un modulo, o prima se i blocchi in attesa
  vengono richiesti 16 volte (un ciclo caldo non aspetta).

### Invalidazione
- Ogni pagina fisica con blocchi è sorvegliata. `vetro_machine::Ram` tiene
  una bitmap delle pagine sorvegliate: ogni scrittura che passa da lì (CPU,
  DMA virtio via `GuestRam`, `load_linux`) segna la pagina sporca, e i suoi
  blocchi si scartano prima della corsa successiva del JIT. Per questo i byte
  della RAM non sono più pubblici.
- Uno store di un blocco su una pagina sorvegliata fa uscire il blocco
  subito dopo (`STOP`).

### Concatenamento
- I blocchi di tutti i moduli stanno in una tabella di funzioni (`env.tbl`,
  `TABLE_SIZE` voci) in cui li mette il motore (`Engine::place`). Un modulo
  generato, il **dispatcher**, passa da un blocco al successivo con
  `call_indirect` senza tornare all'host, finché la **cache dei salti**
  (8192 voci `{pc, ctx, slot|passi}` nella memoria condivisa) ha una voce per
  il nuovo `pc` con il contesto corrente e il blocco sta nel limite di passi.
- Una voce mancante la chiede all'host con `env.resolve` (una chiamata
  diretta da WebAssembly a WebAssembly anche in V8): se il blocco c'è già,
  l'host verifica la traduzione del fetch e scrive la voce.
- Il contesto è EL più un'epoca che cambia con i registri di traduzione
  (SCTLR, TCR, TTBR0/1, MAIR), con ogni TLBI (`Tlb::flushes`) e con ogni
  invalidazione di blocchi: dentro un'epoca la traduzione del fetch di una
  voce non può cambiare.
- Solo il dispatcher importa la tabella: V8 dà a ogni istanza che importa
  una tabella una sua tabella di dispatch grande quanto quella (con un'istanza
  per modulo di blocchi la memoria finiva in pochi secondi).

### TLB software
- Quattro tabelle (EL0/EL1 × lettura/scrittura) di 512 voci `{pagina
  virtuale, addend}` nella memoria condivisa. Un load o store allineato che
  trova la sua pagina accede direttamente a `va + addend` nella memoria del
  motore; altrimenti chiama `ld`/`st`.
- L'host riempie una voce solo dopo un accesso riuscito attraverso la MMU
  (stessi permessi, stessa pagina), solo per pagine di RAM che il motore
  raggiunge (`Engine::host_address`: nel browser la memoria dei blocchi è
  quella di vetro-wasm, che contiene la RAM del guest; wasmtime solo per la
  RAM che sta nella sua memoria, cioè nei test) e, per le scritture, solo per
  pagine senza blocchi (e le tabelle di scrittura si svuotano quando una
  pagina nuova prende blocchi). Si svuota con i registri di traduzione e con
  le TLBI, come il TLB della MMU.
- Solo accessi allineati: così la memoria Device (dove i disallineati fanno
  fault) non ha bisogno di un caso a parte.

### Differenza ammessa
Il TLB della MMU vede meno accessi che con l'interprete (niente fetch dentro
i blocchi, niente accessi dal percorso veloce). Il risultato può cambiare
solo per un guest che modifica le tabelle delle pagine senza TLBI, che
l'architettura lascia imprevedibile e Linux non fa (e che già distingue
Vetro da QEMU).

### Nel browser
- vetro-wasm esporta la sua tabella di funzioni (`--export-table`,
  `--growable-table`, da `build.rs`): il JS vi mette il dispatcher e Rust lo
  chiama come un puntatore a funzione, senza passare da JS a ogni corsa.
- `ld`/`st`/`resolve` sono export di vetro-wasm passati come import ai
  moduli: V8 li chiama direttamente.

### ABI (docs/specs/jit.md)
- `Host::ld/st` ricevono anche la memoria del motore (per la TLB software);
  nuovo `Host::resolve`.
- `Engine`: nuovi `place`, `reserve`, `host_address`.
- `JitState` cresce a 384 byte (contesto, limite, registri di sistema,
  monitor); dopo `JitState` c'è l'area della modalità sistema
  (`state::area`). `st` con `size` 64 è DC ZVA.
- Le uscite di un blocco vanno a una coda comune che riscrive tutti i
  registri che il blocco scrive (quelli non ancora scritti hanno il valore
  d'ingresso): il codice cresce in modo lineare invece che quadratico.

## Verifica
- Avvio del kernel guest (copione di `tests/boot`) col JIT su wasmtime
  (`VETRO_JIT=1`) e su V8 (`tools/wasm-boot.sh --jit`): stesse istruzioni e
  stesso log, byte per byte, dell'interprete.
- La sonda bare-metal di `tests/isa/system` col JIT: stessa uscita di QEMU e
  stesse istruzioni dell'interprete.
- `vetro-jit-native/tests/sys_parity.rs`: programmi bare-metal casuali con
  MMU, permessi e attributi diversi, EL0 ed EL1, eccezioni, esclusive,
  codice che si modifica da sé e TLB software attiva; eccezioni, passi, CPU e
  RAM identici all'interprete (un errore introdotto apposta nel percorso
  veloce o nel monitor viene trovato).
- kselftest del kernel guest col JIT: stesso log dell'interprete.

## Conseguenze
- La soglia di M4 si misura nel job `boot` della CI con `tools/wasm-boot.sh
  --jit`, che fallisce se il JIT in V8 è più lento dell'interprete nativo.
- `vetro boot --jit` e `VETRO_JIT=1` in `tests/boot` usano wasmtime; lì la
  compilazione di Cranelift domina e il JIT è più lento dell'interprete:
  serve alla parità, non alla velocità.
- La copertura cresce solo con test di parità, come in modalità utente.
