# ADR 0021 — Modifica dei database SQLite e delle SharedPreferences dal gestore dei file

- Stato: accettata (M8, sul guest Linux, 2026-09-26). Estende l'ADR 0020
  (demone `vetro-files` su virtio-vsock); protocollo in `docs/specs/files.md`
  (versione 2).

## Contesto
L'uscita di M8 chiede che una modifica fatta dal pannello (un valore nelle
SharedPreferences e **una riga di un database SQLite**) sia letta dall'app
dopo il riavvio dell'attività. Con l'ADR 0020 il pannello legge i database
con un lettore del formato scritto da noi (`web/app/sqlite.mjs`), solo
lettura e senza il WAL, e le SharedPreferences si modificano solo come
testo XML.

Un database di un'app è quasi sempre **aperto** mentre lo si guarda: Android
apre i database in WAL (`journal_mode=WAL`, predefinito da Android 9), con i
file `-wal` e `-shm` accanto e i lock POSIX sui byte del file e della
memoria condivisa. Riscrivere le pagine del file dall'host (o con una
scrittura atomica del demone, che sostituisce l'inode) ignorerebbe lock,
WAL e cache delle pagine dell'app: l'app continuerebbe a leggere il WAL e
la sua memoria, o corromperebbe il database alla prossima scrittura.

## Decisione

### SQL nel guest con il motore SQLite vero
- Nuova richiesta **SQL** (tipo 14) del demone: percorso del database, SQL
  (una o più istruzioni), parametri legati (`NULL`, intero a 64 bit, reale,
  testo, BLOB, per posizione `?N`), numero di righe cambiate atteso e flag
  (sola lettura). La risposta porta il codice di SQLite e il suo messaggio,
  righe cambiate, ultimo rowid inserito, colonne e righe dell'ultima
  istruzione che ne restituisce (al più 10000 righe e 16 MiB).
- Il demone esegue il SQL con **SQLite linkato staticamente** (sorgente
  *amalgamation* ufficiale 3.53.4, sha256 fissato in
  `tools/guest-kernel/build.sh`, dominio pubblico), quindi con lock POSIX,
  journal di rollback o WAL, `-shm` e checkpoint esattamente come l'app:
  una scrittura dell'host è una transazione come quelle dell'app.
