# Gestore dei file: `vetro-files` e il suo client (M8)

Decisioni e motivi nell'ADR 0020. Qui il protocollo, le interfacce e i
test. Codice: `guest/kernel/initramfs/vetro-files.c` (demone nel guest),
`crates/vetro-machine/src/files.rs` e `files/proto.rs` (client),
`crates/vetro-wasm/src/files.rs` (ABI 7), `web/node/vetro.mjs`
(`GuestFiles`), `web/app/files.mjs` e `web/app/sqlite.mjs` (pannello),
`crates/vetro-cli/src/files.rs` (`vetro boot --files-*`).

## Trasporto
virtio-vsock (`docs/specs/platform.md`): il demone ascolta sulla porta
**5200** del guest (`VMADDR_CID_ANY`), l'host si collega da CID 2. Nel
guest di M3 `/init` lo avvia se c'è un dispositivo virtio-vsock. Un solo
flusso per connessione, fino a 8 connessioni insieme.

## Protocollo (versione 1)
Tutto little endian. `str` = `u16` lunghezza + byte (UTF-8 per l'host, byte
del file system per il guest; senza NUL); `bytes` = `u32` lunghezza + byte.

Frame: `u32 lunghezza` (byte che seguono) · `u8 tipo` · `u32 id` · corpo.
Il demone rifiuta (chiude la connessione) una richiesta più corta di 5 o
più lunga di 1 MiB + 16 KiB; il client un frame più lungo di 64 MiB.

### Saluto (demone → host, primo frame, id 0)
| Tipo | Corpo |
|---|---|
| `0x80` HELLO | `u32` magia `"VTRF"` (0x46525456) · `u16` versione (1) · `u16` flag (bit 0: `/sys/fs/selinux` esiste) · `u32` pezzo massimo (1 MiB) |

