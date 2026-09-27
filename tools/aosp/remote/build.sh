#!/bin/bash
# On the build VM: builds the Vetro image (launched detached by
# tools/aosp/build.sh). The AOSP build is incremental: if the VM
# is stopped, relaunching resumes where it was.
# Status in ~/$WORK/build.status: RUNNING, OK or FAIL <code>; log in
# ~/$WORK/build.log (appended on every attempt). ~/$WORK/build.rev = the synced
# Vetro commit (sync.rev) this build is the result of.
# No set -u: build/envsetup.sh uses undefined variables.
# "sconosciuta" (unknown) is a value read by tools/aosp/upload.sh: keep it.
set -o pipefail
cd || exit 1
tree="$HOME/${VETRO_AOSP_TREE:-aosp}"
work="$HOME/${VETRO_AOSP_WORK:-vetro-aosp}"
lunch_target="${VETRO_AOSP_LUNCH:-vetro_arm64-bp1a-userdebug}"
mkdir -p "$work"
echo RUNNING > "$work/build.status"
echo $$ > "$work/build.pid"
rm -f "$work/build.rev"
rev="$(cat "$work/sync.rev" 2>/dev/null || echo sconosciuta)"
start=$(date +%s)
(
  echo "=== $(date -u +%FT%TZ) build of $lunch_target (Vetro $rev), $(nproc) CPU, $(free -g | awk '/^Mem:/ {print $2}') GiB"
  cd "$tree" || exit 1
  # ccache: off by default. The build sandbox (nsjail) mounts everything
  # read-only except the tree and out/, so the cache lives in out/.ccache.
  # Turning it on (VETRO_AOSP_CCACHE=1, needs /usr/bin/ccache) changes the command
  # line of every C/C++ compilation (CC_WRAPPER and, with USE_CCACHE,
  # -Wno-unused-command-line-argument): the first build recompiles all the
  # C/C++, then it pays off only after an `m clean` or an AOSP tag change. The
  # images do not change (the wrapper and the extra warning do not touch the
  # generated code; CCACHE_COMPILERCHECK=content).
  if [ "${VETRO_AOSP_CCACHE:-0}" = 1 ] && command -v ccache >/dev/null; then
    cc="$(command -v ccache)"
    export USE_CCACHE=1 CCACHE_EXEC="$cc" CC_WRAPPER="$cc"
    export CCACHE_DIR="$tree/out/.ccache" CCACHE_COMPILERCHECK=content
    mkdir -p "$CCACHE_DIR"
    ccache -M 50G >/dev/null
    echo "ccache: $CCACHE_DIR"
  else
    [ "${VETRO_AOSP_CCACHE:-0}" = 1 ] && echo "ccache requested but not installed (sudo apt install ccache): building without"
    unset USE_CCACHE CCACHE_EXEC CC_WRAPPER CCACHE_DIR
  fi
  source build/envsetup.sh
  lunch "$lunch_target" || exit 1
  # droid = all images (boot, vendor_boot, init_boot, super,
  # userdata, vbmeta). The GPT disk is assembled on the Mac (tools/aosp/mkdisk.sh).
  # -k: do not stop at the first error, to see them all in one run.
  m -k droid
) >> "$work/build.log" 2>&1
code=$?
end=$(date +%s)
{
  # Measurements (performance is measured): duration and size of the host tools.
  echo "=== duration $(( (end - start) / 60 )) min"
  du -sh "$tree"/out/host/* 2>/dev/null | sed 's/^/=== host: /'
  [ "${VETRO_AOSP_CCACHE:-0}" = 1 ] && CCACHE_DIR="$tree/out/.ccache" ccache -s 2>/dev/null | sed 's/^/=== ccache: /'
} >> "$work/build.log" 2>&1
if [ "$code" -eq 0 ]; then
  echo "$rev" > "$work/build.rev"
  echo OK > "$work/build.status"
else
  echo "FAIL $code" > "$work/build.status"
fi
echo "=== $(date -u +%FT%TZ) end, code $code" >> "$work/build.log"
exit "$code"
