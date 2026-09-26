# ADR 0008 — Kernel guest di M3 e protocollo di avvio diretto

- Stato: accettata (M3, preparazione, 2026-09-24)

## Contesto
M3 chiede un kernel Linux arm64 con initramfs fino alla shell, prima sotto
l'oracolo (`qemu-system-aarch64 -M virt`) e poi sotto Vetro. Servono un
kernel riproducibile, un modo per verificarlo, e un caricatore che metta
kernel, initramfs e DTB dove li mette QEMU, così i due avvii si possono
confrontare.

## Decisione
- **Kernel:** Linux 6.18.53 (longterm), tarball di kernel.org fissato con
  SHA-256 in `tools/guest-kernel/build.sh`, senza patch. Build in container
  Alpine 3.22 arm64 su un volume Docker (i sorgenti hanno nomi che
  differiscono solo per maiuscole: APFS non li distingue). Data, utente e
  host della build sono fissati (`KBUILD_BUILD_*`).
- **Configurazione:** `make allnoconfig` + `guest/kernel/config/vetro.config`.
  Lo script verifica che ogni opzione del frammento sia arrivata nella
  `.config` e che il `savedefconfig` risultante coincida con
  `guest/kernel/config/defconfig` (versionato): una deriva si vede in revisione.
  Contenuto: PL011 con earlycon, PL031, GICv3, timer generico, virtio-mmio,
  virtio-blk/net/console, devtmpfs, initramfs gzip, printk; niente moduli,
  niente PCI (i dispositivi virtio della virt sono anche su mmio e alla
  piattaforma di Vetro bastano quelli), niente KASLR (determinismo), niente
  estensioni oltre ARMv8.0 (ADR 0005).
- **SMP:** su arm64 `CONFIG_SMP` è sempre attivo; `NR_CPUS=2` (minimo
  ammesso). Il guest gira con una CPU (`-smp 1`, default di QEMU). Il kernel
  interroga comunque PSCI via `HVC` (versione, e `SYSTEM_OFF` per
  `poweroff`): la piattaforma di Vetro deve rispondere a queste chiamate.
- **Initramfs:** `usr/gen_init_cpio` del kernel con `-t 0` e `gzip -n`
  (niente root, niente date: riproducibile), BusyBox statica di
  `tools/guest-bins`, `/init` e `/etc/autotest.sh` in
  `guest/kernel/initramfs/`. Marcatori: `VETRO-BOOT-OK` (avvio di `/init`),
  `VETRO-AUTOTEST-FINE: ok` (autotest riuscito). Poi shell su `ttyAMA0`
  (`setsid cttyhack sh`). `vetro.noautotest` e `vetro.poweroff` sulla riga
  di comando cambiano il flusso per gli avvii scriptati.
- **Oracolo di sistema:** `qemu-system-aarch64` nativo in CI (runner arm64),
  su macOS `tools/guest-kernel/qemu-system-aarch64-docker.sh` con
  un'immagine Debian trixie dedicata (`Dockerfile.qemu`, stessa base e
  stessa versione di QEMU dell'oracolo user mode). `tools/oracle/Dockerfile`
  resta invariato: il pacchetto di sistema pesa e serve solo a questi test.
- **Verifica:** `tests/boot` (`vetro-boot-tests`) avvia il comando di
  riferimento, controlla i marcatori entro `VETRO_BOOT_TIMEOUT`, scrive un
  comando sulla console e ne legge il risultato (la shell è viva), spegne con
  `poweroff -f`. Log di riferimento in `guest/kernel/reference/qemu-boot.log`.
- **Caricatore** (`vetro-cli::boot`, puro): header `Image` (magic, text_offset,
  image_size, flags; kernel big-endian e granulo 16 KiB rifiutati, la
  Cortex-A53 non lo ha). Layout come `hw/arm/boot.c` di QEMU: kernel a
  `0x4000_0000 + text_offset`, spostato di 2 MiB se `text_offset < 4 KiB`
  (QEMU tiene lì il suo stub), quindi `0x4020_0000` per i kernel moderni;
  initramfs a `base + min(ram/2, 128 MiB)` e comunque oltre la bss del
  kernel; DTB subito dopo, allineato a 2 MiB. Ingresso: `x0` = DTB,
  `x1..x3` = 0, PC = inizio dell'Image, EL1h, DAIF mascherati
  (PSTATE `0x3c5`), MMU spenta. Lo stub di QEMU non si emula: i registri si
  impostano direttamente.

## Conseguenze
- Licenze: `target/guest-kernel/sources/` contiene il tarball esatto, il
  frammento, il defconfig e gli script; per BusyBox (GPL anche lei) si indica
  il pacchetto Alpine e l'aports corrispondente. Prima di distribuire
  immagini va aggiunto il tarball dei sorgenti BusyBox con le patch Alpine.
- Cambiare versione del kernel: nuovo SHA-256, `VETRO_KERNEL_UPDATE_CONFIG=1`,
  nuovo log di riferimento (`VETRO_BOOT_UPDATE_REFERENCE=1`).
