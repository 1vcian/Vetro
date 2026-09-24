# Spec — vetro-platform

## Perimetro
Dispositivi della piattaforma `virt` per M3 (modalità sistema): bus MMIO,
GICv3, timer generico, UART PL011, RTC PL031, trasporto virtio-mmio con
virtio-blk, virtio-net e virtio-console, device tree. La mappa ricalca `qemu-system-aarch64 -M virt`, così kernel e
device tree si confrontano con QEMU senza adattamenti. Una sola CPU.

## Mappa della memoria (`map.rs`)
| Regione | Base | Dimensione | Interrupt (INTID) |
|---|---|---|---|
| GICD (distributore) | `0x0800_0000` | `0x1_0000` | — |
| GICR (redistributore, CPU 0: frame RD + SGI) | `0x080A_0000` | `0x2_0000` | — |
| UART PL011 | `0x0900_0000` | `0x1000` | SPI 1 (33), livello |
| RTC PL031 | `0x0901_0000` | `0x1000` | SPI 2 (34), livello |
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
- `Virt`: bus già montato + `timer`; `gic_mut()`, `uart_mut()`, `rtc_mut()`;
  `update_irqs(cntpct)` porta al GIC le linee di timer, UART, RTC e dei 32
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
  - I dispositivi si raggiungono con `virtio_mut(slot)?.device_as_mut::<T>()`
    e i backend con `backend_as_mut::<T>()`.
- `FdtBuilder`: `begin_node`, `end_node`, `prop_u32`, `prop_u64`,
  `prop_u32_list`, `prop_u64_list`, `prop_str`, `prop_strs`, `prop_bytes`,
  `prop_empty`, `reserve_memory`, `boot_cpuid`, `finish() -> Result<Vec<u8>, FdtError>`.
  Formato DTB v17 (last_comp 16), stringhe deduplicate.
- `virt_dtb(&VirtDtbConfig) -> Vec<u8>`: memoria, cpus (`enable-method =
  "psci"`), psci (`arm,psci-1.0`, metodo `hvc` di default), timer
  (`arm,armv8-timer`), GIC (`arm,gic-v3`), clock fisso 24 MHz, PL011, PL031,
  32 virtio-mmio, `chosen` con `bootargs`, `stdout-path = "/pl011@9000000"`
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
  scaricati a pezzi di M5).
- **virtio-net**: MAC, STATUS, MRG_RXBUF (disattivabile); niente offload,
  coda di controllo né multiqueue. Header di 12 byte a zero tranne
  num_buffers. Il backend si interroga solo con buffer liberi e link su;
  con MRG_RXBUF un frame che non entra aspetta altri buffer, senza si
  scarta. TX oltre 64 KiB + header: errore della coda.
- **virtio-console**: una porta, senza MULTIPORT (code 0 rx e 1 tx),
  EMERG_WRITE offerta; max_nr_ports = 1.

## Invarianti
- Nessuna dipendenza da `std::fs`, `std::process`, thread, né crate
  esterni: compila in `wasm32-unknown-unknown`. I dispositivi virtio
  parlano con l'esterno solo tramite i trait dei backend e `GuestRam`.
- **Determinismo**: nessun dispositivo legge l'orologio dell'host; il tempo
  (CNTPCT, secondi dell'RTC) e l'input della UART entrano solo come
  argomenti, dall'unico punto registrabile del motore; lo stesso vale per i
  backend virtio (frame, byte della console, dati del disco), che il motore
  implementa sopra quel punto.
- Le regioni del bus non si sovrappongono; un accesso a cavallo della fine
  di una regione non raggiunge nessun dispositivo.

## Test
`cargo test -p vetro-platform`: test unitari per modulo (bus, PL011, PL031,
timer, GIC, virtio, FDT con parser minimo del DTB, piattaforma montata).
I test virtio usano un driver di prova (`virtio/testdrv.rs`) che fa ciò che
fa Linux su una RAM finta: negoziazione, setup delle code, catene dirette e
indirette, notifiche, used ring con aggiornamento di used_event,
interrupt; un test lo fa passare dal bus di `Virt` fino all'INTID nel GIC.
Il DTB prodotto è stato decompilato anche con `dtc -I dtb -O dts` senza
errori (verifica manuale, non in CI). Il confronto con QEMU arriva in M3,
avviando lo stesso kernel su entrambi.
