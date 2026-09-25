# `vetro-wasm`: la macchina in WebAssembly (M4, dispositivi in M5)

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
`>>> 0`). Anche i puntatori vanno letti con `>>> 0`: con più di 2 GiB di
memoria lineare (due macchine da 1 GiB) arrivano negativi.

## Export

Versione: `vetro_abi_version() -> u32`, oggi **4**. Cambia a ogni modifica
incompatibile delle firme o dei codici qui sotto; il caricatore JS
(`web/node/vetro.mjs`) la controlla.

- 2 (M4): JIT della modalità sistema (`vetro_machine_set_jit`, import
  `vetro_jit.entry/place/reset`).
- 3 (M5): dispositivi (`vetro_machine_new_with`), display di virtio-gpu,
  virtio-input, GPIO, dischi virtio-blk con i dati dal JS, codice
  d'arresto 5 `Blocked`. `vetro_machine_new` resta, con i dispositivi di
  default; la GPU di vetro-wasm mostra su `WebDisplay` (RGBA) invece di
  `MemDisplay` (al guest non cambia niente: stesse istruzioni).
- 4 (M6): snapshot della macchina (`vetro_snapshot_*`, ADR 0015).

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
| 5 | `Blocked`: un disco aspetta blocchi dal JS (`vetro_disk_wanted`); il tempo del guest è fermo (ADR 0014) |

La macchina è deterministica: lo stesso kernel, initramfs, riga di comando e
ingresso (agli stessi numeri di istruzione) danno la stessa uscita e lo stesso
conteggio che in nativo. `tools/wasm-boot.sh` lo verifica.

### Dispositivi (ABI 3)

| Export | Firma | Significato |
|---|---|---|
| `vetro_machine_new_with` | `(ram_size: u64, now_secs: u64, seed: u64, devices: u32, width: u32, height: u32) -> *mut Vm` | come `vetro_machine_new`, con i dispositivi scelti: bit `GPU` 1, `KEYBOARD` 2, `TABLET` 4, `MULTITOUCH` 8 (vince su `TABLET`), `NET` 16 (virtio-net con `vetro-net` e il sinkhole, `NetSetup::default`); 23 = `Devices::default`. `width`x`height`: risoluzione iniziale dello scanout 0 (0 = 1280x800). Slot come `Devices` (GPU 31, tastiera 30, puntatore 29, rete 28; i dischi dopo) |
| `vetro_display_size` | `(vm, scanout: u32) -> u64` | `(larghezza << 32) \| altezza`; 0 se spento o senza GPU |
| `vetro_display_ptr` | `(vm, scanout) -> *const u8` | pixel RGBA (4 byte, righe da `larghezza * 4`), nullo se spento. Valido fino alla prossima `vetro_run` |
| `vetro_display_updates` | `(vm, scanout) -> u64` | contatore degli aggiornamenti (immagine o spegnimento): se non cambia, niente da ridisegnare |
| `vetro_display_take_dirty` | `(vm, scanout, out: *mut u32) -> u32` | unione dei rettangoli cambiati dall'ultima chiamata: scrive `x, y, w, h` e restituisce 1; 0 = niente. Dopo un cambio di dimensione, l'intero scanout |
| `vetro_display_resize` | `(vm, scanout, width, height) -> u32` | risoluzione chiesta dall'host (`VirtioGpu::set_display`, evento al driver); 0 senza GPU. È un ingresso |
| `vetro_cursor_state` | `(vm, scanout, out: *mut u32) -> u32` | 6 valori: risorsa (0 = nascosto), x, y, hot_x, hot_y, numero di cambi; 0 senza GPU |
| `vetro_cursor_image` | `(vm, scanout) -> *const u8` | cursore 64x64 RGBA, nullo se non c'è |
| `vetro_input_key` | `(vm, code: u32, down: u32) -> u32` | tasto Linux `KEY_*` con SYN_REPORT; 0 se non c'è la tastiera |
| `vetro_input_abs` | `(vm, x: u32, y: u32) -> u32` | posizione del tablet (0..=32767 per asse) con SYN_REPORT |
| `vetro_input_button` | `(vm, code, down) -> u32` | pulsante del puntatore (`BTN_LEFT` 0x110, `BTN_RIGHT` 0x111, `BTN_MIDDLE` 0x112) |
| `vetro_input_touch` | `(vm, slot, x, y, down) -> u32` | contatto del touchscreen (protocollo B, tracking id = slot); `down` 0 lo toglie |
| `vetro_input_events` | `(vm, device: u32, events: *const u32, count: usize) -> u32` | eventi evdev grezzi (`tipo, codice, valore` come tre `u32`) su tastiera (0) o puntatore (1); i SYN_REPORT li mette chi chiama (es. la rotella: `EV_REL REL_WHEEL ±1`) |
| `vetro_input_leds` | `(vm) -> u32` | LED accesi dal guest (bit `LED_*`) |
| `vetro_gpio_input` | `(vm, line: u32, level: u32)` | linea d'ingresso del PL061 (`Board::gpio_input`); la 3 è il tasto di accensione (`gpio-keys`, KEY_POWER): premuto = 1, rilasciato = 0 |
| `vetro_power_key_line` | `() -> u32` | 3 |

