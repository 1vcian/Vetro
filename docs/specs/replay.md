# Record & replay della macchina (M10)

Decisioni e motivi nell'ADR 0019. Qui l'interfaccia e il formato. Codice:
`crates/vetro-machine/src/record.rs` (tipi e formato del log) e
`crates/vetro-machine/src/machine/record.rs` (registrazione, replay,
salto, lettura dello stato).

## Ingressi dell'host

`Machine::input(Input) -> Reply` è l'unico punto da cui l'host cambia ciò
che il guest vede. L'ingresso arriva al guest prima della prossima
istruzione.

| `Input` | Effetto | `Reply` |
|---|---|---|
| `Console(Vec<u8>)` | byte nella coda d'ingresso della PL011 | `Done` |
| `Keyboard(Vec<InputEvent>)`, `Pointer(Vec<InputEvent>)` | `VirtioInput::inject` (chi chiama mette i SYN_REPORT; aiuti `Input::key_events`, `move_abs_events`, `touch_events`) | `Done` / `NoDevice` |
| `Gpio { line, level }` | linea d'ingresso del PL061 (3 = tasto di accensione) | `Done` |
| `Display { scanout, width, height }` | `VirtioGpu::set_display` | `Done` / `NoDevice` |
| `NetFrame(Vec<u8>)` | frame Ethernet per il guest, consegnato prima di quelli dello stack | `Done` / `NoDevice` |
| `NetLink(bool)` | `VirtioNet::set_link_up` | `Done` / `NoDevice` |
| `Vsock(VsockOp)` | `Listen(p)`, `Unlisten(p)`, `Accept(p)`, `Connect(p)`, `Send(c, byte)`, `Recv(c, max)`, `ShutdownSend(c)`, `Close(c)`, `Reset(c)`, `Release(c)`, `TransportReset` | `Vsock(Result)`, `Conn(Option)`, `Data`, `Done` |
| `HostNet(HostNetOp)` | `Connect(porta)`, `Send(id, byte)`, `Recv(id, max)`, `Shutdown(id)`, `Abort(id)`, `Release(id)` sullo stack (`Stack::host_*`), con `poll` forzato | `HostConn(Option<ConnId>)`, `Accepted(n)`, `Data`, `Done` |

In più: `Reply::Deferred` (registrazione in corso e macchina ferma su un
disco: l'ingresso si applica e si registra alla fine del primo quanto dopo
lo sblocco) e `Reply::Ignored` (replay in corso).

`console_input(&[u8])` e `gpio_input(line, level)` sono scorciatoie di
`input`.

Accessi che **non** sono ingressi e non si registrano: `device_view`,
`gpu_view`, `vsock_view`, `net_view` (sola lettura), `host_link` (dati per
un disco che la macchina aspetta, ADR 0014), `console_output`.
`device`, `gpu`, `keyboard`, `pointer`, `vsock`, `net` (chiusure mutabili)
restano, ma durante una registrazione ogni uso è un evento opaco.

## Registrazione

| Metodo | |
|---|---|
| `start_recording(RecordOptions { keyframe_every })` | da qui, fra due `run`; con `keyframe_every > 0` il primo keyframe subito |
| `is_recording()`, `recorded_events()` | |
| `stop_recording() -> Option<Log>` | con l'impronta dello stato di adesso |

