# Spec — l'immagine AOSP di Vetro (`guest/aosp`, `tools/aosp`)

Decisioni: ADR 0022. Ricerca: `docs/research/m5-immagini-android.md`,
`docs/research/m5-avvio-gki.md`. Bootloader: ADR 0018,
`docs/specs/android-boot.md`.

## Perimetro
Un'immagine AOSP 15 arm64 solo 64 bit, prodotto `vetro_arm64`
(`vetro_arm64-bp1a-userdebug`, tag `android-15.0.0_r36`), che parte sulla
macchina virt di Vetro e di QEMU (`-M virt,gic-version=3,its=off -cpu
cortex-a53 -smp 1 -m 3G`, virtio-mmio) con il kernel GKI android15-6.6
prebuilt e il bootloader di Vetro, e porta microG. Build sulla VM Linux
x86_64 dedicata; prove di avvio sul Mac.

## File
| Percorso | Contenuto |
|---|---|
| `guest/aosp/device/vetro/vetro_arm64/` | prodotto (`vetro_arm64.mk`, `AndroidProducts.mk`), scheda (`BoardConfig.mk`: moduli della prima fase, bootconfig), `device.mk`, `fstab.vetro`, `init.vetro.rc`, `modules.blocklist`, `sepolicy/`, `vetro-files/` (Android.bp e rc; il `.c` arriva da `guest/kernel/initramfs`) |
| `guest/aosp/vendor/vetro/microg/` | `microg.lock` (file, sha256, URL della release), moduli `VetroGmsCore`/`VetroGmsCompanion`, allowlist dei permessi privilegiati, permessi di default, sysconfig |
| `guest/aosp/patches/<progetto>/NNNN-*.patch` | patch al tree AOSP (oggi solo `frameworks/base`: spoofing della firma limitato a microG) |
| `tools/aosp/` | script dal Mac: `sync.sh`, `build.sh`, `fetch.sh`, `mkdisk.sh`, `qemu.sh`, `vetro.sh`, `upload.sh`, `gpl-sources.sh`; `remote/` sulla VM: `prepare.sh`, `build.sh`, `pack.sh`, `gpl.sh` |

## Flusso
1. `tools/aosp/sync.sh`: rsync di `device/vetro`, `vendor/vetro` (senza i
   prebuilt scaricati) e del sorgente di `vetro-files` nel tree della VM,
   patch applicate se mancano (una patch che non si applica ferma tutto),
   APK di microG scaricati e verificati con lo sha256 del lock.
2. `tools/aosp/build.sh start|status|wait`: `lunch` + `m droid` staccato
   (nohup/setsid; stato in `~/vetro-aosp/build.status`: RUNNING, OK,
   FAIL n; log in `build.log`). Incrementale in `out/`; niente ccache (la
   sandbox della build monta la home in sola lettura).
3. `tools/aosp/fetch.sh`: `remote/pack.sh` copia le immagini e le proprietà,
   scrive `build-info.txt` e `SHA256SUMS`; rsync in `target/aosp/out/`,
   verifica degli hash, controlli (variante ISA di ART `cortex-a53`, niente
   ABI a 32 bit), poi `mkdisk.sh`.
4. `tools/aosp/mkdisk.sh`: `target/aosp/disk.img` (vedi "Disco").
5. Avvio: `tools/aosp/qemu.sh > qemu.log` (oracolo) e
   `tools/aosp/vetro.sh [secondi di guest] > vetro.log`.
6. `tools/aosp/gpl-sources.sh`, poi `tools/aosp/upload.sh` (R2).

