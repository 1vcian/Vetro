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

Versione: `vetro_abi_version() -> u32`, oggi **11**. Cambia a ogni modifica
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
- 5 (M5): connessioni TCP dal JS verso i servizi del guest (`vetro_net_*`,
  inoltro di porte come `hostfwd` di QEMU; la base di adb nel browser).
- 6 (M6): overlay copy-on-write persistente dei dischi (`vetro_overlay_*`,
  ADR 0017); `vetro_snapshot_restore` segna gli overlay per il confronto
  completo.
- 7 (M8): virtio-vsock (bit `VSOCK` di `vetro_machine_new_with`) e gestore
  dei file (`vetro_files_*`, ADR 0020, `docs/specs/files.md`).
- 8 (M7, M10): ispettore di rete, timeline input→effetti, record & replay
  (`vetro_result_*`, `vetro_capture_*`, `vetro_inspect_*`,
  `vetro_timeline_*`, `vetro_record_*`, `vetro_rr_status`, `vetro_log_*`,
  `vetro_replay_start`, `vetro_registers_text`, `vetro_read_virt`,
  `vetro_translate`, `vetro_read_phys`; ADR 0023). Gli ingressi di
  tastiera, puntatore, console, GPIO e risoluzione si annotano nella
  timeline (l'esecuzione non cambia).
- 9 (M8): SQL del gestore dei file e percorsi come byte (ADR 0021).
- 10 (M4): JIT a regioni (ADR 0024): import `vetro_jit.runtime` (il modulo
  di runtime `rt.*` dei moduli generati), export `vetro_jit_vsync`
  (`env.vsync` del runtime), contatore `yields` in fondo a
  `vetro_jit_stats`.

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
| `vetro_jit_stats` | `(vm, out: *mut u64, cap: usize) -> usize` | contatori del JIT (`SysJitStats`: `jit_steps`, `runs`, `resolves`, `calls`, `blocks`, `modules`, `reused`, `invalidated_pages`, `faults`, `svcs`, `stops`, `epochs`, `tlb_flushes`, `tlb_fills`, `resets`, `yields`) in `out`; restituisce quanti (0 senza JIT) |

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
| `vetro_machine_new_with` | `(ram_size: u64, now_secs: u64, seed: u64, devices: u32, width: u32, height: u32) -> *mut Vm` | come `vetro_machine_new`, con i dispositivi scelti: bit `GPU` 1, `KEYBOARD` 2, `TABLET` 4, `MULTITOUCH` 8 (vince su `TABLET`), `NET` 16 (virtio-net con `vetro-net` e il sinkhole, `NetSetup::default`), `VSOCK` 32 (virtio-vsock, CID 3, ABI 7); 23 = `Devices::default`. `width`x`height`: risoluzione iniziale dello scanout 0 (0 = 1280x800). Slot come `Devices` (GPU 31, tastiera 30, puntatore 29, rete 28; i dischi dopo) |
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
| `vetro_snapshot_version` | `() -> u32` | versione del formato degli snapshot (oggi 2): da mettere nella chiave della cache, così uno snapshot di un'altra versione non si prova nemmeno |
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

Se i dischi hanno un overlay persistente (sotto), `vetro_overlay_open` va
chiamata **prima** di `vetro_snapshot_restore`; dopo il ripristino la
prossima `vetro_overlay_take` confronta tutti i cluster con il file e scrive
solo quelli diversi.

### Overlay persistente dei dischi (ABI 6, ADR 0017)

Le scritture del guest su un disco con copy-on-write (`vetro_disk_add` o
`vetro_disk_add_mem` senza `READ_ONLY`) si conservano in un file tenuto dal
JS (OPFS), nel formato di `vetro_snapshot::overlay` (lo stesso di `vetro
boot --overlay`, `docs/specs/snapshot.md`). Rust decide che cosa scrivere e
dove; il JS legge il file all'apertura e applica le scritture.

| Export | Firma | Significato |
|---|---|---|
| `vetro_overlay_open` | `(vm, disk: u32, identity: *const u8, identity_len: usize, data: *const u8, data_len: usize) -> u32` | apre l'overlay del disco dal contenuto del file (`data_len` 0 se non c'è) per l'immagine base `identity` (UTF-8: URL, dimensione, ETag); i cluster letti entrano nel copy-on-write. Prima di `vetro_run` e di `vetro_snapshot_restore`. Codici: 0 `LOADED`, 1 `NEW` (file vuoto), 2 `MISMATCH` (overlay di un'altra base o dimensione: scartato), 3 `CORRUPT` (illeggibile: scartato), 4 `NO_DISK` (disco sconosciuto o in sola lettura); motivo nel messaggio. Con 2 e 3 la prossima `take` tronca il file |
| `vetro_overlay_take` | `(vm, disk) -> usize` | prepara le scritture che portano il file allo stato del copy-on-write (i cluster scritti dal guest dall'ultima volta, o tutti dopo un ripristino) e ne restituisce la lunghezza; 0 = niente da scrivere. Codifica: u64 lunghezza a cui troncare prima (`u64::MAX` = no), u32 numero di scritture, poi per ognuna u64 offset, u32 lunghezza, byte (LE). In ordine: l'intestazione (offset 0) è l'ultima, da scrivere dopo un flush dei dati. Fra due `vetro_run` qualsiasi: non tocca il guest |
| `vetro_overlay_ptr` | `(vm) -> *const u8` | i byte dell'ultima `take` (nullo se vuota), validi fino alla prossima `take`, a `vetro_overlay_clear` o a `vetro_machine_free` |
| `vetro_overlay_clear` | `(vm)` | libera il buffer |
| `vetro_overlay_info` | `(vm, disk, out: *mut u64, cap: usize) -> usize` | generazione (cresce a ogni `take` che scrive qualcosa), cluster nel file, slot nel file, slot rovinati trovati all'apertura, lunghezza del file; 0 = disco senza overlay |

### Rete: connessioni verso il guest (ABI 5)

Il JS apre connessioni TCP verso una porta del guest (10.0.2.15), che le
vede arrivare dal gateway 10.0.2.2 da una porta effimera (49152, 49153, …),
come con `-netdev user,hostfwd=…` di QEMU. È lo stesso
`Stack::host_connect` di `vetro boot --hostfwd` (`docs/specs/net.md`). Serve
una macchina con la rete (bit `NET`) e un guest che ha già fatto il DHCP.
L'id della connessione è un `u64` (> 0, in JS `BigInt`).

| Export | Firma | Significato |
|---|---|---|
| `vetro_net_connect` | `(vm, guest_port: u32) -> u64` | apre una connessione verso `guest_port`; il SYN parte prima della prossima istruzione. 0 senza rete o con porta 0 / > 65535 |
| `vetro_net_send` | `(vm, conn: u64, src: *const u8, len: usize) -> usize` | mette in coda byte per il guest; restituisce quanti ne ha presi (al più 256 KiB in coda: il resto va riproposto dopo un `vetro_run`). 0 se chiusa, sconosciuta o dopo `vetro_net_shutdown` |
| `vetro_net_recv` | `(vm, conn, dst: *mut u8, cap: usize) -> usize` | copia e consuma al più `cap` byte arrivati dal guest; 0 = niente (senza toccare la macchina) |
| `vetro_net_shutdown` | `(vm, conn) -> u32` | chiude il verso JS→guest: FIN dopo i byte in coda. 1 fatto, 0 sconosciuta |
| `vetro_net_abort` | `(vm, conn) -> u32` | interrompe: RST al guest |
| `vetro_net_release` | `(vm, conn) -> u32` | dimentica la connessione (se è viva, prima la interrompe); da chiamare dopo `CLOSED` e l'ultima lettura |
| `vetro_net_state` | `(vm, conn, out: *mut u32, cap: usize) -> u32` | stato: 0 sconosciuta (o senza rete), 1 in apertura, 2 aperta (anche durante la chiusura), 3 chiusa. In `out` (al più `cap`): motivo della chiusura (0 nessuno, 1 `Normal`, 2 `GuestReset`, 3 `RemoteReset`, 4 `Refused` = nessuno in ascolto nel guest, 5 `Timeout`), byte leggibili, spazio per `vetro_net_send`, fine del flusso dal guest (1 = il guest ha chiuso e tutto è stato letto), byte in coda non ancora presi dal guest. Non tocca la macchina |

I byte si muovono mentre la macchina esegue: chi chiama alterna `vetro_run`
e `send`/`recv`, come per la console. Aprire, scrivere, leggere byte pronti,
chiudere e interrompere sono ingressi (arrivano al guest prima della
prossima istruzione, da registrare per il replay di M10); `vetro_net_state`
e una `vetro_net_recv` senza byte pronti non cambiano l'esecuzione.

In JS: `Machine.connectGuest(port)` restituisce un `GuestSocket`
(`web/node/vetro.mjs`) con `send(bytes)`, `recv()`, `shutdown()`,
`abort()`, `release()` e `state()` (`{ state, reason, readable, writable,
guestEof, unsent }`, nomi in `NET_STATE` e `NET_REASON`). Prova:
`tests/web/hostfwd.mjs` (in `tools/web-test.sh`): `nc -l -e cat` nel
guest, eco di 200 KB dal JS, chiusura, porta senza servizio, stesse
istruzioni con e senza JIT e in due esecuzioni.

### Gestore dei file (ABI 7, ADR 0020; ABI 9, ADR 0021; `docs/specs/files.md`)

Il client di `vetro_machine::files` verso il demone `vetro-files` del guest
(porta vsock 5200). Serve il bit `VSOCK`.

| Export | Firma | Significato |
|---|---|---|
| `vetro_files_open` | `(vm, port: u32) -> u32` | crea il client (porta 0 = 5200), al posto di quello che c'era; 1 fatto, 0 senza vsock. Dopo `vetro_load_linux` o `vetro_snapshot_restore`: le connessioni al demone rimaste nello snapshot si chiudono al primo `pump` |
| `vetro_files_close` | `(vm)` | chiude la connessione e toglie il client |
| `vetro_files_status` | `(vm, out: *mut u32, cap: usize) -> u32` | 0 nessun client, 1 in collegamento (o in attesa di riprovare), 2 collegato. In `out`: operazioni non finite, saluti ricevuti (cresce a ogni ricollegamento: le osservazioni vanno rifatte), pezzo massimo e flag del saluto (bit 0 SELinux). Non tocca la macchina |
| `vetro_files_request` | `(vm, op: u32, a: *const u8, a_len, b: *const u8, b_len, x: u64, y: u64) -> u32` | chiede un'operazione sul percorso `a` (byte del guest, anche non UTF-8, dall'ABI 9; vuoto = rifiutata): 1 `STAT`, 2 `LIST`, 3 `READ` (`x` offset, `y` byte, `u64::MAX` = fino alla fine, al più 256 MiB), 4 `WRITE` (`b` contenuto, `x` modo di un file nuovo), 5 `MKDIR` (`x` modo), 6 `CREATE` (`x` modo), 7 `DELETE` (`x` 1 = ricorsivo), 8 `RENAME` (`b` destinazione, byte), 9 `WATCH`, 10 `UNWATCH` (`x` wd), 11 `SQL` (ABI 9: `b` = `u32` lunghezza e SQL UTF-8, `u16` numero di parametri, parametri nel formato dei valori del protocollo (`proto::encode_sql_args`); `x` righe cambiate attese, `u64::MAX` = qualsiasi; `y` bit 0 sola lettura). Restituisce l'id (> 0) o 0 |
| `vetro_files_pump` | `(vm) -> u32` | fa avanzare il client (fra un quanto e l'altro) e restituisce i messaggi pronti |
| `vetro_files_take` | `(vm) -> usize` | prepara il prossimo messaggio e ne dà la lunghezza (0 = nessuno) |
| `vetro_files_ptr` | `(vm) -> *const u8` | i byte del messaggio, validi fino alla prossima `take` |

Messaggio: `u32` lunghezza del JSON, JSON UTF-8, poi i byte di una lettura.
JSON di una risposta: `{"kind":"reply","op":N,"ok":true,"type":T,...}` con
`T` = `stat` (`stat`), `list` (`entries: [{name, stat}]`), `data` (`size`,
`length`: i byte seguono), `written` (`stat`), `watch` (`wd`), `sql`
(`changes`, `lastRowid` come stringa decimale, `truncated`, `columns`,
`rows`: valori `null`, `["i","<intero>"]`, `["f","<reale>"]` (`inf`,
`-inf`, `NaN` compresi), `["t","<testo>"]`, `["b","<esadecimale>"]`),
`done`; o
`{"kind":"reply","op":N,"ok":false,"error":"ENOENT (2)","errno":2,"code":"ENOENT"}`
(`code` `PROTOCOL` o `DISCONNECTED` con `errno` null; `SQLITE` con
`sqlite` = codice di SQLite se SQLite ha rifiutato). `stat` = `{kind,
mode, uid, gid, size, mtime, mtimeNs, nlink, link, selinux}`. Evento:
`{"kind":"event","wd":N,"mask":N,"cookie":N,"name":"..."}`. Nomi e
destinazioni dei collegamenti sono byte del guest in *surrogateescape*: un
byte che non fa parte di UTF-8 valido è `\udcXX` (surrogato solitario
U+DC80 + byte − 0x80); `pathBytes` di `vetro.mjs` fa l'inverso per i
percorsi mandati.

Collegarsi, mandare e leggere sono ingressi (`Machine::input`, registrati
per il replay); `status`, `take` e `ptr` no. In JS: `Machine.files(port)`
→ `GuestFiles` (Promise per operazione, `sql(percorso, sql, parametri,
{ expect, readonly })`, `onEvent`, `status()`, `pump()`, `close()`),
costanti `FILES_OP`, `FILES_STATUS`, `INOTIFY`; `pathBytes`,
`pathString`, `displayName`, `encodeSqlArgs`, `sqlValue`.

### Buffer dei risultati (ABI 8)

Le funzioni che producono byte (JSON, HAR, pcapng, log, keyframe, registri)
li mettono nel buffer dei risultati della macchina e ne restituiscono la
lunghezza (0 = niente).

| Export | Firma | Significato |
|---|---|---|
| `vetro_result_ptr` | `(vm) -> *const u8` | i byte dell'ultimo risultato (nullo se vuoto), validi fino al prossimo risultato |
| `vetro_result_clear` | `(vm)` | libera il buffer |

### Ispettore di rete e timeline (ABI 8, ADR 0023)

La cattura dei frame di virtio-net (`Machine::net_tap`, ADR 0016) si
raccoglie nella macchina di vetro-wasm a ogni `vetro_run` (al più 64 MiB);
lista e dettaglio sono il JSON di `vetro_analysis::net::view`, la timeline
quello di `Timeline::to_json` (`docs/specs/analysis.md`). Niente di questo
cambia l'esecuzione.

| Export | Firma | Significato |
|---|---|---|
| `vetro_capture_set` | `(vm, on: u32) -> u32` | accende (1) o spegne la cattura; 1 fatto, 0 senza rete |
| `vetro_capture_clear` | `(vm)` | svuota frame e analisi |
| `vetro_capture_stats` | `(vm, out: *mut u64, cap) -> usize` | accesa, frame, byte, frame scartati oltre il limite |
| `vetro_inspect_requests` | `(vm) -> usize` | la lista in JSON (`requests_json`) |
| `vetro_inspect_request` | `(vm, index: u32) -> usize` | il dettaglio della richiesta `index` in JSON (`exchange_json`); 0 se non c'è |
| `vetro_inspect_har` | `(vm, epoch_us: u64) -> usize` | l'HAR 1.2 (`epoch_us`: µs Unix del tempo 0 del guest) |
| `vetro_inspect_pcapng` | `(vm, epoch_us: u64) -> usize` | il pcapng dei frame |
| `vetro_timeline_input` | `(vm, kind: u32, weak: u32, text, len)` | annota un ingresso dell'utente che la macchina non riconosce da sé (comando del gestore dei file: `kind` 4) all'istruzione corrente |
| `vetro_timeline_effect` | `(vm, kind: u32, text, len) -> u32` | annota un effetto (file cambiato: `kind` 3) all'istruzione corrente; 0 se il tipo non esiste |
| `vetro_timeline_json` | `(vm, window_us: u64) -> usize` | la timeline in JSON con gli effetti di rete della cattura; finestra di attribuzione `window_us` (0 = 3 s) |
| `vetro_timeline_version` | `(vm) -> u64` | cambia quando cambiano ingressi, effetti o frame: se è uguale, niente da ridisegnare |
| `vetro_timeline_clear` | `(vm)` | svuota la timeline |

Tipi: ingressi `InputKind` (0 tasto, 1 puntatore, 2 tocco, 3 console, 4
file, 5 accensione, 6 schermo, 7 altro), effetti `EffectKind` (0 http, 1
dns, 2 tls, 3 file, 4 console). Gli ingressi che passano da
`vetro_console_write`, `vetro_input_*`, `vetro_gpio_input` e
`vetro_display_resize` si annotano da soli (`analysis::Describer`: tasti e
pulsanti premuti, tocchi nuovi, righe della console, tasto di accensione,
risoluzione; non movimenti, rilasci, risposte del terminale); l'uscita della
console si annota quando `vetro_console_read` la legge.

### Record & replay (ABI 8, ADR 0019 e 0023, `docs/specs/replay.md`)

| Export | Firma | Significato |
|---|---|---|
| `vetro_record_start` | `(vm, keyframe_every: u64)` | registra da qui, keyframe ogni tante istruzioni (il primo subito; 0 = nessuno) |
| `vetro_record_stop` | `(vm) -> u32` | finisce; il log resta nella macchina. 1 fatto, 0 non si registrava |
| `vetro_rr_status` | `(vm, out: *mut u64, cap) -> u32` | 0 fermo, 1 registrazione, 2 replay, 3 replay finito identico, 4 replay diverso (motivo nel messaggio). In `out`: eventi registrati o prossimo evento, eventi del log, keyframe, istruzione di partenza e di fine, 1 se c'è un log |
| `vetro_log_encode` | `(vm) -> usize` | il file del log con i keyframe presenti |
| `vetro_log_load` | `(vm, data, len) -> u32` | carica un file di log; 0 fatto, 1 non valido (motivo nel messaggio) |
| `vetro_log_info` | `(vm, out: *mut u64, cap) -> usize` | partenza, fine, eventi, keyframe, intervallo, JIT, 1 se della stessa configurazione, byte degli eventi; 0 senza log |
| `vetro_log_events` | `(vm) -> usize` | gli eventi in JSON: `[{i, step, kind, label, weak, user}]` |
| `vetro_log_keyframe` | `(vm, index, out: *mut u64, cap) -> usize` | istruzione, byte e hash della console, dimensione, 1 se presente |
| `vetro_log_keyframe_take` | `(vm, index) -> usize` | sposta i byte del keyframe nel buffer dei risultati (nel log resta la posizione) |
| `vetro_log_keyframe_put` | `(vm, index, data, len) -> u32` | li rimette; 1 fatto, 0 indice o lunghezza sbagliati |
| `vetro_log_keyframe_for` | `(vm, step: u64) -> i32` | il keyframe da cui parte il replay verso `step`, -1 nessuno |
| `vetro_replay_start` | `(vm, step: u64) -> u32` | replay dall'ultimo keyframe non oltre `step` (0 = dall'inizio). Codici: 0 fatto, 1 nessun log, 2 keyframe non presente, 3 rifiutato (motivo nel messaggio). Cattura e timeline ripartono (timeline con gli ingressi del log), il client del gestore dei file si chiude |
| `vetro_registers_text` | `(vm) -> usize` | i registri (`Machine::registers_text`) |
| `vetro_read_virt` | `(vm, va: u64, dst, len, fault: *mut u64) -> u32` | memoria virtuale (tabelle correnti, solo RAM); 1 fatto, 0 con il primo indirizzo illeggibile in `fault` |
| `vetro_translate` | `(vm, va: u64) -> u64` | indirizzo fisico, `u64::MAX` se non mappato |
| `vetro_read_phys` | `(vm, pa: u64, dst, len) -> u32` | RAM all'indirizzo fisico; 0 fuori dalla RAM |

