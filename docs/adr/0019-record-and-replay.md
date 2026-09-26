# ADR 0019 — Record & replay della macchina: un punto d'ingresso, un log, il salto a un'istruzione

- Stato: accettata (M10, nucleo, 2026-09-26). Estende l'ADR 0011
  (`Machine::run`), l'ADR 0014 (tempo fermo sui dischi) e l'ADR 0015
  (snapshot).

## Contesto
M10 chiede "replay identico con ritorno al momento esatto di una chiamata".
La macchina è già deterministica: il tempo è il numero di istruzioni (ADR
0011), un disco che aspetta dati ferma il tempo del guest (ADR 0014), la
casualità offerta al guest viene dal seme della configurazione, l'RTC parte
da un'ora fissata nella configurazione, lo stack di rete e il sinkhole
stanno dentro la macchina e usano il tempo virtuale. Da uno snapshot si
riparte istruzione per istruzione come se la macchina non si fosse mai
fermata (ADR 0015).

Restano gli **ingressi dell'host**, che arrivano fra un quanto e l'altro in
momenti che dipendono dall'host: byte della console, eventi di tastiera e
puntatore, tasto di accensione (GPIO), ridimensionamento del display,
operazioni dell'host su vsock, connessioni dell'host verso il guest
(inoltro di porte, entrato su main in parallelo) e, nuovo, frame di rete
consegnati dall'host. Finora passavano da chiusure (`Machine::device`,
`Machine::net`, `Board::gpio_input`) che non si possono registrare.

## Decisione

### Un solo punto d'ingresso: `Machine::input(Input) -> Reply`
- `Input` elenca tutto ciò che l'host può fare al guest: `Console(byte)`,
  `Keyboard(eventi)`, `Pointer(eventi)`, `Gpio { line, level }`,
  `Display { scanout, width, height }`, `NetFrame(frame)`, `NetLink(bool)`,
  `Vsock(VsockOp)` (listen, unlisten, accept, connect, send, recv,
  shutdown, close, reset, release, transport reset), `HostNet(HostNetOp)`
  (connect, send, recv, shutdown, abort, release: i metodi `Stack::host_*`).
  Anche le **letture** dell'host (`recv` di vsock e della rete) sono
  ingressi: liberano credito o finestra, e il guest lo vede.
- `Reply` porta l'esito (connessione aperta, byte accettati o letti, errori
  di vsock, `NoDevice`).
- `console_input` e il nuovo `Machine::gpio_input` passano da `input`;
  `vetro-cli` (`--hostfwd`) e `vetro-wasm` (tasti, puntatore, display,
  GPIO, `vetro_net_*`) usano `input` al posto delle chiusure. Gli aiuti
  `Input::key_events`, `move_abs_events`, `touch_events` danno gli stessi
  eventi dei metodi di `VirtioInput` (provato confrontando lo stato salvato).
- **Frame di rete dell'host** (`Input::NetFrame`): il collegamento di rete
  della macchina (`NetLink`) ha una coda di frame dell'host che virtio-net
  consegna prima di quelli dello stack. È stato del guest: entra nello
  snapshot, e `FORMAT_VERSION` passa da 2 a 3.
- Le chiusure restano (`device`, `gpu`, `keyboard`, `pointer`, `vsock`,
  `net`) ma durante una registrazione ogni loro uso diventa un **evento
  opaco** nel log: il replay si ferma lì (`Divergence::Opaque`). Per ciò che
  non è un ingresso ci sono accessi dedicati che non si registrano:
  `device_view`, `gpu_view`, `vsock_view`, `net_view` (sola lettura) e
  `host_link` (dati per un disco che la macchina aspetta: ADR 0014, il tempo
  è fermo e in replay il disco dà gli stessi dati).

### Che cosa non si registra, e perché
- **Completamenti dei dischi asincroni**: con l'ADR 0014 la macchina non
  esegue istruzioni finché i dati non arrivano, e la richiesta si completa
  allo stesso numero d'istruzioni di un disco sempre pronto. Il log non ha
  bisogno di sapere quando sono arrivati; serve solo lo stesso contenuto
  (il disco è un collegamento, come negli snapshot).
