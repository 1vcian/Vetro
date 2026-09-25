# Immagine dell'emulatore Android 15 sotto QEMU e sotto Vetro (M5)

Esperimento di M5 (`docs/research/m5-avvio-gki.md`): fin dove arriva il
kernel GKI android15-6.6 dell'emulatore SDK sulla macchina di Vetro, sotto
`qemu-system-aarch64 -M virt` e sotto `vetro boot`, con la stessa
configurazione.

**Licenza.** L'immagine è dell'Android SDK (licenza SDK di Google): solo uso
locale di sviluppo. Non si committa, non si ridistribuisce, non va in CI né
negli artefatti. Sta in `target/android-emu/`, ignorata da git.

## Preparazione (una volta)

```sh
mkdir -p target/android-emu && cd target/android-emu
curl -fLO https://dl.google.com/android/repository/sys-img/android/arm64-v8a-35_r02.zip
unzip -q arm64-v8a-35_r02.zip
gunzip -c arm64-v8a/kernel-ranchu > Image     # il caricatore vuole l'Image non compresso
# /data: ext4 vuoto da 2 GiB (sparso), come lo crea l'host dell'emulatore
docker run --rm -v "$PWD:/w" debian:trixie-slim sh -c \
  'apt-get update -qq && apt-get install -y -qq e2fsprogs >/dev/null && mkfs.ext4 -q -L userdata /w/userdata.img 2G'
```

Un disco a zero non basta: `/data` non è `formattable` nel fstab, vold non
trova la chiave della cifratura dei metadati e init riavvia in recovery.

`ramdisk.img` si usa così com'è: sono due archivi cpio (ramdisk generico e
ramdisk del vendor con `fstab.ranchu` e i moduli virtio) in un flusso LZ4
legacy, e il kernel ha `CONFIG_RD_LZ4=y`.

## Avvio

```sh
tools/android-emu/qemu.sh  < /dev/null > qemu.log    # oracolo (Docker su macOS); si ferma con Ctrl-C
cargo build --release -p vetro-cli
tools/android-emu/vetro.sh 300 < /dev/null > vetro.log   # 300 s di tempo del guest
tools/android-emu/compare.py qemu.log vetro.log      # prima divergenza e righe in un solo log
```

Stessa macchina per tutti e due: `virt,gic-version=3,its=off`, Cortex-A53,
una CPU, 2 GiB, virtio-mmio moderno, niente rete, niente GPU/input; tre
virtio-blk in copy-on-write (i file non cambiano). La riga di comando del
kernel è in `cmdline.sh` (con i commenti sull'ordine dei dischi).