Durante il replay `vetro_run` si ferma agli eventi del log e alla fine
confronta l'impronta; gli ingressi del JS si ignorano. Per saltare a
un'istruzione: `vetro_replay_start(step)`, poi `vetro_run` con budget
`min(quanto, step - istruzioni)` finché non ci si arriva (i dischi si
servono come sempre).

In JS: `Machine.capture`, `captureStats`, `inspectRequests`,
`inspectRequest`, `inspectHar`, `inspectPcapng`, `timelineInput`,
`timelineEffect`, `timeline`, `timelineVersion`, `recordStart`,
`recordStop`, `rrStatus`, `logEncode`, `logLoad`, `logInfo`, `logEvents`,
`logKeyframe`, `logKeyframeTake`, `logKeyframePut`, `logKeyframeFor`,
`replayStart`, `registersText`, `readVirt`, `translate`, `readPhys`;
costanti `TIMELINE_INPUT`, `TIMELINE_EFFECT`, `RR_STATE`, `REPLAY_START`;
`Recording` (`web/node/recording.mjs`) per i keyframe in un archivio.

### Ponte JIT

| Export | Firma | Significato |
|---|---|---|
| `vetro_jit_ld` | `(state: usize, va: u64, size: u32) -> u64` | `env.ld` dei moduli generati (spec `jit.md`) |
| `vetro_jit_st` | `(state: usize, va: u64, size: u32, value: u64) -> u32` | `env.st` dei moduli generati |
| `vetro_jit_resolve` | `(state: usize) -> u32` | `env.resolve` del dispatcher |
| `vetro_jit_vsync` | `(state: usize)` | `env.vsync` del runtime: V0..V31 della `Cpu` nel `JitState` (ABI 10) |
| `vetro_jit_simd` | `(state: usize, word: u32, x: u64, nzcv: u32) -> u64` | `env.simd` del runtime: istruzione SIMD/FP senza memoria eseguita dall'interprete sul `JitState` (ABI 11, ADR 0026) |
| `__indirect_function_table` | tabella | la tabella delle funzioni di vetro-wasm, esportata ed estendibile (`build.rs`): il JS vi mette il dispatcher, che Rust chiama come un puntatore a funzione |
| `vetro_jit_selftest` | `(wasm: *const u8, len: usize) -> u64` | prova del giro completo con un modulo di prova (sotto) |

