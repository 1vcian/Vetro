# Spec — vetro-net

## Perimetro
Stack di rete lato host: la parte "dall'altra parte del cavo" di virtio-net.
Gateway virtuale IPv4 come la rete user di QEMU, TCP e UDP terminati lato
host (come slirp), backend a trait (`Upstream`) con sinkhole e relay,
registro degli eventi per l'analisi. Scelte in ADR 0007.

## Rete virtuale (valori predefiniti di `NetConfig`)
| | |
|---|---|
| Guest (via DHCP) | 10.0.2.15/24 |
| Gateway | 10.0.2.2, MAC 52:55:0a:00:02:02 |
| DNS | 10.0.2.3 (stesso MAC) |
| MTU | 1500, MSS annunciato 1460 |
| Lease DHCP | 86400 s (T1 metà, T2 7/8) |

- **ARP**: risposte solo per gateway e DNS; mai per l'indirizzo del guest
  (i probe ARP del client DHCP non devono vedere conflitti).
- **DHCP** (RFC 2131): DISCOVER→OFFER, REQUEST→ACK (NAK se l'indirizzo non
  è quello del guest; silenzio se il server id è di un altro server),
  INFORM→ACK; RELEASE e DECLINE solo registrati. Risposte ≥ 300 byte, in
  broadcast IP se il client non ha ancora `ciaddr`.
- **ICMP**: echo verso gateway e DNS sempre risposto; verso l'esterno lo
  decide `Upstream::ping`. UDP verso porte del gateway senza servizio: ICMP
  port unreachable.
- **UDP**: ogni quadrupla è un flusso con un `ConnId`; chiuso dopo
  `udp_idle_timeout_us` (60 s) di inattività. Il DNS è un flusso UDP verso
  10.0.2.3:53, risposto dall'upstream; lo stack ne decodifica domande e
  risposte (record A) per il registro.
- **TCP**: ogni SYN crea una connessione, aperta verso l'upstream; il
  SYN-ACK parte solo quando l'upstream risponde `Connected` (RST se
  `Refused` o dopo `tcp_connect_timeout_us`, 75 s). Dettagli del TCP in
  ADR 0007 e in testa a `src/tcp.rs`.
- **Non supportato** (contato in `Stats`, mai in panic): IPv6 (il guest non
  riceve Router Advertisement e resta con il solo link-local; il sinkhole
  risponde alle domande AAAA senza record, così le app ripiegano su IPv4),
  frammenti IPv4 (Linux usa DF e PMTU; il DNS su UDP sta sotto l'MTU),
  multicast e broadcast diversi da DHCP, VLAN, opzioni TCP diverse da MSS.
- Con `verify_checksums` (predefinito) i pacchetti con checksum errato si
  scartano: la piattaforma non deve offrire `VIRTIO_NET_F_CSUM` al guest, o
  deve completare i checksum parziali prima di chiamare `receive`.

## Interfaccia pubblica
- `Stack<U: Upstream>`:
  - `new(NetConfig, U)`;
  - `receive(now, &frame)`: frame Ethernet trasmesso dal guest (esegue anche
    un `poll`);
  - `poll(now)`: timer (ritrasmissioni, TIME-WAIT, attese, flussi UDP) e
    scambi con l'upstream;
  - `pop_frame() -> Option<Vec<u8>>`: prossimo frame per il guest;
  - `next_deadline() -> Option<VirtualTime>`: entro quando richiamare `poll`
    (l'upstream asincrono richiede un `poll` anche quando ha dati nuovi);
  - `events()`, `take_events()`, `stats()`, `upstream()`, `upstream_mut()`.
- `VirtualTime`: microsecondi di tempo virtuale, non decrescente, fornito da
  chi chiama. `ConnId`: `u64` unico per TCP e UDP, crescente da 1.
  `Flow { guest, remote }`: quadrupla vista dal guest.
- `trait Upstream` (sans-I/O, mai richiamato dallo stack in modo
  rientrante): `tcp_open`, `tcp_status` (`Pending`/`Connected`/`Refused`),
  `tcp_write` (restituisce i byte accettati: contropressione),
  `tcp_read` (`Data(n)`/`WouldBlock`/`Eof`/`Reset`), `tcp_shutdown` (FIN del
  guest), `tcp_close(reset)` (ultima chiamata per quell'id), `udp_send`,
  `udp_recv`, `udp_close`, `ping`.
- `Sinkhole` (`SinkholeConfig`): accetta tutto salvo `refused_ports`;
  risposte `TcpReply { on_connect, on_data, close_after_reply }` per porta
  (predefinita: HTTP 200 vuoto sulla 80); chiude quando il guest chiude;
  DNS A → indirizzi finti in 198.18.0.0/15 in ordine di prima domanda
  (198.18.0.1, .2, …), nomi in minuscolo; registra connessioni
  (`TcpRecord`: nome risolto, byte del guest, chiusura), flussi UDP e
  domande DNS.
- `Relay` (trasporto non bloccante di `RelayMessage`), `RelayUpstream<R>`
  (adattatore `Upstream` → messaggi), `MemoryRelay` (relay di prova: echo
  TCP/UDP, DNS da tabella). Codifica sul filo e controllo di flusso
  host↔relay: M7.
- Moduli di formato pubblici (`wire`, `dhcp`, `dns`) per test e strumenti.

## Registro degli eventi (`NetEvent { at, kind }`)
`Dhcp`, `IcmpEcho`, `TcpOpen`, `TcpEstablished`,
`TcpData { dir, len }` (solo byte nuovi: dal guest quando entrano in ordine,
verso il guest quando lo stack li prende dall'upstream), `TcpClosed { reason,
bytes_to_remote, bytes_to_guest }`, `UdpOpen`, `UdpData`, `UdpClosed`,
`DnsQuery { txid, name, qtype }`, `DnsAnswer { rcode, addrs }`. Il contenuto
dei byte non è nel registro: lo tiene l'upstream (il sinkhole lo conserva).

## Invarianti
- Compila in `wasm32-unknown-unknown`; nessuna dipendenza a runtime; niente
  `std::time`, thread, I/O, `HashMap` (le tabelle sono `BTreeMap`, l'ordine
  dei frame prodotti è per `ConnId`).
- Determinismo: stesse chiamate con stessi argomenti → stessi frame e stesso
  registro. Gli ISN TCP derivano da `NetConfig::seed` e dal `ConnId`
  (SplitMix64); gli ident IP sono un contatore.
- Nessun panic su frame arbitrari (i parser restituiscono `None`).
- Ogni pacchetto prodotto ha checksum corretti, DF, TTL 64.

## Test
`cargo test -p vetro-net`: finto guest che costruisce i frame (DHCP con il
codificatore di smoltcp, ARP, ICMP, DNS, UDP, TCP) e verifica risposte e
registro; ogni frame prodotto è validato da `smoltcp::wire` e da checksum
ricalcolati nel test. TCP: handshake, SYN e SYN-ACK ritrasmessi,
ritrasmissione con raddoppio, esaurimento con RST, finestra zero e sonde,
contropressione dell'upstream, MSS e finestra del guest rispettati, fuori
ordine e sovrapposizioni, RST (validi e fuori finestra), ACK di sfida,
chiusure da entrambi i lati con TIME-WAIT, trasferimento contemporaneo nei
due versi con il 20% di frame persi, determinismo con lo stesso seme.
