#
# Prodotto Vetro arm64 (telefono, solo 64 bit). Parte da
# device/google/cuttlefish/vsoc_arm64_only/phone/aosp_cf.mk (AOSP 15) e ne
# cambia solo ciò che la macchina di Vetro richiede: vedi
# docs/adr/0022-immagine-aosp-di-vetro.md e docs/specs/guest-image.md.
#

#
# Tutto ciò che va in system (come il GSI)
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
# vendor: quello del telefono Cuttlefish (HAL virtuali, SwiftShader,
# minigbm, HWC ranchu/drm_hwcomposer, KeyMint e Gatekeeper software).
# Niente packages/modules/Virtualization: la virt di Vetro non ha KVM.
#
$(call inherit-product, device/google/cuttlefish/shared/phone/device_vendor.mk)
$(call inherit-product, device/google/cuttlefish/vsoc_arm64/bootloader.mk)

# Parti nostre: fstab, init, microG.
$(call inherit-product, device/vetro/vetro_arm64/device.mk)

# Esclude le funzioni non disponibili sui dispositivi AOSP.
PRODUCT_COPY_FILES += \
    frameworks/native/data/etc/aosp_excluded_hardware.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/aosp_excluded_hardware.xml

PRODUCT_NAME := vetro_arm64
PRODUCT_DEVICE := vetro_arm64
PRODUCT_BRAND := Vetro
PRODUCT_MANUFACTURER := Vetro
PRODUCT_MODEL := Vetro arm64
PRODUCT_MAX_PAGE_SIZE_SUPPORTED := 16384

PRODUCT_VENDOR_PROPERTIES += \
    ro.soc.manufacturer=Vetro \
    ro.soc.model=vetro_arm64