Alla fine di ogni quanto registrato: un keyframe se sono passate almeno
`keyframe_every` istruzioni dal precedente (non con la macchina ferma su un
disco, non se allo stesso istante c'è già un evento), poi gli ingressi
rimandati.

## Replay e salto

| Metodo | |
|---|---|
| `start_replay(&Log) -> Result<(), Divergence>` | dallo stato attuale, che dev'essere quello di partenza (configurazione e impronta) |
| `replay_from(&Log, n)` | dall'ultimo keyframe non oltre `n` (senza keyframe: come `start_replay`) |
| `goto(&Log, n) -> Result<u64, Divergence>` | `replay_from` + replay fino al primo confine con almeno `n` istruzioni; restituisce le istruzioni raggiunte |
| `replay_status() -> Option<&ReplayStatus>` | `Running { next }`, `Finished`, `Diverged(Divergence)` |
| `digest() -> Digest` | impronta dello stato (porta l'uscita della UART nel buffer della macchina) |

Durante il replay `run(budget)` si ferma a ogni evento (quanti tagliati,
JIT compreso) e alla fine della registrazione; lì confronta l'impronta
(senza TLB se registrazione o replay hanno usato il JIT). Dopo `Finished` o
`Diverged` la macchina continua libera.

`Divergence`: `Start(motivo)`, `Event { index, step, what }` (registri o
byte della console diversi prima di un ingresso), `Missed { index, step, at
}`, `Opaque { index, step, slot }`, `End { what }` (istruzioni, console,
CPU, RAM, dispositivi, MMU e TLB). `Display` in italiano.

Lettura dello stato: `read_phys(pa, buf) -> bool` (RAM),
`translate(va) -> Option<u64>` e `read_virt(va, buf) -> Result<(), u64>`
(tabelle correnti all'EL corrente, senza TLB, solo RAM), `registers_text()`
(istruzioni, PC, SP, NZCV, EL, DAIF, X0–X30, SP_EL0/1, ELR, SPSR, ESR, FAR,
VBAR, SCTLR, TCR, TTBR0/1, TPIDR_EL0).

## Formato del log (versione 1)

```
"VETROREC"  u32 LOG_VERSION  u64 hash della configurazione
u64 lunghezza del contenuto  u64 hash64 del contenuto
contenuto: sezioni HEAD, EVTS, KEYF, END  (in quest'ordine)
```

(`vetro_snapshot::encode_container` / `decode_container`.)

| Sezione | Campi |
|---|---|
| `HEAD` | u32 versione degli snapshot dei keyframe; u64 RAM, u64 ora, u64 seme; bool JIT; u64 intervallo dei keyframe; impronta di partenza |
| `EVTS` | seq di eventi: u64 istruzione, u64 hash della CPU, u64 byte della console; u8 0 = ingresso (codifica sotto), 1 = opaco (opt u32 slot) |
| `KEYF` | seq di keyframe: u64 istruzione, u64 byte e u64 hash della console, bytes snapshot (`Machine::save`) |
| `END ` | impronta finale |

Impronta: 7 × u64 (istruzioni, CPU, MMU, piattaforma, RAM, byte della
console, hash della console). Hash: `hash64` di `Cpu::save`, `Mmu::save`,
`Virt::save`, dei byte della RAM; console FNV-1a a 64 bit.

Ingressi: u8 tipo, poi 0 `Console` bytes; 1 `Keyboard`, 2 `Pointer` seq di
(u16 tipo, u16 codice, u32 valore); 3 `Gpio` u32, bool; 4 `Display` 3 × u32;
5 `NetFrame` bytes; 6 `NetLink` bool; 7 `Vsock` u8 operazione (0 listen, 1
unlisten, 2 accept, 3 connect: u32 porta; 4 send: conn, bytes; 5 recv:
conn, u64; 6 shutdown_send, 7 close, 8 reset, 9 release: conn; 10
transport reset), conn = u32 porta dell'host, u32 porta del guest; 8
`HostNet` u8 operazione (0 connect: u16; 1 send: u64 id, bytes; 2 recv: u64
id, u64; 3 shutdown, 4 abort, 5 release: u64 id).

`Log::decode` controlla magia, versione, somma, sezioni, eventi in ordine
(non decrescenti, dentro l'intervallo di partenza e fine) e keyframe
crescenti.

## CLI

`vetro boot ... --record=FILE [--keyframes=N]` (default 100 milioni di
istruzioni, 0 = nessuno), `--replay=FILE` (config dal log; partenza da
`--kernel`, `--restore` o dal keyframe iniziale), `--goto=N` e
`--dump=VA:BYTE`. Codici: 0 replay identico, 1 diverso, 2 errore d'uso o
di file.

## Nel browser (ADR 0023)

vetro-wasm (ABI 8, `docs/specs/wasm.md`): `vetro_record_start/stop`,
`vetro_rr_status`, `vetro_log_encode/load/info/events`, keyframe spostabili
fuori dalla macchina (`vetro_log_keyframe`, `_take`, `_put`, `_for`),
`vetro_replay_start(step)` dal keyframe più vicino (il salto a
un'istruzione: poi `vetro_run` con il quanto limitato fino a lì),
`vetro_registers_text`, `vetro_read_virt`, `vetro_translate`,
`vetro_read_phys`. In JS `Recording` (`web/node/recording.mjs`) tiene i
keyframe in un `SnapshotStore` (OPFS `vetro-recordings/` nel Worker: `kf-<i>`
e `log` senza i byte dei keyframe) e ricompone il file completo per il
download. L'app: pannello "Registrazione" (registra, rigioca, scarica,
carica, vai all'istruzione, continua, registri, dump di memoria) e "vai qui"
sugli ingressi della timeline.

## Test

`vetro-machine` `record::tests` e `machine::record::tests`;
`tests/boot/tests/replay.rs` (kernel guest, `VETRO_REQUIRE_GUEST_KERNEL=1`,
release); `crates/vetro-cli/tests/boot_replay.rs`; nel browser
`cargo test -p vetro-wasm` (`replay::tests`), `tests/web/replay.mjs` e
`tests/web/browser-analysis.mjs`.
