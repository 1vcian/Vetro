# JIT verso WASM: ABI e interfacce (ADR 0012, ADR 0013, ADR 0024)

## Regioni
L'unità di traduzione è la **regione** (ADR 0024): i blocchi base di una
pagina da 4 KiB raggiungibili da un ingresso `pc` con salti diretti (presi e
non presi, anche all'indietro), al più `MAX_REGION` = 64 istruzioni. Un
blocco base finisce con un salto (anche condizionato), una SVC, prima di
un'istruzione non tradotta, dell'inizio di un altro blocco, della fine della
pagina o dopo `MAX_BLOCK` = 64 istruzioni. I salti fra blocchi della regione
restano nella funzione; ogni altro salto (indiretto, fuori pagina, fuori
regione) è un'uscita con `NEXT`.

Ogni blocco base con almeno un'istruzione eseguita è un **ingresso** della
regione (indice < 64): chi chiama la funzione scrive l'indice in
`JitState::entry`. Un `pc` che è un blocco base di una regione già compilata
usa quella regione invece di tradurne un'altra.

## Moduli generati
Un modulo contiene una o più regioni. Importa `env.mem` (la memoria lineare
con `JitState`) e le funzioni del **runtime** `rt.<nome>` (tabella sotto),
tutte e nello stesso ordine. Esporta `b<N>: (state: i32) -> i32` per ogni
regione `N`. Il risultato:

| Codice | Significato |
|---|---|
| 0 `NEXT` | regione finita, `pc` è la prossima istruzione |
| 1 `FAULT` | un accesso è fallito (o, in modalità sistema, deve farlo l'interprete: MMIO, SP non allineato, esclusiva non allineata, Q a cavallo di pagina...): `pc` e `steps` sono quelli dell'istruzione, i registri come dopo le istruzioni precedenti; il dettaglio lo tiene l'host |
| 2 `STOP` | fermati dopo l'istruzione corrente (scrittura su codice sorvegliato): `pc` è la successiva |
| 3 `SVC` | la regione finisce con SVC: `pc` punta all'istruzione (l'host la esegue con l'interprete). BRK e HVC chiudono il blocco *prima* di sé con `NEXT` |
| 4 `YIELD` | (modalità sistema) MSR DAIF/DAIFClr ha smascherato interrupt: `pc` è l'istruzione successiva, l'host ricontrolla gli interrupt prima di continuare |

Ogni blocco base, prima di iniziare, controlla che `steps + passi del blocco
<= limit`, altrimenti esce con `NEXT` al suo inizio: il numero di istruzioni
resta esatto anche nei cicli dentro la regione.

### Il runtime (`translate::runtime`)
Un modulo compilato una volta per motore (`Engine::runtime`): importa
`env.mem`, `env.ld`, `env.st`, `env.vsync` ed esporta:

| Funzione | Tipo | Significato |
|---|---|---|
| `save` | `(state, pc0: i64, steps: i64, packed: i32)` | `pc = pc0 + (packed & 0xfff)`, `steps = steps + (packed >> 12)` in `JitState` |
| `ld_slow`, `st_slow` | `(state, va, size, [valore,] pc0, steps, packed) -> (valore, fault)` / `-> esito` | `save`, poi `env.ld`/`env.st` |
| `nzcv` | `(k, a, b, r, vecchio) -> i32` | NZCV dei flag pigri (`state::lazy_nzcv`) |
| `ld<el>_<n>`, `st<el>_<n>` | come sopra, senza `size` | TLB software dell'EL per gli accessi allineati, poi quella per i non allineati, poi l'host |
| `ldp<el>_<n>`, `stp<el>_<n>`, `ldp_slow`, `stp_slow` | coppie a `va` e `va + n` | il secondo accesso non si fa se il primo fallisce; un load in coppia non restituisce nulla se uno dei due fallisce |
| `ldq<el>`, `stq<el>`, `ldq_slow`, `stq_slow` | accessi Q (16 byte) come due da 8 | a cavallo di pagina: `FAULT` senza scrivere nulla; allineati a 8 ma non a 16 (modalità sistema): metà con `SIZE_PART_OF_MISALIGNED` |
| `ldu<el>`, `stu<el>` | metà da 8 byte di un Q non allineato a 16 | TLB dei non allineati, poi l'host |
| `finish` | `(state, codice, pc, steps) -> codice` | fine di una regione con un codice diverso da `NEXT` |
| `vsync` | `(state)` | se `v_valid` = 0, `env.vsync(state)` |

I percorsi lenti salvano `pc` e `steps` dell'istruzione prima di chiamare
l'host (la spec li vuole salvati durante `ld`/`st`): per `FAULT` la regione
esce senza riscriverli.

### Import dell'host
| Import | Tipo | Significato |
|---|---|---|
| `env.ld` | `(state: i32, va: i64, size: i32) -> i64` | lettura di 1/2/4/8 byte, estesa a zero; in caso di fault scrive 1 in `exit_detail` e restituisce 0 |
| `env.st` | `(state: i32, va: i64, size: i32, value: i64) -> i32` | scrittura; 0, oppure 1 se la regione deve fermarsi: `exit_detail` = 1 per un fault, 2 per una scrittura su una pagina con blocchi (STOP). `size` = 64 (modalità sistema) è DC ZVA: azzera i 64 byte allineati a `va` |
| `env.vsync` | `(state: i32)` | copia V0..V31 della `Cpu` in `JitState::v` e mette `v_valid` = 1 |

`size` con il bit `SIZE_PART_OF_MISALIGNED` (0x80): metà di un accesso da 16
byte non allineato a 16; l'host la tratta come non allineata (SCTLR_EL1.A,
memoria Device), come l'interprete tratta l'accesso intero.

### Il dispatcher (modalità sistema)
Un modulo a parte importa `env.mem`, `env.tbl` (tabella `funcref` di
`TABLE_SIZE` = 2¹⁸ voci) ed `env.resolve: (state: i32) -> i32`, ed esporta
`b0: (state: i32) -> i32`. In ciclo: cerca `pc` nella cache dei salti
(`area::JC`, indice `(pc >> 2) & 8191`); se la voce è di un altro `pc` o di un
altro `ctx` chiama `env.resolve` (1 = l'host ha scritto la voce, 0 = torna
con `NEXT`); se `steps + passi massimi dell'ingresso > limit` torna con
`NEXT`; altrimenti scrive l'ingresso in `JitState::entry` e chiama la regione
(`call_indirect` sulla voce della tabella) e continua finché la regione
restituisce `NEXT`. Restituisce il codice d'uscita dell'ultima regione.

Le regioni non importano la tabella: ce le mette il motore
(`Engine::place`). V8 dà a ogni istanza che importa una tabella una sua
tabella di dispatch grande quanto quella.

## `JitState`
Struttura `#[repr(C)]` in `vetro_jit::state`, a un indirizzo allineato a 16
byte scelto dall'host (`state` è l'indirizzo assoluto nella memoria `env.mem`):

