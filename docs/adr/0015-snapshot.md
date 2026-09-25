# ADR 0015 — Snapshot della macchina: formato, contenuto, determinismo

- Stato: accettata (M6, prima parte, 2026-09-26).

## Contesto
M6 chiede la home di Android in meno di 15 s dal secondo avvio: si parte da
uno snapshot della macchina già avviata. M10 (record & replay, salto a un
evento) avrà bisogno della stessa cosa, con un requisito più forte: da uno
snapshot si deve ripartire **esattamente** come se la macchina non si fosse
mai fermata, istruzione per istruzione. La macchina è già deterministica
(tempo = istruzioni, ADR 0011; tempo fermo sui dischi, ADR 0014), quindi
basta che lo snapshot contenga tutto lo stato che influisce sul futuro.

Lo stato è sparso in sei crate (`vetro-cpu`, `vetro-mmu`, `vetro-platform`,
`vetro-net`, `vetro-machine`, e i backend dell'host in `vetro-cli` e
`vetro-wasm`), con campi privati. Serve un formato senza dipendenze
esterne che compili anche per wasm32.

## Decisione

### Il crate `vetro-snapshot`
- Senza dipendenze. Dà `Writer`/`Reader` (interi little endian a larghezza
  fissa, booleani stretti 0/1, byte con lunghezza, opzioni, sequenze,
  **sezioni** con etichetta di 4 byte e lunghezza), il trait
  `Snapshot { save(&self, &mut Writer); restore(&mut self, &mut Reader) }`,
  l'intestazione del file e la compressione.
- Ogni crate implementa `Snapshot` per il proprio stato, accanto al codice
  che lo possiede (i campi restano privati): `Cpu`, `SysState`, `Mmu`,
  `Tlb`, `GenericTimer`, `Gic`, `Pl011`, `Pl031`, `Pl061`, `Virtqueue`,
  `VirtioMmio`, `Virt`, `Stack<U: Snapshot>`, `Sinkhole`, `Ram`.
  `vetro-machine` mette insieme le sezioni (`Machine::save`,
  `Machine::load_state`, `Machine::restore`).
- `restore` parte sempre da un oggetto **costruito con la stessa
  configurazione** e lo porta nello stato salvato. Ciò che è configurazione
  (dimensioni, CID, MAC, capacità dei dischi, numero di code) si scrive e al
  ripristino si **controlla** (`Reader::expect_u64`), non si sovrascrive.

### Interfacce che cambiano (`docs/specs/platform.md`)
- `VirtioDevice` ha due metodi obbligatori, `save_state` e
  `restore_state`: ogni dispositivo deve dire il suo stato (un dispositivo
  che se ne dimenticasse non compila).
- `BlockBackend`, `NetBackend`, `ConsoleBackend` hanno `save_state` e
  `restore_state` con implementazione vuota: di norma un backend è un
  **collegamento** verso l'esterno che l'host ricrea prima del ripristino.
  Chi tiene dati scritti dal guest li salva: `MemBackend` scrivibile (tutto
  il contenuto; in sola lettura solo l'hash, per controllare che sia la
  stessa immagine), `CowBackend` (i cluster scritti, poi la base),
  `QueueNet`, `BufferConsole`, `NetLink` di `vetro-machine` (lo stack di
  rete intero). `DisplayBackend` non cambia: al ripristino la GPU gli
  rimanda immagine e cursore di ogni scanout.

### Formato del file (versione 1)
```
"VETROSNP"  u32 versione  u64 hash della configurazione
u64 lunghezza del contenuto  u64 somma di controllo del contenuto
contenuto: sezioni MACH, CPU , MMU , PLAT, RAM  (in quest'ordine)
```
- **Versione** (`vetro_snapshot::FORMAT_VERSION`): cambia a ogni modifica
  di ciò che si scrive. Uno snapshot di un'altra versione si rifiuta con
  `Error::Version` e un messaggio chiaro; niente conversioni (gli snapshot
  sono cache, si rifanno).
- **Hash della configurazione**: RAM, ora iniziale e seme
  (`MachineConfig`), `Devices` (le configurazioni annidate di GPU e rete
  nella loro forma `Debug`, stabile), e per ognuno dei 32 slot virtio tipo,
  feature offerte e dimensioni delle code (copre i dischi montati dall'host
  dopo la costruzione). Diverso = `Error::Config`, macchina non toccata.
