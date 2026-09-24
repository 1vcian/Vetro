# ADR 0007 — Stack di rete lato host scritto in casa; smoltcp solo nei test

- Stato: accettata (preparazione M3, 2026-09-24)

## Contesto
Il guest parla con virtio-net; dall'altra parte del cavo serve un gateway
virtuale che risponda ad ARP/DHCP/ICMP e che **termini** ogni connessione
TCP e ogni flusso UDP del guest verso qualsiasi destinazione (come
slirp/libslirp in QEMU), consegnandone i dati a un backend: il sinkhole
(tutto finto e registrato) o il relay WebSocket (M7, perché il browser non
apre socket). Vincoli: compila in `wasm32-unknown-unknown`, è deterministico
(tempo solo come parametro, nessuna casualità non seminata: serve al replay
di M10), e il suo stato dovrà entrare negli snapshot (M6).

L'alternativa naturale è `smoltcp` (0BSD, no_std, compila in WASM, maturo).

## Decisione
Lo stack è scritto in `crates/vetro-net`, senza dipendenze. `smoltcp` entra
solo come **dev-dependency**, e solo il suo modulo `wire`, come parser
indipendente che valida ogni frame prodotto nei test (checksum IPv4, TCP,
UDP, ICMP; DHCP e DNS decodificati da un'altra implementazione).

Motivi per non usare smoltcp nel crate:
- **Terminazione "a qualsiasi indirizzo".** smoltcp è fatto per essere un
  host con i suoi indirizzi e socket in ascolto. Per fare slirp bisogna
  intercettare ogni SYN verso un IP arbitrario, creare al volo un socket in
  ascolto su quella quadrupla prima di consegnargli il pacchetto, usare
  `any_ip` e rotte finte, e gestire a parte DHCP server (smoltcp ha solo il
  client), ARP per gli indirizzi del gateway e i flussi UDP. Si finisce a
  scrivere comunque metà dello stack attorno a lui.
- **Snapshot e replay.** Lo stato delle connessioni (numeri di sequenza,
  buffer, timer) dovrà essere serializzato negli snapshot (M6) e ripetuto
  identico (M10). Con strutture nostre è un `derive`; i socket di smoltcp
  hanno campi privati e buffer presi in prestito.
- **Registro eventi e attribuzione.** Il motore di analisi vuole eventi
  esatti (byte nuovi per verso, mai contati due volte; apertura, chiusura e
  motivo; domande e risposte DNS) con il tempo virtuale: più semplice
  emetterli dove nasce il dato.
- **Controllo di flusso verso un upstream asincrono.** L'upstream decide
  quanti byte accettare (`tcp_write` restituisce il numero): la finestra
  annunciata al guest segue direttamente la contropressione del relay.

Il TCP scritto qui è volutamente piccolo e lecito per la RFC 9293: niente
window scaling/SACK/timestamp (non annunciati nel SYN-ACK, quindi il guest
non li usa), segmenti fuori ordine scartati con ACK duplicato, ACK immediati,
RTO della RFC 6298 in tempo virtuale con Karn e raddoppio (annullato da un
ACK di dati nuovi), go-back-N al timeout, fast retransmit, Reno, sonde a
finestra zero, ACK di sfida della RFC 5961.

## Conseguenze
- Nessuna dipendenza a runtime: il build WASM non cambia.
- La correttezza del TCP è nostra responsabilità: la coprono i test del
  crate (handshake, ritrasmissioni, finestre, FIN/RST, trasferimento in
  entrambi i versi su un cavo che perde il 20% dei frame) e, da M3, il
  kernel Linux vero come guest.
- Throughput limitato dalla finestra di 64 KiB senza scaling: sul cavo
  virtuale (RTT di microsecondi) basta; se non basterà, lo si misura e si
  aggiunge il window scaling con un ADR.
- Dev-dependency: `smoltcp` 0.14 (0BSD) con `managed` (0BSD), `heapless`,
  `hash32`, `stable_deref_trait`, `bitflags`, `cfg-if` (MIT/Apache-2.0) e
  `byteorder` (Unlicense/MIT). Nessuna entra nei binari distribuiti.
