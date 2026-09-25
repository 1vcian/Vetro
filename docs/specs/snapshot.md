# `vetro-snapshot` e gli snapshot della macchina (M6)

Decisioni e motivi nell'ADR 0015. Qui l'interfaccia e il formato.

## Crate `vetro-snapshot`

Nessuna dipendenza; compila per wasm32. Lo usano `vetro-cpu`, `vetro-mmu`,
`vetro-platform`, `vetro-net` e `vetro-machine` (che lo ri-esporta come
`vetro_machine::vetro_snapshot`).

| Elemento | Significato |
|---|---|
| `trait Snapshot { save(&self, &mut Writer); restore(&mut self, &mut Reader) -> Result<()> }` | stato che si salva e si ripristina su un oggetto costruito con la stessa configurazione |
| `Writer` | `u8/u16/u32/u64/u128` LE, `bool` (0/1), `len_of`, `raw`, `bytes` (u64 + byte), `str`, `opt` (0 / 1 + valore), `seq` (u64 + elementi), `section(tag, f)` (4 byte + u64 + contenuto), `put(&impl Snapshot)` |
| `Reader` | le letture corrispondenti; `bool` e `opt` rifiutano valori diversi da 0/1; `len_of(min_item)` rifiuta lunghezze oltre i byte rimasti; `section(tag)` dà un lettore del solo contenuto, da chiudere con `finish()` (byte avanzati = errore); `expect_u64(cosa, atteso)` controlla un valore di configurazione; `get(&mut impl Snapshot)` |
| `compress(w, data)` / `decompress_into(r, out, visit)` / `decompress(r)` | dati grandi a blocchi da `BLOCK` = 4096 byte: u64 lunghezza, u64 blocchi presenti, poi per blocco u64 indice (crescente), u8 codifica (0 crudo, 1 LZ con u32 lunghezza), contenuto. I blocchi a zero non ci sono; `decompress_into` scrive solo i presenti (`out` va passato a zero, o chi chiama azzera gli assenti: `visit` dice quali sono) |
| `lz::compress` / `lz::decompress` | LZ77 di un blocco: gettoni LEB128, pari = `t >> 1` letterali, dispari = copia di `(t >> 1) + 4` byte da `d` indietro (LEB128, può sovrapporsi) |
| `hash64(&[u8]) -> u64` | FNV-1a su parole da 8 byte, rotazione e rimescolamento SplitMix64; stabile (somma di controllo e hash di configurazione) |
| `encode_file(config_hash, payload)` / `decode_file(bytes) -> (Header, payload)` | intestazione: `"VETROSNP"`, u32 `FORMAT_VERSION`, u64 hash della configurazione, u64 lunghezza, u64 `hash64` del contenuto. `decode_file` controlla magia, versione (prima di tutto il resto), lunghezza e somma |
| `Error` | `BadMagic`, `Version { found, expected }`, `Config { found, expected }`, `Checksum`, `Truncated`, `Section { expected, found }`, `Trailing { section, bytes }`, `Invalid(String)`; `Display` in italiano con il motivo |

