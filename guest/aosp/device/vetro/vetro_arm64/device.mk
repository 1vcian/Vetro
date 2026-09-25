#
# Parti del prodotto proprie di Vetro, sopra il vendor di Cuttlefish.
#

PRODUCT_SOONG_NAMESPACES += device/vetro/vetro_arm64

# fstab della virt: in vendor (seconda fase) e nel vendor_ramdisk (prima
# fase, first_stage_ramdisk/fstab.vetro). Scelto da androidboot.fstab_suffix.
PRODUCT_PACKAGES += \
    fstab.vetro \
    fstab.vetro.vendor_ramdisk

# init e ueventd della scheda.
PRODUCT_COPY_FILES += \
    device/vetro/vetro_arm64/init.vetro.rc:$(TARGET_COPY_OUT_VENDOR)/etc/init/init.vetro.rc

# Rete: eth0 resta eth0 e la gestisce EthernetService con DHCP (lo stack
# di Vetro o lo slirp di QEMU). Cuttlefish la rinomina e passa dal Wi-Fi
# simulato verso un OpenWRT sull'host, che qui non c'è.
PRODUCT_VENDOR_PROPERTIES += \
    ro.vendor.disable_rename_eth0=1

# adbd su TCP 5555 senza autorizzazione: SOLO nella build di sviluppo
# userdebug (Cuttlefish fa lo stesso: shared/device.mk mette già
# persist.adb.tcp.port=5555 e ro.adb.secure=0). Una build per utenti deve
# togliere ro.adb.secure=0 e usare le chiavi adb (ADR 0021).
PRODUCT_SYSTEM_EXT_PROPERTIES += \
    ro.adb.secure=0

# ART: CPU di Vetro = Cortex-A53, ARMv8.0 + CRC32 + crypto (ADR 0005).
# TARGET_CPU_VARIANT := cortex-a53 (BoardConfig di Cuttlefish) fa scrivere
# alla build dalvik.vm.isa.arm64.variant=cortex-a53 e features=default in
# vendor/build.prop; tools/aosp/fetch.sh lo controlla sugli artefatti. Con
# varianti più nuove il JIT genererebbe LSE e FP16, che la CPU non ha.

# microG (GmsCore e Companion) come app privilegiate, con lo spoofing della
# firma limitato ai loro certificati (guest/aosp/patches/frameworks/base).
PRODUCT_PACKAGES += \
    VetroGmsCore \
    VetroGmsCompanion \
    privapp-permissions-vetro-microg.xml \
    default-permissions-vetro-microg.xml \
    sysconfig-vetro-microg.xml
