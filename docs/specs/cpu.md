# Spec — vetro-cpu

## Perimetro
AArch64, ARMv8.0-A (ADR 0005). M1: istruzioni intere a EL0 (dati, salti,
load/store, esclusive, alcune di sistema). SIMD/FP e EL1 arrivano dopo.

## Interfaccia pubblica
- `Cpu`: stato architetturale. `x[0..31]`, `sp` (SP_EL0), `pc`, `nzcv`
  (bit 31:28), `tpidr_el0`, `tpidrro_el0`, monitor esclusivo locale.
- `decode(u32) -> Insn`: funzione pura, senza stato. `Insn` è la forma
  decodificata che userà anche il JIT (M4).
- `Cpu::step(&mut self, &mut impl Memory) -> Result<(), Exception>`:
  esegue un'istruzione. In caso di eccezione lo stato non cambia e `pc`
  punta all'istruzione colpevole, tranne per `Svc`, dove `pc` punta già
  all'istruzione successiva (come ELR per SVC).
- `trait Memory`: `read`, `write`, `fetch` su indirizzi virtuali, con errore
  `MemFault`. In M1 lo implementa `UserMemory` (regioni con permessi); da
  M3 lo implementa la MMU (`vetro-mmu`).

## Eccezioni
| `Exception` | Segnale Linux in user mode |
|---|---|
| `Svc(imm)` | syscall |
| `Breakpoint(imm)` | SIGTRAP |
| `Undefined(raw)` | SIGILL |
| `Unimplemented { raw, what }` | SIGILL, ma è un nostro limite: va segnalato |
| `DataAbort { addr, write }`, `InstructionAbort { addr }` | SIGSEGV |
| `Alignment { addr }`, `PcAlignment { addr }` | SIGBUS |

## Invarianti
- Nessuna dipendenza da `std::fs`, `std::process`, thread: compila in
  `wasm32-unknown-unknown`.
- Comportamenti CONSTRAINED UNPREDICTABLE: si replica la scelta di QEMU
  quando è economico, e i generatori casuali li escludono.
- Esclusive: il monitor ricorda indirizzo, dimensione e valore letto; STXR
  riesce se indirizzo e dimensione coincidono e la memoria contiene ancora
  quel valore (stesso modello di QEMU user mode).

## Test
- `tests/isa`: casi per istruzione con valori attesi, verificati anche
  contro QEMU quando l'oracolo è presente.
- `tests/diff`: programmi casuali confrontati con QEMU (ADR 0006).
