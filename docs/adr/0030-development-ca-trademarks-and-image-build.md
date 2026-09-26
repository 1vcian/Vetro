# ADR 0030 — CA di sviluppo nel trust store, marchi e build più leggera dell'immagine AOSP

- Stato: accettata (M5/M7, 2026-09-26), preparata a VM spenta: nessuna
  build l'ha ancora provata (vedi "Verifica"). Sopra l'ADR 0022 (immagine
  AOSP) e l'ADR 0029 (hook TLS: server TLS della sinkhole). Dettagli:
  `docs/specs/guest-image.md`.
- Numero: al rebase su main rinumerare se 0030 è già preso (0028 è
  dell'agente del browser, branch `web/android-nel-browser`).

## Contesto
Tre cose aspettavano la prossima ricompilazione di AOSP, e la VM di build si
accende una volta sola per tutte:
1. per l'endpoint TLS della sinkhole (ADR 0029, M7/M8) l'app nel guest deve
   concludere l'handshake: il certificato che la sinkhole presenta deve
   risalire a una CA di cui il sistema si fida;
2. la UI di AOSP mostra marchi non nostri: la barra di ricerca "Google" di
   Launcher3, il robot come icona del programma di installazione e delle app
   senza icona, `ro.product.system.*` = Android/mainline/generic; CLAUDE.md:
   il prodotto non si presenta come "Android";
3. `m droid` compila strumenti host che a Vetro non servono (diario di M5).

## Decisione

### CA di sviluppo di Vetro
- **Una CA nostra, EC P-256, ECDSA-SHA256**, 10 anni, `CA:TRUE, pathlen:0`
  (firma solo certificati finali), `keyCertSign, cRLSign`; soggetto
  `O=Vetro, OU=Vetro development builds, CN=Vetro Development CA (not for
  production)`. P-256 perché BoringSSL e Conscrypt la accettano in ogni
  versione e la sinkhole dovrà firmare al volo un certificato per ogni SNI.
- **Nel repository solo il certificato** (`guest/aosp/vendor/vetro/dev-ca/
  vetro-dev-ca.pem`, `79e94fc0.0`); **la chiave privata fuori**, in
  `~/.config/vetro/dev-ca/vetro-dev-ca.key` (0600, con una copia del
  certificato). `tools/aosp/dev-ca.sh new` la genera (rifiuta di
  sovrascriverla senza `--force`); se si perde: `new --force`, nuove patch,
  nuova build. `tools/aosp/dev-ca.sh` (check, lanciato anche da `sync.sh`)
  verifica che patch e certificato coincidano e, se la chiave c'è, che
  corrisponda.
- **Dove la legge AOSP 15.** `TrustedCertificateStore` (Conscrypt) e
  `SystemCertificateSource` (framework) usano
  `/apex/com.android.conscrypt/cacerts` quando l'SDK è ≥ 34 e la cartella non
  è vuota; `/system/etc/security/cacerts` solo con la proprietà Java
  `system.certs.enabled=true`. Il contenuto dell'APEX viene da
  `external/conscrypt/apex/ca-certificates/files/*` (`prebuilt_etc_cacerts`
  `cacerts_apex`); nella nostra build l'APEX si costruisce dal sorgente
  (`com.android.conscrypt.capex` firmato con la chiave di prova, verificato
  sull'immagine `…-9d91633`).
- **Metodo: una patch per progetto** che aggiunge `79e94fc0.0` nel formato
  di AOSP (PEM, testo, impronta), applicata da `tools/aosp/remote/prepare.sh`
  come quella di `frameworks/base`: `guest/aosp/patches/external/conscrypt/`
  (il trust store vero) e `guest/aosp/patches/system/ca-certificates/`
  (`/system/etc/security/cacerts`, tenuto uguale per il ripiego). Le patch
  finiscono anche in `sources/aosp/vetro-patches/` pubblicati con
  l'immagine. `prepare.sh` toglie dai due trust store i file non tracciati
  che nessuna patch attuale crea: dopo un cambio di CA la vecchia non resta.
  Si ricostruiscono solo l'APEX di conscrypt (il jar e l'immagine di boot di
  ART non cambiano) e system.
- **Il rischio, scritto.** Chi ha la chiave può farsi credere qualunque sito
  da un'immagine di sviluppo di Vetro. Per questo la chiave non sta in git; ma
  una sinkhole che firma al volo nel browser deve averla: **un'immagine con
  questa CA e un vetro-wasm pubblico che contiene la chiave vanno insieme solo
  finché il traffico del guest non esce su Internet** (sinkhole). Se un giorno
  Vetro inoltra il traffico verso la rete vera, la CA nel trust store va
  tolta dall'immagine pubblica o sostituita da una per installazione.
  La CI non ha la chiave: i test della sinkhole TLS devono generarsi una CA
  effimera; la CA dell'immagine serve solo alle prove con Android.

### Marchi
Solo overlay e configurazione, nessuna modifica ai sorgenti di AOSP:
- **Launcher3 senza "Google"**: la barra di ricerca della prima pagina è il
  widget di QuickSearchBox (è lui l'attività di ricerca globale, e il campo
  usa `hint_google`, con attività e stringhe "Google Search"). Non si
  installa: `overrides: ["QuickSearchBox"]` su `VetroFrameworkOverlay`
  (`module-overrides` di `main.mk` lo toglie dai `PRODUCT_PACKAGES`
  ereditati da `handheld_product.mk`). Senza provider Launcher3 mostra la sua
  barra neutra `qsb_default_view` ("Search"). `QSB_ON_FIRST_SCREEN` è una
  costante di `BuildConfig`: toglierla vorrebbe dire patchare Launcher3.
- **Niente robot**: overlay statici (RRO in `/product/overlay`, senza
  certificato di piattaforma: le risorse non dichiarate `overlayable` si
  sovrascrivono da overlay preinstallati) — `VetroFrameworkOverlay`
  sostituisce `mipmap/sym_def_app_icon` (icona delle app senza icona) con
  un vettore `anydpi-v26`, che vince sulle PNG per densità;
  `VetroPackageInstallerOverlay` il primo piano `app_icon_foreground`
  dell'icona adattiva del programma di installazione.
- **Sfondo nostro**: `ro.config.wallpaper=/product/media/wallpaper/vetro.png`
  (`WallpaperManager.openDefaultWallpaper` lo legge prima della risorsa del
  framework); PNG 1920×1920 generato da `tools/aosp/wallpaper.py`
  (deterministico, 94 KiB, gradiente e lastre, niente testo). I colori di
  sistema (Monet) seguono lo sfondo.
- **`ro.product.system.*` = Vetro** (`PRODUCT_SYSTEM_*` in `vetro_arm64.mk`):
  `generic_system.mk` li mette Android/mainline/generic per il GSI; le
  impronte erano già tutte `Vetro/vetro_arm64/…`.
- Restano come AOSP i testi senza marchio ("Phone is starting…", "Search"),
  il campo "Android version" delle Impostazioni (è la versione del sistema,
  non il nome del prodotto) e l'etichetta "Android System" del framework
  (tradotta in ~80 lingue: un overlay solo inglese la cambierebbe a metà; da
  fare con tutte le traduzioni se serve).

### Build più leggera, stesse immagini
- **Niente host cross**: Cuttlefish mette `HOST_CROSS_OS := linux_musl`
  (arm64), e con `TARGET_BOARD_PLATFORM := vsoc_arm64` il suo
  `cvd_host_package` entra in `droidcore` (`build/cvd-host-package.go`): ogni
  `m droid` costruisce il pacchetto host di Cuttlefish anche per host arm64
  musl. `BoardConfig.mk` lo azzera (`HOST_CROSS_OS/ARCH/2ND_ARCH :=`): per
  Make è lo stato di `envsetup.mk` con `BUILD_HOST_static` (niente combo
  cross, `ifdef HOST_CROSS_OS` falso), per Soong `CrossHost ""` (nessun
  target cross). Scartato riportarlo a windows (il default di AOSP): vuole i
  prebuilt mingw nel checkout, che non sappiamo se la VM ha. Cambia solo
  `out/host`. In questa build incrementale il guadagno è solo l'analisi
  (gli strumenti musl sono già in `out/`); conta alla prossima build da zero.
  `remote/build.sh` scrive nel log durata e peso di `out/host/*`.
- **ccache facoltativo** (`VETRO_AOSP_CCACHE=1`, spento di default): la
  sandbox nsjail monta tutto in sola lettura tranne il tree e `out/`, quindi
  `CCACHE_DIR=$TREE/out/.ccache`; il wrapper passa da `CC_WRAPPER` (Soong) e
  `USE_CCACHE`/`CCACHE_EXEC` (Make). Spento perché cambia la riga di comando
  di ogni compilazione C/C++: acceso ora ricompilerebbe tutto il nativo una
  volta. Conviene prima di un cambio di tag AOSP o dopo un `m clean`.
- Scartati: togliere i `PRODUCT_HOST_PACKAGES` di Cuttlefish (non si
  filtrano dall'alto con `inherit-product`, e sono pochi) e cambiare
  `TARGET_BOARD_PLATFORM` (lo usano gli HAL e la sepolicy di Cuttlefish).

### Profilo RAM per il browser
Non in questa build: l'ADR 0028 non indica proprietà da cambiare (2 GiB
bastano: sotto mezzo GiB di memoria anonima a fine avvio) e prevede un
bootconfig per il browser solo se lmkd si rivelasse troppo aggressivo.

## Alternative scartate
- **CA solo in `/system/etc/security/cacerts` con
  `dalvik.vm.extra-opts=-Dsystem.certs.enabled=true`**: sposta l'intero trust
  store su un percorso che in AOSP 15 esiste per i test, e cambia le opzioni
  di ogni processo di zygote.
- **Montare sopra `/apex/com.android.conscrypt/cacerts` all'avvio** (come
  fanno i proxy di analisi sui telefoni): dipende dagli spazi dei nomi di
  mount di zygote e di ogni app; fragile.
