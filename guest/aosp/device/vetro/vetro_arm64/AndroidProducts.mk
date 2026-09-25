#
# Prodotti di Vetro (docs/adr/0022-immagine-aosp-di-vetro.md).
#

PRODUCT_MAKEFILES := \
    vetro_arm64:$(LOCAL_DIR)/vetro_arm64.mk

COMMON_LUNCH_CHOICES := \
    vetro_arm64-bp1a-userdebug
