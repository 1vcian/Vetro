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
# No telephony_system_ext.mk (CarrierConfig, EmergencyInfo): no modem (ADR
# 0040) and no telephony apps (ADR 0043).

#
# product
#
# aosp_product.mk without telephony_product.mk (Dialer,
# ImsServiceEntitlement), messaging and PhotoTable, and with AOSP's current
# sound set instead of every sound ever shipped (AllAudio.mk: 220 files the
# first media scan reads, against 57): ADR 0043.
$(call inherit-product, $(SRC_TARGET_DIR)/product/handheld_product.mk)
$(call inherit-product, frameworks/base/data/sounds/AudioPackage14.mk)
PRODUCT_PACKAGES += \
    initial-package-stopped-states-vetro.xml \
    preinstalled-packages-platform-aosp-product.xml \
    ThemePicker
# Default sounds from AudioPackage14 (handheld_system.mk's defaults are
# optional vendor properties naming files of AllAudio.mk).
PRODUCT_VENDOR_PROPERTIES += \
    ro.config.ringtone=Atria.ogg \
    ro.config.notification_sound=Tethys.ogg \
    ro.config.alarm_alert=Argon.ogg

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
# No telephony: Cuttlefish's RIL (com.google.cf.rild) talks to the host's
# modem simulator, which isn't there, so IRadioModem/slot1 never registers and
# com.android.phone waits for it once a second forever (servicemanager, init
# and logd work at every round; measured on the settled home screen, ADR
# 0037). Cuttlefish's own switch (shared/telephony/device_vendor.mk, used by
# its auto products): no rild and none of the telephony features its APEX
# declares, so PhoneGlobals does not create phones (no FEATURE_TELEPHONY).
TARGET_NO_TELEPHONY := true
# Cuttlefish's phone vendor without the HALs for hardware the virt machine
# lacks (camera, face, fingerprint, IR, identity, reboot escrow, secure
# element): vetro_vendor.mk, ADR 0043.
$(call inherit-product, device/vetro/vetro_arm64/vetro_vendor.mk)
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
