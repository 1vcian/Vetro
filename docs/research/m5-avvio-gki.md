# M5 — avvio del kernel GKI android15-6.6 sulla macchina di Vetro (2026-09-25)

Esperimento: l'immagine dell'emulatore SDK Android 15 arm64 "default"
(`arm64-v8a-35_r02.zip`, kernel `6.6.30-android15-8`, build
`AE3A.240806.019`) avviata sulla macchina di Vetro sotto
`qemu-system-aarch64` 10.0 e sotto `vetro boot`, con la stessa
configurazione, per vedere fin dove arrivano e dove divergono. Solo uso
locale: l'immagine è sotto licenza SDK e non va committata né distribuita
(`tools/android-emu/README.md`).

## Configurazione comune
- Macchina: `virt,gic-version=3,its=off`, Cortex-A53, una CPU, 2 GiB,
  virtio-mmio moderno, niente rete, niente GPU né input (i `-device` di
  QEMU e `--no-devices` di Vetro).
- Avvio diretto: `Image` (il `kernel-ranchu` decompresso) e `ramdisk.img`
  così com'è: due cpio (ramdisk generico e ramdisk del vendor con
  `fstab.ranchu` e i moduli virtio) in un flusso LZ4 legacy; il GKI ha
  `CONFIG_RD_LZ4=y`. `virtio_mmio`, `virtio_blk` e il resto sono moduli,
  caricati dalla prima fase di init (`modules.load`).
- Dischi (copy-on-write: `snapshot=on` in QEMU, `CowBackend` sopra il file
  in Vetro), nell'ordine dei `-device`; il primo va nello slot virtio-mmio
  più alto e Linux numera i dischi per indirizzo crescente:
  | Slot | Indirizzo | File | Linux | Uso |
  |---|---|---|---|---|
  | 31 | `a003e00` | `userdata.img` (ext4 vuoto, 2 GiB) | vdc | `/data` (il fstab vuole `/dev/block/vdc`) |
  | 30 | `a003c00` | `encryptionkey.img` (GPT "metadata") | vdb | `/metadata` (il fstab vuole `a003c00.virtio_mmio`) |
  | 29 | `a003a00` | `system.img` (GPT "vbmeta" + "super") | vda | partizioni logiche system, system_ext, product, vendor, system_dlkm |
  Il `vendor.img` separato non serve: vendor sta in super.
- Riga di comando (`tools/android-emu/cmdline.sh`): `console=ttyAMA0
  nokaslr 8250.nr_uarts=1 printk.devkmsg=on loop.max_part=7
  androidboot.hardware=ranchu androidboot.qemu=1
  androidboot.selinux=permissive androidboot.boot_devices=a003a00.virtio_mmio
  androidboot.console=ttyAMA0` più i tre `androidboot.vbmeta.*` di
  `VerifiedBootParams.textproto`. `nokaslr` rende confrontabili gli
  indirizzi (entrambi danno `kaslr-seed` nel DTB). Il fstab non chiede
  AVB/verity, quindi basta così.
- `/data`: con un disco a zero init non lo formatta (niente `formattable`),
  vold trova la cifratura dei metadati senza chiave e init riavvia in
  recovery (`init_user0_failed`). Con un ext4 vuoto (`mkfs.ext4`, come fa
  l'host dell'emulatore) vold genera la chiave, monta `/data` su
  dm-default-key e il boot prosegue. Senza `/data` del tutto: `bpfloader`
  fallisce e init riavvia (`netbpfload-missing`).

## Fin dove arrivano
Tempi in secondi di guest (timestamp di printk):

| Tappa | QEMU | Vetro |
|---|---|---|
| `/init` (prima fase) | 0,53 | 2,25 |
| seconda fase di init | 1,70 | 3,73 |
| `mount_all --late` riuscito (vold, `/data` cifrato) | 19,6 | 39,2 |
| `init_user0` riuscito | 38,0 | 106,3 |
| `bpfloader` finito (status 0) | 60,2 | 135,3 |
| **zygote avviato** | **60,6** | **135,8** |
| surfaceflinger avviato | 71,7 | 151,4 |
| surfaceflinger abortisce, zygote riavviato | 84,3 | 167,5 |

Tutti e due arrivano allo stesso punto: **zygote parte, poi surfaceflinger
abortisce (SIGABRT) e init riavvia zygote a ciclo** (ogni ~16 s di guest in
QEMU, ~20 s in Vetro). Il motivo è lo stesso nei due: l'immagine
dell'emulatore compone via gfxstream e dispositivi goldfish (pipe, sync,
address space) che la virt non ha. Anche con `-device virtio-gpu-device`
(2D, `number of cap sets: 0`) surfaceflinger abortisce allo stesso modo in
QEMU. Falliscono allo stesso modo, nei due, anche i servizi goldfish:
`vendor.sensors-hal-multihal` (SIGABRT), `goldfish-logcat` (status 6),
`qemu-props`, `misctrl`, `kcmdlinectrl`; `keystore2` va in crash solo
senza `/data`. L'insieme dei servizi morti e dei segnali coincide.

