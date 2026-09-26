# vetro-probe — test app for M7 (TLS) and M8 (Binder/privacy)

Minimal Android app for testing Vetro from the outside. When the
activity starts (or via `am start`/`am broadcast`) it does, in a thread:

1. **Sensitive accesses (M8):** reads the clipboard
   (`ClipboardManager.getPrimaryClip` → Binder `IClipboard.getPrimaryClip`)
   and `Settings.Secure.ANDROID_ID`
   (`IContentProvider.call` to the settings provider). The
   Binder decoder must report them as sensitive (clipboard, ANDROID_ID).
2. **HTTPS request (M7):** a `POST` to `https://<host>/v1/eventi` with
   JSON body `{"android_id":"...","ts":...}`, using `HttpsURLConnection`
   (Conscrypt: internal `libssl`). With a `TrustManager` that accepts Vetro's
   development CA (or, for the test only, any certificate), so
   the connection completes against the sinkhole's TLS endpoint. The
   TLS hooks capture URL, headers and body in clear text, attributed to
   `com.vetro.probe` and to the library.

The request host is passed as the extra `--es host api.esempio.test`
(default `api.esempio.test`, which the sinkhole resolves to a fake address).

## Building

`tools/aidl-nonneeded`. Requires the Android SDK (build-tools ≥ 34, platform
android-34+), Java 17+. `./build.sh` produces `vetro-probe.apk` signed with
a debug key generated on the fly. Then `adb install vetro-probe.apk` and
`adb shell am start -n com.vetro.probe/.MainActivity`.

## Status

Sources ready; **not yet tested on Android** (needs a snapshot at
`boot_completed`, a ~1 h boot, and the sinkhole's TLS endpoint or the development
CA in the image's trust store — see `docs/progress/M7.md`).
