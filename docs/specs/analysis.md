# Spec — vetro-analysis

## Perimetro
Motore di analisi dall'esterno del guest. Oggi: decodifica delle syscall
per `vetro run --strace` (M2, modulo `syscall`), analisi di rete (M7,
modulo `net`, ADR 0016) e timeline input→effetti (M7, modulo `timeline`,
ADR 0023). Hook TLS, Binder, ART e scripting arrivano con M7–M9.

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
- `view` (ADR 0023): l'ispettore in JSON per l'app web. `requests_json(&a)`:
  `{"frames", "requests": [riga], "dns": [{name, type, queryUs, answerUs,
  rcode, addrs}], "tls": [{flow, server, sni, startedUs}]}`; riga = `{i, flow,
  method, url, host, path, status, reason, mime, reqBytes, respBytes,
  reqWire, respWire, reqKind, respKind, complete, client, server,
  resolvedName, timings: {startedUs, blockedUs, dnsUs, connectUs, sendUs,
  waitUs, receiveUs, totalUs}}` (`null` per ciò che manca).
  `exchange_json(&x)`: `{row, request: {method, target, version, complete,
  headers: [[k, v]], body}, response: {status, reason, version, complete,
  interim, headers, body} | null}`; body = `{wire, raw, size, encoding,
  decodeError, chunked, contentType, kind, text, textTruncated, base64,
  truncated}` più `json` (il valore), `fields` ([[k, v]] del form), `parts`
  (multipart: `{name, filename, contentType, size, kind, text, ...}`),
  `note` (binario). `text` è `Decoded::to_text` (al più 512 KiB), `base64`
  i primi 256 KiB del corpo decodificato; `split_url`.

## `timeline`: interfaccia pubblica (ADR 0023)
- Tempo: µs di tempo del guest, `step_us(istruzioni) = istruzioni / 100`
  (lo stesso dei frame catturati).
- `InputKind` (`Key`, `Pointer`, `Touch`, `Console`, `Files`, `Power`,
  `Display`, `Other`; `name`, `code`/`from_code` = posizione in `ALL`),
  `UserInput { step, at_us, kind, label, weak, seq }` (`UserInput::new`).
- `EffectKind` (`Http`, `Dns`, `Tls`, `File`, `Console`), `Effect { at_us,
  kind, label, detail, bytes, seq }` (`Effect::new`: `seq = u64::MAX`, dopo
  gli ingressi dello stesso istante).
- `cause(inputs, at_us, seq, window_us, strong_only) -> Option<usize>`.
- `Timeline`: `push_input`, `push_effect` (in ordine di tempo, `seq` di
  arrivo), `push_console(at_us, bytes)` (si unisce all'ultimo effetto della
  console se nel frattempo non è arrivato altro), `attributed(extra,
  window_us)`, `to_json(extra, window_us)` = `{windowUs, dropped, version,
  inputs: [{i, step, atUs, kind, label, weak, effects}], effects: [{atUs,
  kind, label, ref, bytes, cause}]}`, `clear`, `version`, limiti
  `MAX_INPUTS`/`MAX_EFFECTS` (i più vecchi si scartano, `dropped`).
- `network_effects(&NetworkAnalysis)`: richieste HTTP (a
  `timings.started_us`, `detail` = indice nell'ispettore), domande DNS,
  flussi TLS.
- `LineEditor` (righe battute alla console: DEL/BS, ^U, ^C, CR/LF, escape
  ignorati), `printable(bytes)`, `key_name(code)`, `is_enter(code)`.

**Regola di attribuzione (euristica):** l'effetto è dell'ultimo ingresso
che lo precede (istante minore, o uguale e arrivato prima) entro la
finestra (default 3 s); per rete e file solo gli ingressi di comando
(`weak == false`: Invio, clic, tocco, comando del gestore, accensione), per
la console qualunque ingresso. Senza ingresso nella finestra: senza causa.

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
- vetro-wasm (ABI 8, ADR 0023): cattura nella `Vm` a ogni `vetro_run`,
  `vetro_capture_*`, `vetro_inspect_*` (lista, dettaglio, HAR, pcapng),
  `vetro_timeline_*`; ingressi descritti da `analysis::Describer`
  (`docs/specs/wasm.md`).

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
- `timeline` e `view`: test unitari in `cargo test -p vetro-analysis`;
  dall'API in `cargo test -p vetro-wasm` e `tests/web/inspector.mjs`
  (kernel M3: wget al sinkhole, corpi decodificati, attribuzione al
  comando, due esecuzioni uguali); nell'app `tests/web/browser-analysis.mjs`.