Variabili (`tools/aosp/common.sh`): `VETRO_AOSP_HOST` (default
`target/aosp/vm-host`, poi l'indirizzo della VM), `VETRO_AOSP_KEY`
(`~/.ssh/vetro_aosp`), `VETRO_AOSP_TREE` (`aosp`), `VETRO_AOSP_WORK`
(`vetro-aosp`), `VETRO_AOSP_LUNCH`.

## Artefatti (`target/aosp/out/`, mai in git)
| File | Contenuto |
|---|---|
| `boot.img` | header v4, kernel GKI (`kernel-6.6`), niente ramdisk |
| `init_boot.img` | header v4, ramdisk generico (LZ4) |
| `vendor_boot.img` | header v4: ramdisk del vendor (prima fase, moduli, `fstab.vetro`), riga di comando del vendor, sezione bootconfig |
| `super.img` | sparso: partizioni logiche dello slot A (erofs) |
| `userdata.img` | sparso: f2fs vuoto da 8 GiB |
| `build-info.txt` | tag del manifest, BUILD_ID, stringa del kernel e id della build, vermagic e scmversion dei moduli, commit di cuttlefish, prebuilt del kernel e frameworks/base |
| `props/` | `build.prop` delle partizioni, `fstab.vetro`, `modules.load` del vendor_ramdisk |
| `SHA256SUMS` | sha256 di tutti i file |

## Disco (`target/aosp/disk.img`)
GPT, partizioni allineate a 1 MiB, file sparso:
| # | Nome | Dimensione | Contenuto |
|---|---|---|---|
| 1 | `misc` | 1 MiB | zeri |
| 2 | `frp` | 1 MiB | zeri |
| 3 | `metadata` | 64 MiB | ext4 vuoto (`mkfs.ext4 -L metadata`) |
| 4 | `super` | 7 GiB | `super.img` espanso |
| 5 | `userdata` | 8 GiB | `userdata.img` espanso |

La prima fase monta `/metadata` e le partizioni logiche di `super`
(`/dev/block/by-name/*` dai nomi GPT, sui dispositivi di
`androidboot.boot_devices`); `/data` si monta a `mount_all --late`, cifrata
da vold con la chiave in `/metadata`.

## Macchina e dispositivi
Stessi dispositivi, nello stesso ordine, in QEMU e in Vetro (il primo va
nello slot virtio-mmio 31, `0x0a003e00`):
| Slot | QEMU | Vetro (`vetro boot`, default) |
|---|---|---|
| 31 | `virtio-gpu-device` (2D) | GPU 1280x800 |
| 30 | `virtio-keyboard-device` | tastiera |
| 29 | `virtio-tablet-device` | tablet |
| 28 | `virtio-net-device` + `-netdev user,hostfwd=tcp::5555-:5555` | rete + `--hostfwd=tcp:127.0.0.1:5555-:5555` |
| 27 | `virtio-blk-device` (`disk.img`, `snapshot=on`) | `--disk=disk.img` (copy-on-write in memoria) |

Con `--vsock` (gestore dei file) Vetro mette vsock nello slot 27 e il disco
nel 26: `boot_devices` li elenca tutti, quindi non cambia niente. QEMU in
Docker non ha vsock.

## Riga di comando e bootconfig
- Riga di comando: quella di `boot.img` (vuota) + vendor (`printk.devkmsg=on
  audit=1 panic=-1 8250.nr_uarts=1 cma=0 firmware_class.path=/vendor/etc/
  loop.max_part=7 init=/init bootconfig console=ttyAMA0`, da Cuttlefish e
  `BoardConfig.mk`) + `nokaslr` degli script (log confrontabili).
- Bootconfig (sezione del vendor_boot, `BOARD_BOOTCONFIG`): `hardware=cutf_cvm`,
  `boot_devices` (32 slot), `slot_suffix=_a`, `force_normal_boot=1`,
  `verifiedbootstate=orange`, `fstab_suffix=vetro`, `console=ttyAMA0`,
  `serialconsole=1`, grafica (`cpuvulkan.version=4202496`,
  `hardware.gralloc=minigbm`, `hardware.hwcomposer=ranchu`,
  `display_finder_mode=drm`, `display_framebuffer_format=rgba`,
  `hardware.egl=angle`, `hardware.vulkan=pastel`,
  `opengles.version=196609`, APEX del composer ranchu, `lcd_density=240`),
  KeyMint e Gatekeeper software, `serialno=VETRO00001`, `ddr_size=3072MB`,
  `setupwizard_mode=DISABLED`, `enable_bootanimation=0`,
  `hw_timeout_multiplier=50`, `hypervisor.*=0`, `selinux=permissive`.
- Il bootloader di Vetro aggiunge in coda i parametri `androidboot.*` di
  `--append` (ADR 0018): gli script non ne passano.

## Invarianti controllati
- `dalvik.vm.isa.arm64.variant=cortex-a53` e niente `abilist32`
  (`fetch.sh`).
- APK di microG con lo sha256 del lock (`prepare.sh`), certificato di microG
  nella patch (`9bd06727…d14165`, `keytool -printcert -jarfile`).
- Allowlist dei permessi privilegiati = permessi `android.permission.*` delle
  APK (`aapt2 dump permissions`, da rigenerare con la release).
- `SHA256SUMS` verificato a ogni passaggio (VM → Mac → R2).
- Una versione su R2 non cambia: `upload.sh` rifiuta un oggetto con lo stesso
  nome e sha256 diverso, e non pubblica con modifiche non committate.

## R2
`aosp/<versione>/` con `<versione>` = `<tag>-<BUILD_ID>-<commit>`: le cinque
immagini, `build-info.txt`, `SHA256SUMS`, `manifest.json` (`version`,
`base_url`, `files[]` con `path`, `size`, `sha256`) e `sources/` (sorgenti
GPL, README). URL pubblico: `R2_PUBLIC_URL` + chiave.

## Stato delle prove (versione `android-15.0.0_r36-BP1A.250505.005.D1-9d91633`)
Stessa immagine, stessi dispositivi, disco in copy-on-write (ogni avvio è un
primo avvio). Tempi in secondi di guest (timestamp di printk):

| Tappa | QEMU 10 (TCG, Docker) | Vetro (`--jit`) |
|---|---|---|
| prima fase di init | 1,2 | ~5 |
| zygote avviato | 81 | 181 |
| surfaceflinger avviato | 96 | 203 |
| `sys.boot_completed=1` | 476 | 801 |
| adb (`adb connect 127.0.0.1:5555/5556`) | sì | sì |
| home disegnata (screencap) | sì | sì (a ~1600 s; barra di stato non ancora disegnata) |

Tempo reale: QEMU ~8 min fino a `boot_completed`, Vetro ~1 h 15 min.
Servizi che muoiono prima di `boot_completed`: stesso insieme e stessi
segnali nei due (HAL di Cuttlefish senza host, `odsign` e `hwservicemanager`
terminati da init, `idmap2d` a riposo, `misctrl`, `cppreopts`); nessun
errore del kernel. `adb install -r` di un APK riuscito in tutti e due.

## GPL
`tools/aosp/gpl-sources.sh` → `target/aosp/sources/`: `linux-<versione>.tar.xz`
(kernel/common al commit `-g…` della stringa del kernel), moduli
virtual-device al commit di `scmversion`, `aosp/` con gli archivi dei
progetti AOSP con `MODULE_LICENSE_*GPL*` (esclusi prebuilts/, kernel/,
toolchain/), `manifest-pinned.xml`, `vetro-patches/`, `README`,
`SHA256SUMS`.