- **Tempo e casualità dell'host**: non ci sono. RTC e seme sono
  configurazione (nel log, e nell'hash della configurazione).
- **Uscite** (console, immagine dello scanout, registro di rete): si
  ricalcolano; la console entra nei controlli.

### Ingressi con la macchina ferma su un disco
Un ingresso che arriva mentre `run` restituisce `Stop::Blocked` si
**rimanda** (`Reply::Deferred`) alla fine del primo quanto dopo lo sblocco,
e si registra lì. Applicarlo subito lo metterebbe in un punto che in replay
(con un disco pronto) non esiste: dentro la WFI interrotta dal blocco, o
fra il servizio che ha trovato il disco non pronto e quello che lo completa.
Vale solo durante una registrazione: senza, il comportamento resta quello
dell'ADR 0014.

### Il log
Contenitore di `vetro-snapshot` (stessa intestazione degli snapshot, con
magia `"VETROREC"` e versione propria `LOG_VERSION` = 1; nuovi
`encode_container`/`decode_container`), sezioni:
- `HEAD`: versione degli snapshot dei keyframe, `MachineConfig` (RAM, ora,
  seme), JIT usato, intervallo dei keyframe, impronta di partenza;
- `EVTS`: per evento il numero d'istruzione, un hash della CPU (lo stato
  `Cpu` dello snapshot) e i byte usciti dalla console fino a lì, poi
  l'ingresso (o l'evento opaco);
- `KEYF`: snapshot periodici (istruzione, conto della console, snapshot);
- `END `: impronta finale.

L'**impronta** (`Digest`) è: istruzioni, hash della CPU, della MMU col TLB,
della piattaforma (tutti i dispositivi con i backend interni), della RAM, e
byte e hash (FNV-1a incrementale) della console dall'inizio. Il TLB non si
confronta se registrazione o replay hanno usato il JIT (ADR 0013). L'uscita
della console si conta quando la macchina la toglie dalla UART; per non
dipendere da quando l'host legge, gli eventi usano i byte usciti **compresa**
l'uscita ancora nella UART, e l'impronta prima porta l'uscita nel buffer
della macchina (che `console_output` restituisce).

### Replay
- `start_replay(&log)` dallo stato di partenza (stesso kernel caricato o
  stesso snapshot ripristinato: si controllano configurazione e impronta),
  oppure `replay_from(&log, n)` dall'ultimo keyframe non oltre `n`.
- `run(budget)` in replay taglia il quanto all'istruzione del prossimo
  evento: lì confronta hash della CPU e byte della console, applica
  l'ingresso e continua. Il JIT riceve come limite la fine del quanto
  (`jit_budget`), quindi **non supera mai un evento**: nessun cambio al JIT.
  Alla fine della registrazione confronta l'impronta: `ReplayStatus::Finished`
  o `Diverged(...)`. Dopo la fine (o una differenza) la macchina continua
  libera. Gli ingressi dell'host durante il replay si ignorano
  (`Reply::Ignored`).
- Si appoggia a una proprietà già usata dagli ADR 0014 e 0015 e ora provata
  anche qui: **i confini dei quanti non cambiano l'esecuzione**. Un
  ingresso applicato fra due quanti all'istruzione N dà lo stesso risultato
  qualunque sia il quanto dell'host.
- Un evento che la macchina oltrepassa senza fermarcisi, o una macchina che
  si ferma da sola (spenta, inattiva) prima di un evento, è
  `Divergence::Missed`. Inattiva sull'istruzione del prossimo evento va bene:
  la registrazione aveva visto lo stesso `Stop::Idle` e dato l'ingresso lì.

### Salto a un'istruzione
`goto(&log, n)`: `replay_from` dal keyframe più vicino e replay fino al
primo confine con almeno `n` istruzioni (una WFI può saltare oltre, come
per `--save-at`). Da lì `cpu`, `read_phys`, `read_virt` (traduzione con le
tabelle correnti senza TLB, solo RAM: niente effetti sui dispositivi),
`translate`, `registers_text`. I keyframe si prendono alla fine dei quanti,
mai con la macchina ferma su un disco e mai dopo un evento dello stesso
istante (gli eventi di un istante vengono dopo il suo keyframe); il primo
alla partenza, così il log basta da solo a ripartire.

