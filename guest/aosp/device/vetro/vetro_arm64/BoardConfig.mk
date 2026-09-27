#
# Vetro arm64: board derived from Cuttlefish arm64 64-bit only
# (device/google/cuttlefish/vsoc_arm64_only), adapted to the virt machine of
# Vetro and QEMU (virtio-mmio, no PCI, no Cuttlefish host).
# Choices and reasons: docs/adr/0022-vetro-aosp-image.md.
#

include device/google/cuttlefish/vsoc_arm64_only/BoardConfig.mk

TARGET_BOOTLOADER_BOARD_NAME := vetro

# No "cross" host (ADR 0030). Cuttlefish sets HOST_CROSS_OS := linux_musl
# (arm64) to ship its tools to arm64 hosts too, and with TARGET_BOARD_PLATFORM
# vsoc_arm64 its cvd_host_package enters droidcore
# (build/cvd-host-package.go): `m droid` would build the whole host package a
# second time for linux_musl-arm64, which Vetro doesn't need (the build VM is
# x86_64 and Cuttlefish's launcher isn't used). Empty = no cross host: the
# same state as envsetup.mk with BUILD_HOST_static, and Soong skips the target
# (CrossHost ""). Only out/host changes: the images don't depend on the cross
# host tools.
HOST_CROSS_OS :=
HOST_CROSS_ARCH :=
HOST_CROSS_2ND_ARCH :=

