#!/bin/sh
# M5: test dell'app web (web/app) e dell'API dei dispositivi di vetro-wasm,
# senza dipendenze npm:
#   1. controllo della sintassi dei moduli JS (node --check);
#   2. test unitari JS (tests/web/unit.mjs): server con Range, DiskFeeder,
#      mappa dei tasti, terminale, persistenza (MemFile, cache degli snapshot),
#      lettore SQLite e visualizzatori del gestore dei file;
#   3. riferimento nativo (tests/boot/tests/web.rs): gli stessi copioni con
#      l'API di vetro-wasm compilata per l'host, interprete, disco locale;
#   4. disco via HTTP Range (tests/web/boot-disk.mjs): il kernel M3 legge e
#      scrive un disco servito da un server locale; istruzioni e log uguali
#      a quelli con un disco locale e al riferimento nativo, anche col
#      riavvio dalla cache;
#   5. display e ingressi via API (tests/web/devices.mjs): framebuffer RGBA
#      uguale al motivo del guest, cursore, tastiera, tablet, LED; istruzioni
#      e log uguali al riferimento nativo;
#   6. connessioni dal JS verso un servizio TCP del guest
#      (tests/web/hostfwd.mjs, GuestSocket, inoltro di porte): eco di
#      200 KB, chiusura, rifiuto; due esecuzioni uguali;
#   7. snapshot e overlay persistente dei dischi via API (tests/web/snapshot.mjs,
#      M6): salva al prompt e a metà avvio, ripristina su macchine nuove (JIT
#      e interprete), stesso seguito del log, stesse istruzioni, stesso file
#      dell'overlay; scritture ritrovate da un avvio da zero; overlay di
#      un'altra base scartato; tempi e dimensioni in V8;
#   8. gestore dei file via API (tests/web/files.mjs, M8, ADR 0020): list,
#      letture, scritture che conservano modo e proprietario lette dal guest,
#      evento di inotify entro 1 s di tempo del guest, 1,2 MB a pezzi; SQL
#      nel guest su un database in WAL aperto, visto dal lettore nel -wal,
#      SharedPreferences riscritte, nome non UTF-8 (ADR 0021); due
#      esecuzioni uguali;
#   9. ispettore di rete e timeline via API (tests/web/inspector.mjs, M7,
#      ADR 0023): POST JSON e form di wget al sinkhole, lista, dettaglio con
#      corpi decodificati, HAR, pcapng, richieste e DNS attribuiti al
#      comando; due esecuzioni uguali;
#  10. record & replay via API (tests/web/replay.mjs, M10): registrazione con
#      keyframe, log da file, keyframe nell'archivio (Recording), replay
#      identico con JIT e interprete (console, ispettore e timeline uguali),
#      salto a un'istruzione con gli stessi registri e la stessa memoria;
#  11. l'app in Chrome headless (tests/web/browser.mjs), se Chrome c'è
#      (VETRO_CHROME; altrimenti SKIP, che non è un test passato;
#      VETRO_REQUIRE_BROWSER=1 lo rende un errore): anche snapshot e dischi
#      persistenti in OPFS, il secondo avvio dallo snapshot (tempo misurato)
#      e il pannello del gestore dei file (albero dal vivo, un file modificato
#      e salvato nel pannello, riletto dal guest con cat; una cella SQLite e
#      una preferenza cambiate dal pannello e rilette dal guest).
#  12. ispettore, timeline e record & replay nell'app in Chrome
#      (tests/web/browser-analysis.mjs): wget verso il sinkhole nell'ispettore
#      con il corpo JSON decodificato e legato al comando nella timeline,
#      download di log, HAR e pcapng, replay identico, salto a un'istruzione
#      con registri e memoria, log ricaricato e rigiocato.
#  13. avvio da immagini Android e RAM di 3 GiB (tests/web/android-boot.mjs,
#      M5, ADR 0028): boot.img e init_boot.img di mkbootimg.py intorno al
#      kernel M3, vetro_load_android; istruzioni e log uguali al riferimento
#      nativo; snapshot e ripristino su una macchina nuova da 3 GiB;
#  14. client ADB contro un finto adbd (tests/web/adb.mjs): CNXN, AUTH,
#      shell v2, push, install, devices.
#
#   tools/web-test.sh [--no-jit]
#
# Serve Node >= 22 e target/guest-kernel (tools/guest-kernel/build.sh).
set -eu
cd "$(dirname "$0")/.."

for a in "$@"; do
  case "$a" in
    --no-jit) ;;
    *) echo "uso: tools/web-test.sh [--no-jit]" >&2; exit 2 ;;
  esac
done

command -v node >/dev/null || { echo "ERRORE: node non trovato (serve Node >= 22)" >&2; exit 1; }
major=$(node -p 'process.versions.node.split(".")[0]')
[ "$major" -ge 22 ] || { echo "ERRORE: Node $(node --version), serve >= 22" >&2; exit 1; }
[ -f target/guest-kernel/Image ] && [ -f target/guest-kernel/initramfs.cpio.gz ] \
  || { echo "ERRORE: target/guest-kernel mancante: esegui tools/guest-kernel/build.sh" >&2; exit 1; }

echo "==> vetro-wasm (release, wasm32-unknown-unknown)"
cargo build --release --target wasm32-unknown-unknown -p vetro-wasm

echo "==> sintassi dei moduli JS"
for f in web/app/*.mjs web/node/*.mjs tools/web-serve.mjs tests/web/*.mjs; do
  node --check "$f"
done

echo "==> test unitari JS"
node tests/web/unit.mjs

echo "==> riferimento nativo (stessi copioni, interprete, disco locale)"
cargo test --release -p vetro-boot-tests --test web -- --nocapture
# I test in Node devono dare le stesse istruzioni e lo stesso log.
export VETRO_WEB_NATIVE=1

echo "==> disco via HTTP Range"
node tests/web/boot-disk.mjs "$@"

echo "==> display e ingressi via API"
node tests/web/devices.mjs "$@"

echo "==> connessioni verso il guest via API"
node tests/web/hostfwd.mjs "$@"

echo "==> snapshot e overlay persistente dei dischi via API (M6)"
node tests/web/snapshot.mjs "$@"

echo "==> gestore dei file via API (M8)"
node tests/web/files.mjs "$@"

echo "==> ispettore di rete e timeline via API (M7)"
node tests/web/inspector.mjs "$@"

echo "==> record & replay via API (M10)"
node tests/web/replay.mjs "$@"

echo "==> avvio da boot.img e RAM di 3 GiB (M5)"
node tests/web/android-boot.mjs "$@"

echo "==> client ADB contro un finto adbd (M5)"
node tests/web/adb.mjs

echo "==> app in Chrome headless"
node tests/web/browser.mjs

echo "==> ispettore, timeline e record & replay nell'app in Chrome"
node tests/web/browser-analysis.mjs
