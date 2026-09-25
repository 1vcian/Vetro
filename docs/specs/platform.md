# Spec — vetro-platform

## Perimetro
Dispositivi della piattaforma `virt` per M3 (modalità sistema): bus MMIO,
GICv3, timer generico, UART PL011, RTC PL031, trasporto virtio-mmio con
virtio-blk, virtio-net e virtio-console, device tree. Per M5: virtio-gpu 2D,
virtio-input (tastiera, tablet, touchscreen), virtio-vsock e GPIO PL061
con il tasto di spegnimento (`gpio-keys`, visto dal GKI di Android). La mappa ricalca `qemu-system-aarch64 -M virt`, così kernel e
device tree si confrontano con QEMU senza adattamenti. Una sola CPU.

## Mappa della memoria (`map.rs`)
| Regione | Base | Dimensione | Interrupt (INTID) |
|---|---|---|---|
| GICD (distributore) | `0x0800_0000` | `0x1_0000` | — |
| GICR (redistributore, CPU 0: frame RD + SGI) | `0x080A_0000` | `0x2_0000` | — |
| UART PL011 | `0x0900_0000` | `0x1000` | SPI 1 (33), livello |
| RTC PL031 | `0x0901_0000` | `0x1000` | SPI 2 (34), livello |
| GPIO PL061 | `0x0903_0000` | `0x1000` | SPI 7 (39), livello |
| virtio-mmio, 32 slot | `0x0A00_0000` + k·`0x200` | `0x200` | SPI 16+k (48+k), fronte |
| RAM | `0x4000_0000` | configurabile | — |

Timer generico: PPI 27 (virtuale), PPI 30 (fisico non sicuro); nel device
tree compaiono anche 29 (fisico sicuro) e 26 (hypervisor), mai pilotati.
La RAM non passa dal bus MMIO: la gestisce la memoria della CPU/MMU.

## Interfaccia pubblica
- `trait MmioDevice: Any { read(&mut self, offset, size) -> u64; write(&mut self, offset, size, value) }`:
  `offset` relativo alla base della regione, `size` 1/2/4/8, valori nei bit
  bassi. `Any` serve al downcast lato host.
- `Bus`: `map(base, size, nome, Box<dyn MmioDevice>) -> Result<DeviceId, BusError>`
  (rifiuta sovrapposizioni e intervalli vuoti), `read(addr, size) -> Option<u64>`,
  `write(addr, size, value) -> bool`. `None`/`false` = nessun dispositivo
  copre tutto l'accesso: la CPU in M3 lo trasforma in abort esterno.
  `device::<T>(id)` / `device_mut::<T>(id)` per l'accesso tipizzato.
- `Pl011`: `push_input(&[u8])`, `output()`, `take_output()`,
  `pending_input()`, `irq_level()`.
- `Pl031`: `new(now_secs)`, `set_time(now_secs)`, `count()`,
  `seconds_to_alarm()`, `irq_level()`.
