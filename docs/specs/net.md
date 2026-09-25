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
- Inoltro di porte (host → guest, sezione sotto): `host_connect`,
  `host_send`, `host_recv`, `host_shutdown`, `host_abort`, `host_conn`
  (`HostConnInfo`, `HostConnState`), `host_conns`, `host_release`; modulo
  `hostfwd` (`HOST_BUFFER`, `FIRST_EPHEMERAL_PORT`).

## Inoltro di porte: connessioni dall'host verso il guest
Come `-netdev user,hostfwd=tcp:…` di QEMU: l'host apre una connessione TCP
verso un servizio del guest (per esempio adbd sulla 5555). Lo stack fa da
client TCP verso il guest; nessun socket vero nel core, tutto sincrono.

- `host_connect(porta) -> Option<ConnId>`: SYN da **10.0.2.2** (il gateway,
  come slirp traduce le connessioni da localhost) verso
  `guest_ip:porta`, da una porta effimera deterministica (49152, 49153, …,
  saltando le quadruple in uso). L'id è dello stesso contatore delle
  connessioni del guest. Il SYN parte al **prossimo `poll`**: tutte le
  azioni dell'host hanno effetto lì.
- Dal SYN-ACK la connessione è la stessa macchina a stati di `tcp.rs` (in
  più solo lo stato `SynSent`: SYN con MSS 1460 e finestra 65535,
  ritrasmesso con l'RTO della RFC 6298 e il raddoppio; rinuncia dopo
  `tcp_connect_timeout_us` senza RST; RST|ACK valido del guest = `Refused`;
  ACK sbagliato = RST; apertura simultanea non gestita). Al posto
  dell'upstream c'è il lato host (`HostSide`): due code di byte.
- `host_send` accetta al più `HOST_BUFFER` (256 KiB) in coda e restituisce
  quanti byte ha preso (contropressione verso l'host); lo stack li manda
  al ritmo della finestra del guest. `host_recv` legge i byte del guest:
  anche questa coda tiene al più 256 KiB, oltre i quali la finestra
  annunciata al guest si chiude finché l'host non legge (l'aggiornamento
  di finestra parte al `poll` successivo).
- `host_shutdown`: FIN dopo i byte in coda (poi TIME-WAIT di 4 s se il
  guest chiude dopo). `host_abort`: RST al guest (`RemoteReset`); prima
  del SYN, chiusura senza pacchetti. `host_release`: dimentica una
  connessione chiusa (se è viva la interrompe prima).
- `host_conn(id)`: `state` (`Connecting`, `Open`, `Closed(CloseReason)`:
  `Normal`, `Refused`, `GuestReset`, `RemoteReset`, `Timeout`),
  `readable`, `writable`, `guest_eof` (FIN del guest e tutto letto),
  `unsent`, `flow`.
