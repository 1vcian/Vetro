# Spec — avvio dalle immagini Android (`vetro_machine::android`)

## Perimetro
Il lavoro del bootloader Android davanti al caricatore di Linux di M3
(`vetro_machine::boot`, ADR 0008): da `boot.img`, `vendor_boot.img` e
`init_boot.img` a un `Image` arm64 non compresso, un initrd e una riga di
comando, che poi seguono la strada di `-kernel/-initrd/-append` (layout di
QEMU, DTB generato da Vetro). Modulo puro: niente file, niente memoria del
guest; compila anche per wasm32. Decisioni: ADR 0018.

Formati di riferimento: `system/tools/mkbootimg` di AOSP (`bootimg.h`,
`mkbootimg.py`; copia fissata in `tools/mkbootimg/`), source.android.com
(Boot image header, Vendor boot partitions, Implement bootconfig),
`Documentation/admin-guide/bootconfig.rst` del kernel.

## Formati letti
| Immagine | Versioni | Pagina | Sezioni (ciascuna allineata alla pagina) |
|---|---|---|---|
| `boot.img` | 0, 1, 2 | dall'header (2–16 KiB) | header, kernel, ramdisk, second, recovery_dtbo (v1+, offset dall'header), dtb (v2) |
| `boot.img` | 3, 4 | 4096 fissa | header, kernel, ramdisk, firma GKI (v4) |
| `init_boot.img` | 4 | 4096 | header, ramdisk (kernel vuoto, obbligatorio) |
| `vendor_boot.img` | 3 | dall'header | header (2112 byte), ramdisk del vendor, dtb |
| `vendor_boot.img` | 4 | dall'header | header (2128 byte), ramdisk (frammenti concatenati), dtb, tabella dei ramdisk (voci da 108 byte), bootconfig |

Riga di comando di `boot.img`: v0–v2 `cmdline` (512) seguita senza
separatore da `extra_cmdline` (1024), come `unpack_bootimg.py`
(mkbootimg spezza una riga lunga a 511 byte); v3/v4 un campo da 1536.

Ignorati (non servono alla virt): indirizzi di caricamento e dei tag,
`second`, `recovery_dtbo`, i DTB (`boot` v2 e vendor: Vetro genera il suo),
la firma GKI, `board_id` (letto ed esposto, non usato per scegliere).

## Interfaccia pubblica
- `BootImage::parse(&[u8])`, `BootImage::parse_init_boot(&[u8])`:
  sezioni come slice dell'immagine, `cmdline`, `name`, `os_version`
  (`OsVersion`: versione e livello delle patch), `header_version`,
  `page_size`.
- `VendorBoot::parse(&[u8])`: `cmdline`, `name`, `ramdisk_section`,
  `ramdisks: Vec<VendorRamdisk>` (`kind: RamdiskType`, `name`, `board_id`,
  `data`; la v3 ha un solo ramdisk `Platform` senza nome), `dtb`,
  `bootconfig`.
- `assemble(&BootImage, Option<&VendorBoot>, Option<&BootImage>, &BootOptions)
  -> Result<AndroidBoot, AndroidError>`; `AndroidBoot::from_images(boot,
  vendor_boot, init_boot, opts)` legge e combina.
- `BootOptions { params: String, recovery: bool }`: parametri aggiunti dal
  bootloader (sintassi della riga di comando) e avvio in recovery.
- `AndroidBoot { kernel, kernel_format, initrd, cmdline, bootconfig, ramdisks }`,
  `initrd() -> Option<&[u8]>`.
- `Machine::load_android(&AndroidBoot)` = `load_linux(kernel, initrd, cmdline)`.
- `android::decompress`: `detect`, `decompress` (`Image` così com'è, gzip,
  LZ4 legacy, LZ4 frame), `gunzip`, `inflate`, `lz4_legacy`, `lz4_frame`,
  `lz4_block`, `crc32`. Uscita massima 512 MiB.
- `android::bootconfig`: `append(initrd, testo)`, `split(initrd)` (il
  contrario, come il kernel), `checksum`, `split_cmdline`, `key_value`,
  `param_line`.
- Errori: `AndroidError` (troncata, magic, versione, pagina, sezione fuori
  dall'immagine, tabella dei ramdisk, `init_boot` non valida, immagini
  incompatibili, kernel mancante o non decomprimibile, bootconfig,
  `BootError` del caricatore).

