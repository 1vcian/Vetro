#
# Vetro arm64 with the low-RAM "Go" profile (ADR 0037): the vetro_arm64
# product plus go/go.mk. Lunch target vetro_arm64_go-bp1a-userdebug; images
# published as their own version, never as the site's default.
#

$(call inherit-product, device/vetro/vetro_arm64/vetro_arm64.mk)
$(call inherit-product, device/vetro/vetro_arm64/go/go.mk)

PRODUCT_NAME := vetro_arm64_go
