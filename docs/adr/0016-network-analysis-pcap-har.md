# ADR 0016 — Analisi di rete: punto di cattura, pcapng, HTTP e HAR

- Stato: accettata (M7, prima parte, 2026-09-25).

## Contesto
M7 chiede un ispettore di rete con esportazioni HAR e pcap, decodifica dei
corpi (JSON, protobuf, form) e, per l'uscita, HTTPS in chiaro legato
all'azione dell'utente. La parte HTTPS richiede gli hook su BoringSSL e
Conscrypt dentro Android, che non gira ancora; tutto il resto si può fare
oggi sul traffico del kernel guest di M3 (BusyBox), che passa dallo stack
di `vetro-net` (ADR 0007) attraverso virtio-net.

Vincoli: determinismo (M10 rigioca le sessioni: tempi e ordine dei dati
devono dipendere solo dall'esecuzione), nessuna dipendenza esterna,
compilazione per wasm32 (l'ispettore finirà nel browser), e nessuna
modifica alla logica di `vetro-net`, che ha un altro proprietario.

## Decisione

### Punto di cattura al confine di virtio-net
- `NetLink` (il backend di virtio-net in `vetro-machine`) copia ogni frame
  Ethernet nei due versi, se la cattura è accesa: `FromGuest` in `send`
  (prima di `Stack::receive`), `ToGuest` in `recv` (dopo
  `Stack::pop_frame`). L'istante è `NetLink::now`, il tempo virtuale che la
  macchina fissa prima di servire virtio-net (CNTPCT in microsecondi): lo
  stesso dello stack, quindi deterministico.
- API: `Machine::net_tap(on) -> bool` e `Machine::net_tap_take() ->
  Vec<TappedFrame>`. Accedono al backend senza segnare i dispositivi da
  servire (a differenza di `Machine::net`): accendere, spegnere e svuotare
  la cattura non cambia l'esecuzione. La cattura non entra negli snapshot
  (è osservazione, non stato del guest).
- Il punto è lato macchina e non dentro lo stack: vede esattamente ciò che
  vede il guest (compresi DHCP e ARP), non richiede di toccare
  `vetro-net`, e funzionerà uguale con il relay (M7) o altri upstream.

### `vetro-analysis::net`: tutto dai frame
L'analisi lavora solo sui frame catturati (`Frame { at_us, dir, data }`),
non sul registro degli eventi né sui byte del sinkhole: così è la stessa
per qualsiasi fonte (cattura dal vivo, file pcapng riletto, in futuro il
browser) e non dipende dai tipi di `vetro-net`.
- **pcapng** (non pcap classico): un'interfaccia `LINKTYPE_ETHERNET` con
  `if_tsresol` = 6 e il verso in `epb_flags` (`outbound` i frame del
  guest). Tempo = epoca Unix + tempo del guest (epoca 0 di default: il
  file dice "1970-01-01 00:00:01.5" per 1,5 s dopo l'accensione). Un
  lettore nostro rilegge i file (test di andata e ritorno).
- **Flussi TCP** ricostruiti per numero di sequenza (svolto attorno alla
  posizione corrente: attraversa 2^32), ritrasmissioni e sovrapposizioni
  scartate, fuori ordine in attesa (fino a 16 MiB per verso), buchi mai
  colmati contati; ogni pezzo porta l'istante in cui è diventato
  leggibile, da cui i tempi delle richieste. Cliente = chi manda il SYN;
  un SYN su una quadrupla chiusa apre un flusso nuovo. **UDP** per
  quadrupla.
- **HTTP/1.1** (RFC 9112): pipelining, `chunked` con estensioni e
  trailer, `Content-Length`, fino alla chiusura, niente corpo per
  HEAD/1xx/204/304, risposte 1xx intermedie; `Content-Encoding` gzip e
  deflate con un DEFLATE nostro (alla maniera di `puff.c`, CRC-32 e
  Adler-32 verificati, uscita limitata a 256 MiB). Brotli e zstd non sono
  decodificati: il corpo resta com'è e la voce lo annota.
- **DNS** dai flussi verso la porta 53: domande, risposte (A, AAAA, nomi
  compressi), abbinate per id; il nome risolto dà l'host delle
  connessioni e la fase `dns` della prima richiesta verso quell'indirizzo.
- **Corpi**: JSON (parser nostro che conserva ordine e numeri come
  scritti), form urlencoded, multipart (con decodifica ricorsiva delle
  parti), protobuf senza schema sul filo come `protoc --decode_raw` (un
  campo a lunghezza delimitata è un messaggio se lo è e non è testo
  stampabile, poi stringa UTF-8, poi byte), gRPC non compresso. Scelta dal
  `Content-Type`, altrimenti dal contenuto.
- **Ispettore**: `NetworkAnalysis::from_frames` → flussi, scambi DNS,
  richieste HTTP con corpo decodificato e `Timings` (le fasi dell'HAR:
  blocked, dns, connect, send, wait, receive; la somma è il totale),
  flussi TLS con lo SNI del ClientHello. `requests()` dà le righe della
  lista.
- **HAR 1.2**: una voce per richiesta; `content.text` decodificato (base64
  se non UTF-8), resa dei decodificatori nei `comment`, `_encoding` per il
  corpo binario di una richiesta (HAR non ha `encoding` in `postData`),
  `status: 0` senza risposta, `connection` = indice del flusso.

### Esposizione
- `vetro boot --pcap FILE --har FILE --net-requests` (anche con `=`):
  cattura accesa dall'inizio, file scritti all'uscita, lista su stderr.
- vetro-wasm e l'app web seguiranno (stesse funzioni, già compilate per
  wasm32).

### Verifica con strumenti esterni
`tools/analysis/check.sh` (immagine Docker `tools/analysis/Dockerfile`):
`capinfos` e `tcpdump` contano i pacchetti, `tshark` decodifica richieste
e risposte HTTP, `har-validator` 5.1.5 valida l'HAR contro lo schema HAR
1.2, `haralyzer` lo riapre in Python. Il test `tests/boot/tests/analysis.rs`
lo usa quando Docker c'è (`VETRO_REQUIRE_ANALYSIS_TOOLS=1` rende la sua
assenza un errore).

## Conseguenze
- M7 resta aperta finché non ci sono gli hook TLS in Android (BoringSSL,
  Conscrypt), la timeline input→effetti e l'uscita sulle 10 app: questi
  pezzi useranno lo stesso modello (`HttpExchange` arriverà anche dai
  byte in chiaro degli hook, con gli stessi decodificatori e lo stesso
  HAR).
- IPv6, frammenti IPv4, HTTP/2 e HTTP/3 non sono analizzati (il guest e
  lo stack oggi non li usano: vedi `docs/specs/net.md`); si aggiungono
  come nuovi parser sopra gli stessi flussi.
- La cattura tiene tutti i frame in memoria finché l'host non li prende:
  chi la accende per sessioni lunghe deve svuotarla (`vetro boot` lo fa a
  ogni quanto).
