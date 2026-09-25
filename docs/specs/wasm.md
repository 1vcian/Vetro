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

Versione: `vetro_abi_version() -> u32`, oggi **1**. Cambia a ogni modifica
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
| `vetro_jit_selftest` | `(wasm: *const u8, len: usize) -> u64` | prova del giro completo con un modulo di prova (sotto) |

## Import

Il JS li fornisce all'istanziazione (`web/node/vetro.mjs`):

| Import | Firma | Significato |
|---|---|---|
| `vetro_host.panic` | `(ptr: *const u8, len: usize)` | messaggio UTF-8 di un panic, subito prima della trappola `unreachable` |
| `vetro_jit.compile` | `(ptr: *const u8, len: usize) -> i32` | compila e istanzia un modulo generato; indice ≥ 0, o < 0 se rifiutato |
| `vetro_jit.run` | `(module: i32, index: u32, state: usize) -> u32` | chiama l'export `b<index>(state)` del modulo; restituisce il codice d'uscita del blocco |
| `vetro_jit.drop` | `(module: i32)` | libera il modulo |

## Il motore JIT in JavaScript

ADR 0012: nel browser il codice generato lo compila ed esegue l'API
`WebAssembly` di JS. I pezzi:

- `web/node/jit-engine.mjs`, classe `JitEngine`, l'equivalente JS del trait
  `vetro_jit::Engine` di `jit.md`:
  - `compile(bytes)`: `new WebAssembly.Module(bytes)` e subito
    `new WebAssembly.Instance(module, { env: { mem, ld, st } })`, con
    `env.mem` = `memory` di vetro-wasm e `env.ld`/`env.st` =
    `vetro_jit_ld`/`vetro_jit_st`. Un export di un'istanza passato come import
    di un'altra è chiamato da V8 direttamente, senza passare dal JS;
  - `run(id, index, state)`: il codice d'uscita di `b<index>(state)`;
  - la memoria condivisa è la memoria lineare di vetro-wasm;
  - `imports()`: gli import `vetro_jit.*` qui sopra; `attach(exports)` dopo
    l'istanziazione.
- `crates/vetro-wasm/src/jit.rs`, lato Rust:
  - `JsEngine` ha la forma del trait `Engine`: `compile(&[u8]) ->
    Result<JsModule, String>`, `run(&JsModule, index, state, &mut dyn Host) ->
    u32`, `memory() -> &mut [u8]`. La memoria condivisa è un buffer di 4 KiB
    allineato a 16 dentro vetro-wasm: `state` è un offset in quel buffer, e al
    blocco arriva l'indirizzo assoluto (buffer + `state`), perché per il
    blocco `env.mem` è l'intera memoria lineare;
  - durante `run` l'`Host` è raggiungibile da `vetro_jit_ld`/`vetro_jit_st`
    (una cella per thread, impostata e ripristinata da `run`, quindi anche
    rientrante);
  - fault: `vetro_jit_ld` scrive `FAULT` (1) in `exit_detail` (offset 276) e
    restituisce 0; `vetro_jit_st` fa lo stesso e restituisce 1. Il blocco
    generato controlla `exit_detail` dopo ogni `ld` (è la lettura di questa
    spec di "scrive `exit = FAULT`" in `jit.md`: da confermare con il
    traduttore);
  - `Host` è per ora una copia locale di `vetro_jit::Host`. Quando
    `vetro-jit` esporrà `Engine` e `Host`, `vetro-wasm` ne dipenderà:
    `impl vetro_jit::Engine for JsEngine` e `Host` diventa quello di
    `vetro-jit`.
- `vetro_jit_selftest` e `web/node/jit-selftest.mjs`: il JS codifica un
  modulo con un blocco `b0` che fa `x2 = ld(x0) + x1; st(x0 + 8, x2);
  pc += 12; steps += 3`; Rust lo compila con `JsEngine`, lo esegue su un
  `JitState` con `x0 = 0x1000`, `x1 = 5` sopra una RAM di prova che contiene
  37, e restituisce il valore scritto (42). Prova il giro Rust → JS → modulo
  generato → `ld`/`st` in Rust.

Resta da fare, quando il traduttore c'è: il ciclo di esecuzione con i blocchi
nella `Machine` (in `vetro-machine` o `vetro-jit`, non qui), la cache dei
moduli per pagina e l'invalidazione (ADR 0012), e il raggruppamento di più
blocchi in un modulo per ammortizzare `new WebAssembly.Module`.

## Node

- `web/node/vetro.mjs`: `instantiate(bytes)` e la classe `Machine`, che avvolge
  l'API (buffer, console, conteggi). Non usa API di Node.
- `web/node/boot.mjs`: il copione di `tests/boot/tests/vetro.rs` (marcatore di
  `/init`, autotest ok, `echo VETRO-SHELL-$((6*7))` a prompt completo,
  `poweroff -f` fino a `PowerOff`), con i tempi reali, `--expect-steps N` e il
  log in `target/guest-kernel/node-boot.log`.
- `tools/wasm-boot.sh`: costruisce il .wasm, esegue `jit-selftest.mjs`,
  l'avvio nativo e l'avvio in Node; istruzioni e log devono coincidere.
  Gira nel job `boot` della CI (Node 22).