Gli ingressi (tasti, puntatore, tocco, console, GPIO, risoluzione)
arrivano al guest prima della prossima istruzione eseguita: sono gli eventi
da registrare per il replay (M10). Le letture (display, cursore, LED,
contatori) non toccano la macchina: il momento in cui la pagina le fa non
cambia l'esecuzione.

### Dischi (ABI 3, ADR 0014)

| Export | Firma | Significato |
|---|---|---|
| `vetro_disk_add` | `(vm, size: u64, block_size: u32, max_blocks: u32, flags: u32) -> i32` | disco virtio-blk di `size` byte (arrotondati per difetto a 512, come QEMU per i raw) con i dati dal JS a blocchi allineati di `block_size` byte (potenza di due, >= 512); al più `max_blocks` blocchi in memoria (0 = nessun limite; oltre, si tolgono i più vecchi). `flags`: 1 = sola lettura per il guest; senza, le scritture del guest vanno in un livello copy-on-write in memoria (cluster da 4 KiB). Restituisce l'indice del disco o -1 (motivo nel messaggio). Nel primo slot libero dall'alto; da chiamare prima di `vetro_run` |
| `vetro_disk_add_mem` | `(vm, data: *const u8, len: usize, flags: u32) -> i32` | disco col contenuto già in memoria (copiato), sempre pronto; stessi arrotondamento, `flags` e copy-on-write |
| `vetro_disk_wanted` | `(vm, out: *mut u64, cap: usize) -> usize` | blocchi chiesti dal guest, coppie `(disco, blocco)` in `out` (al più `cap`); ogni blocco compare una volta sola finché non arriva o fallisce |
| `vetro_disk_fill` | `(vm, disk: u32, block: u64, data: *const u8, len: usize) -> u32` | consegna un blocco (`len` = `block_size`, o il resto per l'ultimo); anche non chiesto (lettura anticipata). 0 ok, 1 disco sconosciuto (o in memoria), 2 blocco fuori dal disco, 3 lunghezza sbagliata |
| `vetro_disk_fail` | `(vm, disk, block: u64) -> u32` | il blocco non si può avere: la richiesta che lo aspetta finisce con IOERR |
| `vetro_disk_stats` | `(vm, disk, out: *mut u64, cap: usize) -> usize` | dimensione, dimensione del blocco, blocchi in memoria, letture mancate, blocchi consegnati, tolti, falliti, cluster copy-on-write scritti; 0 = disco sconosciuto |

Il giro con un disco via rete:

1. `vetro_run` restituisce 5 (`Blocked`): una richiesta del guest tocca
   blocchi assenti. Nessuna istruzione è stata eseguita dopo la richiesta,
   e altre `vetro_run` restituiscono subito 5 finché i dati mancano.
2. Il JS legge `vetro_disk_wanted`, procura i blocchi (cache OPFS, poi
   HTTP Range o `File`) e li consegna con `vetro_disk_fill`.
3. La `vetro_run` successiva ripete la richiesta e la completa allo stesso
   numero di istruzioni che con un disco locale; il resto dell'esecuzione è
   identico. Per confrontare esecuzioni, chi chiama continua il quanto fino
   al suo confine prima di guardare la console o dare ingressi
   (`tests/web/lib.mjs`, `Session.quantum`).

### Snapshot (ABI 4, ADR 0015, `docs/specs/snapshot.md`)

| Export | Firma | Significato |
|---|---|---|
| `vetro_snapshot_version` | `() -> u32` | versione del formato degli snapshot (oggi 1): da mettere nella chiave della cache, così uno snapshot di un'altra versione non si prova nemmeno |
| `vetro_snapshot_save` | `(vm) -> usize` | salva la macchina intera in un buffer interno e ne restituisce la lunghezza. Prima leggere la console: l'uscita già tolta alla UART e non consegnata al JS non entra |
| `vetro_snapshot_ptr` | `(vm) -> *const u8` | i byte dell'ultimo salvataggio (nullo se non ce n'è), validi fino al prossimo salvataggio, a `vetro_snapshot_clear` o a `vetro_machine_free` |
| `vetro_snapshot_clear` | `(vm)` | libera il buffer |
| `vetro_snapshot_restore` | `(vm, data: *const u8, len: usize) -> u32` | ripristina; il buffer si può liberare subito dopo. Codici: 0 `OK`, 1 `BAD_MAGIC` (non è uno snapshot), 2 `VERSION` (altro formato), 3 `CONFIG` (macchina configurata diversamente), 4 `CORRUPT` (rovinato o incoerente: la macchina va scartata); motivo nel messaggio. Con 1, 2 e 3 la macchina non cambia |

