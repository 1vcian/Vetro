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

# Slim image (ADR 0043): what an emulator in a browser tab does not need,
# removed from the inherited system, system_ext, product and vendor lists, and
# the features of the framework services that would serve it declared
# unavailable (vetro_slim.xml: SystemServer then starts no PrintManagerService,
# BackupManagerService, MidiService or WifiP2pService). Kept: what an ordinary
# app needs (package and activity managers, input, graphics, network, storage,
# WebView, notifications, keyboard, the contacts, calendar, telephony and
# download providers, documents, the permission controller) and every
# mainline APEX (their jars are on the boot and system server classpaths the
# preopted code is compiled against).
include $(CLEAR_VARS)
LOCAL_MODULE := VetroSlim
LOCAL_LICENSE_KINDS := legacy_notice
LOCAL_LICENSE_CONDITIONS := notice
LOCAL_NOTICE_FILE := $(LOCAL_PATH)/LICENSE
LOCAL_MODULE_CLASS := ETC
LOCAL_MODULE_STEM := vetro_slim.xml
LOCAL_SRC_FILES := vetro_slim.xml
LOCAL_VENDOR_MODULE := true
LOCAL_MODULE_RELATIVE_PATH := permissions
# Telephony apps (no modem, TARGET_NO_TELEPHONY: com.android.phone was
# persistent and idle), printing, backup, MIDI, secure element and NFC tag,
# MTP, cameras, dreams and live wallpapers, demo, trace and diagnostic apps,
# work-profile provisioning, the accessibility menu, the old browser's
# bookmarks, music and calendar apps; Cuttlefish's host-service app and test
# servers; vendor HAL APEXes for hardware the virt machine lacks or for
# alternatives the bootconfig does not select.
LOCAL_OVERRIDES_MODULES := \
    TeleService \
    ONS \
    CarrierDefaultApp \
    CallLogBackup \
    com.android.cellbroadcast \
    CellBroadcastLegacyApp \
    MmsService \
    SimAppDialog \
    Stk \
    QualifiedNetworksService \
    CFSatelliteService \
    GbaService \
    PrintSpooler \
    BuiltInPrintService \
    PrintRecommendationService \
    LocalTransport \
    SharedStorageBackup \
    BackupRestoreConfirmation \
    WallpaperBackup \
    BluetoothMidiService \
    SecureElement \
    Tag \
    MtpService \
    CameraExtensionsProxy \
    DeviceAsWebcam \
    Camera2 \
    BasicDreams \
    PhotoTable \
    LiveWallpapersPicker \
    EasterEgg \
    Traceur \
    DeviceDiagnostics \
    DynamicSystemInstallationService \
    ManagedProvisioning \
    AccessibilityMenu \
    BookmarkProvider \
    PartnerBookmarksProvider \
    Music \
    Calendar \
    CuttlefishService \
    aidl_lazy_test_server \
    aidl_lazy_cb_test_server \
    com.android.hardware.authsecret \
    com.android.hardware.cas \
    com.android.hardware.contexthub \
    com.android.hardware.neuralnetworks \
    com.android.hardware.net.nlinterceptor \
    com.android.hardware.tetheroffload \
    com.android.hardware.security.secretkeeper \
    com.android.hardware.gatekeeper.cf_remote \
    com.android.hardware.keymint.rust_cf_remote \
    com.android.hardware.keymint.rust_cf_guest_trusty_nonsecure \
    com.android.hardware.graphics.composer.drm_hwcomposer \
    com.google.cf.confirmationui
include $(BUILD_PREBUILT)
