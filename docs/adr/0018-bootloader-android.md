# ADR 0018 — Il bootloader Android di Vetro: boot.img, vendor_boot, init_boot, bootconfig

- Stato: accettata (M5, 2026-09-25). Estende l'ADR 0008 (avvio diretto).

## Contesto
Le immagini Android da GKI in poi (Cuttlefish, la nostra `guest/aosp`)
non danno un `Image` e un initrd, ma `boot.img` (kernel, header v4),
`vendor_boot.img` (ramdisk del vendor in frammenti, riga di comando del
vendor, sezione bootconfig) e `init_boot.img` (ramdisk generico). Tra le
immagini e il kernel c'è un bootloader (u-boot di Cuttlefish, ABL dei
telefoni) che le combina; QEMU non lo fa: vuole `-kernel/-initrd/-append`.
Vetro ha già l'avvio diretto con il layout di QEMU (ADR 0008) e deve restare
confrontabile con l'oracolo.

## Decisione
- **Il bootloader è un modulo puro di `vetro-machine`**
  (`vetro_machine::android`), davanti al caricatore di M3: produce `Image`,
  initrd e riga di comando, poi `Machine::load_android` = `load_linux`.
  Nessun firmware nel guest, nessun layout diverso da QEMU: gli stessi tre
  pezzi (scritti da `vetro boot --android-dump`) vanno a
  `qemu-system-aarch64 -kernel -initrd -append`, ed è quello il confronto con
  l'oracolo. Nel browser il modulo gira uguale (niente dipendenze).
- **Decompressori propri, senza dipendenze** (gzip con CRC32, LZ4 legacy e
  frame), solo per il kernel: `vetro-machine` compila per wasm32 e il core
  resta senza crate esterni. I ramdisk non si toccano: li apre il kernel,
  come su un telefono.
- **Ordine dei ramdisk:** frammenti del vendor nell'ordine della tabella,
  saltando quelli di tipo recovery (salvo `--recovery`), poi il generico
  (`init_boot` se c'è), senza allineamento: è l'esempio di source.android.com
  per l'avvio normale.
- **Parametri del bootloader** (`--append`): con `vendor_boot` v4 gli
  `androidboot.*` vanno nel bootconfig, dopo la sezione del vendor (è dove
  il bootloader aggiunge i parametri noti solo all'avvio), gli altri in coda
  alla riga di comando (`boot`, `vendor`, bootloader, come u-boot). Senza v4
  tutto sulla riga di comando. Gli `androidboot.*` già scritti nelle righe
  di comando delle immagini si lasciano lì: sono una scelta della build
  (la migrazione incrementale di AOSP li tiene in entrambi).
- **`bootconfig` sulla riga di comando:** AOSP lo mette nella build
  (`BOARD_KERNEL_CMDLINE += bootconfig`); Vetro lo aggiunge se c'è un blocco
  e la riga non lo contiene, altrimenti gli `androidboot.*` spostati da noi
  sparirebbero in silenzio. Il kernel guest ha `CONFIG_BOOT_CONFIG=y` senza
  `FORCE`, come il GKI.
- **Formato del blocco** quello di `tools/bootconfig -a` del kernel (testo,
  NUL, riempimento all'allineamento a 4 dell'initrd, size, checksum,
  magic): verificato byte per byte con lo strumento compilato dai sorgenti
  del kernel guest. Valori tra virgolette (virgole, `#`, `;` resterebbero
  altrimenti sintassi); chiavi ripetute nei parametri rifiutate (il kernel
  scarterebbe tutto il blocco). Una chiave dei parametri che ripete una
  chiave della sezione del vendor non si controlla: il kernel rifiuta il
  blocco e lo dice nel log (`Failed to parse bootconfig: Value is
  redefined`, verificato con `tools/bootconfig`).
- **mkbootimg di AOSP come riferimento dei test**, copia non modificata in
  `tools/mkbootimg/` a un commit fissato (blob git e sha256 nel README):
  le immagini di prova sono quelle che produce AOSP, non quelle che
  crediamo. Serve solo `python3`; in CI (`VETRO_REQUIRE_ORACLE=1`) la sua
  assenza fa fallire i test.
- **Si ignora** quello che la virt non usa: indirizzi di caricamento, DTB
  delle immagini (Vetro genera il suo, come QEMU), `second`,
  `recovery_dtbo`, firma GKI e AVB (niente verifica: le immagini di Vetro
  sono userdebug con vbmeta disattivato, `m5-immagini-android.md`).

## Conseguenze
- `vetro boot --boot-img/--vendor-boot/--init-boot` avvia le immagini di una
  build GKI senza passaggi a mano; `--android-dump` dà i file per QEMU.
- Il kernel guest cambia configurazione (`BOOT_CONFIG`, `RD_LZ4`): la cache
  di CI si rinnova da sola (chiave su `guest/kernel/config/**`), il log di
  riferimento di QEMU è rigenerato.
- Scegliere i frammenti del vendor per `board_id`, verificare AVB e la
  firma GKI resta fuori: se servirà, un ADR nuovo.
