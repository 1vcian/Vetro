# ADR 0030 — Development CA in the trust store, trademarks and a lighter AOSP image build

- Status: accepted (M5/M7, 2026-09-26), prepared with the VM off: no
  build has tried it yet (see "Verification"). Builds on ADR 0022 (AOSP
  image) and ADR 0029 (TLS hooks: sinkhole TLS server). Details:
  `docs/specs/guest-image.md`.
- Number: on rebase onto main, renumber if 0030 is already taken (0028
  belongs to the browser agent, branch `web/android-nel-browser`).

## Context
Three things were waiting for the next AOSP rebuild, and the build VM is
turned on only once for all of them:
1. for the sinkhole's TLS endpoint (ADR 0029, M7/M8) the app in the guest must
   complete the handshake: the certificate the sinkhole presents must chain
   up to a CA the system trusts;
2. the AOSP UI shows trademarks that are not ours: Launcher3's "Google"
   search bar, the robot as the icon of the package installer and of apps
   without an icon, `ro.product.system.*` = Android/mainline/generic; CLAUDE.md:
   the product does not present itself as "Android";
3. `m droid` builds host tools Vetro does not need (M5 progress log).

## Decision

### Vetro development CA
- **Our own CA, EC P-256, ECDSA-SHA256**, 10 years, `CA:TRUE, pathlen:0`
  (signs only end-entity certificates), `keyCertSign, cRLSign`; subject
  `O=Vetro, OU=Vetro development builds, CN=Vetro Development CA (not for
  production)`. P-256 because BoringSSL and Conscrypt accept it in every
  version and the sinkhole will have to sign a certificate on the fly for
  every SNI.
- **Only the certificate in the repository** (`guest/aosp/vendor/vetro/dev-ca/
  vetro-dev-ca.pem`, `79e94fc0.0`); **the private key outside**, in
  `~/.config/vetro/dev-ca/vetro-dev-ca.key` (0600, with a copy of the
  certificate). `tools/aosp/dev-ca.sh new` generates it (it refuses to
  overwrite it without `--force`); if it is lost: `new --force`, new patches,
  new build. `tools/aosp/dev-ca.sh` (check, also run by `sync.sh`)
  verifies that patches and certificate match and, if the key is present,
  that it corresponds.
- **Where AOSP 15 reads it.** `TrustedCertificateStore` (Conscrypt) and
  `SystemCertificateSource` (framework) use
  `/apex/com.android.conscrypt/cacerts` when the SDK is ≥ 34 and the folder is
  not empty; `/system/etc/security/cacerts` only with the Java property
  `system.certs.enabled=true`. The APEX content comes from
  `external/conscrypt/apex/ca-certificates/files/*` (`prebuilt_etc_cacerts`
  `cacerts_apex`); in our build the APEX is built from source
  (`com.android.conscrypt.capex` signed with the test key, verified
  on the image `…-9d91633`).
- **Method: one patch per project** that adds `79e94fc0.0` in AOSP's format
  (PEM, text, fingerprint), applied by `tools/aosp/remote/prepare.sh`
  like the `frameworks/base` one: `guest/aosp/patches/external/conscrypt/`
  (the real trust store) and `guest/aosp/patches/system/ca-certificates/`
  (`/system/etc/security/cacerts`, kept identical for the fallback). The
  patches also end up in `sources/aosp/vetro-patches/` published with
  the image. `prepare.sh` removes from the two trust stores the untracked
  files that no current patch creates: after a CA change the old one does not
  remain. Only the conscrypt APEX (the jar and ART's boot image do not
  change) and system are rebuilt.
- **The risk, in writing.** Whoever has the key can impersonate any site to
  a Vetro development image. That is why the key is not in git; but a
  sinkhole that signs on the fly in the browser must have it: **an image with
  this CA and a public vetro-wasm containing the key go together only
  as long as the guest's traffic does not go out to the Internet** (sinkhole).
  If one day Vetro forwards traffic to the real network, the CA in the trust
  store must be removed from the public image or replaced by a per-installation
  one. CI does not have the key: the TLS sinkhole tests must generate an
  ephemeral CA; the image's CA is only for the tests with Android.

### Trademarks
Only overlays and configuration, no changes to AOSP sources:
- **Launcher3 without "Google"**: the search bar on the first page is the
  QuickSearchBox widget (it is the global search activity, and the field
  uses `hint_google`, with "Google Search" activities and strings). It is not
  installed: `overrides: ["QuickSearchBox"]` on `VetroFrameworkOverlay`
  (`module-overrides` in `main.mk` removes it from the `PRODUCT_PACKAGES`
  inherited from `handheld_product.mk`). Without a provider Launcher3 shows
  its neutral bar `qsb_default_view` ("Search"). `QSB_ON_FIRST_SCREEN` is a
  `BuildConfig` constant: removing it would mean patching Launcher3.
- **No robot**: static overlays (RROs in `/product/overlay`, without a
  platform certificate: resources not declared `overlayable` can be
  overridden by preinstalled overlays) — `VetroFrameworkOverlay`
  replaces `mipmap/sym_def_app_icon` (the icon of apps without an icon) with
  an `anydpi-v26` vector, which wins over the per-density PNGs;
  `VetroPackageInstallerOverlay` the foreground layer `app_icon_foreground`
  of the package installer's adaptive icon.
- **Our own wallpaper**: `ro.config.wallpaper=/product/media/wallpaper/vetro.png`
  (`WallpaperManager.openDefaultWallpaper` reads it before the framework
  resource); 1920×1920 PNG generated by `tools/aosp/wallpaper.py`
  (deterministic, 94 KiB, gradient and slabs, no text). The system colours
  (Monet) follow the wallpaper.
