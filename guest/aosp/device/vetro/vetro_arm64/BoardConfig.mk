#
# Vetro arm64: scheda derivata da Cuttlefish arm64 solo 64 bit
# (device/google/cuttlefish/vsoc_arm64_only), adattata alla macchina virt
# di Vetro e di QEMU (virtio-mmio, niente PCI, niente host Cuttlefish).
# Scelte e motivi: docs/adr/0021-immagine-aosp-di-vetro.md.
#

include device/google/cuttlefish/vsoc_arm64_only/BoardConfig.mk

TARGET_BOOTLOADER_BOARD_NAME := vetro

# Kernel GKI android15-6.6 prebuilt dal tree (lo stesso di Cuttlefish):
# KERNEL_MODULES_PATH = kernel/prebuilts/common-modules/virtual-device/6.6/arm64,
# SYSTEM_DLKM_SRC = kernel/prebuilts/6.6/arm64.
#
# Prima fase di init: sulla virt i dischi, la GPU, l'input, la rete e vsock
# stanno su virtio-mmio, e virtio_mmio.ko è un modulo. Cuttlefish lo carica
# solo nella seconda fase (vendor_dlkm) perché usa virtio-pci; per noi deve
# stare nel vendor_ramdisk, altrimenti la prima fase non trova super.
BOARD_VENDOR_RAMDISK_KERNEL_MODULES += $(KERNEL_MODULES_PATH)/virtio_mmio.ko
BOARD_VENDOR_KERNEL_MODULES := \
    $(filter-out $(BOARD_VENDOR_RAMDISK_KERNEL_MODULES),\
                 $(wildcard $(KERNEL_MODULES_PATH)/*.ko))

# Moduli che sulla virt non servono o non hanno dispositivi (goldfish è
# l'emulatore SDK; vkms e dummy-cpufreq sono già bloccati da Cuttlefish).
BOARD_VENDOR_KERNEL_MODULES_BLOCKLIST_FILE := \
    device/vetro/vetro_arm64/modules.blocklist

# Riga di comando del vendor: console sulla PL011 della virt.
BOARD_KERNEL_CMDLINE += console=ttyAMA0

# Parametri che su Cuttlefish mette il launcher (assemble_cvd, vedi
# host/commands/assemble_cvd/bootconfig_args.cpp e
# host/libs/vm_manager/qemu_manager.cpp con --gpu_mode=guest_swiftshader):
# qui stanno nella sezione bootconfig del vendor_boot, così il bootloader
# di Vetro (ADR 0018) e QEMU (-initrd con il blocco in coda) non devono
# aggiungere niente.
#
# Slot virtio-mmio della virt (0x0a000000 + k*0x200, k = 31..0): tutti, così
# il disco si trova in qualunque slot finisca (con i dispositivi di default
# di `vetro boot` e lo stesso ordine di -device in QEMU è lo slot 27,
# a003600). fs_mgr divide la lista come array di bootconfig.
BOARD_BOOTCONFIG += \
    androidboot.boot_devices=a003e00.virtio_mmio,a003c00.virtio_mmio,a003a00.virtio_mmio,a003800.virtio_mmio,a003600.virtio_mmio,a003400.virtio_mmio,a003200.virtio_mmio,a003000.virtio_mmio,a002e00.virtio_mmio,a002c00.virtio_mmio,a002a00.virtio_mmio,a002800.virtio_mmio,a002600.virtio_mmio,a002400.virtio_mmio,a002200.virtio_mmio,a002000.virtio_mmio,a001e00.virtio_mmio,a001c00.virtio_mmio,a001a00.virtio_mmio,a001800.virtio_mmio,a001600.virtio_mmio,a001400.virtio_mmio,a001200.virtio_mmio,a001000.virtio_mmio,a000e00.virtio_mmio,a000c00.virtio_mmio,a000a00.virtio_mmio,a000800.virtio_mmio,a000600.virtio_mmio,a000400.virtio_mmio,a000200.virtio_mmio,a000000.virtio_mmio

# Avvio normale dello slot A (boot_image_utils.cc di assemble_cvd), AVB non
# verificato (stato "orange": userdebug sbloccato).
BOARD_BOOTCONFIG += \
    androidboot.slot_suffix=_a \
    androidboot.force_normal_boot=1 \
    androidboot.verifiedbootstate=orange

# fstab nostro (fstab.vetro: stesse partizioni di Cuttlefish, senza avb).
BOARD_BOOTCONFIG += androidboot.fstab_suffix=vetro

# Console seriale con la shell di debug (userdebug).
BOARD_BOOTCONFIG += \
    androidboot.console=ttyAMA0 \
    androidboot.serialconsole=1

# Grafica: SwiftShader (Vulkan "pastel") con ANGLE sopra per GLES,
# gralloc minigbm e HWC ranchu (composizione nel guest) sul DRM di
# virtio-gpu 2D: gli stessi valori di --gpu_mode=guest_swiftshader di
# Cuttlefish sotto QEMU (host/libs/vm_manager/qemu_manager.cpp).
# cpuvulkan.version = VK_API_VERSION_1_2 = (1 << 22) | (2 << 12).
BOARD_BOOTCONFIG += \
    androidboot.cpuvulkan.version=4202496 \
    androidboot.hardware.gralloc=minigbm \
    androidboot.hardware.hwcomposer=ranchu \
    androidboot.hardware.hwcomposer.display_finder_mode=drm \
    androidboot.hardware.hwcomposer.display_framebuffer_format=rgba \
    androidboot.hardware.egl=angle \
    androidboot.hardware.vulkan=pastel \
    androidboot.opengles.version=196609 \
    androidboot.vendor.apex.com.android.hardware.graphics.composer=com.android.hardware.graphics.composer.ranchu \
    androidboot.lcd_density=240

# HAL di sicurezza in software dentro il guest: su Cuttlefish KeyMint e
# Gatekeeper stanno sull'host (cf_remote), che qui non c'è.
BOARD_BOOTCONFIG += \
    androidboot.vendor.apex.com.android.hardware.keymint=com.android.hardware.keymint.rust_nonsecure \
    androidboot.vendor.apex.com.android.hardware.gatekeeper=com.android.hardware.gatekeeper.nonsecure

# Il resto dei parametri del launcher, con i valori per una macchina senza
# host Cuttlefish e con una CPU emulata lenta.
BOARD_BOOTCONFIG += \
    androidboot.serialno=VETRO00001 \
    androidboot.ddr_size=3072MB \
    androidboot.setupwizard_mode=DISABLED \
    androidboot.enable_bootanimation=0 \
    androidboot.enable_confirmationui=0 \
    androidboot.audio.tinyalsa.ignore_output=true \
    androidboot.audio.tinyalsa.simulate_input=true \
    androidboot.wifi_mac_prefix=5554 \
    androidboot.hw_timeout_multiplier=50 \
    androidboot.hypervisor.vm.supported=0 \
    androidboot.hypervisor.protected_vm.supported=0

# SELinux permissivo finché la sepolicy non copre i percorsi virtio-mmio
# (build di sviluppo userdebug; obiettivo: enforcing con una sepolicy nostra
# in device/vetro/vetro_arm64/sepolicy, scritta dai rifiuti registrati nel
# primo avvio, vedi ADR 0021).
BOARD_BOOTCONFIG += androidboot.selinux=permissive

# Policy SELinux nostra (demone vetro-files, ADR 0020/0021).
BOARD_VENDOR_SEPOLICY_DIRS += device/vetro/vetro_arm64/sepolicy

# Dimensioni di super e userdata: quelle di Cuttlefish (super 7 GiB, userdata
# 8 GiB f2fs). Le immagini sono sparse: lo spazio vuoto non si trasferisce.