# Prebuilt GKI android15-6.6 kernel from the tree (the same as Cuttlefish's):
# KERNEL_MODULES_PATH = kernel/prebuilts/common-modules/virtual-device/6.6/arm64,
# SYSTEM_DLKM_SRC = kernel/prebuilts/6.6/arm64.
#
# First stage of init: on the virt machine the disks, GPU, input, network and
# vsock are on virtio-mmio, and virtio_mmio.ko is a module. Cuttlefish loads it
# only in the second stage (vendor_dlkm) because it uses virtio-pci; for us it
# must be in the vendor_ramdisk, otherwise the first stage doesn't find super.
BOARD_VENDOR_RAMDISK_KERNEL_MODULES += $(KERNEL_MODULES_PATH)/virtio_mmio.ko
BOARD_VENDOR_KERNEL_MODULES := \
    $(filter-out $(BOARD_VENDOR_RAMDISK_KERNEL_MODULES),\
                 $(wildcard $(KERNEL_MODULES_PATH)/*.ko))

# Modules the virt machine doesn't need or has no devices for (goldfish is the
# SDK emulator; vkms and dummy-cpufreq are already blocked by Cuttlefish).
BOARD_VENDOR_KERNEL_MODULES_BLOCKLIST_FILE := \
    device/vetro/vetro_arm64/modules.blocklist

# Vendor command line: console on the virt machine's PL011.
BOARD_KERNEL_CMDLINE += console=ttyAMA0

# Parameters that Cuttlefish's launcher sets (assemble_cvd, see
# host/commands/assemble_cvd/bootconfig_args.cpp and
# host/libs/vm_manager/qemu_manager.cpp with --gpu_mode=guest_swiftshader):
# here they are in the vendor_boot's bootconfig section, so Vetro's
# bootloader (ADR 0018) and QEMU (-initrd with the block at the end) don't
# have to add anything.
#
# virtio-mmio slots of the virt machine (0x0a000000 + k*0x200, k = 31..0): all
# of them, so the disk is found whatever slot it ends up in (with `vetro
# boot`'s default devices and the same -device order in QEMU it's slot 27,
# a003600). fs_mgr splits the list as a bootconfig array.
BOARD_BOOTCONFIG += \
    androidboot.boot_devices=a003e00.virtio_mmio,a003c00.virtio_mmio,a003a00.virtio_mmio,a003800.virtio_mmio,a003600.virtio_mmio,a003400.virtio_mmio,a003200.virtio_mmio,a003000.virtio_mmio,a002e00.virtio_mmio,a002c00.virtio_mmio,a002a00.virtio_mmio,a002800.virtio_mmio,a002600.virtio_mmio,a002400.virtio_mmio,a002200.virtio_mmio,a002000.virtio_mmio,a001e00.virtio_mmio,a001c00.virtio_mmio,a001a00.virtio_mmio,a001800.virtio_mmio,a001600.virtio_mmio,a001400.virtio_mmio,a001200.virtio_mmio,a001000.virtio_mmio,a000e00.virtio_mmio,a000c00.virtio_mmio,a000a00.virtio_mmio,a000800.virtio_mmio,a000600.virtio_mmio,a000400.virtio_mmio,a000200.virtio_mmio,a000000.virtio_mmio

# Normal boot of slot A (assemble_cvd's boot_image_utils.cc), AVB not
# verified ("orange" state: unlocked userdebug).
BOARD_BOOTCONFIG += \
    androidboot.slot_suffix=_a \
    androidboot.force_normal_boot=1 \
    androidboot.verifiedbootstate=orange

# Our fstab (fstab.vetro: the same partitions as Cuttlefish, without avb).
BOARD_BOOTCONFIG += androidboot.fstab_suffix=vetro

# Serial console with the debug shell (userdebug).
BOARD_BOOTCONFIG += \
    androidboot.console=ttyAMA0 \
    androidboot.serialconsole=1

# Graphics: SwiftShader (Vulkan "pastel") with ANGLE on top for GLES, minigbm
# gralloc and the ranchu HWC (composition in the guest) on virtio-gpu 2D's DRM:
# the same values as Cuttlefish's --gpu_mode=guest_swiftshader under QEMU
# (host/libs/vm_manager/qemu_manager.cpp).
# cpuvulkan.version = VK_API_VERSION_1_2 = (1 << 22) | (2 << 12).
# display_framebuffer_format=bgra: virtio-gpu 2D resources are B8G8R8X8 on the
# host (dumb buffers), so the HWC converts its RGBA frame to BGRA before the
# flush (guest/aosp/patches/device/generic/goldfish-opengl; with rgba the
# scanout of QEMU and Vetro shows red and blue swapped).
BOARD_BOOTCONFIG += \
    androidboot.cpuvulkan.version=4202496 \
    androidboot.hardware.gralloc=minigbm \
    androidboot.hardware.hwcomposer=ranchu \
    androidboot.hardware.hwcomposer.display_finder_mode=drm \
    androidboot.hardware.hwcomposer.display_framebuffer_format=bgra \
    androidboot.hardware.egl=angle \
    androidboot.hardware.vulkan=pastel \
    androidboot.opengles.version=196609 \
    androidboot.vendor.apex.com.android.hardware.graphics.composer=com.android.hardware.graphics.composer.ranchu \
    androidboot.lcd_density=240

# Security HALs in software inside the guest: on Cuttlefish KeyMint and
# Gatekeeper are on the host (cf_remote), which isn't there.
BOARD_BOOTCONFIG += \
    androidboot.vendor.apex.com.android.hardware.keymint=com.android.hardware.keymint.rust_nonsecure \
    androidboot.vendor.apex.com.android.hardware.gatekeeper=com.android.hardware.gatekeeper.nonsecure

# The rest of the launcher's parameters, with the values for a machine without
# a Cuttlefish host and with a slow emulated CPU.
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

# Permissive SELinux until the sepolicy covers the virtio-mmio paths
# (userdebug development build; goal: enforcing with our own sepolicy in
# device/vetro/vetro_arm64/sepolicy, written from the denials logged at first
# boot, see ADR 0022).
BOARD_BOOTCONFIG += androidboot.selinux=permissive

# Precompiled code (odex/vdex) of the apps in their partitions, not in
# system_other: Cuttlefish puts it in slot B (system_other.img) and copies it
# to /data at first boot (cppreopts), but our disk has only slot A. Without
# it, at first boot ArtService recompiles every app: tens of minutes of guest
# time under QEMU, much more under Vetro.
BOARD_USES_SYSTEM_OTHER_ODEX :=

# Our SELinux policy (vetro-files daemon, ADR 0020/0022).
BOARD_VENDOR_SEPOLICY_DIRS += device/vetro/vetro_arm64/sepolicy

# Sizes of super and userdata: Cuttlefish's (super 7 GiB, userdata 8 GiB
# f2fs). The images are sparse: empty space isn't transferred.