- **`ro.product.system.*` = Vetro** (`PRODUCT_SYSTEM_*` in `vetro_arm64.mk`):
  `generic_system.mk` sets them to Android/mainline/generic for the GSI; the
  fingerprints were already all `Vetro/vetro_arm64/…`.
- What stays as in AOSP: the texts without a trademark ("Phone is starting…",
  "Search"), the "Android version" field in Settings (it is the system
  version, not the product name) and the framework's "Android System" label
  (translated into ~80 languages: an English-only overlay would change it
  halfway; to be done with all the translations if needed).

### Lighter build, same images
- **No cross host**: Cuttlefish sets `HOST_CROSS_OS := linux_musl`
  (arm64), and with `TARGET_BOARD_PLATFORM := vsoc_arm64` its
  `cvd_host_package` enters `droidcore` (`build/cvd-host-package.go`): every
  `m droid` builds the Cuttlefish host package for arm64 musl hosts too.
  `BoardConfig.mk` clears it (`HOST_CROSS_OS/ARCH/2ND_ARCH :=`): for
  Make it is the state of `envsetup.mk` with `BUILD_HOST_static` (no cross
  combo, `ifdef HOST_CROSS_OS` false), for Soong `CrossHost ""` (no cross
  target). Setting it back to windows (AOSP's default) was rejected: it needs
  the mingw prebuilts in the checkout, which we do not know if the VM has. Only
  `out/host` changes. In this incremental build the gain is only the analysis
  (the musl tools are already in `out/`); it matters at the next clean build.
  `remote/build.sh` writes to the log the duration and size of `out/host/*`.
- **Optional ccache** (`VETRO_AOSP_CCACHE=1`, off by default): the
  nsjail sandbox mounts everything read-only except the tree and `out/`, hence
  `CCACHE_DIR=$TREE/out/.ccache`; the wrapper goes through `CC_WRAPPER` (Soong)
  and `USE_CCACHE`/`CCACHE_EXEC` (Make). Off because it changes the command
  line of every C/C++ compilation: turned on now it would recompile all the
  native code once. Worth it before an AOSP tag change or after an `m clean`.
- Rejected: removing Cuttlefish's `PRODUCT_HOST_PACKAGES` (they cannot be
  filtered from above with `inherit-product`, and there are few of them) and
  changing `TARGET_BOARD_PLATFORM` (Cuttlefish's HALs and sepolicy use it).

### RAM profile for the browser
Not in this build: ADR 0028 names no properties to change (2 GiB
are enough: under half a GiB of anonymous memory at the end of the boot) and
foresees a browser bootconfig only if lmkd turned out too aggressive.

## Rejected alternatives
- **CA only in `/system/etc/security/cacerts` with
  `dalvik.vm.extra-opts=-Dsystem.certs.enabled=true`**: moves the whole trust
  store to a path that in AOSP 15 exists for tests, and changes the options
  of every zygote process.
- **Mounting over `/apex/com.android.conscrypt/cacerts` at boot** (as
  analysis proxies do on phones): depends on the mount namespaces of zygote
  and of every app; fragile.
- **CA in the user trust store**: apps with targetSdk ≥ 24 do not trust it.
- **conscrypt `override_apex`**: different package name, with effects on the
  bootclasspath; too much to add one file.
- **Overlays that redraw QuickSearchBox**: it is branded "Google" everywhere
  (activities, icon, strings, field background); removing it is cleaner.
- **Committed private key** (like AOSP's test-keys): convenient for
  CI, but it would make public forever a CA that Vetro images
  treat as a system CA.

## Verification
On the Mac (without the VM): `tools/aosp/dev-ca.sh` (certificate, patches and
key consistent); the patches applied and reverse-applied with `git apply`
as `prepare.sh` does; `79e94fc0.0` absent from AOSP 15's 145 certificates;
`bpfmt -d` on the `Android.bp` files; the two overlays compiled and linked with
the SDK's `aapt2` (resources `mipmap/sym_def_app_icon` anydpi-v26 and
`drawable/app_icon_foreground`); `BoardConfig.mk` and the product `.mk` files
evaluated with GNU make and stubs (cross host empty after the Cuttlefish
include, packages, copied files, properties, `PRODUCT_SYSTEM_*`); `shellcheck`
on the scripts; `init.vetro.rc` and sepolicy unchanged.
On the VM (`remote/pack.sh`, stops if one is missing): `79e94fc0.0` in the
`apex_build_info.pb` of the conscrypt capex and in
`/system/etc/security/cacerts`, overlays and wallpaper installed, no
QuickSearchBox, `ro.config.wallpaper` and `ro.product.system.brand=Vetro`.
On the guest (QEMU, then Vetro): `ls /apex/com.android.conscrypt/cacerts/
79e94fc0.0`, `cmd overlay list` with the two overlays active, screencap of
the home screen without the "Google" bar and with the wallpaper. Until these
pass, the decision is prepared and not tested.

## Consequences
- The TLS sinkhole (ADR 0029) can sign, with the key in
  `~/.config/vetro/dev-ca/`, certificates the image accepts; the test app
  that ignores the certificate is no longer needed for the handshake.
- Every new CA = `dev-ca.sh new --force` + new build and new version on R2.
- If the build rejects `overrides` on an RRO targeting an app (Soong's
  documentation says "only other overlays", Make does not check it) or the
  empty cross host, the two lines can be removed without touching the rest:
  they are independent.
