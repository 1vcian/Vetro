#
# Vetro modules that remove inherited packages (ADR 0037). In make, not in
# Android.bp: the `overrides` of a Soong prebuilt_etc never reaches make
# (AOSP 15 emits LOCAL_OVERRIDES_* only for apps, RROs, binaries and
# libraries), so VetroGoRemovals and VetroMissingHardware did nothing as
# prebuilt_etc (measured: the HAL APEXes were still in vendor.img). An ETC
# module's LOCAL_OVERRIDES_MODULES is honoured by main.mk (module-overrides):
# the overridden modules leave the product when these are installed.
#

LOCAL_PATH := $(call my-dir)

# Hardware the virt machine doesn't have (device.mk): the features declared
# unavailable (SystemConfig drops them whoever declared them, e.g.
# Cuttlefish's android.hardware.uwb.xml) and the HAL APEXes not installed.
include $(CLEAR_VARS)
LOCAL_MODULE := VetroMissingHardware
LOCAL_LICENSE_KINDS := legacy_notice
LOCAL_LICENSE_CONDITIONS := notice
LOCAL_NOTICE_FILE := $(LOCAL_PATH)/LICENSE
LOCAL_MODULE_CLASS := ETC
LOCAL_MODULE_STEM := vetro_missing_hardware.xml
LOCAL_SRC_FILES := vetro_missing_hardware.xml
LOCAL_VENDOR_MODULE := true
LOCAL_MODULE_RELATIVE_PATH := permissions
LOCAL_OVERRIDES_MODULES := \
    com.android.hardware.uwb \
    com.android.hardware.threadnetwork \
    ThreadNetworkDemoApp \
    com.google.cf.nfc
include $(BUILD_PREBUILT)

# The vetro_arm64_go product only (go/go.mk): apps it does not install and
# microG's battery-saving exemption (it dozes and defers its jobs like any app:
# it kept ~27% of the idle CPU).
include $(CLEAR_VARS)
LOCAL_MODULE := VetroGoRemovals
LOCAL_LICENSE_KINDS := legacy_notice
LOCAL_LICENSE_CONDITIONS := notice
LOCAL_NOTICE_FILE := $(LOCAL_PATH)/LICENSE
LOCAL_MODULE_CLASS := ETC
LOCAL_MODULE_STEM := vetro-go-removals.txt
LOCAL_SRC_FILES := go/go-removals.txt
LOCAL_PRODUCT_MODULE := true
LOCAL_OVERRIDES_MODULES := \
    BasicDreams \
    PhotoTable \
    EasterEgg \
    Traceur \
    DeviceAsWebcam \
    Music \
    Calendar \
    sysconfig-vetro-microg.xml
include $(BUILD_PREBUILT)
