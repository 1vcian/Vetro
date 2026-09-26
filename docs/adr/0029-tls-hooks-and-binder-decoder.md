# ADR 0029 — Hook TLS (M7) e decoder Binder (M8) dall'esterno

- Stato: accettata (M7–M8, 2026-09-26). Sopra l'ADR 0027 (introspezione
  dall'esterno: syscall e punti d'arresto invisibili) e l'ADR 0016
  (analisi di rete, HAR).
- Numero: al rebase su main rinumerare se 0028/0029 sono già presi
  (l'altro agente può prendere 0028).

## Contesto
M7 chiede le richieste HTTPS in chiaro, legate all'azione, nell'HAR come
quelle in chiaro; M8 le transazioni Binder decodificate (interfaccia e
metodo AIDL, mittente e destinatario) e un ispettore privacy. Entrambi
partono dagli agganci dell'ADR 0027, senza toccare il guest.

## Decisione

### Hook TLS (M7)
- **Testo in chiaro dagli hook, non dai record cifrati.** Punti d'arresto
  invisibili (ADR 0027) su `SSL_write`, `SSL_read`, `SSL_write_ex`,
  `SSL_read_ex` di `libssl.so`, risolti dai simboli ELF di **ogni**
  processo (BoringSSL di sistema e quella dentro Conscrypt,
  `/apex/com.android.conscrypt`). `SSL_write` ha il buffer già
  all'ingresso; `SSL_read` no: il numero di byte è il valore di ritorno,
  quindi il buffer si legge al ritorno, con un punto d'arresto
  sull'indirizzo di ritorno (LR) messo la prima volta che lo si vede.
- **Connessione dalla sequenza di syscall, non dalle strutture di
  BoringSSL.** Le strutture interne di `SSL`/`BIO` cambiano versione; la
  4-tupla si ricava invece dal fd: `connect(fd, ...)` sullo stesso thread
  dà il fd, e `Linux::socket_endpoints` la 4-tupla dalla `struct sock` del
  kernel (offset dal BTF: `SockLayout`). Ripiego: il primo socket IPv4
  connesso fra i fd aperti.
- **Unione con l'analisi di rete.** Ogni connessione TLS diventa una
  `TlsConversation` (verso il server = richiesta, dal server = risposta,
  con i tempi del guest); `NetworkAnalysis::merge_tls` ne ricostruisce le
  richieste HTTP come `HttpExchange { secure: true, attribution }` e le
  ordina con quelle in chiaro. Nell'HAR compaiono con `_secure` e `_vetro`
  (pid, processo, pacchetto, libreria); nell'ispettore con `secure` e
  `attribution`.
- **Server TLS della sinkhole (da completare).** Perché la connessione
  dell'app si concluda serve un endpoint che parli TLS: la sinkhole oggi
  termina TCP. Il testo in chiaro viene comunque dagli hook, quindi la
  validità del certificato conta solo perché l'handshake riesca. Due
  strade, entrambe con una ricompilazione di AOSP o un'app di prova
  (VM spenta, vedi `docs/progress/M7.md`): (a) TLS 1.3 nella sinkhole con
  la CA di sviluppo di Vetro (`tls_ports`, predefinita 443) e la CA
  installata nell'immagine (rebuild); (b) un'app di prova che si fida
  della nostra CA o ignora il certificato. Le primitive (SHA-256, HMAC,
  HKDF, ChaCha20-Poly1305) sono verificate coi vettori RFC ma non ancora
  integrate: si aggiungono col server, provato contro `openssl s_client`
  come oracolo prima di dichiararlo fatto. La strada (a) è preparata
  nell'ADR 0030: CA di sviluppo EC P-256 (`guest/aosp/vendor/vetro/dev-ca/
  vetro-dev-ca.pem`, chiave in `~/.config/vetro/dev-ca/`, fuori dal
  repository) nell'APEX di conscrypt con una patch; in attesa della build.

### Decoder Binder (M8)
- **Mappa AIDL dall'immagine, non a mano.** `tools/aosp/aidl-map.sh`
  estrae da `super.img` la partizione `system` (EROFS) e legge con
  `dexdump` le costanti che AIDL genera negli stub (`TRANSACTION_*` di
  `<interfaccia>$Stub`, `*_TRANSACTION` di `IContentProvider`):
  descrittore + codice → nome del metodo, per l'immagine che gira
  (`aidl_aosp15.tsv`, 1306 interfacce, 11637 metodi; incluse
  IActivityManager, IPackageManager, ILocationManager, ITelephony,
  IContentProvider, IClipboard, ICameraService). I codici riservati di
  `IBinder` (`_PNG`, `_DMP`, ...) sono nel codice.
- **Le due metà della chiamata.** Ogni transazione si vede dal mittente
  (`BC_TRANSACTION`, con un handle) e dal destinatario (`BR_TRANSACTION`,
  con pid/euid del mittente dal kernel); `BinderLog` le accoppia per
  codice e byte del Parcel, così la chiamata ha mittente e destinatario
  (pid, uid, pacchetto dal nome del processo).
- **Ispettore privacy.** Regole su interfaccia+metodo e, per i content
  provider, sulle stringhe del Parcel (autorità, chiavi come `android_id`):
  posizione, contatti, registro chiamate, sms, calendario, appunti,
  identificativi (ANDROID_ID, IMEI/IMSI/ICCID, seriale), fotocamera,
  microfono, account, app installate.

### Esposizione
`vetro boot --kernel-profile=boot.img [--system-map --kernel-btf] --tls
--binder-log=FILE`: `--tls` mette le richieste HTTPS nell'HAR (`--har`) e
nella lista (`--net-requests`); `--binder-log` scrive le chiamate (JSON o
righe) e stampa gli accessi sensibili. API in `vetro_analysis::net`
(`TlsConversation`, `merge_tls`), `vetro_analysis::introspect`
(`BinderCall`, `BinderLog`, `privacy`), `vetro_machine`
(`tls::TlsTracer`/`tls_service`, `analysis::{BinderTracer, Tracers}`).
Snapshot a `boot_completed` con `vetro boot --save-on=TESTO:FILE`
(`--save-delay`, `--exit-after-save`), così le prove partono da `--restore`.

## Alternative scartate
- **Strutture interne di BoringSSL per la 4-tupla**: fragili fra versioni;
  la sequenza di syscall è stabile.
- **Decifrare i record TLS catturati**: servirebbe la chiave di sessione;
  gli hook danno il chiaro direttamente e senza crittografia.
- **Tabella AIDL scritta a mano**: si rompe a ogni immagine; gli stub
  compilati hanno i codici esatti.
- **Server TLS con crittografia non verificata**: contro la regola d'oro;
  si integra solo con un oracolo (`openssl s_client`).

## Verifica
- Unitari (`cargo test -p vetro-analysis -p vetro-machine -p vetro-cli`):
  Parcel (intestazione SYST, String16, troncati), mappa AIDL, `BinderLog`
  (le due metà, ANDROID_ID e appunti come sensibili), privacy, `TlsConversation`
  → HTTP in chiaro con attribuzione, `merge_tls` nell'HAR.
- `vetro boot --tls --binder-log` sul kernel di prova (senza Android): il
  profilo si carica, i tracciatori girano, nessun crash (0 chiamate/0
  conversazioni, non c'è Binder né libssl).
- **Prova lunga su Android (`VETRO_ANDROID=1`, da snapshot): non ancora
  eseguita** (serve un avvio ~1 h e il server TLS o un'app che si fida
  della CA). Piano e stato in `docs/progress/M7.md` e `M8.md`.

## Conseguenze
- L'uscita di M7 (10 app con HTTPS in chiaro) e di M8 (accesso sensibile
  su un'app di prova) restano da chiudere con Android da snapshot.
- Per il server TLS con la CA di sviluppo serve una ricompilazione di AOSP
  (CA nel trust store dell'immagine): preparata, non fatta (VM spenta).
