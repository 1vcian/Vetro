# JIT verso WASM: ABI e interfacce (ADR 0012, ADR 0013)

## Moduli generati
Un modulo contiene uno o più blocchi. Importa:

| Import | Tipo | Significato |
|---|---|---|
| `env.mem` | memoria | la memoria lineare che contiene `JitState` (condivisa con l'host) |
| `env.ld` | `(state: i32, va: i64, size: i32) -> i64` | lettura di 1/2/4/8 byte, estesa a zero; in caso di fault scrive 1 in `exit_detail` e restituisce 0: il blocco controlla `exit_detail` dopo ogni `ld` |
| `env.st` | `(state: i32, va: i64, size: i32, value: i64) -> i32` | scrittura; restituisce 0, oppure 1 se il blocco deve fermarsi: `exit_detail` = 1 per un fault, 2 per una scrittura su una pagina con blocchi (STOP). `size` = 64 (modalità sistema) è DC ZVA: azzera i 64 byte allineati a `va` |

Esporta `b<N>: (state: i32) -> i32` per ogni blocco `N`. Il risultato:

| Codice | Significato |
|---|---|
| 0 `NEXT` | blocco finito, `pc` è la prossima istruzione |
| 1 `FAULT` | un accesso è fallito (o, in modalità sistema, deve farlo l'interprete: MMIO, SP non allineato, esclusiva non allineata...): `pc` e `steps` sono quelli dell'istruzione, i registri come dopo le istruzioni precedenti; il dettaglio lo tiene l'host |
| 2 `STOP` | fermati dopo l'istruzione corrente (scrittura su codice sorvegliato): `pc` è la successiva |
| 3 `SVC` | il blocco finisce con SVC: `pc` punta all'istruzione (l'host la esegue con l'interprete). BRK e HVC chiudono il blocco *prima* di sé con `NEXT` |

I salti condizionati in mezzo a un blocco escono con `NEXT` solo se presi
(uscita laterale); quelli in fondo escono comunque.

### Il dispatcher (modalità sistema)
Un modulo a parte importa `env.mem`, `env.tbl` (tabella `funcref` di
`TABLE_SIZE` = 2¹⁸ voci) ed `env.resolve: (state: i32) -> i32`, ed esporta
`b0: (state: i32) -> i32`. In ciclo: cerca `pc` nella cache dei salti
(`area::JC`, indice `(pc >> 2) & 8191`); se la voce è di un altro `pc` o di un
altro `ctx` chiama `env.resolve` (1 = l'host ha scritto la voce, 0 = torna
con `NEXT`); se `steps + passi massimi del blocco > limit` torna con `NEXT`;
altrimenti chiama il blocco (`call_indirect` sulla voce della tabella) e
continua finché il blocco restituisce `NEXT`. Restituisce il codice
d'uscita dell'ultimo blocco.

I blocchi non importano la tabella: ce li mette il motore
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
| 272 | `nzcv` | u32, bit 31:28 come `Cpu::nzcv` |
| 276 | `exit_detail` | u32: 0 all'ingresso del blocco (lo azzera l'host), 1 fault, 2 STOP |
| 280 | `el` | u32, livello di eccezione (0 in modalità utente) |
| 284 | `ctx` | u32: contesto della cache dei salti (modalità sistema) |
| 288 | `limit` | u64: passi massimi della corsa del dispatcher |
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
| 376 | — | riempimento fino a 384 |

I campi da 284 in poi servono alla modalità sistema. I registri SIMD/FP, lo
stato di sistema non elencato e (in modalità utente) il monitor restano nella
`Cpu`: le istruzioni che li usano non si traducono. Prima di una corsa l'host
copia in `JitState` i campi della `Cpu` (`from_cpu`, `from_cpu_sys`), e dopo
li ricopia indietro (`to_cpu`, `to_cpu_sys`). Per i blocchi concatenati
resta valida la copia in `JitState`.

### Area della modalità sistema (`vetro_jit::state::area`)
Offset dall'inizio di `JitState`:

| Offset | Contenuto |
|---|---|
| 512 | cache dei salti: 8192 voci da 16 byte `{pc: u64, ctx: u32, w: u32}`, `w = slot << 8 \| passi massimi` |
| 131584 | TLB software: 4 tabelle (EL0 lettura, EL0 scrittura, EL1 lettura, EL1 scrittura) di 512 voci da 16 byte `{tag: u64, addend: u64}`, indice `(va >> 12) & 511` |

Un accesso allineato di `n` byte a `va` usa la voce se `tag == va & (!0xfff
| (n - 1))`, cioè la pagina coincide e l'accesso è allineato; l'indirizzo
nella memoria del motore è `(va + addend) mod 2³²`. `tag = 0x800` è una voce
vuota. Area totale: 164352 byte.

## Trait
```rust
pub trait Engine {
    type Module;
    /// Compila un modulo WASM generato dal traduttore.
    fn compile(&mut self, wasm: &[u8]) -> Result<Self::Module, String>;
    /// Esegue il blocco `index` del modulo sullo stato all'indirizzo
    /// `state` della memoria condivisa; `ld`/`st`/`resolve` chiamano `host`.
    fn run(&mut self, m: &Self::Module, index: u32, state: u32, host: &mut dyn Host) -> u32;
    /// La memoria condivisa (dove sta `JitState`).
    fn memory(&mut self) -> &mut [u8];
    /// Mette `b0..b<count-1>` del modulo nelle voci `base..` di `env.tbl`.
    fn place(&mut self, m: &Self::Module, count: u32, base: u32);
    /// Libera tutti i moduli (e la tabella; wasmtime tiene al più 10000
    /// istanze per store): il driver lo chiama quando `compile` fallisce o la
    /// tabella è piena. Default vuoto.
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
}
```
`mem` è la memoria del motore (`Engine::memory`): l'host vi scrive la TLB
software e la cache dei salti.

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
    /// Solo blocchi tradotti, al più `budget` passi.
    pub fn run(&mut self, cpu: &mut Cpu, mmu: &mut Mmu, phys: &mut dyn SysPhys, budget: u64) -> SysRun;
}
pub struct SysRun { pub steps: u64, pub next: Next }
pub enum Next { Jit, One, Cold }   // dopo: JIT, un passo dell'interprete, interprete fino al prossimo salto
```
Il chiamante (`Machine::run`) non chiama `run` quando l'interprete
prenderebbe un interrupt, con PSTATE.IL o con PC non allineato, e non
concede più passi di quelli fino al prossimo evento della piattaforma.
`SysJitDyn` è lo stesso come oggetto (per `Machine::set_jit`).

## Copertura
Si traducono:
- aritmetica e logica (immediata, registro, estesa, con carry, coi flag),
  MOVZ/MOVN/MOVK, ADR/ADRP, bitfield, EXTR, CCMP/CCMN, CSEL e varianti,
  RBIT/REV/CLZ/CLS, divisioni e shift variabili, moltiplicazioni, CRC32;
- MRS/MSR NZCV; tutti i salti;
- LDR/STR interi (immediato, pre/post, registro, letterale), LDP/STP/LDPSW,
  LDAR/STLR;
- solo in modalità sistema: LDXR/STXR e varianti (anche a coppie), DC ZVA,
  MRS/MSR di TPIDR_EL0, TPIDRRO_EL0 (MSR solo a EL1), TPIDR_EL1 e SP_EL0
  (a EL1; SP_EL0 con SPSel = 1), MRS di TCR_EL1, DCZID_EL0, CurrentEL (a
  EL1).

Restano all'interprete: SIMD/FP, gli altri registri di sistema, SVC/BRK/HVC,
e in modalità sistema anche WFI, LDTR/STTR e le manutenzioni delle cache a
EL0. La copertura cresce solo con test di parità.

## Note dall'implementazione
- **Memoria importata.** `env.mem` si dichiara secondo la configurazione
  (`MemoryImport`): nel browser con i thread serve una memoria condivisa
  (`shared`).
- **Sorveglianza del codice in modalità utente.** Sta in `UserMemory`:
  `space_id` distingue gli spazi d'indirizzamento, `watch_code(page)` segna
  le pagine tradotte, `take_code_dirty()` restituisce quelle scritte da store,
  `poke`, mmap, munmap, mprotect, mremap e dalla crescita dello stack.
  Dettagli in `crates/vetro-jit/src/driver.rs`.
- **Sorveglianza in modalità sistema.** Sta in `vetro_machine::Ram` (bitmap
  per pagina fisica): ogni scrittura fisica passa da lì.
- **Coda comune.** Tutte le uscite di un blocco saltano a una coda che
  riscrive i registri scritti dal blocco (caricati all'ingresso anche se non
  letti), `pc`, `steps` e il codice.
- **Nel browser** il dispatcher si chiama da Rust come un puntatore a
  funzione nella tabella di vetro-wasm (`docs/specs/wasm.md`).
