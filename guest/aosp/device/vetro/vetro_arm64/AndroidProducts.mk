#
# Vetro products (docs/adr/0022-vetro-aosp-image.md; the low-RAM "Go"
# variant: docs/adr/0037-accelerated-guest-graphics.md).
#

PRODUCT_MAKEFILES := \
    vetro_arm64:$(LOCAL_DIR)/vetro_arm64.mk \
    vetro_arm64_go:$(LOCAL_DIR)/vetro_arm64_go.mk

COMMON_LUNCH_CHOICES := \
    vetro_arm64-bp1a-userdebug \
    vetro_arm64_go-bp1a-userdebug