| Offset | Campo | Tipo |
|---|---|---|
| 0 | `x[0..31]` | 31 × u64 |
| 248 | `sp` | u64 |
| 256 | `pc` | u64 |
| 264 | `steps` | u64: istruzioni eseguite, aggiornato come nell'interprete |
| 272 | `nzcv` | u32, bit 31:28 come `Cpu::nzcv` (vale se `fk` = 0) |
| 276 | `exit_detail` | u32: 0 all'ingresso (lo azzera l'host), 1 fault, 2 STOP |
| 280 | `el` | u32, livello di eccezione (0 in modalità utente) |
| 284 | `ctx` | u32: contesto della cache dei salti (modalità sistema) |
| 288 | `limit` | u64: passi massimi della corsa |
| 296 | `tpidr_el0` | u64 |
| 304 | `tpidrro_el0` | u64 |
| 312 | `tpidr_el1` | u64 |
| 320 | `sp_el0` | u64: SP_EL0 quando non è lo SP in uso (EL1, SPSel = 1) |
| 328 | `tcr` | u64: TCR_EL1 (solo lettura) |
| 336 | `dczid` | u64: DCZID_EL0 per l'EL corrente (solo lettura) |
| 344 | `mon_addr` | u64: monitor esclusivo, indirizzo |
| 352 | `mon_lo`, `mon_hi` | 2 × u64: valore letto (128 bit) |
| 368 | `mon_valid` | u32: 1 se il monitor è attivo |
| 372 | `mon_bytes` | u32: byte dell'accesso esclusivo |
| 376 | `entry` | u32: blocco base d'ingresso della regione chiamata |
| 380 | `daif` | u32: PSTATE.DAIF (bit 9:6) |
| 384 | `elr_el1`, `spsr_el1` | 2 × u64 (MRS/MSR a EL1) |
| 400 | `esr_el1`, `far_el1` | 2 × u64 (solo MRS a EL1) |
| 416 | `v_valid` | u32: 1 se `v` ha i registri della `Cpu` |
| 420 | `fk` | u32: tipo dei flag pigri (0 = NZCV in `nzcv`) |
| 424 | — | riempimento |
| 432 | `v[0..32]` | 32 × 16 byte: V0..V31 (metà bassa, poi alta) |
| 944 | `fa`, `fb`, `fr` | 3 × u64: operandi e risultato dei flag pigri |
| 968 | — | riempimento fino a 976 |

