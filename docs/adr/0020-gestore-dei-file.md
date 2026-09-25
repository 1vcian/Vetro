# ADR 0020 — Gestore dei file: demone di Vetro nel guest su virtio-vsock

- Stato: accettata (M8, base sul guest Linux, 2026-09-26). Usa virtio-vsock
  (M5, `docs/specs/platform.md`), l'unico punto d'ingresso dell'host
  (ADR 0019) e gli snapshot (ADR 0015).

## Contesto
M8 chiede un gestore dei file dell'app in primo piano (`docs/PLAN.md`):
albero di `/data/data/<pacchetto>`, `/data/user_de/0/<pacchetto>`,
`/sdcard/Android/{data,media}/<pacchetto>`, aggiornato dal vivo,
visualizzatori (testo, JSON, XML delle SharedPreferences, SQLite, immagini,
esadecimale) e modifica con salvataggio immediato nel guest; proprietario,
permessi e contesto SELinux conservati; ogni modifica dell'utente è un
ingresso registrato, così il replay resta identico.

Leggere e scrivere l'immagine del disco dall'host (ext4/f2fs nel file
dell'overlay) è escluso: con il guest acceso il kernel ha cache, journal e
metadati in memoria, e una scrittura da fuori corrompe il file system; anche
una lettura da fuori vede uno stato vecchio. I file vanno letti e scritti
**dal kernel del guest**, da un processo con i privilegi necessari (root
nell'immagine userdebug).

Android non c'è ancora; il guest Linux di M3 (BusyBox, initramfs) basta per
fare e provare il meccanismo.

## Decisione

### Un demone di Vetro nel guest su virtio-vsock
- `vetro-files` (`guest/kernel/initramfs/vetro-files.c`): C statico (musl
  oggi, bionic nell'immagine Android), un solo processo con un ciclo
  `poll()`, fino a 8 connessioni, ognuna con il suo inotify e un buffer
  d'uscita non bloccante (un host che non legge non ferma il demone; oltre
  8 MiB in uscita il demone smette di leggere richieste ed eventi di quella
  connessione). Solo POSIX e header UAPI di Linux.
- **Porta vsock fissa 5200**, in ascolto da qualsiasi CID; l'host (CID 2) si
  collega (come adb su vsock). Se nessuno ascolta ancora (demone non
  partito) il client riprova ogni 100 ms di tempo del guest.
- Nel guest di M3 lo avvia `/init` quando c'è un dispositivo virtio-vsock
  (id 19 in `/sys/bus/virtio/devices/*/device`): sotto QEMU (senza vsock)
  non parte e il log di confronto non cambia. Nell'immagine Android sarà un
  servizio di init (`vetro_files`, dominio SELinux proprio o `su` nella
  userdebug); non in questo lavoro.
- Il kernel guest ha ora `CONFIG_INOTIFY_USER` (eventi dal vivo) e
  `CONFIG_TMPFS_XATTR` (xattr su tmpfs: i test provano che gli xattr,
  compreso `security.selinux`, si conservano; senza LSM tmpfs lo tiene come
  un xattr qualsiasi). La cache della CI si rinnova da sola (chiave su
  `guest/kernel/config/**` e `guest/kernel/initramfs/**`).

### Protocollo binario piccolo e versionato
Dettaglio in `docs/specs/files.md`. Frame `u32 lunghezza, u8 tipo, u32 id,
corpo` in little endian; il demone apre ogni connessione con un saluto
(`"VTRF"`, versione 1, flag SELinux, pezzo massimo 1 MiB). Richieste: STAT,
LIST (con i metadati di ogni voce: tipo, modo, uid, gid, dimensione, mtime,
nlink, destinazione dei collegamenti, contesto da `security.selinux`),
READ (a pezzi: offset e lunghezza), scrittura in tre passi
(WOPEN/WDATA/WCOMMIT, WABORT), MKDIR, CREATE, DELETE (anche ricorsivo),
RENAME, WATCH/UNWATCH (inotify); risposte con errno di Linux; eventi di
inotify non richiesti. Versione nuova = numero nuovo nel saluto: il client
rifiuta una versione che non conosce.

### Scrittura atomica che conserva i metadati
- WOPEN crea un file temporaneo `.vetro-tmp.<n>.<nome>` **nella stessa
  cartella** (stesso file system: il rename è atomico) con `O_EXCL`; i
  WDATA ci scrivono; WCOMMIT copia sul temporaneo proprietario e gruppo
  (`fchown`, prima di `fchmod`: chown toglie setuid/setgid), modo e **tutti
  gli xattr** del file che sostituisce (`security.selinux` obbligatorio: se
  non si copia la scrittura fallisce), `fsync`, `rename` sul file vero,
  `fsync` della cartella. Chi legge il file vede il vecchio o il nuovo,
  mai una via di mezzo; un errore in qualsiasi passo toglie il temporaneo e
  il file vero non cambia.
- Un collegamento simbolico resta: si sostituisce il file a cui punta.
- Un file (o una cartella) **nuovo** prende proprietario, gruppo e contesto
  SELinux della cartella che lo contiene, e il modo chiesto dall'host
  (`umask` 0): è ciò che fa Android per i file di un'app, tutti con uid e
  contesto (categorie MLS comprese) della sua cartella dei dati.
- Gli eventi di inotify dei file temporanei non arrivano all'host: la
  scrittura si vede come `IN_MOVED_TO` del file vero.
- Limite noto: il rename rompe i collegamenti fisici (il file vero diventa
  un inode nuovo), come ogni editor che salva in modo atomico.

### Client dell'host in Rust, senza dipendenze
- `vetro_machine::files` (`proto`: codifica e lettura dei frame; client
  `FilesClient`), wasm32 senza dipendenze. Il client tocca la macchina solo
  con `Machine::input(Input::Vsock(..))` (connessione, invio, lettura) e
  `Machine::vsock_view` (stato e byte pronti): legge solo quando ci sono
  byte, così il log di una sessione ferma resta vuoto.
- Operazioni asincrone con un id e una `Completion`: una lettura lunga
  diventa READ da 256 KiB uno dopo l'altro; una scrittura diventa
  WOPEN + WDATA + WCOMMIT mandati insieme (il demone li serve in ordine, un
  WDATA fallito fa fallire il WCOMMIT).
- Le **radici da mostrare** sono un'interfaccia che il chiamante imposta
  (`FilesClient::set_roots`, `window.vetroFiles.setRoots` nella pagina,
  `app_roots(pacchetto)` per le cartelle di un'app): oggi a mano o dall'URL,
  in futuro dal rilevamento dell'app in primo piano nel decoder Binder
  (ActivityTaskManager), che non è parte di questo lavoro.
- Esposto da `vetro-wasm` (ABI 7, `vetro_files_*`, risposte in JSON più i
  byte letti), da `web/node/vetro.mjs` (`GuestFiles`, Promise) e da
  `vetro boot --files-ls/--files-cat/--files-put` per i test.

### Determinismo
Le richieste dell'host entrano nel guest come byte del vsock host→guest, e
passano **tutte** dall'unico punto d'ingresso `Machine::input` (ADR 0019):
connessione, invio e lettura dell'host sono `Input::Vsock(..)` registrati
con il numero d'istruzione. Una sessione del gestore registrata si rigioca
identica senza client (test `files.rs`); lo stesso copione dà le stesse
istruzioni e le stesse risposte. Il client va chiamato fra un quanto e
l'altro (come la console), non con la macchina ferma su un disco (gli
ingressi sarebbero rimandati: il client non fa niente in quel caso) né
dopo lo spegnimento (un ingresso dopo l'ultima istruzione non arriverebbe
mai al guest e il replay lo segnalerebbe come mancato).

### Snapshot
Connessione, crediti e byte in transito sono stato di virtio-vsock (nello
snapshot); il client è collegamento dell'host. Un taglio a metà sessione
con lo stesso client continua identico (test in `snapshot.rs`). Una
sessione nuova (pagina riaperta da uno snapshot in cache) crea un client
nuovo: al primo collegamento chiude con RST le connessioni verso la porta
del demone rimaste nello snapshot, che nessuno leggerebbe più.

## Conseguenze
- Il gestore funziona sul guest Linux di oggi e nell'app web (pannello
  accanto allo schermo); per Android servono il servizio di init e la
  policy SELinux del demone, il rilevamento dell'app in primo piano e le
  radici dei pacchetti.
- Il demone ha i privilegi di root del guest: legge e scrive qualsiasi file
  (è lo scopo); vale solo per le immagini userdebug di Vetro.
- SQLite si legge nella pagina con un lettore del formato scritto da noi
  (`web/app/sqlite.mjs`, solo lettura, senza WAL); la modifica di una riga
  (uscita di M8) resta da fare: scriverla nel formato del file è rischioso
  con l'app che tiene il database aperto, e andrà fatta con `sqlite3` nel
  guest o nel demone, con un ADR.
- Nomi di file non UTF-8 arrivano all'host con i caratteri sostituiti
  (U+FFFD) e non si possono riaprire: limite noto, raro su Android.