- **Somma di controllo** (`hash64`, FNV-1a su parole con rimescolamento
  finale, non crittografica): un file rovinato dà `Error::Checksum`.
- **Dati grandi** (RAM, pixel delle risorse della GPU, cluster
  copy-on-write, dischi in memoria) a blocchi da 4 KiB: i blocchi a zero
  non si scrivono, gli altri con un LZ77 semplice (`vetro_snapshot::lz`,
  gettoni LEB128 di letterali o copie, anche sovrapposte = RLE), o crudi se
  non conviene. Il compressore è goloso e dipende solo dal blocco.

### Che cosa c'è
| Sezione | Contenuto |
|---|---|
| `MACH` | istruzioni eseguite (l'orologio), scadenze in cache del timer e dello stack di rete, WFI in sospeso (ADR 0014), CNTPCT, linee da aggiornare, virtio da servire, disco in attesa |
| `CPU ` | X0–X30, SP, PC, NZCV, TPIDR*, V0–V31, FPCR/FPSR, monitor esclusivo, PSTATE, SP_EL0/1, tutti i registri di sistema di EL1 e di debug, SError in attesa, configurazione (PSCI, MPIDR) |
| `MMU ` | PARange (controllato), SCTLR/TCR/TTBR0/TTBR1/MAIR, **le voci del TLB** |
| `PLAT` | timer (CTL/CVAL dei due canali, CNTVOFF), GIC (ogni INTID, distributore, interfaccia CPU, priorità attive), PL011 (registri, FIFO di ricezione, ingresso dell'host non ancora in FIFO, uscita non letta), PL031, PL061, e per ogni slot virtio il trasporto (selettori, feature negoziate, stato, interrupt, generazione, ultimo errore), le code (dimensione, pronta, indirizzi, `last_avail`, `used_idx`, `signalled_used`, EVENT_IDX e INDIRECT) e il dispositivo |
| `RAM ` | tutta la RAM, pagine a zero omesse |

Dispositivi: **virtio-blk** la richiesta in sospeso (catena di descrittori)
e il backend; **virtio-net** link, MRG_RXBUF negoziato, frame in attesa di
buffer, e lo stack di `vetro-net` (MAC del guest, frame in uscita, ogni
connessione TCP con stato, sequenze, finestre, controllo di congestione,
dati in transito, RTO e timer; flussi UDP; indici; contatori; registro
degli eventi; il sinkhole con tutto ciò che ha registrato e i nomi finti
assegnati) più l'istante corrente; **virtio-gpu** risorse (pixel, backing,
scanout), scanout (risoluzione chiesta, risorsa, rettangolo, cursore con
immagine), eventi; **virtio-input** finestra di configurazione, eventi in
coda, LED, stato; **virtio-vsock** porte in ascolto, connessioni con crediti
e dati, backlog, pacchetti di controllo, prossima porta; **virtio-console**
il backend.

Casualità: non c'è una sorgente di casualità con stato. Il seme della
macchina è configurazione (va nel device tree, cioè in RAM); gli ISN del
TCP derivano dal seme della rete (configurazione) e dal contatore delle
connessioni (salvato).

### Che cosa non c'è, e perché non cambia niente
- **Il JIT.** Blocchi, cache dei salti, TLB software e contatori di
  "calore" non entrano: il risultato del JIT è per costruzione quello
  dell'interprete (ADR 0013). Al ripristino, anche sopra una macchina che
  ha già un JIT con blocchi tradotti: ogni pagina sorvegliata risulta
  scritta (i blocchi si scartano), il contatore di invalidazioni del TLB
  (`Tlb::flushes`) cresce (nuova epoca, TLB software vuota), e l'interprete
  riparte da `Next::Jit`.
- **Cache senza effetti osservabili**: le traduzioni recenti della MMU
  (valide solo finché lo slot del TLB non cambia: ripartono vuote, e le
  generazioni degli slot non servono), il livello in cache della linea IRQ,
  l'ultimo fault di `VirtMemory` (solo diagnosi in modalità utente), la
  bitmap delle pagine sorvegliate dal JIT.
- **I backend esterni**: display (`MemDisplay`, `WebDisplay`), file dei
  dischi (`FileBackend`), immagini via HTTP (`HostDisk` e la sua cache di
  blocchi), il relay. L'host li ricollega prima del ripristino; il loro
  stato non è stato del guest. La base di un disco copy-on-write è un
  collegamento: se ne controlla la dimensione.
- **Il quanto in corso**: si salva solo fra due `Machine::run`. Dentro una
  WFI la macchina salta già alla scadenza prima di tornare, quindi il
  confine è sempre pulito; l'unico caso a metà è la WFI interrotta da
  `Stop::Blocked`, che è stato (`wfi_pending`).

### Il TLB entra, anche se "si potrebbe ricostruire vuoto"
Un TLB vuoto al ripristino cambierebbe il risultato per un guest che
modifica le tabelle senza TLBI (il walk vedrebbe la tabella nuova dove la
macchina originale usava la voce vecchia), e più in generale per ogni
sequenza che l'architettura lascia al TLB. È poco (al più 512 voci), quindi
si salva: `tlb_nello_snapshot` (vetro-mmu) lo verifica con una voce stantia.
Col JIT il TLB vede meno accessi che con l'interprete (differenza già
ammessa dall'ADR 0013): i test d'equivalenza col JIT confrontano tutto
tranne il TLB.

### Garanzie
1. Salvare non cambia la macchina, e due salvataggi nello stesso punto
   danno gli stessi byte (niente tabelle hash, niente orologi, niente
   puntatori; le mappe sono `BTreeMap`).
2. Una macchina ripristinata risalva esattamente i byte letti.
3. Salva a N istruzioni, ripristina in una macchina nuova (o in una usata),
   continua: stesso log della console, stesso numero di istruzioni finale,
   stessa RAM e stesso stato di ogni dispositivo dell'esecuzione senza
   interruzioni, col JIT o senza prima e dopo.
4. Uno snapshot incompatibile (versione, configurazione, file rovinato) si
   rifiuta con un errore che dice il motivo; con i primi tre la macchina non
   cambia. Un errore più avanti (contenuto incoerente con una somma giusta)
   lascia la macchina da scartare.

## Verifica
- `vetro-machine`, `salva_e_ripristina_in_molti_punti`: sonda bare-metal
  con GICv3, timer, IRQ, SVC, WFI ed esclusive; 19 tagli e 5 casi limite
  trovati un'istruzione alla volta (monitor esclusivo armato, dentro il
  gestore d'interrupt, interrupt attivo con IRQ mascherati, dopo la SVC,
  dopo la WFI). La sonda accumula l'ELR di ogni interrupt, così ogni
  istruzione in più o in meno dopo il ripristino si vede (senza il monitor
  nello snapshot il test fallisce). Più: ripristino sopra una macchina
  usata, snapshot incompatibili, richiesta virtio-blk in volo (`Blocked`).