I campi da 284 in poi (tranne `entry`, `v_valid`, `fk`, `v`, `fa`, `fb`,
`fr`, usati anche in modalità utente) servono alla modalità sistema. Prima
di una corsa l'host copia in `JitState` i campi della `Cpu` (`from_cpu`,
`from_cpu_sys`: `v_valid` = 0 e `fk` = 0, i registri V non si copiano), e
dopo li ricopia indietro (`to_cpu`, `to_cpu_sys`: V solo se `v_valid`, NZCV
calcolato dai flag pigri con `state::lazy_nzcv`). Per le regioni concatenate
resta valida la copia in `JitState`.

**Flag pigri.** Un'istruzione che scrive NZCV (ADDS/SUBS/CMP/CMN, ANDS/TST)
lascia il tipo in `fk` (1 somma 64, 2 differenza 64, 3 somma 32, 4
differenza 32, 5 logica 64, 6 logica 32), gli operandi in `fa`, `fb` e il
risultato in `fr` (troncati a 32 bit per i tipi a 32). I salti condizionati
e CSEL/CCMP con i flag di tipo noto nel blocco base calcolano la condizione
dagli operandi; altrimenti `rt.nzcv`. Le regioni passano i flag pigri alla
successiva così come sono.

### Area della modalità sistema (`vetro_jit::state::area`)
Offset dall'inizio di `JitState`:

| Offset | Contenuto |
|---|---|
| 1024 | cache dei salti: 8192 voci da 16 byte `{pc: u64, ctx: u32, w: u32}`, `w = ingresso << 26 \| slot << 8 \| passi massimi dell'ingresso` |
| 132096 | TLB software degli accessi allineati: 4 tabelle (EL0 lettura, EL0 scrittura, EL1 lettura, EL1 scrittura) di 512 voci da 16 byte `{tag: u64, addend: u64}`, indice `(va >> 12) & 511` |
| 164864 | TLB degli accessi non allineati: 4 tabelle come sopra (`area::tlb_u`) |

Un accesso allineato di `n` byte a `va` usa la voce se `tag == va & (!0xfff
| (n - 1))`; uno non allineato usa la TLB dei non allineati se `tag == va &
!0xfff` e non sconfina nella pagina successiva. L'indirizzo nella memoria
del motore è `(va + addend) mod 2³²`. `tag = 0x800` è una voce vuota. Una
voce della TLB dei non allineati c'è solo dopo un accesso non allineato
riuscito (memoria Normal, SCTLR_EL1.A = 0). Area totale: 197632 byte.

`ctx` = epoca << 5 | parametri della regione (EL, TBI0, TBI1, SPSel, FP):
una voce della cache dei salti vale solo per gli stessi parametri.

## Trait
```rust
pub trait Engine {
    type Module;
    /// Installa il modulo di runtime: i suoi export diventano gli import
    /// `rt.*` dei moduli compilati dopo (anche dopo `reset`).
    fn runtime(&mut self, wasm: &[u8]) -> Result<(), String>;
    /// Compila un modulo WASM generato dal traduttore.
    fn compile(&mut self, wasm: &[u8]) -> Result<Self::Module, String>;
    /// Esegue la regione `index` del modulo sullo stato all'indirizzo
    /// `state` della memoria condivisa; `ld`/`st`/`resolve`/`vsync`
    /// chiamano `host`.
    fn run(&mut self, m: &Self::Module, index: u32, state: u32, host: &mut dyn Host) -> u32;
    /// La memoria condivisa (dove sta `JitState`).
    fn memory(&mut self) -> &mut [u8];
    /// Mette `b0..b<count-1>` del modulo nelle voci `base..` di `env.tbl`.
    fn place(&mut self, m: &Self::Module, count: u32, base: u32);
    /// Libera tutti i moduli (e la tabella; il runtime resta).
    fn reset(&mut self) {}
    /// Almeno `bytes` byte in `memory()`. Default: controlla.
    fn reserve(&mut self, bytes: usize);
    /// Indirizzo in `env.mem` di `len` byte dell'host da `p`, se i blocchi li
    /// raggiungono (browser: sempre; wasmtime: se stanno nella sua memoria).
    fn host_address(&mut self, p: *const u8, len: usize) -> Option<u32> { None }
}

pub trait Host {
    fn ld(&mut self, mem: &mut [u8], va: u64, size: u32) -> Result<u64, ()>;
    /// Ok(true) = fermati dopo questa istruzione.
    fn st(&mut self, mem: &mut [u8], va: u64, size: u32, value: u64) -> Result<bool, ()>;
    /// `env.resolve`: vero se l'host ha scritto la voce della cache dei
    /// salti per il `pc` di `JitState`. Default falso.
    fn resolve(&mut self, mem: &mut [u8]) -> bool { false }
    /// `env.vsync`: V0..V31 della `Cpu` nel `JitState` a `state`.
    fn vsync(&mut self, mem: &mut [u8], state: u32);
}
```
`mem` è la memoria del motore (`Engine::memory`): l'host vi scrive la TLB
software, la cache dei salti e i registri V.