`FORMAT_VERSION` oggi vale **2** (2: connessioni aperte dall'host, inoltro di porte).

## `vetro-machine`

- `Machine::save(&self) -> Vec<u8>`: non cambia la macchina; fra due
  `run` qualsiasi.
- `Machine::load_state(&mut self, &[u8]) -> Result<(), Error>`: sulla
  macchina costruita e completata come quella salvata (stessi
  `MachineConfig` e `Devices`, stessi dispositivi montati dopo con i loro
  backend esterni). Il JIT, se c'è, resta.
- `Machine::restore(&MachineConfig, &Devices, &[u8]) -> Result<Machine, Error>`:
  `with_devices` + `load_state`.
- `Machine::config_hash()`.

Sezioni del contenuto, in ordine:

| Etichetta | Campi |
|---|---|
| `MACH` | u64 istruzioni; opt u64 scadenza del timer; opt u64 scadenza della rete; bool WFI in sospeso; u64 CNTPCT; bool linee da aggiornare; bool virtio da servire; bool disco in attesa |
| `CPU ` | `Cpu`: 31 × u64 X, u64 SP, u64 PC, u32 NZCV, u64 TPIDR_EL0, u64 TPIDRRO_EL0, opt monitor (u64 indirizzo, u32 byte, u128 valore), 32 × u128 V, u32 FPCR, u32 FPSR, poi `SysState`: u8 modalità, `SysConfig` (u8 PSCI, u64 MPIDR, bool GICv3, u64 CBAR), u8 EL (0/1), bool SPSel, u32 DAIF, bool IL, 2 × u64 SP_ELx, 17 × u64 registri di EL1 (ELR, SPSR, VBAR, ESR, FAR, SCTLR, TCR, TTBR0, TTBR1, MAIR, CONTEXTIDR, CPACR, TPIDR, PAR, CNTKCTL, CSSELR, MDSCR), bool OSLK, u64 OSDLR, 20 × u64 DBGB/WVR/CR, u8 CLAIM, u64 PMUSERENR, opt u32 SError |
| `MMU ` | u64 PARange (controllato), 5 × u64 registri di traduzione, u32 voci del TLB, per voce u32 slot (crescente), u64 base VA, u64 dimensione, u64 base PA, u16 ASID, bool globale, u8 livello, u8 AP, bool UXN, bool PXN, u8 AttrIndx, u8 SH |
| `PLAT` | sezioni `TIMR`, `GIC3`, `UART`, `RTC `, `GPIO`, poi 32 × `VIO ` (u64 slot, trasporto, code, sezione `VDEV` col dispositivo se c'è) |
| `RAM ` | `compress` di tutta la RAM |

## `vetro-platform`

- `VirtioDevice::save_state(&self, &mut Writer)` e
  `restore_state(&mut self, &mut Reader) -> Result<()>` (obbligatori).
- `BlockBackend`, `NetBackend`, `ConsoleBackend`: `save_state` /
  `restore_state` con implementazione vuota (collegamento esterno).
  Salvano: `MemBackend` (scrivibile: il contenuto; in sola lettura: hash),
  `CowBackend` (u64 dimensione, cluster scritti, poi la base), `QueueNet`,
  `BufferConsole`.
- `Snapshot` per `Virt`, `VirtioMmio`, `Virtqueue`, `Gic`,
  `GenericTimer`, `Pl011`, `Pl031`, `Pl061`; `DescChain::save/restore`.
- `VirtioGpu::restore_state` rimanda al `DisplayBackend` immagine
  (o `disable`) e cursore di ogni scanout.

## `vetro-net`

`impl<U: Upstream + Snapshot> Snapshot for Stack<U>` e
`impl Snapshot for Sinkhole` (in `stack/snapshot.rs`,
`sinkhole/snapshot.rs`, `tcp/snapshot.rs`). La configurazione di stack e
sinkhole non si salva.

## `vetro-cpu`, `vetro-mmu`

`impl Snapshot for Cpu`, `SysState`, `SysConfig` (`vetro-cpu/src/snapshot.rs`);
`impl Snapshot for Mmu`, `Tlb`. Al ripristino la MMU svuota la cache delle
traduzioni recenti e fa crescere `Tlb::flushes`.

## Host

- `vetro boot --save-at=ISTRUZIONI:FILE` (ripetibile: primo confine fra
  quanti con almeno quelle istruzioni) e `--restore=FILE` (senza
  `--kernel`; stesse opzioni di macchina e stessi `--disk`).
- `vetro boot --disk=FILE --overlay=FILE`: overlay persistente (sotto).
- vetro-wasm ABI 4 (`docs/specs/wasm.md`): `vetro_snapshot_version`,
  `vetro_snapshot_save`, `vetro_snapshot_ptr`, `vetro_snapshot_clear`,
  `vetro_snapshot_restore` con codici `OK`, `BAD_MAGIC`, `VERSION`,
  `CONFIG`, `CORRUPT`.

## Test

- `vetro-snapshot`: interi, sezioni, lunghezze impossibili, intestazione
  (versione, somma, troncamento, magia), hash fissato, LZ (anche blocchi
  rovinati), blocchi a zero.
- `vetro-cpu` `cpu_completa_andata_e_ritorno`, `vetro-mmu`
  `tlb_nello_snapshot`, `vetro-net` `stack_ripristinato_prosegue_uguale`.
- `vetro-machine` `machine::snapshot::tests` (sonda bare-metal, casi
  limite, ripristino sopra macchina usata, snapshot incompatibili,
  virtio-blk in volo).
- `tests/boot/tests/snapshot.rs` (kernel guest, release,
  `VETRO_REQUIRE_GUEST_KERNEL=1`): equivalenza con tagli durante avvio,
  shell, rete, disco, GPU/input/vsock, interprete e JIT; misure.
- `crates/vetro-cli/tests/boot_snapshot.rs`, `vetro-wasm`
  `snapshot_dall_api`.

## Overlay persistente dei dischi (`vetro_snapshot::overlay`, ADR 0017)

File delle scritture del guest su un disco copy-on-write, uguale per la CLI
e per il browser (OPFS). Little endian.

| Parte | Contenuto |
|---|---|
| intestazione (4096 byte) | `"VETROCOW"`, u32 versione (1), u32 cluster (4096), u64 dimensione del disco, u64 generazione, u64 slot, u32 lunghezza dell'identità, identità (al più 4032 byte), zeri, u64 `hash64` dei primi 4088 byte all'offset 4088 |
| slot k (offset 4096 + k × 4112) | u64 cluster (`u64::MAX` = libero), u64 controllo = `hash64(dati) ^ rotl(cluster, 17) ^ costante`, 4096 byte di dati (l'ultimo cluster del disco completato con zeri) |

- `Overlay::load(file, identità, dimensione)`: file vuoto = overlay nuovo;
  identità o dimensione diverse = `LoadError::Mismatch`; magia, versione,
  cluster o intestazione rovinata = `LoadError::Corrupt` (in entrambi i casi
  si riparte da un overlay vuoto e il file si tronca). Slot con il controllo
  sbagliato, indice fuori dal disco o doppio: ignorati (contati in
  `damaged`) e liberi; slot oltre la fine del file: liberi.
- `Overlay::update(cambi)`: `(cluster, Some(dati))` scrive (se il controllo
  è cambiato) nello slot del cluster, o in uno libero, o in fondo;
  `(cluster, None)` libera lo slot. `Overlay::sync(tutti)`: lo stato completo
  (toglie i cluster assenti). Restituiscono `Patches { truncate, writes }`;
  se c'è qualcosa la generazione cresce e l'ultima scrittura è
  l'intestazione. `Patches::encode` è la codifica di `vetro_overlay_take`.
- `CowBackend` (vetro-platform): `take_dirty` (cluster scritti dal guest
  dall'ultima volta; non negli snapshot, svuotato da `restore_state`),
  `cluster`, `clusters`, `load_cluster`.
- CLI (`vetro-cli/src/disk.rs`): `FileOverlay::open/persist/after_restore`,
  identità `file:<nome>|<dimensione>|<mtime in ns>`; le scritture si
  applicano con `write_at`, l'intestazione dopo `sync_data`. `vetro boot`
  salva a ogni confine di quanto e all'uscita, e dopo `--restore` riallinea
  il file ai cluster dello snapshot.