## Regole del bootloader (invarianti)
1. **Kernel:** riconosciuto dai primi byte; gzip controlla CRC32 e
   lunghezza di ogni membro; LZ4 legacy (`lz4 -l`, blocchi indipendenti,
   stream concatenati) e frame (blocchi collegati, checksum xxHash non
   verificati). Formato sconosciuto: errore.
2. **Initrd:** ramdisk del vendor nell'ordine della tabella, senza quelli
   di tipo `Recovery` salvo `recovery: true`, poi il ramdisk generico
   (`init_boot` se data, altrimenti `boot`), attaccati senza spazi né
   allineamento. I ramdisk non si decomprimono: li apre il kernel, quindi
   uno stream LZ4 può essere seguito solo da un altro stream LZ4 (come
   chiede AOSP: vendor e generico nello stesso formato).
3. **Riga di comando:** `boot`, poi `vendor_boot`, poi i parametri di
   `BootOptions::params` che non vanno nel bootconfig, separati da uno
   spazio.
4. **Bootconfig** (solo con `vendor_boot` v4): testo = sezione bootconfig
   del vendor (senza NUL finali, con `\n` finale aggiunto se manca) seguita
   da una riga `chiave = "valore"` per ogni `androidboot.*` di
   `BootOptions::params` (apici se il valore ha virgolette; chiave ripetuta
   nei parametri = errore). A parameter with the key of a line of the
   vendor section replaces that line in place instead of being appended
   (ADR 0028: repeated, the kernel would discard the whole block). Se il
   testo non è vuoto: blocco
   `testo, NUL, riempimento a NUL fino a un initrd multiplo di 4, size (le32),
   checksum (le32, somma dei byte), "#BOOTCONFIG\n"` in coda all'initrd
   (identico a `tools/bootconfig -a` del kernel), e `bootconfig` aggiunto
   alla riga di comando se non c'è già. Massimo 32767 byte (`XBC_DATA_MAX`).
   Gli `androidboot.*` già nelle righe di comando delle immagini restano lì.
5. Senza `vendor_boot` v4 (v3, v0–v2, o `boot` da solo) i parametri
   `androidboot.*` vanno sulla riga di comando.
6. Combinazioni rifiutate: `vendor_boot` con `boot` v0–v2, `init_boot` con
   `boot` prima della v4, `init_boot` con un kernel o prima della v4.

## CLI
`vetro boot --boot-img FILE [--vendor-boot FILE] [--init-boot FILE]
[--recovery] [--append RIGA] [--android-dump DIR]`: `--append` diventa
`BootOptions::params`; `--android-dump` scrive `Image`, `initrd` e
`cmdline` come li riceve il kernel (gli stessi file per
`qemu-system-aarch64 -kernel -initrd -append`). Tutte le opzioni con valore
di `boot` accettano `--opzione=valore` e `--opzione valore`.

## Kernel guest
`guest/kernel/config/vetro.config` ha `CONFIG_BOOT_CONFIG=y` (senza
`FORCE`, come il GKI: il blocco si usa solo con `bootconfig` sulla riga di
comando) e `CONFIG_RD_LZ4=y` (ramdisk LZ4 legacy del GKI).

## Test
- `android::tests`, `android::bootconfig::tests` (unitari): vettori di
  `gzip` e `lz4` veri, andata e ritorno con gli strumenti di sistema se ci
  sono, initramfs vero del guest, byte del blocco uguali a quelli di
  `tools/bootconfig -a` del kernel 6.18.53, regole 2–6, errori.
- `crates/vetro-machine/tests/mkbootimg.rs`: immagini di `mkbootimg.py`
  (v0, v1, v2 con pagine da 2, 4 e 16 KiB e riga di comando spezzata; v3
  con vendor v3; v4 con tre frammenti, board id, bootconfig e
  `init_boot`): ogni sezione letta uguale al file dato. Serve `python3`
  (`VETRO_REQUIRE_ORACLE=1` rende obbligatorio).
- `tests/boot/tests/android.rs` (release): kernel guest in `boot.img` v4
  (gzip), initramfs in `init_boot` (LZ4 legacy), tre frammenti del vendor
  (cpio, recovery, LZ4 con un `/init` da coprire), bootconfig del vendor e
  del bootloader. Il guest vede `/proc/cmdline` e `/proc/bootconfig`
  attesi e i file giusti sotto Vetro e sotto QEMU (`-kernel/-initrd/-append`
  con lo stesso spacchettamento); i due log coincidono salvo
  `KNOWN_DIFFERENCES`.
- `crates/vetro-cli/tests/boot_android.rs` (release): `vetro boot
  --boot-img --vendor-boot --android-dump` con le opzioni nella forma con lo
  spazio.
