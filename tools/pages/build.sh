#!/usr/bin/env bash
# Il sito di GitHub Pages: l'app web di Vetro con il kernel guest di M3, più i
# sorgenti GPL di ciò che il sito distribuisce (kernel Linux e BusyBox
# dell'initramfs, vedi CLAUDE.md, "Licenze").
#
#   tools/pages/build.sh [uscita]        # predefinita: target/pages
#
# Serve: target/guest-kernel (tools/guest-kernel/build.sh) e la rete per i
# sorgenti di BusyBox. Costruisce vetro-wasm in release. Struttura:
#
#   index.html, .nojekyll      pagina d'ingresso (rimanda ad app/)
#   app/, node/                web/app e web/node (l'app importa ../node/)
#   wasm/vetro_wasm.wasm       la macchina
#   guest/Image, guest/initramfs.cpio.gz
#   sources/                   sorgenti GPL: kernel (a pezzi da 60 MiB, il
#                              limite di Pages è 100 MiB per file), BusyBox e
#                              patch di Alpine, configurazione, script
#
# Niente immagini Android: le immagini di sistema arrivano come artefatti
# versionati (M5) e quelle dell'SDK di Google non si ridistribuiscono.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
out=${1:-$root/target/pages}
guest=$root/target/guest-kernel

# BusyBox dell'initramfs: pacchetto Alpine busybox-static 1.37.0-r20
# (target/guest-kernel/VERSIONS), cioè i sorgenti upstream più le patch di
# aports al commit che ha introdotto -r20 sul ramo 3.22-stable.
BUSYBOX_VER=1.37.0
BUSYBOX_SHA256=3311dff32e746499f4df0d5df04d7eb396382d7e108bb9250e7b519b837043a4
APORTS_COMMIT=2e97d754a30d558f15524b8e422303c8a96832df
APORTS_PKGREL=20
APORTS_SHA256=f3969b717e36d97febb6f3694d0de12a94383fc45c150c5cb31b7a75df18e502
# Copia dei due archivi su Cloudflare R2 (i server d'origine a volte
# rifiutano i runner di CI); si prova per prima, l'origine è il ripiego.
# Il contenuto è comunque verificato con sha256.
MIRROR=${VETRO_SOURCES_MIRROR:-https://pub-06e88fdd7f374fffb06844d60083f2ae.r2.dev/sources}

# sha256 portabile: sha256sum (Linux) o shasum (macOS).
sha256() { if command -v sha256sum >/dev/null; then sha256sum "$@"; else shasum -a 256 "$@"; fi; }

for f in Image initramfs.cpio.gz VERSIONS sources/README sources/defconfig; do
  [ -f "$guest/$f" ] || { echo "manca $guest/$f: esegui tools/guest-kernel/build.sh" >&2; exit 1; }
done
kernel_tar=$(ls "$guest"/sources/linux-*.tar.xz)
grep -q "busybox-static-$BUSYBOX_VER-r$APORTS_PKGREL " "$guest/VERSIONS" || {
  echo "VERSIONS non indica busybox-static-$BUSYBOX_VER-r$APORTS_PKGREL: aggiorna BUSYBOX_* e APORTS_* in $0" >&2
  exit 1
}

echo "==> vetro-wasm (release, wasm32)"
(cd "$root" && cargo build --release --target wasm32-unknown-unknown -p vetro-wasm)

rm -rf "$out"
mkdir -p "$out"/{wasm,guest,sources}
cp -R "$root/web/app" "$out/app"
cp -R "$root/web/node" "$out/node"
cp "$root/target/wasm32-unknown-unknown/release/vetro_wasm.wasm" "$out/wasm/"
cp "$guest/Image" "$guest/initramfs.cpio.gz" "$out/guest/"
touch "$out/.nojekyll"

echo "==> sorgenti GPL"
src=$out/sources
cp "$guest"/sources/{README,defconfig} "$guest/VERSIONS" "$src/"
cp "$root/guest/kernel/config/vetro.config" "$src/"
for f in "$guest"/sources/*; do
  case $f in *.tar.xz|*/README|*/defconfig|*/VERSIONS) ;; *) cp -R "$f" "$src/" ;; esac
done
(cd "$src" && split -b 60m "$kernel_tar" "$(basename "$kernel_tar")." \
  && sha256 "$kernel_tar" | sed "s|  .*/|  |" > "$(basename "$kernel_tar").sha256")
# fetch FILE SHA256 URL_ORIGINE: dalla cache, poi dal mirror, poi dall'origine.
fetch() {
  local file=$1 sum=$2 origin=$3 url
  for url in "" "$MIRROR/$(basename "$1")" "$origin"; do
    if [ -n "$url" ]; then
      curl -sSfL --retry 3 -o "$file.tmp" "$url" || { echo "   (non scaricato da $url)"; continue; }
      mv "$file.tmp" "$file"
    fi
    [ -f "$file" ] && echo "$sum  $file" | sha256 -c - >/dev/null 2>&1 && return 0
    rm -f "$file"
  done
  echo "impossibile ottenere $(basename "$file") con sha256 $sum" >&2
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

Sorgenti su questo sito:
  $(basename "$kernel_tar").a?  il tarball del kernel a pezzi; ricomponilo con
      cat $(basename "$kernel_tar").a? > $(basename "$kernel_tar")
      shasum -a 256 -c $(basename "$kernel_tar").sha256
  busybox-$BUSYBOX_VER.tar.bz2   sorgenti upstream (sha256 $BUSYBOX_SHA256)
  $(basename "$ap")
      APKBUILD e patch di Alpine per busybox $BUSYBOX_VER-r$APORTS_PKGREL
      (aports, commit $APORTS_COMMIT)
  vetro.config, defconfig, build.sh, Dockerfile, init, ...
      configurazione del kernel e script di costruzione dell'initramfs.
Il codice di Vetro è sotto PolyForm Noncommercial 1.0.0:
https://github.com/1vcian/Vetro
EOF

cat > "$out/index.html" <<'EOF'
<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Vetro</title>
  <style>
    body { font: 16px/1.5 system-ui, sans-serif; max-width: 42rem; margin: 3rem auto; padding: 0 1rem; color: #1b1b1b; background: #fafafa; }
    a.button { display: inline-block; padding: .6rem 1.2rem; background: #1b1b1b; color: #fff; border-radius: .4rem; text-decoration: none; }
    small { color: #555; }
  </style>
</head>
<body>
  <h1>Vetro</h1>
  <p>A full ARM64 system emulator written in Rust, running in your browser as
  WebAssembly: its own AArch64 CPU (interpreter and JIT to WebAssembly) on a
  copy of QEMU's <code>virt</code> board.</p>
  <p>This demo boots a small Linux 6.18 guest with a BusyBox shell. Nothing
  leaves your browser: disk writes and snapshots stay in its private storage
  (OPFS).</p>
  <p><a class="button" href="app/?autostart=1&amp;cmdline=console%3DttyAMA0%20vetro.noautotest">Launch the demo</a></p>
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
