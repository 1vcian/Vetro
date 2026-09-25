# Spec — vetro-analysis

## Perimetro
Motore di analisi dall'esterno del guest. Oggi: decodifica delle syscall
per `vetro run --strace` (M2, modulo `syscall`) e analisi di rete (M7,
modulo `net`, ADR 0016). Hook TLS, Binder, ART e scripting arrivano con
M7–M9.

## Dipendenze ammesse
Nessuna (né a runtime né in sviluppo). Compila in `wasm32-unknown-unknown`.
Niente orologio dell'host, thread, I/O, `HashMap`: tabelle `BTreeMap`/`Vec`,
risultati deterministici. Nessun panic su byte arbitrari (i parser
restituiscono `None`/`Err` o segnano i messaggi incompleti).

## `net`: interfaccia pubblica
- `capture`: `Direction { FromGuest, ToGuest }`, `Frame { at_us, dir, data }`
  (tempo virtuale del guest in µs), `Capture` (frame in ordine di tempo
  non decrescente: `push`, `frames`, `into_frames`, `From<Vec<Frame>>`).
- `pcapng`: `write(&[Frame], &PcapngOptions { epoch_us, interface }) ->
  Vec<u8>`; `read(&[u8]) -> Result<PcapngFile, PcapngError>` (interfacce e
  frame; EPB e SPB, entrambi gli ordini di byte, `if_tsresol`).
- `packet`: `parse(&[u8]) -> Packet` (`Tcp`, `Udp`, `OtherIpv4`, `Arp`,
  `Other`) e le intestazioni `ethernet`, `ipv4`, `tcp`, `udp`.
- `flow`: `Flows::from_frames` → `tcp: Vec<TcpFlow>` (cliente, server,
  istanti di SYN/SYN-ACK/handshake/chiusura, reset, `client_data` e
  `server_data`: `Stream { bytes, marks, fin, missing }`, `time_at(offset)`)
  e `udp: Vec<UdpFlow>` (datagrammi con istante e verso), `other_frames`.
- `dns`: `parse(&[u8]) -> Option<Message>`, `exchanges(&[UdpFlow]) ->
  Vec<DnsExchange>` (nome, tipo, istanti di domanda e risposta, rcode,
  record; `ipv4()`), `type_name`.
- `http`: `looks_like_request`, `parse_requests(&[u8])`,
  `parse_responses(&[u8], methods, eof)`; `Request`/`Response` con
  `Headers` (ricerca senza maiuscole, `has_token`), offset nel flusso,
  `complete`, e `Body { wire_len, raw, decoded, content_encoding,
  decode_error, chunked }`.
- `inflate`: `inflate_raw`, `gunzip` (più membri), `zlib_or_raw`, `crc32`.
- `body`: `decode(content_type, &[u8]) -> Decoded` (`Empty`, `Json`,
  `Form`, `Multipart(Vec<Part>)`, `Protobuf(Vec<Field>)`, `Text`,
  `Binary { len, note }`), `protobuf`, `protobuf_text`, `form`,
  `url_decode`, `media_type`, `param`; `Decoded::kind`, `to_text`.
- `json`: `parse -> Value` (ordine e numeri conservati), `Value::get`,
  `as_str`, `to_compact`, `to_pretty`, `quote`.
- `inspector`: `NetworkAnalysis::from_frames(&[Frame])` → `frames`,
  `flows`, `dns`, `http: Vec<HttpExchange>`, `tls: Vec<TlsFlow>`;
  `requests() -> Vec<RequestRow>` (con `Display`: una riga per richiesta).
  `HttpExchange`: flusso, indirizzi, nome risolto, URL, richiesta,
  risposta, corpi decodificati, `Timings` (`started_us`, `blocked_us`,
  `dns_us`, `connect_us`, `send_us`, `wait_us`, `receive_us`,
  `total_us()`), `first_on_connection`. `tls_sni(&[u8])`.
- `har`: `to_har(&NetworkAnalysis, &HarOptions { epoch_us }) -> String`
  (anche `NetworkAnalysis::to_har`), `iso8601`, `base64`.

## Semantica
- Tempi visti dal guest, al confine di virtio-net. Fasi: `dns` = domanda →
  risposta del DNS che ha dato l'indirizzo (solo la prima connessione che
  lo usa); `connect` = SYN → ACK dell'handshake (prima richiesta della
  connessione); `send` = primo → ultimo byte della richiesta; `wait` =
  ultimo byte della richiesta → primo della risposta (0 se la risposta
  arriva prima, come fa il sinkhole); `receive` → ultimo byte della
  risposta; `blocked` = il resto fino al primo byte della richiesta.
  Il totale è la somma delle fasi.
- URL: `http://` + `Host` (o nome risolto, o indirizzo) + target; target
  assoluto usato così com'è; CONNECT → `https://` + autorità.
- Connessioni aperte dall'host (inoltro di porte, `Stack::host_connect`):
  il cliente è 10.0.2.2, il server il guest; HTTP servito dal guest si
  analizza allo stesso modo.
- HAR: vedi l'intestazione di `src/net/har.rs` e ADR 0016.

## Collegamento alla macchina
- `vetro-machine`: `Machine::net_tap(on)` / `net_tap_take()`,
  `TappedFrame { at: VirtualTime, dir: FrameDir, data }` (ADR 0016). Non
  cambiano l'esecuzione e non entrano negli snapshot.
- `vetro-cli`: `vetro boot --pcap FILE --har FILE --net-requests`
  (`vetro_cli::netcap`).
- vetro-wasm: da fare (le funzioni compilano già per wasm32).

## Test
- `cargo test -p vetro-analysis`: parser con input costruiti (Ethernet/IP,
  flussi con ritrasmissioni, fuori ordine, sequenze oltre 2^32, buchi,
  porte riusate; DNS con puntatori in ciclo e troncati; HTTP pipelining,
  HEAD, 100 Continue, chunked, troncati; gzip dell'host a codici fissi e
  dinamici, più membri, zlib, blocchi non compressi; JSON validi e non;
  form, multipart, protobuf e gRPC; pcapng andata e ritorno; HAR con
  somme dei tempi, date, base64; SNI), e byte casuali senza panic.
- `vetro-machine`: `cattura_dei_frame_nei_due_versi`.
- `cargo test --release -p vetro-boot-tests --test analysis`: il kernel
  guest fa sette richieste con wget/nc di BusyBox (JSON, chunked+gzip
  dell'host, JSON→protobuf, form, multipart, protobuf, corpo gzip di
  BusyBox); verifica richieste, corpi, DNS, tempi, HAR, pcapng riletto
  dal nostro lettore e, con Docker, da `capinfos`/`tcpdump`/`tshark`,
  HAR validato da `har-validator` (schema 1.2) e aperto da `haralyzer`
  (`tools/analysis/check.sh`; `VETRO_REQUIRE_ANALYSIS_TOOLS=1` rende
  l'assenza di Docker un errore); due esecuzioni danno pcapng e HAR
  identici.
- `cargo test --release -p vetro-cli --test boot_pcap_har`: le opzioni di
  `vetro boot`.
