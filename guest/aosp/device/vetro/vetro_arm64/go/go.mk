#
# Vetro "Go" profile, build-time part (ADR 0037, "Low-RAM profile"): the
# vetro_arm64_go product. Android Go's low-RAM defaults (ro.config.low_ram,
# lmkd pressure thresholds, smaller Dalvik heaps, speed-profile for
# system_server, the Go handheld feature list) and fewer preinstalled
# background/demo apps. The same defaults can be switched on at boot on the
# regular image with androidboot.vetro.profile=go (go/init.vetro-go.rc), apart
# from the removed apps and the build-time compiler choices.
#

$(call inherit-product, $(SRC_TARGET_DIR)/product/go_defaults_common.mk)

# Removes apps nothing in the boot or in analysis needs (screensavers,
# demos, the music and calendar apps) and microG's battery-saving exemption
# (it dozes like any app): LOCAL_OVERRIDES_MODULES in Android.mk.
PRODUCT_PACKAGES += VetroGoRemovals

PRODUCT_PRODUCT_PROPERTIES += \
    ro.vetro.profile=go
