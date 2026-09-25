# JIT verso WASM: ABI e interfacce (ADR 0012)

## Moduli generati
Un modulo contiene uno o più blocchi. Importa:

| Import | Tipo | Significato |
|---|---|---|
| `env.mem` | memoria | la memoria lineare che contiene `JitState` (condivisa con l'host) |
| `env.ld` | `(state: i32, va: i64, size: i32) -> i64` | lettura di 1/2/4/8 byte, estesa a zero; in caso di fault scrive 1 in `exit_detail` e restituisce 0: il blocco controlla `exit_detail` dopo ogni `ld` |
| `env.st` | `(state: i32, va: i64, size: i32, value: i64) -> i32` | scrittura; restituisce 0, oppure 1 se il blocco deve fermarsi: `exit_detail` = 1 per un fault, 2 per una scrittura su una pagina con blocchi (STOP) |

Esporta `b<N>: (state: i32) -> i32` per ogni blocco `N`. Il risultato:

| Codice | Significato |
|---|---|
| 0 `NEXT` | blocco finito, `pc` è la prossima istruzione |
| 1 `FAULT` | un accesso è fallito: `pc` e `steps` sono quelli dell'istruzione che ha fatto fault, i registri come dopo le istruzioni precedenti; il dettaglio (indirizzo, tipo) lo tiene l'host |
| 2 `STOP` | fermati dopo l'istruzione corrente (scrittura su codice sorvegliato): `pc` è la successiva |
| 3 `SVC` | il blocco finisce con SVC: `pc` punta all'istruzione (l'host la esegue con l'interprete). BRK e HVC chiudono il blocco *prima* di sé con `NEXT` |

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
| 284 | — | riempimento fino a 288 |

I registri SIMD/FP, TPIDR, il monitor esclusivo e lo stato di sistema restano
nella `Cpu`. Le istruzioni che li usano non si traducono e le esegue
l'interprete. Prima di ogni blocco l'host copia in `JitState` i campi della
`Cpu`, e dopo li ricopia indietro. Per i blocchi concatenati resta valida la
copia in `JitState`.

## Trait
```rust
pub trait Engine {
    type Module;
    /// Compila un modulo WASM generato dal traduttore.
    fn compile(&mut self, wasm: &[u8]) -> Result<Self::Module, String>;
    /// Esegue il blocco `index` del modulo sullo stato all'indirizzo
    /// `state` della memoria condivisa; `ld`/`st` chiamano `host`.
    fn run(&mut self, m: &Self::Module, index: u32, state: u32, host: &mut dyn Host) -> u32;
    /// La memoria condivisa (dove sta `JitState`).
    fn memory(&mut self) -> &mut [u8];
    /// Libera tutti i moduli (wasmtime tiene al più 10000 istanze per store):
    /// il driver lo chiama quando `compile` fallisce, poi riprova. Default vuoto.
    fn reset(&mut self) {}
}

pub trait Host {
    fn ld(&mut self, va: u64, size: u32) -> Result<u64, ()>;
    /// Ok(true) = fermati dopo questa istruzione.
    fn st(&mut self, va: u64, size: u32, value: u64) -> Result<bool, ()>;
}
```

## Copertura iniziale
Si traducono prima le istruzioni intere più frequenti:
- aritmetica e logica, immediata e con registro;
- spostamenti e bitfield;
- salti condizionati e incondizionati, CBZ/TBZ;
- load e store di interi (non esclusivi, non a coppie con writeback
  particolari).

Tutto il resto finisce il blocco e va all'interprete. La copertura cresce
solo con test di parità.

## Note dall'implementazione (M4, modalità utente)
- **Memoria importata.** `env.mem` si dichiara secondo `JitConfig.memory`: nel
  browser con i thread serve una memoria condivisa (`shared`).
- **LDTR/STTR.** In modalità sistema servono accessi non privilegiati: l'ABI
  di `ld`/`st` avrà un flag. Fino ad allora quelle istruzioni restano
  all'interprete.
- **Sorveglianza del codice.** In modalità utente sta in `UserMemory`:
  - `space_id` distingue gli spazi d'indirizzamento;
  - `watch_code(page)` segna le pagine tradotte;
  - `take_code_dirty()` restituisce quelle scritte da store, `poke`, mmap,
    munmap, mprotect, mremap e dalla crescita dello stack.

  Dettagli in `crates/vetro-jit/src/driver.rs`.