### Richieste (host → demone)
| Tipo | Corpo | Risposta (se riuscita) |
|---|---|---|
| 1 STAT | `str` percorso | `stat` (con `lstat`) |
| 2 LIST | `str` percorso | `u32` n · n × (`str` nome · `stat`), in ordine di `strcmp`, senza `.` e `..` |
| 3 READ | `str` percorso · `u64` offset · `u32` byte (al più il pezzo massimo) | `u64` dimensione del file · `bytes` letti (meno di quelli chiesti = fine del file) |
| 4 WOPEN | `u32` handle (scelto dal client) · `str` percorso · `u32` modo di un file nuovo · `u8` flag (bit 0: fallisce se il file c'è) | vuota |
| 5 WDATA | `u32` handle · `u64` offset · `bytes` (al più il pezzo massimo) | vuota |
| 6 WCOMMIT | `u32` handle | `stat` del file dopo il rename |
| 7 WABORT | `u32` handle | vuota |
| 8 MKDIR | `str` percorso · `u32` modo | vuota |
| 9 CREATE | `str` percorso · `u32` modo (file vuoto, `O_EXCL`) | vuota |
| 10 DELETE | `str` percorso · `u8` flag (bit 0: ricorsivo) | vuota |
| 11 RENAME | `str` da · `str` a | vuota |
| 12 WATCH | `str` percorso | `u32` wd |
| 13 UNWATCH | `u32` wd | vuota |

`stat` = `u8` tipo (0 altro, 1 file, 2 cartella, 3 collegamento, 4 char, 5
block, 6 fifo, 7 socket) · `u32` `st_mode` · `u32` uid · `u32` gid · `u64`
dimensione · `i64` mtime (s) · `u32` mtime (ns) · `u32` nlink · `str`
destinazione del collegamento (vuota se non lo è) · `str` contesto SELinux
(xattr `security.selinux` senza NUL finale, vuoto se non c'è).

### Risposte ed eventi (demone → host)
| Tipo | Corpo |
|---|---|
| `0x81` REPLY (id della richiesta) | `u32` stato (0 o errno di Linux) · corpo della tabella sopra se lo stato è 0, niente altrimenti |
| `0x82` EVENT (id 0) | `u32` wd (`0xffffffff` per `IN_Q_OVERFLOW`) · `u32` maschera di inotify · `u32` cookie · `str` nome nella cartella osservata |

Le risposte arrivano nell'ordine delle richieste di ogni connessione. La
maschera osservata è `IN_CREATE | IN_DELETE | IN_MODIFY | IN_CLOSE_WRITE |
IN_MOVED_FROM | IN_MOVED_TO | IN_ATTRIB | IN_DELETE_SELF | IN_MOVE_SELF`
(più `IN_IGNORED`, `IN_ISDIR` dal kernel). Senza inotify nel kernel WATCH
risponde `ENOSYS`.

### Semantica
- **Scrittura atomica.** WOPEN risolve un collegamento simbolico nel file a
  cui punta, rifiuta cartelle (`EISDIR`) e file speciali (`EINVAL`), crea
  `.vetro-tmp.<n>.<nome>` nella stessa cartella (`O_EXCL`, 0600). WDATA
  scrive con `pwrite`; il primo errore resta sull'handle. WCOMMIT: se il
  file c'era, `fchown` a uid e gid, `fchmod` al modo e copia di tutti gli
  xattr del file vecchio (fallisce se `security.selinux` non si copia); se
  è nuovo, uid, gid e `security.selinux` della cartella e il modo di
  WOPEN; poi `fsync`, `rename` sul file vero, `fsync` della cartella. Ogni
  errore toglie il temporaneo; l'handle si libera sempre. Alla chiusura
  della connessione gli handle aperti si annullano.
- **File nuovi** (WCOMMIT di un file che non c'era, MKDIR, CREATE):
  proprietario, gruppo e contesto SELinux della cartella; modo esatto
  (`umask` 0).
- **Eventi** dei nomi che cominciano con `.vetro-tmp.` non partono.
- **DELETE** su una cartella: `rmdir` (vuota) o, col bit ricorsivo,
  `nftw` in profondità senza seguire i collegamenti.

## Client (`vetro_machine::files`)
- `proto`: `PORT`, `MAGIC`, `VERSION`, `CHUNK` (256 KiB), `MAX_FRAME`;
  `Request::{encode, decode}`, `Decoder` (frame a pezzi qualsiasi),
  `Frame::{Hello, Reply, Event}`, `parse_stat/list/read/watch`, `Stat`,
  `Entry`, `Event`, `Kind`, `mask::*`, `errno_name`; gli `encode_*` dei
  corpi servono ai demoni finti dei test.
- `FilesClient::new(porta)` / `default()` (5200). Operazioni (restituiscono
  l'id): `stat`, `list`, `read(percorso, offset, byte | u64::MAX)`,
  `read_file`, `write_file(percorso, dati, modo)`, `create`, `mkdir`,
  `delete(percorso, ricorsivo)`, `rename`, `watch`, `unwatch`. Uscite:
  `take_completion() -> Option<Completion { op, result }>` con
  `Outcome::{Stat, List, Data { size, data }, Written(Stat), Watch(wd),
  Done}` o `FilesError::{Errno, Protocol, Disconnected}`;
  `take_event() -> Option<Event>`. Stato: `state() -> LinkState::{Idle,
  Connecting, Ready(Hello), Waiting { until_ns }}`, `generation()` (saluti
  ricevuti), `pending()`. Radici: `roots`, `set_roots`, `app_roots(pacchetto)`.
- `pump(&mut Machine)` fra un quanto e l'altro: collegamento (o nuovo
  tentativo dopo `RETRY_NS` = 100 ms di tempo del guest), `Input::Vsock
  (Recv)` solo se `vsock_view` dice che ci sono byte, `Input::Vsock(Send)`
  delle richieste in coda; non fa niente senza vsock, durante un replay e
  con la macchina ferma su un disco. Una connessione caduta fa fallire le
  operazioni partite (`Disconnected`); quelle non partite aspettano il
  prossimo saluto. `close(&mut Machine)`.

## vetro-wasm e JS
ABI 7 (`docs/specs/wasm.md`): bit `VSOCK` (32) di `vetro_machine_new_with`,
`vetro_files_open/close/status/request/pump/take/ptr`. In JS
`Machine.files(porta)` → `GuestFiles` (`stat`, `list`, `read`, `writeFile`,
`mkdir`, `create`, `delete`, `rename`, `watch`, `unwatch` come Promise;
`onEvent`; `status()`; `pump()`).

## App web (`web/app`)
Opzione "gestore dei file" (default attiva: la macchina ha virtio-vsock;
`?nofiles=1` la toglie). Il Worker tiene il `GuestFiles`, lo fa avanzare fra
una fetta e l'altra, registra le richieste della pagina in `inputLog` e
manda alla pagina `files-reply`, `files-event`, `files-status`. Il pannello
(`files.mjs`), accanto allo schermo:
- radici da `?files=/a,/b`, dal campo "Radici" o da
  `window.vetroFiles.setRoots([...])` (default `/tmp`, `/root`, `/etc`);
  `window.vetroFiles.state()` per i test;
- albero: clic su una cartella = LIST + WATCH (chiusa: UNWATCH); ogni evento
  rilegge la cartella (dopo 100 ms senza altri eventi); a ogni nuovo saluto
  del demone le cartelle aperte si rileggono e si riosservano; modo,
  proprietario, dimensione, destinazione e contesto SELinux nel titolo;
- visualizzatori scelti dal contenuto (e dall'estensione): SQLite (magia),
  immagini (PNG, JPEG, GIF, WebP, BMP), JSON, XML (con la tabella delle
  SharedPreferences se la radice è `<map>`), testo UTF-8, esadecimale (i
  primi 256 KiB); si può cambiare a mano;
- modifica e "Salva" per testo, JSON, XML (validati prima di scrivere) ed
  esadecimale (si possono aggiungere e togliere byte): scrittura atomica
  con il modo del file; se il file cambia nel guest mentre è aperto si
  ricarica da solo, o avvisa se ci sono modifiche non salvate;
- SQLite (`sqlite.mjs`, lettore del formato scritto da noi, solo lettura):
  tabelle e righe (le prime 500), b-tree a più livelli, overflow, alias del
  rowid, WITHOUT ROWID; non legge il WAL.

## CLI
`vetro boot ... [--vsock] [--files-ls=P]... [--files-cat=P]...
[--files-put=P:FILE]...`: operazioni nell'ordine, risultati su stdout,
uscita 0/1 quando sono finite (`crates/vetro-cli/src/files.rs`).

## Test
- `cargo test -p vetro-machine files`: protocollo (byte esatti, frame a
  pezzi, frame rovinati) e client contro un demone finto (letture e
  scritture a pezzi, errori in mezzo a una scrittura, eventi, connessione
  caduta, radici); `cargo test -p vetro-wasm files` (JSON, API senza
  kernel); `cargo test -p vetro-cli files` (righe di `ls`).
- `tests/boot/tests/files.rs` (kernel guest, release): list, letture
  (anche 1,2 MB a pezzi), errori, scritture che conservano modo,
  proprietario e xattr (`security.selinux`, `user.*`) lette dal guest con
  `stat` e `vetro-dev xattr-get`, attraverso un collegamento, file nuovi con
  proprietario e contesto della cartella, copia grande confrontata con
  `cmp`, create/mkdir/rename/delete, nessun temporaneo rimasto, evento di
  un processo del guest entro 1 s di tempo del guest; due esecuzioni
  uguali; sessione registrata e rigiocata identica (ADR 0019).
- `tests/boot/tests/snapshot.rs`: tagli a metà sessione (lettura in corso,
  evento in arrivo col JIT, scrittura in volo con ritorno indietro).
- `crates/vetro-cli/tests/boot_files.rs`: `--files-put/ls/cat`, errori.
- `tests/web/files.mjs` (Node, in `tools/web-test.sh`), `tests/web/unit.mjs`
  (SQLite su `tests/web/testdata/prova.sqlite`, rigenerabile con
  `make-sqlite.py`; visualizzatori), `tests/web/browser.mjs` (pannello in
  Chrome: albero dal vivo, modifica e salvataggio letti dal guest con
  `cat`, ricollegamento dopo il ripristino dello snapshot).
