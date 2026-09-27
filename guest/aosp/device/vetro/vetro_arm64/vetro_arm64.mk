#
# Vetro arm64 product (phone, 64-bit only). Starts from
# device/google/cuttlefish/vsoc_arm64_only/phone/aosp_cf.mk (AOSP 15) and
# changes only what Vetro's machine needs: see
# docs/adr/0022-vetro-aosp-image.md and docs/specs/guest-image.md.
#

#
# Everything that goes in system (like the GSI)
#
$(call inherit-product, $(SRC_TARGET_DIR)/product/core_64_bit_only.mk)
$(call inherit-product, $(SRC_TARGET_DIR)/product/generic_system.mk)

PRODUCT_ENFORCE_ARTIFACT_PATH_REQUIREMENTS := relaxed

#
# system_ext
#
$(call inherit-product, $(SRC_TARGET_DIR)/product/handheld_system_ext.mk)
$(call inherit-product, $(SRC_TARGET_DIR)/product/telephony_system_ext.mk)

#
# product
#
$(call inherit-product, $(SRC_TARGET_DIR)/product/aosp_product.mk)

#
# vendor: the Cuttlefish phone's (virtual HALs, SwiftShader, minigbm, ranchu
# HWC/drm_hwcomposer, software KeyMint and Gatekeeper). No
# packages/modules/Virtualization: Vetro's virt machine has no KVM.
#
# Cuttlefish HALs that talk to the host (vsock or /dev/hvcN) abort in a loop
# without it: the lights block system_server (LightsService waits for
# ILights/default, declared in the VINTF by the APEX but never registered),
# the OEM lock does the same for OemLockService. They are removed with
# shared/device.mk's switches (they apply before the inherit).
LOCAL_ENABLE_LIGHT := false
LOCAL_ENABLE_OEMLOCK := false
# No Bluetooth: Cuttlefish's HAL (com.google.cf.bt) talks to the host's
# rootcanal over /dev/hvc5, which isn't there; the Bluetooth stack then crashes
# in a loop (crash_dump64 every few seconds, measured on the idle home screen,
# ADR 0037). With false, shared/bluetooth/device_vendor.mk installs no HAL and
# excludes the Bluetooth features, so system_server starts no Bluetooth service.
BOARD_HAVE_BLUETOOTH := false
$(call inherit-product, device/google/cuttlefish/shared/phone/device_vendor.mk)
$(call inherit-product, device/google/cuttlefish/vsoc_arm64/bootloader.mk)

# Our parts: fstab, init, microG, overlays.
$(call inherit-product, device/vetro/vetro_arm64/device.mk)

# Excludes the features not available on AOSP devices.
PRODUCT_COPY_FILES += \
    frameworks/native/data/etc/aosp_excluded_hardware.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/aosp_excluded_hardware.xml

PRODUCT_NAME := vetro_arm64
PRODUCT_DEVICE := vetro_arm64
PRODUCT_BRAND := Vetro
PRODUCT_MANUFACTURER := Vetro
PRODUCT_MODEL := Vetro arm64
PRODUCT_MAX_PAGE_SIZE_SUPPORTED := 16384

# Uncompressed APEXes (updatable_apex.mk sets true; a single-value variable,
# so the product's own assignment wins): with .capex apexd decompresses about
# twenty of them into /data at every first boot (tens of seconds of guest time
# under QEMU, and ~200 MB written to /data, i.e. into the copy-on-write disk
# overlay of the browser and its snapshot). The price is a bigger super.img,
# which the browser reads on demand.
PRODUCT_COMPRESSED_APEX := false

# ro.product.system.*: generic_system.mk sets Android/mainline/generic (for
# the GSI); here the system partition is Vetro's only (ADR 0030).
PRODUCT_SYSTEM_NAME := vetro_arm64
PRODUCT_SYSTEM_DEVICE := vetro_arm64
PRODUCT_SYSTEM_BRAND := Vetro
PRODUCT_SYSTEM_MANUFACTURER := Vetro
PRODUCT_SYSTEM_MODEL := Vetro arm64

PRODUCT_VENDOR_PROPERTIES += \
    ro.soc.manufacturer=Vetro \
    ro.soc.model=vetro_arm64