- **CA nel trust store utente**: le app con targetSdk ≥ 24 non se ne fidano.
- **`override_apex` di conscrypt**: nome di pacchetto diverso, con effetti sul
  bootclasspath; troppo per aggiungere un file.
- **Overlay che ridisegnano QuickSearchBox**: è marchiato "Google" ovunque
  (attività, icona, stringhe, sfondo del campo); toglierlo è più pulito.
- **Chiave privata committata** (come le test-keys di AOSP): comoda per la
  CI, ma renderebbe pubblica per sempre una CA che le immagini di Vetro
  considerano di sistema.

## Verifica
Sul Mac (senza VM): `tools/aosp/dev-ca.sh` (certificato, patch e chiave
coerenti); le patch applicate e riapplicate al contrario con `git apply`
come fa `prepare.sh`; `79e94fc0.0` assente dai 145 certificati di AOSP 15;
`bpfmt -d` sugli `Android.bp`; i due overlay compilati e collegati con
`aapt2` dell'SDK (risorse `mipmap/sym_def_app_icon` anydpi-v26 e
`drawable/app_icon_foreground`); `BoardConfig.mk` e i `.mk` del prodotto
valutati con GNU make e stub (host cross vuoto dopo l'include di Cuttlefish,
pacchetti, file copiati, proprietà, `PRODUCT_SYSTEM_*`); `shellcheck`
sugli script; `init.vetro.rc` e sepolicy invariati.
Sulla VM (`remote/pack.sh`, si ferma se uno manca): `79e94fc0.0` nel
`apex_build_info.pb` della capex di conscrypt e in
`/system/etc/security/cacerts`, overlay e sfondo installati, niente
QuickSearchBox, `ro.config.wallpaper` e `ro.product.system.brand=Vetro`.
Sul guest (QEMU, poi Vetro): `ls /apex/com.android.conscrypt/cacerts/
79e94fc0.0`, `cmd overlay list` con i due overlay attivi, screencap della
home senza barra "Google" e con lo sfondo. Finché questi non passano, la
decisione è preparata e non provata.

## Conseguenze
- La sinkhole TLS (ADR 0029) può firmare con la chiave di
  `~/.config/vetro/dev-ca/` certificati che l'immagine accetta; l'app di
  prova che ignora il certificato non serve più per l'handshake.
- Ogni nuova CA = `dev-ca.sh new --force` + nuova build e nuova versione su R2.
- Se la build rifiuta `overrides` su un RRO verso un'app (la documentazione
  di Soong dice "solo altri overlay", Make non lo controlla) o l'host cross
  vuoto, le due righe si tolgono senza toccare il resto: sono indipendenti.
