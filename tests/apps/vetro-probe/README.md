# vetro-probe — app di prova per M7 (TLS) e M8 (Binder/privacy)

App Android minima per le prove dall'esterno di Vetro. All'avvio
dell'attività (o via `am start`/`am broadcast`) fa, in un thread:

1. **Accessi sensibili (M8):** legge gli appunti
   (`ClipboardManager.getPrimaryClip` → Binder `IClipboard.getPrimaryClip`)
   e `Settings.Secure.ANDROID_ID`
   (`IContentProvider.call` verso il provider delle impostazioni). Il
   decoder Binder li deve segnalare come sensibili (appunti, ANDROID_ID).
2. **Richiesta HTTPS (M7):** una `POST` a `https://<host>/v1/eventi` con
   corpo JSON `{"android_id":"...","ts":...}`, usando `HttpsURLConnection`
   (Conscrypt: `libssl` interno). Con un `TrustManager` che accetta la CA
   di sviluppo di Vetro (o, per la sola prova, qualsiasi certificato), così
   la connessione si completa contro l'endpoint TLS della sinkhole. Gli
   hook TLS catturano URL, intestazioni e corpo in chiaro, legati a
   `com.vetro.probe` e alla libreria.

L'host della richiesta si passa come extra `--es host api.esempio.test`
(default `api.esempio.test`, che la sinkhole risolve a un indirizzo finto).

## Costruzione

`tools/aidl-nonneeded`. Serve l'SDK Android (build-tools ≥ 34, platform
android-34+), Java 17+. `./build.sh` produce `vetro-probe.apk` firmato con
una chiave di debug generata al volo. Poi `adb install vetro-probe.apk` e
`adb shell am start -n com.vetro.probe/.MainActivity`.

## Stato

Sorgenti pronti; **non ancora provata su Android** (serve uno snapshot a
`boot_completed`, un avvio ~1 h, e l'endpoint TLS della sinkhole o la CA di
sviluppo nel trust store dell'immagine — vedi `docs/progress/M7.md`).
