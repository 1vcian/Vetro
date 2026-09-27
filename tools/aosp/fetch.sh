#!/bin/sh
# Brings the artifacts of the last successful build to the Mac:
#   VM: tools/aosp/remote/pack.sh collects images, properties and build-info
#   Mac: target/aosp/out/ (rsync, then SHA256SUMS verified), check of the
#        ART ISA variant, then tools/aosp/mkdisk.sh assembles the GPT disk
#        (target/aosp/disk.img)
# Artifacts: boot.img, vendor_boot.img, init_boot.img, super.img and
# userdata.img (sparse), props/, build-info.txt, SHA256SUMS.
# Never committed (target/ is ignored). Idempotent.
set -eu
. "$(cd "$(dirname "$0")" && pwd)/common.sh"
vm "s=\$(cat $VETRO_AOSP_WORK/build.status 2>/dev/null); [ \"\$s\" = OK ] || { echo \"build not successful: \$s\" >&2; exit 1; }"
vm_rsync -a --delete "$here/remote/" "$VETRO_AOSP_HOST:$VETRO_AOSP_WORK/remote/"
vm "VETRO_AOSP_TREE=$VETRO_AOSP_TREE VETRO_AOSP_WORK=$VETRO_AOSP_WORK bash $VETRO_AOSP_WORK/remote/pack.sh"
mkdir -p "$out/out"
vm_rsync -a --delete --partial "$VETRO_AOSP_HOST:$VETRO_AOSP_WORK/out/" "$out/out/"
(cd "$out/out" && shasum -a 256 -c --quiet SHA256SUMS)
# ART must generate code for Vetro's CPU (Cortex-A53, ARMv8.0 + CRC32 +
# crypto): with newer variants the JIT would use LSE and FP16 (ADR 0005, 0022).
variants="$(grep -h '^dalvik.vm.isa.arm64.variant=' "$out"/out/props/*.prop | sort -u)"
grep -h -E '^dalvik\.vm\.isa\.arm64\.(variant|features)=' "$out"/out/props/*.prop | sort -u
if [ "$variants" != "dalvik.vm.isa.arm64.variant=cortex-a53" ]; then
  echo "ERROR: ART ISA variant other than cortex-a53: ${variants:-missing}" >&2
  exit 1
fi
if grep -h '^ro.product.cpu.abilist32=.' "$out"/out/props/*.prop; then
  echo "ERROR: the image declares 32-bit ABIs" >&2
  exit 1
fi
# Branding and development CA (ADR 0030; pack.sh already checked them on the VM).
grep -h -E '^ro\.(config\.wallpaper|product\.system\.brand)=' "$out"/out/props/*.prop | sort -u
grep -E '^(vetro_rev|dev_ca)=' "$out/out/build-info.txt"
if [ "$(sed -n 's/^dev_ca=//p' "$out/out/build-info.txt")" != "$(openssl x509 -in "$root/guest/aosp/vendor/vetro/dev-ca/vetro-dev-ca.pem" -noout -subject_hash_old).0" ]; then
  echo "ERROR: the image's development CA is not the one in guest/aosp/vendor/vetro/dev-ca" >&2
  exit 1
fi
"$here/mkdisk.sh"