- Registro: `TcpConnect { id, flow }` al posto di `TcpOpen` (riga
  `tcp 1 dall'host 10.0.2.2:49152 -> 10.0.2.15:5555`), poi gli stessi
  `TcpEstablished`, `TcpData` (`ToRemote` = byte del guest verso l'host) e
  `TcpClosed`.
- Serve il MAC del guest, imparato dal suo primo frame (in pratica il
  DHCP): prima, i segmenti andrebbero in broadcast e Linux li scarterebbe.
- Determinismo: stesse chiamate agli stessi istanti → stessi frame e
  registro. Le chiamate dell'host sono **ingressi**: in `vetro-machine`
  passano da `Machine::input` con `Input::HostNet` (che forza un `poll`
  prima della prossima istruzione) e si registrano con il numero di
  istruzione, come i byte della console (M10, ADR 0019).

Piattaforme:
- nativo: `vetro boot --hostfwd=tcp:[ADDR]:PORTA-[10.0.2.15]:PORTA_GUEST`
  (ripetibile; senza indirizzo ascolta su 127.0.0.1; porta 0 = scelta dal
  sistema, stampata su stderr come `vetro: hostfwd tcp 127.0.0.1:PORTA ->
  10.0.2.15:5555`). I socket veri sono in `vetro-cli` (`src/hostfwd.rs`):
  un thread accetta, uno per connessione legge (con un limite di 1 MiB
  letti e non ancora presi dallo stack), uno scrive (idem, 1 MiB); tutto
  entra nella macchina **tra un quanto e l'altro** (2 milioni di
  istruzioni) nel ciclo di `vetro boot`, che tocca lo stack solo se c'è
  qualcosa da fare (senza connessioni `--hostfwd` non cambia
  l'esecuzione). Chiusure verso il client come slirp: FIN dopo `Normal` e
  `Refused` (nessuno in ascolto: il client vede subito la fine del
  flusso), RST (SO_LINGER 0) dopo un reset o un timeout; un RST del client
  diventa `host_abort`.
- browser: `vetro_net_*` di vetro-wasm (ABI 5, `docs/specs/wasm.md`) e
  `GuestSocket` in `web/node/vetro.mjs`.

## Come adb userà l'inoltro (M5/M6)
adbd nel guest Android ascolta su TCP 5555 (`service.adb.tcp.port=5555`,
`docs/research/m5-immagini-android.md`).
- Nativo: `vetro boot … --hostfwd=tcp:127.0.0.1:5555-:5555`, poi
  `adb connect 127.0.0.1:5555` con l'adb vero dell'host: il protocollo ADB
  (CNXN, AUTH con la chiave RSA di `~/.android/adbkey`, OPEN/WRTE/OKAY/
  CLSE) passa trasparente sulla connessione inoltrata. Come con QEMU
  (`hostfwd=tcp::5555-:5555`); l'emulatore di Android Studio usa invece
  la coppia 5554/5555 sulla console, che qui non c'è.
- Browser: nessun socket, quindi niente adb dell'host: servirà un client
  ADB in JS (M5/M6) sopra `GuestSocket` (`connectGuest(5555)`), che parli
  il protocollo ADB (messaggi da 24 byte + dati, chiave RSA generata con
  WebCrypto e conservata in IndexedDB, `shell:`, `sync:` per push/pull,
  `install` via `exec:cmd package install`). In alternativa un ponte
  WebSocket verso un adb dell'host, con il relay di M7.
- Da decidere con l'immagine vera: se adbd parte già in TCP o va attivato
  (proprietà di sistema nell'immagine, ADR di M5), e l'autorizzazione della
  chiave (`ro.adb.secure=0` nelle build di debug, altrimenti la chiave
  pubblica in `/data/misc/adb/adb_keys` nell'immagine).

## Registro degli eventi (`NetEvent { at, kind }`)
`Dhcp`, `IcmpEcho`, `TcpOpen`, `TcpEstablished`,
`TcpData { dir, len }` (solo byte nuovi: dal guest quando entrano in ordine,
verso il guest quando lo stack li prende dall'upstream), `TcpClosed { reason,
bytes_to_remote, bytes_to_guest }`, `UdpOpen`, `UdpData`, `UdpClosed`,
`DnsQuery { txid, name, qtype }`, `DnsAnswer { rcode, addrs }`,
`TcpConnect` (connessione aperta dall'host, inoltro di porte). Il contenuto
dei byte non è nel registro: lo tiene l'upstream (il sinkhole lo conserva).

`NetEvent`, `Flow` e `Mac` hanno `Display`: una riga per evento con il
tempo virtuale in secondi (`[     1.500000] tcp 3 syn 10.0.2.15:40000 ->
198.18.0.1:80`), usata da `vetro boot --net-events`.

## Collegamento alla macchina (`vetro-machine`, M5)
- `Devices::net: Option<NetSetup>` (`mac`, `NetConfig`, `SinkholeConfig`);
  il default ha la rete, montata dopo GPU, tastiera e tablet (slot 28, come
  il quarto `-device` di QEMU). MAC del guest predefinito
  `52:54:00:12:34:56`, quello che QEMU dà al primo `virtio-net-device`.
- Backend di virtio-net: `NetLink` (`send` → `Stack::receive`, `recv` →
  `Stack::pop_frame`). virtio-net offre solo MAC, STATUS e MRG_RXBUF: niente
  `VIRTIO_NET_F_CSUM`, quindi il guest calcola tutti i checksum.
- Tempo: `VirtualTime` = CNTPCT della macchina in microsecondi (per
  difetto; la scadenza `next_deadline` diventa il primo CNTPCT che la
  raggiunge). Nessun orologio dell'host. In `Machine::sync_irqs`, prima di
  servire i dispositivi, lo stack riceve l'istante corrente; alla sua
  scadenza si chiama `poll` e, se ci sono frame, virtio-net viene servito.
  La scadenza dello stack entra nella prossima scadenza della macchina
  insieme a quella del timer: limita i blocchi del JIT e fa da sveglia per
  la WFI (il guest inattivo salta direttamente lì). Stesse istruzioni con e
  senza JIT.
- Accesso dell'host: `Machine::input(Input::HostNet(..))` e
  `Input::NetFrame` (ingressi registrati, ADR 0019),
  `Machine::net(|stack| …)` (mutabile, forza un `poll`; durante una
  registrazione è un evento opaco che ferma il replay) e
  `Machine::net_view(|stack| …)` (sola lettura, non cambia l'esecuzione:
  registro, statistiche, `upstream().tcp_connections()`).
- CLI: `vetro boot` ha la rete di default; `--no-net` la toglie, `--net` la
  rimette anche con `--no-devices`, `--net-events` stampa il registro su
  stderr (`vetro-net: …`).
- Il relay (M7) sarà un altro upstream dietro lo stesso `NetLink`.
- Cattura (M7, ADR 0016): `Machine::net_tap(on)` e `net_tap_take()`
  copiano i frame Ethernet che passano da `NetLink` nei due versi, con
  l'istante virtuale; solo osservazione (non cambia l'esecuzione, fuori
  dagli snapshot). L'analisi è in `vetro-analysis` (`docs/specs/analysis.md`).

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

Con il kernel guest Linux 6.18 (`cargo test --release -p vetro-boot-tests`,
BusyBox nell'initramfs, `udhcpc` con `/usr/share/udhcpc/default.script`):
- `vetro.rs` (autotest, confrontato con QEMU `-netdev user`): lease DHCP
  (10.0.2.15/24, router 10.0.2.2, DNS 10.0.2.3, 86400 s), tabella delle
  rotte, `resolv.conf`, ping a gateway e DNS: stesso log;
- `net.rs` (solo Vetro, perché QEMU manda DNS e TCP sulla rete vera):
  `nslookup` riceve 198.18.0.1; `wget` fa una GET breve, una GET da 300 KB
  (somma `cksum` uguale a quella dell'host) e una POST da 108 KB (corpo
  ritrovato byte per byte nel sinkhole); ping al gateway e a un indirizzo
  finto; l'host verifica DHCP, domande e risposte DNS, connessioni con nome
  risolto, byte nel registro uguali a quelli del sinkhole, chiusure
  `Normal`, nessun checksum errato; due esecuzioni danno stesso log, stesse
  istruzioni, stesso registro;
- `crates/vetro-cli/tests/boot_net.rs`: `vetro boot --no-devices --net
  --net-events`, DHCP e una GET, eventi letti da stderr.

Inoltro di porte:
- `crates/vetro-net/tests/hostfwd.rs` (finto guest server): SYN dal
  gateway con la prima porta effimera, handshake e registro, eco di 200 KB
  oltre finestra e coda, contropressione in entrambi i versi (finestra
  chiusa e riaperta), chiusura dall'host con TIME-WAIT e dal guest, RST|ACK
  del guest (`Refused`, e un RST con ACK sbagliato ignorato), SYN
  ritrasmesso e timeout a 75 s, reset dall'host e dal guest, rilascio,
  determinismo;
- `tests/boot/tests/hostfwd.rs` (kernel M3, API diretta tra un quanto e
  l'altro): `nc -n -v -l -p 5555 -e cat` nel guest, riga `connect to
  10.0.2.15:5555 from 10.0.2.2:…`, eco breve e di 200 KB, chiusura pulita,
  porta senza servizio (`Refused`), servizio che scrive e chiude per primo,
  reset dall'host (`cat: read error: Connection reset by peer` nel guest) e
  dal guest (servizio che esce con dati non letti: `GuestReset`), due
  esecuzioni uguali. Confronto con QEMU
  (`-netdev user,hostfwd=tcp:127.0.0.1:PORTA-:5555`, stesso `nc`): stesse
  righe `listening on 0.0.0.0:5555 ...` e `connect to 10.0.2.15:5555 from
  10.0.2.2:PORTA (10.0.2.2:PORTA)` (porta sorgente a parte) e stessa eco.
  Su macOS il client gira dentro il container di QEMU (`docker exec`,
  `VETRO_ORACLE_NAME`), perché le connessioni che Docker inoltra
  arriverebbero dal suo gateway (172.17.0.1) e non da localhost;
- `crates/vetro-cli/tests/boot_hostfwd.rs`: `vetro boot --hostfwd=
  tcp:127.0.0.1:0-:5555` con un `TcpStream` vero: eco di 200 KB con
  chiusura ordinata nei due versi, porta senza servizio (il client vede la
  chiusura), RST del client visto dal guest;
- `tests/web/hostfwd.mjs`: lo stesso con `GuestSocket` in Node (JIT e
  interprete).

## Snapshot (M6, ADR 0015)

`Stack<U: Upstream + Snapshot>` e `Sinkhole` implementano
`vetro_snapshot::Snapshot` (file `stack/snapshot.rs`, `tcp/snapshot.rs`,
`sinkhole/snapshot.rs`, figli dei moduli per vedere i campi privati): MAC
del guest, frame in uscita, connessioni TCP complete (stato, sequenze,
finestre, congestione, dati in transito, RTO e timer), flussi UDP, indici,
`next_id`, `ip_ident`, contatori, registro degli eventi, connessioni aperte
dall'host (inoltro di porte: code, porta effimera successiva) e stato
dell'upstream. La configurazione non si salva. Chi aggiunge un campo allo
stato lo aggiunge anche lì e incrementa `vetro_snapshot::FORMAT_VERSION`.
