#
# Vetro's vendor composition: Cuttlefish's phone vendor
# (device/google/cuttlefish/shared/phone/device_vendor.mk, AOSP 15) without
# the pieces for hardware the virt machine does not have and no app needs to
# find (ADR 0043, "Slim image"): no camera HAL (emulated cameras, two APEXes
# and a provider process), no face or fingerprint HALs (started after boot),
# no consumer IR, no identity credential HAL (credstore runs without it), no
# reboot-escrow HAL, no secure element HAL, no SIP/VoIP feature. Everything
# else is inherited as Cuttlefish's phone has it, in the same order.
#

PRODUCT_MANIFEST_FILES += device/google/cuttlefish/shared/config/product_manifest.xml
SYSTEM_EXT_MANIFEST_FILES += device/google/cuttlefish/shared/config/system_ext_manifest.xml

$(call inherit-product, $(SRC_TARGET_DIR)/product/handheld_vendor.mk)

PRODUCT_COPY_FILES += \
    frameworks/native/data/etc/handheld_core_hardware.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/handheld_core_hardware.xml

$(call inherit-product, frameworks/native/build/phone-xhdpi-2048-dalvik-heap.mk)
$(call inherit-product, device/google/cuttlefish/shared/bluetooth/device_vendor.mk)
$(call inherit-product, device/google/cuttlefish/shared/gnss/device_vendor.mk)
$(call inherit-product, device/google/cuttlefish/shared/graphics/device_vendor.mk)
$(call inherit-product, device/google/cuttlefish/shared/vibrator/device_vendor.mk)
$(call inherit-product, device/google/cuttlefish/shared/swiftshader/device_vendor.mk)
$(call inherit-product, device/google/cuttlefish/shared/telephony/device_vendor.mk)
$(call inherit-product, device/google/cuttlefish/shared/sensors/device_vendor.mk)
$(call inherit-product, device/google/cuttlefish/shared/virgl/device_vendor.mk)
$(call inherit-product, device/google/cuttlefish/shared/device.mk)

# Support mixing CF system onto previous versions of vendor (as Cuttlefish).
PRODUCT_EXTRA_VNDK_VERSIONS := 30 31 32 33 34

# Cuttlefish's phone product.prop and vendor.prop only set Bluetooth
# properties (class of device, profiles): there is no Bluetooth here.

PRODUCT_COPY_FILES += \
    frameworks/native/data/etc/android.hardware.touchscreen.multitouch.distinct.xml:$(TARGET_COPY_OUT_VENDOR)/etc/permissions/android.hardware.touchscreen.multitouch.distinct.xml

DEVICE_PACKAGE_OVERLAYS += device/google/cuttlefish/shared/phone/overlay

# Runtime Resource Overlays
PRODUCT_PACKAGES += cuttlefish_phone_overlay_frameworks_base_core

TARGET_BOARD_INFO_FILE ?= device/google/cuttlefish/shared/phone/android-info.txt

# Storage: for factory reset protection feature
PRODUCT_VENDOR_PROPERTIES += \
    ro.frp.pst=/dev/block/by-name/frp
