#!/usr/bin/env bash
# The GitHub Pages site: Vetro's web app with the M3 guest kernel, plus the
# GPL sources of what the site distributes (Linux kernel and the initramfs's
# BusyBox, see CLAUDE.md, "Licensing").
#
#   tools/pages/build.sh [output]        # default: target/pages
#
# Needs: target/guest-kernel (tools/guest-kernel/build.sh) and the network for
# the BusyBox sources and the prebuilt Android snapshot lookup. Builds
# vetro-wasm in release. Layout:
#
#   index.html, .nojekyll      landing page (links to app/ and docs/)
#   app/, node/                web/app and web/node (the app imports ../node/)
#   docs/                      the user guide, docs/user/*.md as HTML
#                              (tools/pages/markdown.mjs), with its images
#   app/android-prebuilt.json  the prebuilt Android snapshot for this
#                              vetro-wasm, if R2 has it (ADR 0031)
#   wasm/vetro_wasm.wasm       the machine
#   wasm/vetro_wasm_threads.wasm  the same with a shared memory (ADR 0041,
#                              `?threads=1`, isolation by coi-serviceworker)
#   guest/Image, guest/initramfs.cpio.gz
#   sources/                   GPL sources: kernel (in 60 MiB pieces, Pages's
#                              limit is 100 MiB per file), BusyBox and
#                              Alpine's patches, configuration, scripts
#
# No Android images: the system images are versioned artifacts on R2 (M5), and
# Google's SDK images are not redistributed.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
out=${1:-$root/target/pages}
guest=$root/target/guest-kernel

# The initramfs's BusyBox: Alpine package busybox-static 1.37.0-r20
# (target/guest-kernel/VERSIONS), i.e. the upstream sources plus the aports
# patches at the commit that introduced -r20 on the 3.22-stable branch.
BUSYBOX_VER=1.37.0
BUSYBOX_SHA256=3311dff32e746499f4df0d5df04d7eb396382d7e108bb9250e7b519b837043a4
APORTS_COMMIT=2e97d754a30d558f15524b8e422303c8a96832df
APORTS_PKGREL=20
APORTS_SHA256=f3969b717e36d97febb6f3694d0de12a94383fc45c150c5cb31b7a75df18e502
# Copy of the two archives on Cloudflare R2 (the origin servers sometimes
# refuse CI runners); tried first, the origin is the fallback. The content is
# verified with sha256 either way.
MIRROR=${VETRO_SOURCES_MIRROR:-https://pub-06e88fdd7f374fffb06844d60083f2ae.r2.dev/sources}

# Portable sha256: sha256sum (Linux) or shasum (macOS).
sha256() { if command -v sha256sum >/dev/null; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }

for f in Image initramfs.cpio.gz VERSIONS sources/README sources/defconfig; do
  [ -f "$guest/$f" ] || { echo "missing $guest/$f: run tools/guest-kernel/build.sh" >&2; exit 1; }
done
kernel_tar=$(ls "$guest"/sources/linux-*.tar.xz)
grep -q "busybox-static-$BUSYBOX_VER-r$APORTS_PKGREL " "$guest/VERSIONS" || {
  echo "VERSIONS does not say busybox-static-$BUSYBOX_VER-r$APORTS_PKGREL: update BUSYBOX_* and APORTS_* in $0" >&2
  exit 1
}

echo "==> vetro-wasm (release, wasm32)"
(cd "$root" && cargo build --release --target wasm32-unknown-unknown -p vetro-wasm)
echo "==> vetro-wasm threads build (ADR 0041)"
"$root/tools/wasm-threads.sh"

rm -rf "$out"
mkdir -p "$out"/{wasm,guest,sources}
cp -R "$root/web/app" "$out/app"
cp -R "$root/web/node" "$out/node"
cp "$root/target/wasm32-unknown-unknown/release/vetro_wasm.wasm" "$out/wasm/"
cp "$root/target/wasm32-unknown-unknown/release/vetro_wasm_threads.wasm" "$out/wasm/"
cp "$guest/Image" "$guest/initramfs.cpio.gz" "$out/guest/"
touch "$out/.nojekyll"

# The prebuilt Android snapshot for this vetro-wasm (ADR 0031): the app looks
# it up by its own key; the hint only tells the page its size. Written only if
# R2 has the snapshot for exactly this vetro-wasm (key: snapshot format and
# machine configuration), so the site never announces one it cannot restore.
echo "==> prebuilt Android snapshot"
node "$root/tools/aosp/prebuilt-key.mjs" --wasm="$out/wasm/vetro_wasm.wasm" --write="$out/app/android-prebuilt.json" \
  ${VETRO_REQUIRE_PREBUILT:+--require}

echo "==> user guide"
# docs/user/*.md as HTML pages (tools/pages/markdown.mjs), with their images.
node "$root/tools/pages/markdown.mjs" "$root/docs/user" "$out/docs"

echo "==> GPL sources"
src=$out/sources
cp "$guest"/sources/{README,defconfig} "$guest/VERSIONS" "$src/"
cp "$root/guest/kernel/config/vetro.config" "$src/"
for f in "$guest"/sources/*; do
  case $f in *.tar.xz|*/README|*/defconfig|*/VERSIONS) ;; *) cp -R "$f" "$src/" ;; esac
done
(cd "$src" && split -b 60m "$kernel_tar" "$(basename "$kernel_tar")." \
  && sha256 "$kernel_tar" | sed "s|  .*/|  |" > "$(basename "$kernel_tar").sha256")