## Import

Il JS li fornisce all'istanziazione (`web/node/vetro.mjs`):

| Import | Firma | Significato |
|---|---|---|
| `vetro_host.panic` | `(ptr: *const u8, len: usize)` | messaggio UTF-8 di un panic, subito prima della trappola `unreachable` |
| `vetro_jit.compile` | `(ptr: *const u8, len: usize) -> i32` | compila e istanzia un modulo generato; indice ≥ 0, o < 0 se rifiutato |
| `vetro_jit.runtime` | `(ptr: *const u8, len: usize) -> i32` | compila e istanzia il modulo di runtime (con `env.mem`, `env.ld`, `env.st`, `env.vsync`, `env.simd` dall'ABI 11); i suoi export sono gli import `rt.*` dei moduli compilati dopo, anche dopo `reset`; 0, o < 0 se rifiutato (ABI 10) |
| `vetro_jit.entry` | `(module: i32, index: u32) -> u32` | mette l'export `b<index>` del modulo in una voce nuova di `__indirect_function_table` e la restituisce: `JsEngine::run` la chiama come un puntatore a funzione, senza passare da JS |
| `vetro_jit.place` | `(module: i32, count: u32, base: u32)` | mette `b0..b<count-1>` del modulo nella tabella dei blocchi (`env.tbl` del dispatcher) dalla voce `base` |
| `vetro_jit.reset` | `()` | scarta tutte le istanze e ricrea la tabella dei blocchi |
| `vetro_jit.drop` | `(module: i32)` | libera il modulo |

## Il motore JIT in JavaScript

ADR 0012: nel browser il codice generato lo compila ed esegue l'API
`WebAssembly` di JS. I pezzi:

- `web/node/jit-engine.mjs`, classe `JitEngine`, l'equivalente JS del trait
  `vetro_jit::Engine` di `jit.md`:
  - `runtime(bytes)`: istanzia il modulo di runtime con `env.mem`,
    `env.ld`/`env.st`/`env.vsync` = `vetro_jit_ld`/`vetro_jit_st`/
    `vetro_jit_vsync`, e ne tiene gli export per gli import `rt.*`;
  - `compile(bytes)`: `new WebAssembly.Module(bytes)` e subito
    `new WebAssembly.Instance(module, { env: { mem, tbl, ld, st, resolve }, rt })`,
    con `env.mem` = `memory` di vetro-wasm, `env.tbl` = la tabella dei
    blocchi (la importa solo il dispatcher), `env.ld`/`env.st`/`env.resolve`
    = `vetro_jit_ld`/`vetro_jit_st`/`vetro_jit_resolve` e `rt` = gli export
    del runtime. Un export di un'istanza passato come import di un'altra è
    chiamato da V8 direttamente, senza passare dal JS;
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

## Persistenza in JavaScript (`web/node/persist.mjs`, M6, ADR 0017)

Senza API di Node. I file hanno l'interfaccia di `FileSystemSyncAccessHandle`
(`getSize`, `read`, `write`, `truncate`, `flush`, `close`): quelli di OPFS
nel Worker (`opfsFile(cartella, nome)`), `MemFile` nei test.

- `DiskOverlay.open(machine, disk, file, identity)`: legge il file e chiama
  `vetro_overlay_open` (`opened.code`: `Loaded`, `New`, `Mismatch`,
  `Corrupt`); `persist()` applica le scritture di `vetro_overlay_take`
  (troncamento, dati, flush, intestazione, flush) e dice se ha scritto;
  `generation`, `info`.
- `SnapshotStore.opfs()` / `.memory()`: `save(chiave, metadati, byte)`
  scrive `<chiave>.snap` e poi `<chiave>.json` (con `size`), `load(chiave)`
  restituisce `{ meta, bytes }` solo se i metadati ci sono e la lunghezza
  torna; `remove`.
- `snapshotKey(parti)`: SHA-256 (32 cifre esadecimali) del JSON a chiavi
  ordinate; `sha256Hex`; `staleReason(meta, overlays)`: null se ogni overlay
  è alla generazione salvata nei metadati, altrimenti il motivo;
  `toBase64`/`fromBase64` per la coda della console nei metadati.

La classe `Machine` di `vetro.mjs` ha `snapshotVersion`, `snapshotSave()`
(copia dei byte), `snapshotRestore(bytes)` (lancia un `Error` con `code`
`BadMagic`/`Version`/`Config`/`Corrupt`), `overlayOpen(disk, identity,
bytes)`, `overlayTake(disk)` (`{ truncate, writes: [{ at, bytes }] }` o null),
`overlayInfo(disk)`.

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
`?kernel=URL&initrd=URL&disk=URL&cmdline=...&pointer=multitouch&webgpu=1&autostart=1`,
più `snapshot=0` (niente cache degli snapshot), `persist=0` (dischi non
persistenti), `files=/a,/b` (radici del gestore dei file) e `nofiles=1`
(senza gestore dei file né vsock).

- `main.mjs` (thread della pagina): sceglie kernel, initramfs e disco (URL
  o file locale), opzioni (RAM, risoluzione, tablet o touchscreen, blocchi
  da 64 KiB o 1 MiB, JIT, tempo reale, cache OPFS, dischi persistenti,
  snapshot in cache, WebGPU), avvia il
  Worker; disegna i rettangoli cambiati dello scanout (`display.mjs`:
  Canvas2D con `putImageData`, o WebGPU con `writeTexture` e un triangolo
  a pieno schermo, se scelto e disponibile), il cursore in un secondo
  canvas sopra, la console (`terminal.mjs`: CR/LF/BS/TAB, CSI K/J/C/D/G/H,
  risposta a `ESC[6n`, UTF-8), la barra di stato. Pulsanti "Salva stato"
  (snapshot subito) e, prima dell'avvio, "Cancella dati salvati"
  (cartelle OPFS `vetro-snapshots`, `vetro-overlays`, `vetro-disks`). Dopo
  un ripristino rimostra la coda della console dello snapshot senza
  rispondere alle richieste del terminale. `window.vetroState` (`boot`:
  `{ mode: 'cold' | 'snapshot', ms, times }`, `snapshots`, `disks`) per i
  test.
- Gestore dei file (M8, ADR 0020, `docs/specs/files.md`): opzione attiva di
  default (virtio-vsock nella macchina e nella chiave degli snapshot); il
  Worker tiene `GuestFiles` e lo fa avanzare fra una fetta e l'altra, le
  richieste della pagina sono ingressi registrati in `inputLog`; il
  pannello `files.mjs` accanto allo schermo mostra l'albero delle radici
  (`window.vetroFiles.setRoots`), aggiornato dagli eventi di inotify, con i
  visualizzatori (testo, JSON, XML/SharedPreferences, esadecimale,
  immagini, SQLite con `sqlite.mjs`) e il salvataggio nel guest.
- Analisi (M7, M10, ADR 0023; `analysis.mjs`): sotto lo schermo tre
  pannelli. **Rete**: lista delle richieste (metodo, host, percorso, stato,
  dimensioni, tipo, durata, cascata), filtri (testo, metodo, stato, tipo di
  corpo), dettaglio con tempi per fase, intestazioni e corpi decodificati
  (JSON, form, multipart, protobuf senza schema, testo, esadecimale),
  esportazione HAR e pcapng (Blob + `<a download>`). **Timeline**: ingressi
  nel tempo del guest con i loro effetti (http, dns, tls, file, console),
  asse del tempo, finestra di attribuzione, filtri, clic su una richiesta →
  dettaglio, "vai qui" → replay fino a quell'istruzione. **Registrazione**:
  registra/ferma (keyframe ogni N M istruzioni), rigioca (verdetto "replay
  identico" o la differenza), scarica e carica il log, vai all'istruzione,
  continua, registri e dump esadecimale di un indirizzo virtuale, ingressi
  registrati. `window.vetroAnalysis.state()` per i test. Il Worker manda
  lista e timeline al più ogni 0,7 s se sono cambiate; con la rete la
  cattura è accesa dall'avvio; i comandi del gestore dei file (salva, crea,
  cancella, rinomina) sono ingressi della timeline e gli eventi di inotify
  (creato, scritto, spostato, cancellato) effetti. Registrazioni in OPFS
  (`vetro-recordings/`, anche "Cancella dati salvati" le toglie); durante un
  replay gli ingressi della pagina si scartano, il gestore dei file è
  chiuso, niente tempo reale né snapshot in cache.
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
- Persistenza nel Worker (ADR 0017): per ogni disco scrivibile un overlay
  in `vetro-overlays/<sha256(identità)>.cow`, salvato fra una fetta e
  l'altra (al più ogni secondo, e a ogni arresto); lo snapshot in
  `vetro-snapshots/`, con chiave da versione del formato, hash di kernel e
  initramfs, riga di comando, RAM, risoluzione, dispositivi, identità e
  dimensione dei dischi. All'avvio, se c'è uno snapshot per la chiave e gli
  overlay sono alla generazione dei suoi metadati, si ripristina invece di
  caricare il kernel (messaggio `restored` con i tempi); altrimenti si
  avvia da zero (`cold`). Lo snapshot si salva (dopo gli overlay, messaggio
  `snapshot`) la prima volta che il guest è a riposo, di nuovo a riposo se
  gli overlay sono cambiati, e a richiesta. A riposo: `Idle`, o 1,5 s di
  tempo del guest senza console, scanout, ingressi né dischi.

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
  blocchi contigui, lettura anticipata, errori), mappa dei tasti, terminale,
  `MemFile`, `SnapshotStore`, `snapshotKey`, `staleReason`;
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
- `tests/web/snapshot.mjs` (M6): kernel M3 con disco via HTTP e overlay su
  `MemFile`; snapshot a 40 M istruzioni e al prompt, poi scrittura (dd +
  sync, 75 cluster), rilettura a cache svuotata, `md5sum`, spegnimento;
  ripristino su macchine nuove col JIT e con l'interprete (e da metà avvio):
  seguito del log byte per byte, istruzioni finali e file dell'overlay
  uguali all'esecuzione senza tagli; avvio da zero con l'overlay (la
  scrittura c'è); base cambiata (overlay scartato). Stampa tempi e
  dimensioni di salvataggio e ripristino in V8;
- `tests/web/files.mjs` (M8): il gestore dei file via API sul kernel M3
  con vsock: list, lettura, ENOENT, scrittura che conserva modo e
  proprietario letta dal guest, evento di un processo del guest entro 1 s
  di tempo del guest, 1,2 MB scritti e riletti a pezzi (`cmp` nel guest),
  cancellazione ricorsiva; due esecuzioni uguali;
- `tests/web/browser.mjs`: l'app in Chrome headless pilotato col protocollo
  DevTools (WebSocket di Node 22): avvio col disco via HTTP, `md5sum` e
  motivo del guest sul canvas pixel per pixel con cursore visibile, tasto
  vero dal canvas al guest; snapshot salvato al riposo dopo l'avvio,
  scrittura sul disco e snapshot risalvato; seconda sessione con
  `snapshot=0` (avvio da zero, scrittura ritrovata dall'overlay in OPFS,
  blocchi dalla cache OPFS); terza sessione ripristinata dallo snapshot
  (tempo misurato, console che risponde, scrittura presente); pannello del
  gestore dei file (albero aggiornato dal vivo quando il guest crea un
  file, file aperto, modificato e salvato, riletto dal guest con `cat`,
  ricollegamento dopo il ripristino). Senza Chrome
  (`VETRO_CHROME`) stampa SKIP; `VETRO_REQUIRE_BROWSER=1` lo rende un
  errore;
- `tests/web/inspector.mjs` (M7, ABI 8): kernel M3 con la rete, POST JSON e
  POST form di wget al sinkhole; lista, dettaglio con corpi decodificati,
  HAR, pcapng; richieste e DNS attribuiti alla riga del comando, un
  carattere singolo che non causa rete; due esecuzioni uguali (log, HAR,
  pcapng, timeline);
- `tests/web/replay.mjs` (M10, ABI 8): registrazione con keyframe ogni 10 M
  istruzioni, log da file, keyframe in un archivio in memoria (`Recording`),
  replay identico con JIT e interprete (console byte per byte, ispettore e
  timeline uguali), salto a un'istruzione con gli stessi registri e la
  stessa memoria a VBAR_EL1, log ricomposto uguale al file, keyframe
  alterato rifiutato;
- `tests/web/browser-analysis.mjs` (Chrome, come `browser.mjs`): wget
  nell'ispettore con il JSON decodificato e legato al comando nella
  timeline, scrittura di un file legata al suo comando, download veri di
  log, HAR e pcapng, "Rigioca" identico, "vai qui" dalla timeline con
  registri e dump di memoria, "Continua" identico, log ricaricato con "Carica
  log" e rigiocato.

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