Il tempo di guest di Vetro è più lungo perché la sua CPU virtuale va a 100
MHz nominali (un'istruzione ogni 10 ns, ADR 0011): lo stesso lavoro che in
QEMU (TCG col tempo reale dell'host, circa 1 miliardo di istruzioni al
secondo di guest) dura da 2 a 10 volte di più in secondi di guest. Vetro
nativo esegue 35–55 milioni di istruzioni al secondo reale: zygote dopo
4–5 minuti reali (13,6 miliardi di istruzioni), QEMU in Docker dopo circa
1,5 minuti.

## Divergenze trovate
Confronto riga per riga senza tempi (`tools/android-emu/compare.py`),
prima divergenza e righe presenti in un solo log.

1. **GPIO PL061 e `gpio-keys` mancanti in Vetro — corretta.** Il DTB di
   QEMU virt ha `pl061@9030000` (SPI 7) e `gpio-keys/poweroff` (linea 3,
   KEY_POWER). Il GKI li usa: `input: gpio-keys as .../input0`, e init
   trova `/dev/input` (`EVIOCSMASK not supported`); sotto Vetro init
   stampava `Could not add watch for /dev/input` e Android non aveva il
   tasto di accensione. Ora `vetro-platform` ha il PL061 (logica di
   `hw/gpio/pl061.c`, linee non pilotate a 0 come nella virt), il device
   tree ha gli stessi nodi di QEMU e l'host preme il tasto con
   `Board::gpio_input(3, ..)`. Sotto Vetro compaiono le stesse righe di
   QEMU (`PL061 GPIO chip registered`, `input: gpio-keys`, `EVIOCSMASK`).
   Test: `pl061::tests` (6, tra cui entrambi i fronti come gpio-keys, livello,
   maschera di DATA, ID PrimeCell), `virt::tests::tasto_di_spegnimento_sullo_spi_7`,
   `board::tests::tasto_di_spegnimento_dall_host`, e i nodi nel test del DTB
   (`proprieta_della_piattaforma`, valori presi dal DTB di QEMU 10.0 con
   `dumpdtb`). Il kernel guest di M3 non ha i driver GPIO: i suoi log non
   cambiano.
2. **`jitterentropy: Initialization failed ... requirements: 9` solo in
   Vetro — nota, non corretta.** Errore 9 è `JENT_EHEALTH`: il test di
   ripetizione della sorgente di entropia a jitter fallisce perché in Vetro
   il contatore (CNTVCT) è una funzione esatta delle istruzioni, quindi due
   misure dello stesso ciclo danno lo stesso tempo. È la conseguenza voluta
   del determinismo (ADR 0010/0011); non verificato con `-icount` di QEMU.
   Il GKI non è in modalità FIPS, quindi il fallimento non è fatale e il
   boot prosegue identico. Renderlo "vero" vorrebbe un rumore deterministico
   nel contatore: da decidere con un ADR, se servirà.
3. **Differenze note già in `tests/boot`** (`KNOWN_DIFFERENCES`): QEMU dichiara
   gli LPI senza ITS; Vetro non ha AArch32 (manca "32-bit EL0/EL1 Support":
   l'immagine è solo 64 bit, `zygote_secondary` non esiste in nessuno dei
   due); il DTB di QEMU ha anche PCIe (`pci-host-generic`, nessun
   dispositivo), fw-cfg, flash e PMU (`hw perfevents: armv8_pmuv3`).
   L'ora dell'RTC è quella fissa di Vetro (2026-01-01), le chiavi di
   cifratura di `/data` sono casuali in tutti e due.
4. **Effetti dei tempi, non errori:** in Vetro init stampa più righe
   `Command ... took Nms` (le stampa oltre 50 ms), compare una volta
   `sched: RT throttling activated` (un thread real-time supera il 95% di
   un secondo di guest su una CPU più lenta), l'ordine dei `loopN` di apexd
   cambia, e in un avvio `prng_seeder` ha scritto su kmsg (logd non ancora
   pronto) il fallimento che sotto QEMU va in logcat: in tutti e due manca
   `/dev/hw_random`, perché nessuno dei due ha virtio-rng.

Nessuna istruzione mancante, nessun registro di sistema sbagliato, nessun
crash in più: fino al ciclo di surfaceflinger il comportamento di Vetro
coincide con QEMU, compresi i processi che vanno in crash (stessi segnali,
stessi messaggi d'abort e, per `keystore2` senza `/data`, lo stesso punto
d'abort nel tombstone: `abort+168` in libc con la stessa catena Rust).

## Cosa manca per andare oltre
- **Grafica:** l'immagine dell'emulatore vuole gfxstream + goldfish; con
  una virtio-gpu 2D non compone. Serve l'immagine nostra (fork di
  `vsoc_arm64_only`, `m5-immagini-android.md`) con SwiftShader +
  drm_hwcomposer + minigbm sulla virtio-gpu 2D di Vetro, o un'immagine
  ranchu con la composizione software (`ro.hardware.egl=swiftshader`,
  HWC su DRM). Non è un difetto di Vetro: QEMU si ferma nello stesso punto.
- **virtio-rng** (per `prng_seeder` e l'entropia del kernel): dispositivo
  piccolo, da alimentare con un generatore deterministico seminato da
  `MachineConfig::seed`. Da confrontare con `-device virtio-rng-device`.
- **Dispositivi goldfish** (pipe, sync, address space, batteria): servono
  solo all'immagine SDK, non a quella nostra.
- **PMU** (`armv8_pmuv3`): serve a simpleperf/perfetto, non all'avvio.
- **PCIe (ECAM)** per le immagini Cuttlefish senza modifiche: vedi
  `m5-immagini-android.md`, da decidere con un ADR.
- **Velocità:** a circa 50 MIPS reali il primo zygote arriva dopo 4–5 minuti;
  il JIT di sistema (M4, in corso) è il prossimo moltiplicatore.

## Riproduzione
`tools/android-emu/README.md`: preparazione dei file, `qemu.sh`, `vetro.sh
[secondi di guest]` e `compare.py`. `vetro boot` ha le opzioni nuove
`--no-devices`, `--disk=FILE` (ripetibile, copy-on-write in memoria),
`--guest-secs=N` e `--stats`; il test
`crates/vetro-cli/tests/boot_disk.rs` le prova col kernel guest di M3
(lettura, scrittura, file intatto).