# fetch FILE SHA256 ORIGIN_URL: from the cache, then the mirror, then the origin.
fetch() {
  local file=$1 sum=$2 origin=$3 url
  for url in "" "$MIRROR/$(basename "$1")" "$origin"; do
    if [ -n "$url" ]; then
      curl -sSfL --retry 3 -o "$file.tmp" "$url" || { echo "   (not downloaded from $url)"; continue; }
      mv "$file.tmp" "$file"
    fi
    [ -f "$file" ] && echo "$sum  $file" | sha256 -c - >/dev/null 2>&1 && return 0
    rm -f "$file"
  done
  echo "cannot get $(basename "$file") with sha256 $sum" >&2
  return 1
}
cache=$root/target/pages-cache
mkdir -p "$cache"
bb=$cache/busybox-$BUSYBOX_VER.tar.bz2
fetch "$bb" "$BUSYBOX_SHA256" "https://busybox.net/downloads/busybox-$BUSYBOX_VER.tar.bz2"
cp "$bb" "$src/"
ap=$cache/aports-$APORTS_COMMIT-main-busybox.tar.gz
fetch "$ap" "$APORTS_SHA256" \
  "https://gitlab.alpinelinux.org/alpine/aports/-/archive/$APORTS_COMMIT/aports-$APORTS_COMMIT.tar.gz?path=main/busybox"
tar xzf "$ap" -O "aports-$APORTS_COMMIT-main-busybox/main/busybox/APKBUILD" | grep -x "pkgrel=$APORTS_PKGREL" >/dev/null
cp "$ap" "$src/"
cat >> "$src/README" <<EOF

Sources on this site:
  $(basename "$kernel_tar").a?  the kernel tarball in pieces; put it back together with
      cat $(basename "$kernel_tar").a? > $(basename "$kernel_tar")
      shasum -a 256 -c $(basename "$kernel_tar").sha256
  busybox-$BUSYBOX_VER.tar.bz2   upstream sources (sha256 $BUSYBOX_SHA256)
  $(basename "$ap")
      Alpine's APKBUILD and patches for busybox $BUSYBOX_VER-r$APORTS_PKGREL
      (aports, commit $APORTS_COMMIT)
  vetro.config, defconfig, build.sh, Dockerfile, init, ...
      kernel configuration and initramfs build scripts.
Vetro's code is under PolyForm Noncommercial 1.0.0:
https://github.com/1vcian/Vetro
EOF

cat > "$out/index.html" <<'EOF'
<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Vetro</title>
  <link rel="icon" type="image/png" sizes="32x32" href="app/icons/icon-32.png">
  <link rel="icon" type="image/png" sizes="192x192" href="app/icons/icon-192.png">
  <link rel="apple-touch-icon" href="app/icons/icon-180.png">
  <meta property="og:image" content="app/icons/icon-512.png">
  <style>
    body { font: 16px/1.5 system-ui, sans-serif; max-width: 42rem; margin: 3rem auto; padding: 0 1rem; color: #1b1b1b; background: #fafafa; }
    a.button { display: inline-block; padding: .6rem 1.2rem; margin: 0 .4rem .4rem 0; background: #1b1b1b; color: #fff; border-radius: .4rem; text-decoration: none; }
    a.button.secondary { background: #fff; color: #1b1b1b; border: 1px solid #1b1b1b; }
    small { color: #555; }
  </style>
</head>
<body>
  <h1><img src="app/icons/icon-192.png" alt="" width="96" height="96" style="vertical-align:middle;margin-right:.5rem">Vetro</h1>
  <p>A full ARM64 system emulator written in Rust, running in your browser as
  WebAssembly: its own AArch64 CPU (interpreter and JIT to WebAssembly) on a
  copy of QEMU's <code>virt</code> board.</p>
  <p>This demo boots a small Linux 6.18 guest with a BusyBox shell. Nothing
  leaves your browser: disk writes and snapshots stay in its private storage
  (OPFS).</p>
  <p><a class="button" href="app/?autostart=1&amp;cmdline=console%3DttyAMA0%20vetro.noautotest">Launch the demo</a>
  <a class="button secondary" href="app/">Open the app</a>
  <a class="button secondary" href="docs/">User guide</a></p>
  <p>The app also runs Vetro's own AOSP 15 image with microG: choose it under
  <em>System</em>. The <a href="docs/">user guide</a> explains the first
  visit, using the phone, the network inspector, the file manager, record and
  replay, device profiles and privacy.</p>
  <p><small>Chrome or Edge desktop recommended.
  Source code: <a href="https://github.com/1vcian/Vetro">github.com/1vcian/Vetro</a>
  (PolyForm Noncommercial 1.0.0).
  The guest's Linux kernel and BusyBox are GPL-2.0: <a href="sources/">their
  exact sources</a>.</small></p>
</body>
</html>
EOF
{
  echo '<!doctype html><meta charset="utf-8"><title>Vetro - sources</title><h1>GPL sources</h1><pre>'
  sed 's/&/\&amp;/g; s/</\&lt;/g' "$src/README"
  echo '</pre><ul>'
  (cd "$src" && for f in *; do [ "$f" = index.html ] || echo "<li><a href=\"$f\">$f</a></li>"; done)
  echo '</ul>'
} > "$src/index.html"

echo "==> $out ($(du -sh "$out" | cut -f1))"