### CLI
`vetro boot --record=FILE [--keyframes=N]`, `--replay=FILE` (dall'avvio con
`--kernel`, da `--restore`, o dal keyframe iniziale), `--goto=N` con
`--dump=VA:BYTE`. In replay stdin non si legge e `--hostfwd` si rifiuta
(la rete dell'host viene dal log). Codice 0 se il replay è identico, 1 se
diverge.

## Alternative scartate
- **Registrare alla frontiera dei backend** (frame che virtio-net riceve,
  risposte dell'upstream): più generale per un relay futuro, ma lo stato dei
  dispositivi in replay non sarebbe quello registrato (lo stack non
  girerebbe), e il confronto dello stato fallirebbe. Per il relay di M7 la
  frontiera giusta resterà l'`Upstream` di `vetro-net` (risposte da
  registrare in ordine, perché le chiamate avvengono in tempo virtuale); non
  serve oggi, la macchina ha solo il sinkhole.
- **Iniettare dentro `run`** all'istruzione esatta anche a metà quanto: non
  serve, perché tagliare il quanto all'istruzione dell'evento dà lo stesso
  punto.
- **Keyframe incrementali** (solo le pagine cambiate): meno spazio e meno
  tempo, ma serve tracciare le pagine scritte anche dai blocchi del JIT.
  Rimandato: per ora un keyframe è uno snapshot completo.
- **Hash di tutta la macchina a ogni evento**: 1 GiB di RAM da scorrere a
  ogni tasto. L'hash della CPU e il conto della console bastano a fermare il
  replay al primo evento dopo un ingresso sfuggito (provato), e l'impronta
  completa alla fine copre il resto.

## Verifica
- `vetro-machine`, `record::tests`: andata e ritorno del log con ogni tipo
  di ingresso, log rovinati o di un'altra versione rifiutati, eventi di
  virtio-input uguali ai metodi del dispositivo.
- `vetro-machine`, `machine::record::tests` (sonda bare-metal con eco della
  UART, timer, IRQ, SVC, WFI): replay con quanti di 1, 7919 e 2^40
  istruzioni e da un keyframe su una macchina vuota; `goto` a nove punti
  (anche all'indietro) con registri e RAM dell'esecuzione registrata; un
  byte dato alla UART senza `input` ferma il replay al primo evento dopo
  (fallisce senza il controllo); un accesso opaco ferma il replay; log di
  un'altra macchina o di un altro stato di partenza rifiutati; ingresso con
  il disco in attesa rimandato e replay identico con un disco pronto.
- `tests/boot/tests/replay.rs` (kernel guest, release): sessione con tasti
  battuti uno alla volta, DHCP/HTTP/ping al sinkhole, un frame ICMP
  dall'host (il guest risponde: `InEchos` da 0 a 1), tastiera virtio-input,
  eco di 20 KB da una connessione dell'host (`nc -e cat`), `sleep`,
  spegnimento. Registrare non cambia l'esecuzione (stessa sessione senza
  registrazione: stesso log e stato). Replay dall'avvio con l'interprete a
  quanti diversi e dal keyframe iniziale col JIT: stesso log, istruzioni,
  CPU, RAM, dispositivi. `goto` a tre punti con interprete e JIT: stessi
  registri e RAM. Log senza un tasto o senza il frame ICMP: il replay
  diverge al primo evento dopo.
- `crates/vetro-cli/tests/boot_replay.rs`: `--record` da stdin, `--replay`
  in altri processi (dal keyframe, dall'avvio, col JIT) con la stessa uscita
  e "replay identico"; `--goto`/`--dump` uguali a `Machine::goto` nel
  processo del test; un log con un ingresso in meno dà codice 1.

## Conseguenze
- Chi aggiunge un ingresso dell'host lo aggiunge a `Input` (con la sua
  codifica nel log e `LOG_VERSION` + 1), non come chiusura: altrimenti le
  registrazioni che lo usano si fermano sull'evento opaco.
- Il browser (`vetro-wasm`) passa già da `input`; registrare e rifare nel
  browser (esportare il log, keyframe in OPFS) è lavoro della parte web.
- Costi misurati (`docs/progress/M10.md`): registrare senza keyframe costa
  un hash della CPU per ingresso e due impronte (inizio e fine); i keyframe
  costano un salvataggio ciascuno (~10 MiB alla shell del kernel guest).
