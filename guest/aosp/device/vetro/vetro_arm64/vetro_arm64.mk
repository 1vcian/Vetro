#
# Vetro arm64 product (phone, 64-bit only). Starts from
# device/google/cuttlefish/vsoc_arm64_only/phone/aosp_cf.mk (AOSP 15) and
# changes only what the Vetro machine requires: see
# docs/adr/0022-vetro-aosp-image.md and docs/specs/guest-image.md.
#

#
# Everything that goes into system (like the GSI)
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
# vendor: the one of the Cuttlefish phone (virtual HALs, SwiftShader,
# minigbm, HWC ranchu/drm_hwcomposer, software KeyMint and Gatekeeper).
# No packages/modules/Virtualization: Vetro's virt has no KVM.
#
# Cuttlefish HALs that talk to the host (vsock or /dev/hvcN) and, without a host,
# abort in a loop: lights block system_server (LightsService waits for
# ILights/default, declared in the VINTF by the APEX but never registered), the OEM
# lock does the same for OemLockService. They are removed with the switches of
# shared/device.mk (they apply before the inherit).
LOCAL_ENABLE_LIGHT := false
LOCAL_ENABLE_OEMLOCK := false
$(call inherit-product, device/google/cuttlefish/shared/phone/device_vendor.mk)
$(call inherit-product, device/google/cuttlefish/vsoc_arm64/bootloader.mk)

# Our parts: fstab, init, microG.
$(call inherit-product, device/vetro/vetro_arm64/device.mk)

# Excludes features not available on AOSP devices.
PRODUCT_COPY_FILES += \
    frameworks/native/data/etc/aosp_excluded_hardware.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/aosp_excluded_hardware.xml

PRODUCT_NAME := vetro_arm64
PRODUCT_DEVICE := vetro_arm64
PRODUCT_BRAND := Vetro
PRODUCT_MANUFACTURER := Vetro
PRODUCT_MODEL := Vetro arm64
PRODUCT_MAX_PAGE_SIZE_SUPPORTED := 16384

# ro.product.system.*: generic_system.mk sets Android/mainline/generic (for
# the GSI); here the system partition belongs to Vetro only (ADR 0030).
PRODUCT_SYSTEM_NAME := vetro_arm64
PRODUCT_SYSTEM_DEVICE := vetro_arm64
PRODUCT_SYSTEM_BRAND := Vetro
PRODUCT_SYSTEM_MANUFACTURER := Vetro
PRODUCT_SYSTEM_MODEL := Vetro arm64

PRODUCT_VENDOR_PROPERTIES += \
    ro.soc.manufacturer=Vetro \
    ro.soc.model=vetro_arm64