- `tests/boot/tests/snapshot.rs` sul kernel guest di M3: avvio fino allo
  spegnimento, rete (300 KB verso il guest, POST, ping, TIME-WAIT), disco
  (md5sum, scrittura con `dd`, copy-on-write), GPU/input/vsock; tagli a
  numeri d'istruzione fissi e a qualche quanto da un punto del copione,
  con macchina nuova o ritorno indietro sulla stessa, interprete → JIT →
  interprete. Log, istruzioni, CPU, RAM e stato dei dispositivi uguali
  all'esecuzione senza tagli; registro di rete, sinkhole, scanout e
  cursore visti dall'host uguali.
- `crates/vetro-cli/tests/boot_snapshot.rs`: `--save-at` e `--restore` in
  due processi; il seguito coincide con l'avvio originale.
- `vetro-wasm`, `snapshot_dall_api`: l'API C (ABI 4).

## Conseguenze
- Chi aggiunge un campo di stato a un dispositivo, allo stack di rete o
  alla CPU lo aggiunge anche al suo `save`/`restore` e incrementa
  `FORMAT_VERSION`. I test di equivalenza lo trovano solo se il campo
  influisce sul futuro nei loro scenari: il controllo "risalva gli stessi
  byte" no (un campo dimenticato in entrambi passa).
- `vetro-net` tiene la serializzazione in file figli (`stack/snapshot.rs`,
  `tcp/snapshot.rs`, `sinkhole/snapshot.rs`) per non intrecciarsi con chi
  lavora sullo stack.
- Misure alla shell del kernel guest (RAM 1 GiB, release, macOS su Apple
  silicon): snapshot di 10,1 MiB (30,6 MiB di pagine non a zero),
  salvataggio 206 ms, ripristino 172 ms (`docs/progress/M6.md`).
- Seconda parte di M6: snapshot di Android avviato e sua cache nel browser
  (OPFS), livello copy-on-write persistente fra le sessioni, trascinamento
  dell'APK.