### Modalità sistema (`vetro_jit::sys`)
```rust
pub trait SysPhys: PhysMemory {
    fn ram_read(&mut self, pa: u64, buf: &mut [u8]) -> bool;          // solo RAM, senza effetti
    fn ram_write(&mut self, pa: u64, data: &[u8]) -> Option<bool>;    // Some(true): pagina sorvegliata toccata
    fn watch_code(&mut self, page: u64) -> bool;
    fn is_watched(&self, page: u64) -> bool;
    fn take_code_dirty(&mut self, out: &mut Vec<u64>);
    fn ram_region(&mut self) -> Option<(u64, *mut u8, usize)> { None }
}

impl<E: Engine> SysJit<E> {
    pub fn new(engine: E, cfg: SysJitConfig) -> Self;
    /// Solo regioni tradotte, al più `budget` passi.
    pub fn run(&mut self, cpu: &mut Cpu, mmu: &mut Mmu, phys: &mut dyn SysPhys, budget: u64) -> SysRun;
}
pub struct SysRun { pub steps: u64, pub next: Next }
pub enum Next { Jit, One, Cold }   // dopo: JIT, un passo dell'interprete, interprete fino al prossimo salto
```
Il chiamante (`Machine::run`) non chiama `run` quando l'interprete
prenderebbe un interrupt, con PSTATE.IL o con PC non allineato, e non
concede più passi di quelli fino al prossimo evento della piattaforma. Dopo
`YIELD` `run` torna con `Next::Jit`: il chiamante ricontrolla gli interrupt.
`SysJitDyn` è lo stesso come oggetto (per `Machine::set_jit`). Soglia di
default: 64 ingressi prima di tradurre.

## Copertura
Si traducono:
- aritmetica e logica (immediata, registro, estesa, con carry, coi flag),
  MOVZ/MOVN/MOVK, ADR/ADRP, bitfield, EXTR, CCMP/CCMN, CSEL e varianti,
  RBIT/REV/CLZ/CLS, divisioni e shift variabili, moltiplicazioni, CRC32;
- MRS/MSR NZCV; tutti i salti;
- LDR/STR interi (immediato, pre/post, registro, letterale), LDP/STP/LDPSW,
  LDAR/STLR;
- SIMD (ADR 0024): LDR/STR di registri B/H/S/D/Q (immediato, pre/post,
  registro), LDP/STP di S/D/Q, DUP (elemento e generale), INS, UMOV/SMOV,
  MOVI/MVNI/ORR/BIC immediati (in modalità sistema solo con CPACR_EL1.FPEN
  che le permette all'EL);
- solo in modalità sistema: LDXR/STXR e varianti (anche a coppie), DC ZVA,
  MRS/MSR di TPIDR_EL0, TPIDRRO_EL0 (MSR solo a EL1), TPIDR_EL1 e SP_EL0
  (a EL1; SP_EL0 con SPSel = 1), MRS di TCR_EL1, DCZID_EL0, CurrentEL (a
  EL1); a EL1 anche MRS/MSR di DAIF, ELR_EL1, SPSR_EL1, MRS di ESR_EL1 e
  FAR_EL1, MSR DAIFSet/DAIFClr.

Restano all'interprete: le altre istruzioni SIMD/FP, gli altri registri di
sistema, SVC/BRK/HVC, ERET, e in modalità sistema anche WFI, LDTR/STTR e le
manutenzioni delle cache a EL0. La copertura cresce solo con test di parità.

## Note dall'implementazione
- **Memoria importata.** `env.mem` si dichiara secondo la configurazione
  (`MemoryImport`): nel browser con i thread serve una memoria condivisa
  (`shared`).
- **Sorveglianza del codice in modalità utente.** Sta in `UserMemory`:
  `space_id` distingue gli spazi d'indirizzamento, `watch_code(page)` segna
  le pagine tradotte, `take_code_dirty()` restituisce quelle scritte da store,
  `poke`, mmap, munmap, mprotect, mremap e dalla crescita dello stack.
  Dettagli in `crates/vetro-jit/src/driver.rs`. Le mappature sono a pagine
  intere (come Linux): le due metà di un accesso Q nella stessa pagina hanno
  gli stessi permessi.
- **Sorveglianza in modalità sistema.** Sta in `vetro_machine::Ram` (bitmap
  per pagina fisica): ogni scrittura fisica passa da lì.
- **Coda comune.** Tutte le uscite di una regione saltano a una coda che
  riscrive i registri scritti dalla regione (caricati all'ingresso anche se
  non letti), i flag pigri, `pc`, `steps` e il codice.
- **Nel browser** il dispatcher si chiama da Rust come un puntatore a
  funzione nella tabella di vetro-wasm (`docs/specs/wasm.md`).
