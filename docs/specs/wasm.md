# `vetro-wasm`: la macchina in WebAssembly (M4)

`crates/vetro-wasm` compila `vetro_machine::Machine` per
`wasm32-unknown-unknown` e la espone a JavaScript con un'API C (`extern "C"`,
`#[unsafe(no_mangle)]`). Niente wasm-bindgen né altre dipendenze: al confine
passano solo interi e puntatori nella memoria lineare del modulo (export
`memory`). Lo stesso modulo gira in Node e nel browser.

Costruzione:

```sh
cargo build --release --target wasm32-unknown-unknown -p vetro-wasm
# -> target/wasm32-unknown-unknown/release/vetro_wasm.wasm
```

Tipi WASM: `usize` e i puntatori sono `i32` (in JS `number`), `u64` è `i64`
(in JS `BigInt`), `u32` è `i32` (in JS `number`; per i valori sopra 2³¹ usare
`>>> 0`).

## Export

Versione: `vetro_abi_version() -> u32`, oggi **2** (JIT della modalità
sistema: `vetro_machine_set_jit`, import `vetro_jit.entry/place/reset`). Cambia a ogni modifica
incompatibile delle firme o dei codici qui sotto. Il caricatore JS
(`web/node/vetro.mjs`) la controlla.

### Memoria

| Export | Firma | Significato |
|---|---|---|
| `vetro_alloc` | `(len: usize) -> *mut u8` | buffer di `len` byte allineato a 16; nullo se `len == 0` o se la memoria non basta |
| `vetro_free` | `(ptr: *mut u8, len: usize)` | libera un buffer di `vetro_alloc` con la stessa `len` |

Un'allocazione può far crescere la memoria (`memory.grow`): da quel momento il
vecchio `memory.buffer` è staccato, e ogni `Uint8Array` va ricreata. In
pratica: prendere la vista dopo ogni chiamata che può allocare.

### Macchina

