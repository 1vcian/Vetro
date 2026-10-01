#!/bin/sh
# M5: tests of the web app (web/app) and of vetro-wasm's device API,
# without npm dependencies:
#   1. syntax check of the JS modules (node --check);
#   2. JS unit tests (tests/web/unit.mjs): server with Range, DiskFeeder,
#      key map, terminal, persistence (MemFile, snapshot cache),
#      SQLite reader and file manager viewers;
#   3. native reference (tests/boot/tests/web.rs): the same scripts with
#      vetro-wasm's API compiled for the host, interpreter, local disk;
#   4. disk over HTTP Range (tests/web/boot-disk.mjs): the M3 kernel reads and
#      writes a disk served by a local server; instructions and log equal
#      to those with a local disk and to the native reference, also with the
#      restart from the cache;
#   5. display and inputs via the API (tests/web/devices.mjs): RGBA framebuffer
#      equal to the guest's pattern, cursor, keyboard, tablet, LEDs; instructions
#      and log equal to the native reference;
#   6. connections from JS to a TCP service of the guest
#      (tests/web/hostfwd.mjs, GuestSocket, port forwarding): echo of
#      200 KB, close, refusal; two identical runs;
#   7. snapshots and persistent disk overlay via the API (tests/web/snapshot.mjs,
#      M6): saves at the prompt and halfway through boot, restores on new machines (JIT
#      and interpreter), same continuation of the log, same instructions, same overlay
#      file; writes found again by a boot from scratch; overlay of
#      another base discarded; timings and sizes in V8;
#   8. file manager via the API (tests/web/files.mjs, M8, ADR 0020): list,
#      reads, writes that preserve mode and owner as read by the guest,
#      inotify event within 1 s of guest time, 1.2 MB in chunks; SQL
#      in the guest on an open WAL database, seen by the reader in the -wal,
#      rewritten SharedPreferences, non-UTF-8 name (ADR 0021); two
#      identical runs;
#   9. network inspector and timeline via the API (tests/web/inspector.mjs, M7,
#      ADR 0023): JSON and form POST from wget to the sinkhole, list, detail with
#      decoded bodies, HAR, pcapng, requests and DNS attributed to the
#      command; two identical runs;
#  10. record & replay via the API (tests/web/replay.mjs, M10): recording with
#      keyframes, log from a file, keyframes in the archive (Recording), identical
#      replay with JIT and interpreter (console, inspector and timeline equal),
#      jump to an instruction with the same registers and the same memory;
#  11. the app in headless Chrome (tests/web/browser.mjs), if Chrome is available
#      (VETRO_CHROME; otherwise SKIP, which is not a passed test;
#      VETRO_REQUIRE_BROWSER=1 makes it an error): also snapshots and persistent
#      disks in OPFS, the second boot from the snapshot (time measured)
#      and the file manager panel (live tree, a file edited
#      and saved in the panel, reread by the guest with cat; a SQLite cell and
#      a preference changed from the panel and reread by the guest).
#  12. inspector, timeline and record & replay in the app in Chrome
#      (tests/web/browser-analysis.mjs): wget to the sinkhole in the inspector
#      with the decoded JSON body tied to the command in the timeline,
#      download of log, HAR and pcapng, identical replay, jump to an instruction
#      with registers and memory, log reloaded and replayed.
#  13. boot from Android images and 3 GiB of RAM (tests/web/android-boot.mjs,
#      M5, ADR 0028): boot.img and init_boot.img from mkbootimg.py around the
#      M3 kernel, vetro_load_android; instructions and log equal to the native
#      reference; chunked snapshot and restore on a new 3 GiB machine; a tiny
#      JIT code limit with the same execution;
#  14. ADB client against a fake adbd (tests/web/adb.mjs): CNXN, AUTH,
#      shell v2, push (with progress), install, devices;
#  15. the app catalog against the fake adbd (tests/web/catalog.mjs, M6,
#      ADR 0033): catalog over HTTP, verified download, install, installed
#      packages, tampered APKs rejected before the device;
#  16. the catalog panel in headless Chrome (tests/web/browser-catalog.mjs):
#      cards, icon under COEP, Install -> downloading -> installing -> Open,
#      Retry after a tampered APK, Update, hidden without a catalog.
#  17. two guest cores in parallel in the app in Chrome (tests/web/browser-smp.mjs,
#      ADR 0042): threads build, core 1 in a Worker, two processors seen, the
#      snapshot with the cores in turns, restored with the cores parallel again.
#
#   tools/web-test.sh [--no-jit]
#
# Needs Node >= 22 and target/guest-kernel (tools/guest-kernel/build.sh).
set -eu
cd "$(dirname "$0")/.."

for a in "$@"; do
  case "$a" in
    --no-jit) ;;
    *) echo "usage: tools/web-test.sh [--no-jit]" >&2; exit 2 ;;
  esac
done

command -v node >/dev/null || { echo "ERROR: node not found (Node >= 22 needed)" >&2; exit 1; }
major=$(node -p 'process.versions.node.split(".")[0]')
[ "$major" -ge 22 ] || { echo "ERROR: Node $(node --version), >= 22 needed" >&2; exit 1; }
[ -f target/guest-kernel/Image ] && [ -f target/guest-kernel/initramfs.cpio.gz ] \
  || { echo "ERROR: target/guest-kernel missing: run tools/guest-kernel/build.sh" >&2; exit 1; }

echo "==> vetro-wasm (release, wasm32-unknown-unknown)"
cargo build --release --target wasm32-unknown-unknown -p vetro-wasm

echo "==> syntax of the JS modules"
for f in web/app/*.mjs web/node/*.mjs tools/web-serve.mjs tests/web/*.mjs; do
  node --check "$f"
done

echo "==> JS unit tests"
node tests/web/unit.mjs

echo "==> native reference (same scripts, interpreter, local disk)"
cargo test --release -p vetro-boot-tests --test web -- --nocapture
# The Node tests must give the same instructions and the same log.
export VETRO_WEB_NATIVE=1

echo "==> disk over HTTP Range"
node tests/web/boot-disk.mjs "$@"

echo "==> display and inputs via the API"
node tests/web/devices.mjs "$@"

echo "==> connections to the guest via the API"
node tests/web/hostfwd.mjs "$@"

echo "==> snapshots and persistent disk overlay via the API (M6)"
node tests/web/snapshot.mjs "$@"

echo "==> file manager via the API (M8)"
node tests/web/files.mjs "$@"

echo "==> network inspector and timeline via the API (M7)"
node tests/web/inspector.mjs "$@"

echo "==> record & replay via the API (M10)"
node tests/web/replay.mjs "$@"

echo "==> boot from boot.img and 3 GiB of RAM (M5)"
node tests/web/android-boot.mjs "$@"

echo "==> ADB client against a fake adbd (M5)"
node tests/web/adb.mjs

echo "==> app catalog against a fake adbd (M6)"
node tests/web/catalog.mjs

echo "==> app in headless Chrome"
node tests/web/browser.mjs

echo "==> inspector, timeline and record & replay in the app in Chrome"
node tests/web/browser-analysis.mjs

echo "==> app catalog panel in Chrome (M6)"
node tests/web/browser-catalog.mjs

echo "==> two guest cores in parallel in the app in Chrome (ADR 0042)"
tools/wasm-threads.sh
node tests/web/browser-smp.mjs