Per ripristinare si costruisce la macchina con gli stessi parametri di
`vetro_machine_new_with` (RAM, ora, seme, dispositivi, risoluzione), si
aggiungono gli stessi dischi nello stesso ordine e con gli stessi parametri
(`vetro_disk_add` / `vetro_disk_add_mem`: dimensione, blocco, flag), si
attiva il JIT se si vuole (il risultato non cambia), poi
`vetro_snapshot_restore`. Che cosa è **stato** (nello snapshot) e che cosa
è **collegamento** (lo ricrea il JS):

| Nello snapshot | Collegamento |
|---|---|
| CPU, MMU (TLB compreso), RAM, orologio, timer, GIC, UART (FIFO, uscita non letta dalla UART), RTC, GPIO | il modulo WASM e il motore JIT (blocchi rifatti da capo) |
| trasporti e code virtio, richieste in volo, stato di GPU (risorse e pixel), input, rete (stack e sinkhole), vsock | `WebDisplay`: riceve subito immagine e cursore ripristinati (`vetro_display_updates` cambia) |
| livello copy-on-write dei dischi (le scritture del guest) | i dati dei dischi (`HostDisk`, HTTP Range, OPFS): dopo il ripristino i blocchi si chiedono di nuovo con `BLOCKED` come all'avvio; la dimensione si controlla |
| (niente altro: anche il contenuto di `vetro_disk_add_mem` è una base in sola lettura sotto il copy-on-write) | il contenuto dei dischi in memoria (`vetro_disk_add_mem`), controllato con un hash; l'uscita della console già consegnata al JS; gli ingressi non ancora dati |

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

## Dischi in JavaScript (`web/node/disk.mjs`)