| Export | Firma | Significato |
|---|---|---|
| `vetro_machine_new` | `(ram_size: u64, now_secs: u64, seed: u64) -> *mut Vm` | nuova macchina. 0 in un campo = valore di `MachineConfig::default` (1 GiB, ora e seme fissi dei test nativi) |
| `vetro_machine_free` | `(vm)` | la distrugge |
| `vetro_load_linux` | `(vm, image, image_len, initrd, initrd_len, cmdline, cmdline_len) -> u32` | come `Machine::load_linux`; `initrd` nullo o lungo 0 = nessuno; `cmdline` UTF-8. I buffer si possono liberare subito dopo |
| `vetro_run` | `(vm, budget: u64) -> u32` | esegue al più `budget` istruzioni (`Machine::run`) |
| `vetro_steps` | `(vm) -> u64` | istruzioni eseguite (l'orologio del guest, ADR 0011) |
| `vetro_guest_ns` | `(vm) -> u64` | tempo del guest in ns (10 ns per istruzione) |
| `vetro_console_read` | `(vm, dst: *mut u8, cap: usize) -> usize` | copia e consuma al più `cap` byte dell'uscita della PL011; 0 = niente di nuovo. Il resto resta per la chiamata successiva |
| `vetro_console_write` | `(vm, src: *const u8, len: usize)` | accoda byte in ingresso, come dalla tastiera |
| `vetro_message_ptr` / `vetro_message_len` | `(vm) -> *const u8` / `usize` | ultimo messaggio UTF-8: errore di caricamento o `what` di un'istruzione non implementata. Vale fino alla chiamata successiva sulla macchina |
| `vetro_unimplemented_pc` / `vetro_unimplemented_raw` | `(vm) -> u64` / `u32` | PC e codifica dell'ultima istruzione non implementata |
| `vetro_machine_set_jit` | `(vm, hot_threshold: u32, batch: u32)` | attiva il JIT della modalità sistema (ADR 0013) sul motore JS: ingressi prima di tradurre un blocco, blocchi per modulo (0 = 1). Il risultato non cambia, solo la velocità |
| `vetro_jit_stats` | `(vm, out: *mut u64, cap: usize) -> usize` | contatori del JIT (`SysJitStats`, nell'ordine dei campi) in `out`; restituisce quanti (0 senza JIT) |

Codici di `vetro_load_linux`: 0 riuscito; 1 il caricatore ha rifiutato i file
(motivo nel messaggio); 2 riga di comando non UTF-8.

Codici di `vetro_run` (`Stop` di `vetro-machine`):

| Codice | `Stop` |
|---|---|
| 0 | `Budget`: quanto esaurito, si può continuare |
| 1 | `PowerOff` |
| 2 | `Reset` |
| 3 | `Idle`: il guest aspetta un ingresso |
| 4 | `Unimplemented` (dettagli in `vetro_unimplemented_*` e nel messaggio) |

La macchina è deterministica: lo stesso kernel, initramfs, riga di comando e
ingresso (agli stessi numeri di istruzione) danno la stessa uscita e lo stesso
conteggio che in nativo. `tools/wasm-boot.sh` lo verifica.

### Ponte JIT

| Export | Firma | Significato |
|---|---|---|
| `vetro_jit_ld` | `(state: usize, va: u64, size: u32) -> u64` | `env.ld` dei moduli generati (spec `jit.md`) |
| `vetro_jit_st` | `(state: usize, va: u64, size: u32, value: u64) -> u32` | `env.st` dei moduli generati |
| `vetro_jit_resolve` | `(state: usize) -> u32` | `env.resolve` del dispatcher |
| `__indirect_function_table` | tabella | la tabella delle funzioni di vetro-wasm, esportata ed estendibile (`build.rs`): il JS vi mette il dispatcher, che Rust chiama come un puntatore a funzione |
| `vetro_jit_selftest` | `(wasm: *const u8, len: usize) -> u64` | prova del giro completo con un modulo di prova (sotto) |

## Import

Il JS li fornisce all'istanziazione (`web/node/vetro.mjs`):

| Import | Firma | Significato |
|---|---|---|
| `vetro_host.panic` | `(ptr: *const u8, len: usize)` | messaggio UTF-8 di un panic, subito prima della trappola `unreachable` |
| `vetro_jit.compile` | `(ptr: *const u8, len: usize) -> i32` | compila e istanzia un modulo generato; indice ≥ 0, o < 0 se rifiutato |
| `vetro_jit.entry` | `(module: i32, index: u32) -> u32` | mette l'export `b<index>` del modulo in una voce nuova di `__indirect_function_table` e la restituisce: `JsEngine::run` la chiama come un puntatore a funzione, senza passare da JS |
| `vetro_jit.place` | `(module: i32, count: u32, base: u32)` | mette `b0..b<count-1>` del modulo nella tabella dei blocchi (`env.tbl` del dispatcher) dalla voce `base` |
| `vetro_jit.reset` | `()` | scarta tutte le istanze e ricrea la tabella dei blocchi |
| `vetro_jit.drop` | `(module: i32)` | libera il modulo |

## Il motore JIT in JavaScript

ADR 0012: nel browser il codice generato lo compila ed esegue l'API
`WebAssembly` di JS. I pezzi:

- `web/node/jit-engine.mjs`, classe `JitEngine`, l'equivalente JS del trait
  `vetro_jit::Engine` di `jit.md`:
  - `compile(bytes)`: `new WebAssembly.Module(bytes)` e subito
    `new WebAssembly.Instance(module, { env: { mem, tbl, ld, st, resolve } })`,
    con `env.mem` = `memory` di vetro-wasm, `env.tbl` = la tabella dei
    blocchi (la importa solo il dispatcher) e `env.ld`/`env.st`/`env.resolve`
    = `vetro_jit_ld`/`vetro_jit_st`/`vetro_jit_resolve`. Un export di
    un'istanza passato come import di un'altra è chiamato da V8
    direttamente, senza passare dal JS;
  - `place`, `entry`, `reset` come gli import qui sopra;
  - la memoria condivisa è la memoria lineare di vetro-wasm;
  - `imports()`: gli import `vetro_jit.*`; `attach(exports)` dopo
    l'istanziazione.
- `crates/vetro-wasm/src/jit.rs`, lato Rust: `impl vetro_jit::Engine for
  JsEngine`.
  - La memoria condivisa è un buffer di 256 KiB allineato a 16 dentro
    vetro-wasm (`JitState` e l'area della modalità sistema): `state` è un
    offset in quel buffer, e al blocco arriva l'indirizzo assoluto (buffer +
    `state`), perché per il blocco `env.mem` è l'intera memoria lineare. Per
    lo stesso motivo `host_address` è l'indirizzo stesso: la TLB software dei
    blocchi punta direttamente alla RAM del guest.
  - `run` chiama la funzione attraverso `__indirect_function_table` (voce
    data da `vetro_jit.entry` e tenuta in cache): nessun passaggio da JS.
  - Durante `run` l'`Host` e la memoria condivisa sono raggiungibili da
    `vetro_jit_ld`/`vetro_jit_st`/`vetro_jit_resolve` (celle per thread,
    impostate e ripristinate da `run`, quindi anche rientranti).
  - Fault: `vetro_jit_ld` scrive `FAULT` (1) in `exit_detail` e restituisce
    0; `vetro_jit_st` scrive `FAULT` o `STOP` (2) e restituisce 1.
- `vetro_jit_selftest` e `web/node/jit-selftest.mjs`: il JS codifica un
  modulo con un blocco `b0` che fa `x2 = ld(x0) + x1; st(x0 + 8, x2);
  pc += 12; steps += 3`; Rust lo compila con `JsEngine`, lo esegue su un
  `JitState` con `x0 = 0x1000`, `x1 = 5` sopra una RAM di prova che contiene
  37, e restituisce il valore scritto (42). Prova il giro Rust → JS → modulo
  generato → `ld`/`st` in Rust.

Il ciclo di esecuzione con i blocchi sta in `vetro-machine` e in
`vetro_jit::sys` (ADR 0013).

## Node

- `web/node/vetro.mjs`: `instantiate(bytes)` e la classe `Machine`, che avvolge
  l'API (buffer, console, conteggi). Non usa API di Node.
- `web/node/boot.mjs`: il copione di `tests/boot/tests/vetro.rs` (marcatore di
  `/init`, autotest ok, `echo VETRO-SHELL-$((6*7))` a prompt completo,
  `poweroff -f` fino a `PowerOff`), con i tempi reali, `--expect-steps N` e il
  log in `target/guest-kernel/node-boot.log`. Con `--jit` (`--jit-threshold
  N`, `--jit-batch N`) gira col JIT, stampa i contatori e scrive
  `node-boot-jit.log`.
- `tools/wasm-boot.sh [--jit]`: costruisce il .wasm, esegue
  `jit-selftest.mjs`, l'avvio nativo e l'avvio in Node (e col JIT); istruzioni
  e log devono coincidere. Con `--jit` fallisce se il JIT in V8 è più lento
  dell'interprete nativo (soglia di M4). Gira nel job `boot` della CI
  (Node 22).