- L'esecuzione avviene in un **processo figlio con uid e gid del
  proprietario del file** del database (`setgroups(0)`, `setresgid`,
  `setresuid`): i file `-wal`, `-shm` e `-journal` che SQLite crea hanno il
  proprietario dell'app (un `-shm` di root renderebbe il database
  illeggibile all'app). Dopo l'esecuzione il demone porta su quei file
  anche il contesto SELinux del database se è diverso (su Android il
  processo del demone non ha le categorie MLS dell'app). Il figlio usa
  `busy_timeout` di 2 s: se l'app tiene la scrittura più a lungo la
  richiesta fallisce con `SQLITE_BUSY` e niente cambia.
- Tutte le istruzioni di una richiesta stanno in **una transazione**
  (`BEGIN IMMEDIATE` … `COMMIT`); con un numero di righe cambiate atteso
  (il pannello chiede sempre 1) una differenza fa `ROLLBACK` ed errore: un
  `UPDATE … WHERE rowid = ?` su una riga che l'app ha tolto nel frattempo
  non tocca niente. Le righe contate sono quelle cambiate direttamente
  dalle istruzioni (non dai trigger).
- Il demone aspetta il figlio (ciclo `poll()` fermo per la durata della
  transazione, al più il `busy_timeout` più il lavoro): accettabile per un
  gestore interattivo, le altre connessioni ripartono subito dopo.
- Il database deve esistere (niente database nuovi creati per errore): un
  percorso che non c'è risponde `ENOENT`, una cartella `EISDIR`, un file
  speciale `EINVAL`.
- Nel guest di prova `sqlite3` è un collegamento a `vetro-files`
  (programma multi-chiamata come BusyBox: con `argv[0]` `sqlite3` parte la
  shell ufficiale `shell.c` dello stesso sorgente), così una sola copia del
  motore sta nell'initramfs. Nell'immagine Android (userdebug) il demone si
  compila con bionic dallo stesso sorgente, senza la shell: `sqlite3` c'è
  già nelle build userdebug.

Scartate: scrivere le pagine dall'host o con la scrittura atomica (vedi
sopra); invocare la shell `sqlite3` del guest con il SQL come testo
(quoting dei valori fragile, niente BLOB, uscita da interpretare, e sul
guest Linux la shell andrebbe aggiunta comunque); un motore SQLite in
WebAssembly nella pagina (lavorerebbe su una copia del file, stessi
problemi della scrittura dall'host).

### Lettura: il visualizzatore legge anche il WAL
La lettura resta passiva (nessun lock, nessun processo nel guest): il
pannello legge il file del database e il suo `-wal` e il lettore
(`sqlite.mjs`) ricostruisce l'ultima istantanea confermata come fa SQLite
al recupero: intestazione del WAL, *salt* e checksum cumulativi dei frame,
per ogni pagina l'ultimo frame valido fino all'ultimo frame di commit, che
dà anche il numero di pagine del database. Il `-shm` non serve. Le due
letture non sono atomiche: se l'app fa un checkpoint in mezzo, l'evento di
inotify del file (o del `-wal`) fa rileggere il database. Scartato il
checkpoint prima di leggere: scriverebbe nel database dell'app a ogni
apertura.

### SharedPreferences: tabella modificabile, XML di Android
Il pannello legge l'XML (`<map>` con `string`, `int`, `long`, `float`,
`boolean`, `set` di `string`, `null`) con un lettore XML piccolo scritto da
noi (senza DOM, provabile in Node), mostra una tabella con tipo, nome e
valore modificabili, righe da aggiungere e togliere, e valida i valori
come li rilegge Android (`Integer.parseInt`, `Long.parseLong`,
`Float.parseFloat`, `true`/`false`). Il salvataggio riscrive il file nel
formato di `XmlUtils.writeMapXml` con `FastXmlSerializer` (intestazione
`<?xml version='1.0' encoding='utf-8' standalone='yes' ?>`, rientro di 4
spazi, `<int name="n" value="1" />`, gli stessi caratteri protetti) con la
scrittura atomica già esistente del demone (proprietario, modo e contesto
SELinux conservati). Un file scritto da Android e riletto senza modifiche
dà gli stessi byte. L'app rilegge il file quando ricarica le preferenze
(riavvio del processo o dell'attività, come chiede l'uscita di M8): una
scrittura mentre l'app ha le preferenze in memoria viene sovrascritta alla
sua prossima `apply()`, limite noto di ogni modifica esterna.

### Nomi di file non UTF-8
Nel protocollo i nomi sono già byte. Il client Rust tiene percorsi, nomi e
destinazioni dei collegamenti come byte (`Vec<u8>`) e non più come
`String` con i caratteri sostituiti. Verso JavaScript si usa la
rappresentazione *surrogateescape* (PEP 383): un byte che non fa parte di
UTF-8 valido diventa il surrogato solitario `U+DC80 + (byte - 0x80)`, in
JSON `\udcXX`; il JS rimanda i percorsi a vetro-wasm come byte con la
codifica inversa (`web/node/vetro.mjs`), così un nome qualsiasi fa andata e
ritorno. Il pannello mostra quei byte come `\xNN`. Un nome valido in UTF-8
non contiene mai surrogati (UTF-8 non li codifica), quindi la
rappresentazione non è ambigua.

## Conseguenze
- Versione 2 del protocollo (richiesta SQL); il client accetta demoni di
  versione 1 e 2 (con la 1 la richiesta SQL risponde `ENOSYS`).
- ABI di vetro-wasm 9: operazione SQL, percorsi come byte (non più
  rifiutati se non UTF-8), nomi in surrogateescape.
- L'initramfs cresce di circa 1 MiB (SQLite nel demone); il demone ha
  bisogno di `fork` e dei privilegi per cambiare utente (root nel guest).
- Su Android servono ancora il servizio di init del demone e la sua policy
  SELinux (ADR 0020); il contesto del processo figlio resta quello del
  demone, e i file che crea prendono il contesto del database.
