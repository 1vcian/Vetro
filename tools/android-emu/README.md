# Android 15 emulator image under QEMU and under Vetro (M5)

M5 experiment (`docs/research/m5-gki-boot.md`): how far the SDK emulator's
GKI android15-6.6 kernel gets on the Vetro machine, under
`qemu-system-aarch64 -M virt` and under `vetro boot`, with the same
configuration.

**License.** The image belongs to the Android SDK (Google's SDK license): local
development use only. It is not committed, not redistributed, and does not go into CI
or artifacts. It lives in `target/android-emu/`, ignored by git.

## Preparation (once)

```sh
mkdir -p target/android-emu && cd target/android-emu
curl -fLO https://dl.google.com/android/repository/sys-img/android/arm64-v8a-35_r02.zip
unzip -q arm64-v8a-35_r02.zip
gunzip -c arm64-v8a/kernel-ranchu > Image     # the loader wants the uncompressed Image
# /data: empty 2 GiB ext4 (sparse), as the emulator host creates it
docker run --rm -v "$PWD:/w" debian:trixie-slim sh -c \
  'apt-get update -qq && apt-get install -y -qq e2fsprogs >/dev/null && mkfs.ext4 -q -L userdata /w/userdata.img 2G'
```

A zero-filled disk is not enough: `/data` is not `formattable` in the fstab, vold does not
find the metadata encryption key and init reboots into recovery.

`ramdisk.img` is used as is: it is two cpio archives (generic ramdisk and
vendor ramdisk with `fstab.ranchu` and the virtio modules) in a legacy LZ4
stream, and the kernel has `CONFIG_RD_LZ4=y`.

## Boot

```sh
tools/android-emu/qemu.sh  < /dev/null > qemu.log    # oracle (Docker on macOS); stop it with Ctrl-C
cargo build --release -p vetro-cli
tools/android-emu/vetro.sh 300 < /dev/null > vetro.log   # 300 s of guest time
tools/android-emu/compare.py qemu.log vetro.log      # first divergence and lines in only one log
```

Same machine for both: `virt,gic-version=3,its=off`, Cortex-A53,
one CPU, 2 GiB, modern virtio-mmio, no network, no GPU/input; three
copy-on-write virtio-blk disks (the files do not change). The kernel command line
is in `cmdline.sh` (with comments on the disk order).