Senza API di Node (gira nel Worker dell'app e nei test):

- `RangeSource(url)`: `open()` legge la dimensione dal `Content-Range` di
  una richiesta `Range: bytes=0-0` (serve una risposta 206) e fa la chiave
  della cache con URL, dimensione ed `ETag`/`Last-Modified`; `read(offset,
  length)` con Range, 3 tentativi sugli errori di rete e sui 5xx.
- `BlobSource(file)`: un `File` scelto dall'utente (`Blob.slice`).
- `MemoryCache` e `OpfsCache.open(key, blockSize, blocks)`: la cache OPFS
  usa `FileSystemSyncAccessHandle` (solo in un Worker dedicato): un file
  `.img` con i blocchi al loro posto e un file `.map` con un bit per blocco
  presente, scritto dopo i dati. Al riavvio i blocchi presenti non tornano
  in rete.
- `DiskFeeder(machine)`: `add(source, { cache, blockSize, maxBlocks,
  readOnly, readahead })` aggiunge il disco (`vetro_disk_add`); `serve()`
  dopo `Blocked` consegna i blocchi chiesti, prima dalla cache, poi dalla
  sorgente unendo i blocchi contigui (più `readahead` blocchi dopo ognuno)
  in una richiesta fino a 8 MiB. Un errore della sorgente dopo i tentativi
  diventa `vetro_disk_fail`.

## L'app web (`web/app`)

HTML, CSS e moduli ES serviti così come sono: nessun bundler, nessuna
dipendenza npm.

```sh
cargo build --release --target wasm32-unknown-unknown -p vetro-wasm
node tools/web-serve.mjs            # http://127.0.0.1:8080/app/
```

`tools/web-serve.mjs` serve `web/` (l'app in `/app/`, i moduli comuni in
`/node/`), il `.wasm` in `/wasm/vetro_wasm.wasm`, `target/guest-kernel` in
`/guest/` (kernel e initramfs di M3, già scelti nella pagina) e
`target/web-disks` in `/disks/`, con Range, `ETag` e le intestazioni
`Cross-Origin-Opener-Policy: same-origin` e
`Cross-Origin-Embedder-Policy: require-corp` (più
`Cross-Origin-Resource-Policy: same-origin`). Oggi non servono (un solo
thread, niente `SharedArrayBuffer`), ma la pagina è già
`crossOriginIsolated`: quando arriveranno i thread WASM (ADR 0002) ogni
server che ospita l'app dovrà mandarle, e ogni risorsa di un'altra origine
(es. un disco su una CDN) dovrà avere CORS o `Cross-Origin-Resource-Policy:
cross-origin`. Parametri dell'URL:
`?kernel=URL&initrd=URL&disk=URL&cmdline=...&pointer=multitouch&webgpu=1&autostart=1`.

- `main.mjs` (thread della pagina): sceglie kernel, initramfs e disco (URL
  o file locale), opzioni (RAM, risoluzione, tablet o touchscreen, blocchi
  da 64 KiB o 1 MiB, JIT, tempo reale, cache OPFS, WebGPU), avvia il
  Worker; disegna i rettangoli cambiati dello scanout (`display.mjs`:
  Canvas2D con `putImageData`, o WebGPU con `writeTexture` e un triangolo
  a pieno schermo, se scelto e disponibile), il cursore in un secondo
  canvas sopra, la console (`terminal.mjs`: CR/LF/BS/TAB, CSI K/J/C/D/G/H,
  risposta a `ESC[6n`, UTF-8), la barra di stato.
- Ingressi: tastiera sul canvas con `KeyboardEvent.code` → codice Linux
  (`keymap.mjs`; le ripetizioni del browser si scartano, l'autorepeat lo fa
  il guest con EV_REP; al blur si rilasciano i tasti premuti); mouse con
  coordinate assolute 0..32767 del tablet e pulsanti `BTN_*`, rotella come
  `REL_WHEEL`; col touchscreen ogni `pointerId` prende uno slot (0..9);
  console con i byte di un terminale (Invio = CR, Backspace = DEL, frecce
  CSI, Ctrl+lettera, incolla); pulsante del tasto di accensione (GPIO 3
  premuto e rilasciato).
- `worker.mjs`: istanzia vetro-wasm e il motore JIT, crea la macchina
  (`vetro_machine_new_with`, rete col sinkhole a scelta), i dischi (`DiskFeeder`, cache OPFS per gli
  URL), carica il kernel; esegue quanti da 1 M istruzioni per al più 12 ms
  per fetta, poi manda console, rettangolo cambiato (ArrayBuffer
  trasferito), cursore, statistiche; applica gli ingressi fra una fetta e
  l'altra (registrandoli con il numero di istruzione, per M10); su
  `Blocked` aspetta `DiskFeeder.serve()`; su `Idle` aspetta un messaggio.
  Con "tempo reale" il tempo del guest non corre davanti all'orologio vero
  (il tempo passato ad aspettare i dischi non conta).

## Test web

`tools/web-test.sh [--no-jit]` (job `boot` della CI, dopo
`tools/wasm-boot.sh`):

- `tests/boot/tests/web.rs` (nativo, release): gli stessi copioni di
  `boot-disk.mjs` e `devices.mjs` con l'API di vetro-wasm compilata per
  l'host, l'interprete e un disco locale; scrive istruzioni e log grezzo in
  `target/web-test/native-*`. Con `VETRO_WEB_NATIVE=1` (lo mette
  `tools/web-test.sh`) i test in Node devono dare le stesse istruzioni e lo
  stesso log byte per byte (JIT in V8, disco via HTTP);
- `tests/web/unit.mjs`: server (Range, suffissi, 416, HEAD, COOP/COEP,
  percorsi fuori radice), `RangeSource`, `BlobSource`, `DiskFeeder` (cache,
  blocchi contigui, lettura anticipata, errori), mappa dei tasti, terminale;
- `tests/web/boot-disk.mjs`: il kernel M3 legge e scrive un disco raw di
  prova (`md5sum /dev/vda`, 13 byte scritti, cache svuotata, riletti,
  `md5sum`): disco locale, via HTTP con cache vuota, dalla cache piena
  (nessuna lettura in rete), con blocchi da 4 KiB e lettura anticipata.
  Stesse istruzioni e stesso log in tutti i casi; somme uguali a quelle del
  file; file sul server intatto;
- `tests/web/devices.mjs`: `vetro-dev drm-hold` e il framebuffer letto con
  `vetro_display_ptr` uguale pixel per pixel al motivo del guest (lo stesso
  controllo di `tests/boot/tests/devices.rs`), rettangolo cambiato,
  cursore, spegnimento dello scanout; tastiera e tablet via API letti dal
  guest con evdev; LED; due esecuzioni identiche;
- `tests/web/browser.mjs`: l'app in Chrome headless pilotato col protocollo
  DevTools (WebSocket di Node 22): avvio col disco via HTTP, `md5sum` e
  motivo del guest sul canvas pixel per pixel con cursore visibile, tasto
  vero dal canvas al guest, riavvio con i blocchi da OPFS. Senza Chrome
  (`VETRO_CHROME`) stampa SKIP; `VETRO_REQUIRE_BROWSER=1` lo rende un
  errore.

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