- `Pl061`: `new()`, `set_input(linea, livello)` (ingressi pilotati
  dall'host), `outputs()`, `irq_level()`; `pl061::POWER_KEY_LINE` = 3.
- `GenericTimer` (`cntfrq`, `cntvoff`, canali `phys` e `virt`):
  `cntp_ctl/cval/tval`, `cntv_ctl/cval/tval` e relativi `set_*`, tutti col
  valore di CNTPCT passato dall'esterno; `irq_lines(cntpct)` restituisce
  `[(27, livello), (30, livello)]`; `next_deadline(cntpct)` il prossimo
  valore di CNTPCT a cui una linea sale.
- `Gic`: MMIO (distributore e redistributore) come `MmioDevice` su un'unica
  regione `GICD_BASE .. GICR_BASE + 0x2_0000` (`gic::MMIO_SIZE`), il buco
  in mezzo è RAZ/WI. Linee: `set_irq_level(intid, livello)`,
  `set_spi_level(spi, livello)`, `send_sgi(intid)`. Uscita: `irq_line()`.
  Registri di sistema, da chiamare da MRS/MSR:
  `read_iar1`, `write_eoir1`, `write_dir`, `read_hppir1`, `read/write_pmr`,
  `read/write_ctlr`, `read/write_igrpen1`, `read/write_sre`,
  `read/write_bpr1`, `read_rpr`, `read/write_ap1r0`, `write_sgi1r`.
- `Virt`: bus già montato + `timer`; `gic_mut()`, `uart_mut()`, `rtc_mut()`,
  `gpio_mut()`; `update_irqs(cntpct)` porta al GIC le linee di timer, UART,
  RTC, GPIO e dei 32
  slot virtio; `irq_line()`. Virtio: `attach_virtio(slot, Box<dyn VirtioDevice>)`,
  `attach_virtio_next(dev) -> slot` (slot libero più alto, come QEMU: il primo
  dispositivo va nello slot 31), `virtio(slot)` / `virtio_mut(slot)` ->
  `VirtioMmio`, `service_virtio(&mut dyn GuestRam)`. Errori: `VirtioSlotError`
  (`NoSuchSlot`, `Occupied`, `Full`).
- Virtio (`virtio/`):
  - `trait GuestRam { read(&self, addr, &mut [u8]); write(&mut self, addr, &[u8]) }`
    -> `Result<(), RamError>`: la RAM del guest per il DMA, su indirizzi
    fisici; `GuestRamExt` aggiunge le letture/scritture LE; `VecRam` è una
    RAM contigua in un `Vec`.
  - `VirtioMmio`: il trasporto (`MmioDevice`), `empty()` o `new(dev)`,
    `set_device`, `device_as[_mut]::<T>()`, `service(&mut dyn GuestRam)`,
    `irq_level()`, `signal_config_change()`, `status()`,
    `interrupt_status()`, `last_error()`, `without_features(mask)`.
  - `trait VirtioDevice: Any`: `device_id`, `features` (solo i bit del
    dispositivo), `queue_max_sizes`, `read_config`/`write_config`,
    `negotiate(features) -> bool`, `reset`, `service(&mut ServiceCtx)`.
    `ServiceCtx` espone code, RAM, feature negoziate e `config_changed()`.
  - `Virtqueue`: `pop` -> `DescChain` (buffer leggibili e scrivibili, con
    `read`/`write`/`read_to_vec` su spazio contiguo), `push_used`,
    `available`, `rewind`.
  - `VirtioBlk::new(Box<dyn BlockBackend>, VirtioBlkConfig)`;
    `trait BlockBackend: Any { size, read_only, read_sectors, write_sectors, flush }`
    (settori da 512, `BlockError::{Io, OutOfRange, ReadOnly, NotReady}`);
    `MemBackend` (in memoria, anche in sola lettura) e `CowBackend<B>`
    (copy-on-write a cluster da 4 KiB sopra una base usata solo in lettura).
  - `VirtioNet::new(Box<dyn NetBackend>, mac)`, `with_mrg_rxbuf`,
    `set_link_up`, `rx_dropped`; `trait NetBackend: Any { send(&[u8]); recv() -> Option<Vec<u8>> }`
    con frame ethernet nudi; `QueueNet` in memoria.
  - `VirtioConsole::new(Box<dyn ConsoleBackend>)`;
    `trait ConsoleBackend: Any { write(&[u8]); read(&mut [u8]) -> usize }`;
    `BufferConsole` in memoria.
  - `VirtioGpu::new(Box<dyn DisplayBackend>, GpuConfig)` (`gpu.rs`):
    `GpuConfig { scanouts, width, height, edid, monitor: EdidInfo, max_hostmem }`,
    default 1 scanout 1280x800 con EDID e 256 MiB (come QEMU);
    `set_display(scanout, w, h)` (ridimensionamento chiesto dall'host),
    `frame(scanout) -> Option<Frame>`, `cursor(scanout)`, `set_backend`,
    `resource_count`, `hostmem`, `backend_as[_mut]::<T>()`.
    `trait DisplayBackend: Any { update(scanout, &Frame, dirty: Rect); disable(scanout); cursor(scanout, &Cursor) }`;
    `Frame { width, height, stride, format: PixelFormat, data }` con
    `rgba(x, y)`; `PixelFormat` (gli 8 formati 2D, `to_rgba`); `MemDisplay`
    (in memoria, RGBA, `pixel(scanout, x, y)`).
  - `edid::generate(&EdidInfo, size) -> Vec<u8>` (`edid.rs`): l'EDID del
    monitor virtuale, byte per byte quello di QEMU; `EdidInfo` (produttore,
    nome, seriale, dimensioni, modo preferito, limiti, refresh).
  - `VirtioInput::new(InputConfig)` (`input.rs`): `inject(&[InputEvent])`,
    `key(code, down)`, `move_abs(x, y)`, `touch(slot, Option<(x, y)>)`,
    `pending`, `dropped`, `leds`, `take_status`, `config`.
    `InputConfig::keyboard()`, `tablet()`, `multitouch()` (i profili di
    QEMU) o costruito con `new(nome)`, `serial`, `devids`, `props`,
    `events(tipo, codici, min_len)`, `abs(asse, AbsInfo)`.
    `InputEvent { ty, code, value }`; costanti `EV_*`, `BTN_*`, `ABS_*`, `LED_*`.
  - `VirtioVsock::new(guest_cid)` (`vsock.rs`), l'host (CID 2) dentro il
    dispositivo: `listen(port)`, `unlisten`, `accept(port) -> Option<VsockConn>`,
    `connect(guest_port) -> VsockConn`, `send(c, &[u8])`, `recv(c, max)`,
    `available`, `unsent`, `eof`, `shutdown_send`, `close`, `reset`,
    `release`, `state(c) -> Option<VsockState>`, `connections`,
    `transport_reset`, `guest_cid`, `dropped`.
    `VsockConn { host_port, guest_port }`; `VsockState::{Connecting,
    Connected, Closing, Closed}`; `VsockError::{NotFound, Closed, PortInUse}`.
  - I dispositivi si raggiungono con `virtio_mut(slot)?.device_as_mut::<T>()`
    e i backend con `backend_as_mut::<T>()`.
- `vetro-machine` monta i dispositivi di M5: `Devices { gpu: Option<GpuConfig>,
  keyboard, pointer: Option<Pointer>, vsock_cid: Option<u64> }` con
  `Machine::with_devices(&MachineConfig, &Devices)` (`Machine::new` usa
  `Devices::default()`: GPU 1280x800, tastiera, tablet, niente vsock;
  `Devices::none()` è la macchina di M3). Ordine di montaggio fisso
  (GPU, tastiera, puntatore, vsock), ciascuno nello slot libero più alto:
  31, 30, 29, 28 come i `-device` di QEMU. `Machine::slots()`,
  `Machine::gpu/keyboard/pointer/vsock(|d| ...)` e `Machine::device::<T>(slot, f)`
  danno all'host il dispositivo e lo segnano da servire prima della
  prossima istruzione. `Devices` sta fuori da `MachineConfig` perché
  `vetro-wasm` costruisce `MachineConfig` elencando i campi.
- `FdtBuilder`: `begin_node`, `end_node`, `prop_u32`, `prop_u64`,
  `prop_u32_list`, `prop_u64_list`, `prop_str`, `prop_strs`, `prop_bytes`,
  `prop_empty`, `reserve_memory`, `boot_cpuid`, `finish() -> Result<Vec<u8>, FdtError>`.
  Formato DTB v17 (last_comp 16), stringhe deduplicate.
- `virt_dtb(&VirtDtbConfig) -> Vec<u8>`: memoria, cpus (`enable-method =
  "psci"`), psci (`arm,psci-1.0`, metodo `hvc` di default), timer
  (`arm,armv8-timer`), GIC (`arm,gic-v3`), clock fisso 24 MHz, PL011, PL031,
  PL061 (phandle 3) con `gpio-keys/poweroff` (linea 3, KEY_POWER, come
  QEMU), 32 virtio-mmio, `chosen` con `bootargs`, `stdout-path = "/pl011@9000000"`
  e initrd opzionale.

## Scelte e limiti
- **GICv3**: un solo stato di sicurezza (GICD_CTLR.DS = 1) e ARE = 1, entrambi
  RAO/WI. **Solo gruppo 1 non sicuro**: IGROUPR si memorizza ma un interrupt
  in gruppo 0 non viene mai segnalato (niente FIQ); IGRPMODR/NSACR RAZ/WI.
  Niente LPI/ITS. 256 SPI (ITLinesNumber = 8, IDbits = 9). Interfaccia CPU
  a 5 bit di priorità (PRIbits = 4) come QEMU: PMR maschera `0xF8`, BPR1
  minimo 3. ICC_CTLR_EL1.CBPR è RAZ/WI (BPR0 non modellato); EOImode
  scrivibile (con EOImode = 1 serve ICC_DIR_EL1). SGI1R: con una CPU conta
  solo affinità 0.0.0 e bit 0 della TargetList. GICR_WAKER fa l'handshake
  ma non blocca la consegna. Selezione: priorità numericamente più bassa,
  a parità l'INTID più basso; consegna se IGRPEN1, priorità < PMR e
  priorità di gruppo < priorità in esecuzione. SPI instradati alla CPU 0
  se IROUTER ha affinità 0.0.0.0 o IRM = 1.
- **PL011**: trasmissione istantanea anche con UART spenta (earlycon), come
  QEMU; ricezione solo con UARTEN e RXE, altrimenti i byte restano nella
  coda dell'host. RX a soglia 1 (come QEMU), IFLS solo memorizzato; TXRIS si
  alza a ogni scrittura in DR e scende con ICR. FIFO RX da 16 con FEN,
  da 1 senza. Loopback (CR.LBE) supportato.
- **PL031**: CR legge sempre 1; qualunque scrittura in ICR azzera
  l'interrupt; l'allarme scatta quando DR raggiunge MR avanzando o subito
  se MR = DR dopo una scrittura di MR o LR (come QEMU).
- **PL061** (come `hw/gpio/pl061.c` nella virt di QEMU): 8 linee, quelle
  non pilotate valgono 0 (`pulldowns = 0xff`); DATA con maschera nei bit
  9:2 dell'offset, scrive solo le uscite; interrupt come `pl061_update`
  (fronte: IBE o IEV sul cambio di un ingresso; livello: RIS si riaccende
  finché attivo; IC azzera); linea = RIS & IE. Accessi fino a 4 byte (Linux
  usa `readb`/`writeb`); niente registri Luminary. L'host preme il tasto con
  `Board::gpio_input(3, true/false)` di `vetro-machine`, che segna le linee
  da aggiornare prima della prossima istruzione.
- **Timer**: ISTATUS = ENABLE && contatore >= CVAL (senza segno), 0 con
  ENABLE spento; TVAL a 32 bit con segno (come QEMU). CNTFRQ di default
  62,5 MHz.
- **virtio-mmio** (versione 2, virtio 1.2 §4.2.2): VendorID `0x554D4551`
  come QEMU; registri sotto 0x100 solo a 32 bit allineati, i registri di
  sola scrittura si leggono 0; feature del trasporto VERSION_1
  (obbligatoria: senza, FEATURES_OK non resta in Status), INDIRECT_DESC,
  EVENT_IDX; niente memoria condivisa (SHMLen/SHMBase = -1). La RAM non
  passa dal bus: QueueNotify non fa lavoro, lo fa `service`, che il motore
  chiama dopo gli accessi MMIO agli slot virtio e periodicamente (dati in
  arrivo dai backend), prima di `update_irqs`. Niente lavoro prima di
  DRIVER_OK. Linea di interrupt = `InterruptStatus != 0`, dichiarata a
  fronte di salita nel DTB come QEMU. Un errore nelle code (catena
  invalida, accesso fuori RAM, richiesta malformata) porta a
  DEVICE_NEEDS_RESET con interrupt di configurazione e ferma il dispositivo
  fino al reset.
- **Virtqueue split** (§2.7): QueueNum deve essere potenza di 2 e le aree
  allineate (16/2/4), altrimenti QueueReady resta 0. Scrivibili dopo
  leggibili; catene al massimo lunghe quanto la tabella; INDIRECT solo
  negoziato e solo sul descrittore di testa (NEXT su di esso ignorato, come
  QEMU), vietato dentro una tabella. EVENT_IDX completo: il dispositivo
  pubblica avail_event dopo ogni estrazione e notifica secondo
  `vring_need_event` su used_event; senza EVENT_IDX rispetta
  VRING_AVAIL_F_NO_INTERRUPT.
- **virtio-blk**: feature SIZE_MAX, SEG_MAX, BLK_SIZE, FLUSH e RO se il
  backend o la configurazione sono in sola lettura; code da 256, seg_max
  254 (come QEMU), size_max 1 MiB, blk_size 512. IN/OUT/FLUSH/GET_ID, il
  resto UNSUPP; accessi fuori capacità o non multipli di 512 e scritture in
  sola lettura: IOERR. I/O a pezzi da 64 KiB. `BlockError::NotReady` lascia
  la richiesta in sospeso e la riprova al `service` successivo (per i dischi
  scaricati a pezzi di M5). Con una richiesta in sospeso `Machine::run`
  restituisce `Stop::Blocked` senza eseguire istruzioni finché l'host non
  consegna i dati (ADR 0014): il tempo del guest non dipende dalla rete.
- **virtio-net**: MAC, STATUS, MRG_RXBUF (disattivabile); niente offload,
  coda di controllo né multiqueue. Header di 12 byte a zero tranne
  num_buffers. Il backend si interroga solo con buffer liberi e link su;
  con MRG_RXBUF un frame che non entra aspetta altri buffer, senza si
  scarta. TX oltre 64 KiB + header: errore della coda.
- **virtio-console**: una porta, senza MULTIPORT (code 0 rx e 1 tx),
  EMERG_WRITE offerta; max_nr_ports = 1.
- **virtio-gpu** (§5.7, come QEMU 10.0 `virtio-gpu-device` senza virgl):
  code 64 (controllo) e 16 (cursore); feature EDID; config events_read,
  events_clear (scrittura che azzera), num_scanouts, num_capsets = 0.
  Comandi GET_DISPLAY_INFO, GET_EDID, RESOURCE_CREATE_2D/UNREF,
  SET_SCANOUT, RESOURCE_FLUSH, TRANSFER_TO_HOST_2D,
  RESOURCE_ATTACH/DETACH_BACKING, UPDATE/MOVE_CURSOR; errori e controlli
  di QEMU (id 0 o doppio, formato, `max_hostmem`, rettangoli fuori dalla
  risorsa, scanout sotto 16x16, backing mancante o doppio, più di 16384
  voci, voci fuori RAM; capset, 3D e UUID ERR_UNSPEC; blob
  ERR_INVALID_PARAMETER). Fence: flag, fence_id e ctx_id riportati, già
  segnalato (esecuzione sincrona). Risorse nella memoria dell'host (stride
  = larghezza x 4, come pixman); TRANSFER come QEMU (un colpo se copre
  tutta la larghezza, altrimenti riga per riga da offset + stride x riga;
  ciò che il backing non copre resta invariato). SET_SCANOUT manda subito
  l'immagine intera al backend, FLUSH solo l'intersezione con ogni scanout
  che mostra la risorsa, UNREF e SET_SCANOUT 0 lo spengono. Cursore:
  immagine copiata solo da risorse 64x64. `set_display` = evento DISPLAY
  con interrupt di configurazione. Differenze da QEMU, solo su input che
  Linux non manda: comando più corto della sua struttura →
  ERR_INVALID_PARAMETER (QEMU risponde OK senza eseguire o blocca la coda).
  Risoluzione di default 1280x800, quella di QEMU, perché il test di avvio
  confronta i modi con QEMU; Android sceglierà la sua con `GpuConfig`
  (es. 1080x1920 verticale) e `set_display`.
- **EDID**: generatore equivalente a hw/display/edid-generate.c di QEMU
  (produttore RHT, "QEMU Monitor", modi standard/stabiliti/CTA, descrittore
  dettagliato con tempi proporzionali e 75 Hz, DisplayID oltre 4096 punti);
  verificato byte per byte con l'EDID letto dal guest sotto QEMU. Nome,
  produttore e seriale si cambiano con `EdidInfo` (profili dispositivo, M10).
- **virtio-input** (§5.8): code 64 (eventi, stato), nessuna feature;
  configurazione a finestra select/subsel (voce assente: tutto 0, come
  QEMU). Profili identici a `virtio-keyboard-device` (159 tasti, EV_REP,
  LED num/caps/scroll), `virtio-tablet-device` (ABS_X/Y 0..32767, pulsanti,
  rotella) e `virtio-multitouch-device` (MT slot 0..10, INPUT_PROP_DIRECT) di
  QEMU 10.0 e 8.2, verificati con EVIOCG* e /proc/bus/input/devices nel
  guest. Eventi prima di DRIVER_OK scartati (come QEMU); poi consegnati a
  rapporti interi (fino a SYN_REPORT) solo con buffer per tutto il
  rapporto: QEMU scarta il rapporto, Vetro lo tiene (al più 4096 eventi,
  poi scarta rapporti interi e conta). Coda di stato: EV_LED aggiorna
  `leds`, lunghezza used 0 (QEMU mette i byte letti).
- **virtio-vsock** (§5.10): code 128 (rx, tx, eventi), feature STREAM,
  config guest_cid (default 3). L'host è il dispositivo stesso (CID 2):
  REQUEST verso porta in ascolto → RESPONSE e coda di accept, altrimenti
  RST; pacchetti senza connessione o non stream → RST; CID sbagliati o
  lunghezze invalide scartati. Credito come Linux: l'host non supera
  `buf_alloc - (tx_cnt - fwd_cnt)` del guest, annuncia 256 KiB, manda
  CREDIT_UPDATE quando consuma e il guest vede meno di 64 KiB liberi o su
  CREDIT_REQUEST. SHUTDOWN completo del guest → RST; chiusura dell'host:
  SHUTDOWN dopo gli ultimi dati (anche se chiesta prima della RESPONSE).
  Pacchetti fino a 64 KiB e al buffer rx del guest. Ordine deterministico:
  pacchetti di controllo in ordine di nascita, poi dati per (porta host,
  porta guest); porte locali da 49152 in sequenza. TRANSPORT_RESET su
  richiesta (snapshot, M6). Non confrontato con QEMU: `vhost-vsock-device`
  vuole `/dev/vhost-vsock`, assente in Docker Desktop e nei runner.

## Invarianti
- Nessuna dipendenza da `std::fs`, `std::process`, thread, né crate
  esterni: compila in `wasm32-unknown-unknown`. I dispositivi virtio
  parlano con l'esterno solo tramite i trait dei backend, `GuestRam` e
  l'API host dei dispositivi (input, vsock, `set_display`), che il motore
  chiama dall'unico punto registrabile.
- **Determinismo**: nessun dispositivo legge l'orologio dell'host; il tempo
  (CNTPCT, secondi dell'RTC) e l'input della UART entrano solo come
  argomenti, dall'unico punto registrabile del motore; lo stesso vale per i
  backend virtio (frame, byte della console, dati del disco), che il motore
  implementa sopra quel punto.
- Le regioni del bus non si sovrappongono; un accesso a cavallo della fine
  di una regione non raggiunge nessun dispositivo.

## Test
`cargo test -p vetro-platform`: test unitari per modulo (bus, PL011, PL031, PL061,
timer, GIC, virtio, FDT con parser minimo del DTB, piattaforma montata).
GPU, input e vsock hanno test con il driver di prova (comandi ed errori,
formati, backing a pezzi, fence, cursore, ridimensionamento, reset; profili
di input contro i valori letti sotto QEMU, rapporti interi, coda piena,
LED; handshake, rifiuti, credito, chiusure, reset del trasporto,
determinismo); l'EDID è confrontato con i 256 byte letti sotto QEMU.
Con il kernel guest (`cargo test --release -p vetro-boot-tests`):
- `vetro.rs`: l'avvio con GPU, tastiera e tablet dà lo stesso log di QEMU
  con gli stessi `-device` (`QEMU_MACHINE`), compreso l'autotest che esegue
  `vetro-dev drm` (modi, dumb buffer, modeset, DIRTYFB, cursore), l'EDID da
  sysfs, `/proc/bus/input/devices` e le capacità evdev, e con virtio-net
  (in QEMU `-netdev user`) DHCP, rotte, DNS configurato e ping a gateway e
  DNS;
- `net.rs` (solo Vetro): la rete del guest con lo stack di `vetro-net` e il
  sinkhole (vedi `docs/specs/net.md`);
- `devices.rs` (solo Vetro, con vsock): l'host confronta ogni pixel dello
  scanout con il motivo disegnato dal guest e il cursore, inietta tasti e
  movimenti letti dal guest con evdev, vede il LED acceso dal guest, e
  scambia dati vsock nei due versi (300 KB verso il guest, oltre il suo
  credito; 200 KB di eco); due esecuzioni danno lo stesso log e le stesse
  istruzioni.
I test virtio usano un driver di prova (`virtio/testdrv.rs`) che fa ciò che
fa Linux su una RAM finta: negoziazione, setup delle code, catene dirette e
indirette, notifiche, used ring con aggiornamento di used_event,
interrupt; un test lo fa passare dal bus di `Virt` fino all'INTID nel GIC.
Il DTB prodotto è stato decompilato anche con `dtc -I dtb -O dts` senza
errori (verifica manuale, non in CI). Il confronto con QEMU arriva in M3,
avviando lo stesso kernel su entrambi.
