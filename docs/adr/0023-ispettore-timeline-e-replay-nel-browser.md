# ADR 0023 — Ispettore di rete, timeline input→effetti e record & replay nel browser

- Stato: accettata (M7 e M10, parte web, 2026-09-26). Estende l'ADR 0016
  (analisi di rete) e l'ADR 0019 (record & replay); vetro-wasm passa
  all'ABI 8.
- Numero: 0021 è già usato da due rami in corso (modifica di SQLite e
  preferenze, immagine AOSP) e 0022 è probabile per il loro riordino; se al
  momento del merge i numeri sono liberi, si può rinumerare senza toccare la
  sostanza.

## Contesto
L'analisi di rete (ADR 0016) e il record & replay (ADR 0019) esistono nella
macchina e nella CLI, ma non nella pagina. M7 chiede un ispettore di rete e
la timeline input→effetti; M10 registrazione e replay nel browser con il
salto a un momento e la vista di registri e memoria. Vincoli: `vetro-analysis`
senza dipendenze e deterministico, nessun cambio a `vetro-machine`, `vetro-net`
e al JIT (altri proprietari), la macchina gira in un Worker e la pagina non
deve cambiare l'esecuzione guardando.

## Decisione

### Ispettore di rete in vetro-wasm
- La `Vm` di vetro-wasm tiene la cattura: a ogni `vetro_run` porta i frame di
  `Machine::net_tap_take` in una `Capture` (al più 64 MiB, poi conta gli
  scartati). Accendere, svuotare e leggere non tocca il guest.
- L'analisi è quella di `vetro_analysis::net` (rifatta solo se sono arrivati
  frame). Lista e dettaglio passano al JS in JSON prodotto da Rust
  (`vetro_analysis::net::view`: campi in camelCase, corpi con la resa del
  decodificatore, la struttura per JSON/form/multipart e i byte in base64 fino
  a 256 KiB), così la pagina non ridecodifica niente e il formato si prova in
  Rust. HAR e pcapng sono quelli dell'ADR 0016.
- Un solo **buffer dei risultati** per macchina (`vetro_result_ptr`): JSON,
  HAR, pcapng, log, keyframe e registri escono tutti da lì, validi fino al
  risultato successivo.

### Timeline: modello in vetro-analysis, euristica dichiarata
- `vetro_analysis::timeline`: ingressi dell'utente (`UserInput`: istruzione,
  tipo, testo, *debole* o di comando) ed effetti (`Effect`: http, dns, tls,
  file, console) nel **tempo del guest in µs** (`istruzioni / 100`, lo stesso
  dei frame catturati).
- **Attribuzione**: un effetto è dell'ultimo ingresso che lo precede entro
  una finestra (3 s di tempo del guest, scelta dalla pagina). Per rete e file
  contano solo gli ingressi di comando (Invio, clic, tocco, comando del
  gestore dei file, tasto di accensione): un carattere battuto a metà riga non
  causa una richiesta; per l'uscita della console conta anche il carattere
  (l'eco). Allo stesso istante vale l'ordine di arrivo: l'uscita letta alla
  fine di un quanto viene prima degli ingressi dati a quel confine. È
  un'euristica, non causalità: il guest può fare richieste per conto suo
  dentro la finestra. La causalità vera arriverà dai tracer di syscall e
  Binder (M8–M9), che potranno sostituire la regola senza cambiare il modello.
- **Ingressi**: vetro-wasm li descrive da sé dall'`Input` che passa a
  `Machine::input` (tasti premuti, pulsanti, tocchi nuovi, righe della
  console ricostruite da `LineEditor`, tasto di accensione, risoluzione;
  non i movimenti, i rilasci, le risposte automatiche del terminale, vsock e
  rete dell'host). La stessa descrizione si applica agli eventi di un log:
  la timeline di un replay si ricostruisce dal log. I comandi del gestore dei
  file li annota il JS (sono traffico vsock, non si riconoscono dall'`Input`);
  le letture del pannello non sono ingressi.
- **Effetti**: rete dalla cattura (richiesta HTTP all'inizio, domande DNS,
  ClientHello TLS); file dagli eventi di inotify delle osservazioni del
  gestore (li annota il Worker: creato, scritto, spostato, cancellato);
  console quando il JS la legge (precisione: il quanto, ≤ 10 ms di tempo del
  guest), unita finché non arriva altro.

### Record & replay nel browser
- Registrazione e replay sono quelli della macchina (ADR 0019). Il log finito
  (o caricato da un file) resta nella `Vm`; i **keyframe si spostano fuori**
  uno alla volta (`vetro_log_keyframe_take`) e rientrano solo quando un
  replay parte da loro (`vetro_log_keyframe_put`). Nel Worker vanno in OPFS
  (`vetro-recordings/`: `kf-<i>` e `log` senza i loro byte, con lo stesso
  `SnapshotStore` degli snapshot; prima i keyframe, poi il log), così la
  memoria del modulo non tiene decine di snapshot da ~10 MB e la
  registrazione sopravvive al ricaricamento della pagina.
- Il **file scaricato** è il log completo (`Log::encode` con tutti i keyframe
  rimessi per il tempo della codifica): basta da solo a rifare la sessione su
  una macchina configurata allo stesso modo, e caricarlo lo riarchivia.
- **Replay e salto** partono dal keyframe più vicino (`vetro_replay_start`);
  il salto a un'istruzione è lo stesso avvio seguito dai soliti `vetro_run`
  con il quanto limitato fino a lì. Non si usa `Machine::goto`: si ferma su
  `Blocked`, mentre nel browser i dischi si servono in modo asincrono fra un
  quanto e l'altro. Il risultato è lo stesso (confini dei quanti irrilevanti,
  ADR 0014/0015/0019).
- Durante il replay la pagina non manda ingressi (il Worker li scarta; la
  macchina li ignorerebbe comunque), il client del gestore dei file è chiuso
  (le sue operazioni sono nel log) e si riapre alla fine, niente tempo reale
  né snapshot in cache. Alla fine: "replay identico" (`Finished`, impronta
  uguale) o la differenza; poi la macchina continua libera.
- Vista dello stato al punto raggiunto: `Machine::registers_text`,
  `read_virt` (traduzione con le tabelle correnti, solo RAM, nessun effetto
  sui dispositivi), `translate`, `read_phys`.

### Esportazioni
Blob e `<a download>` nella pagina: sull'app pubblicata (GitHub Pages) il
browser scarica il file; il test in Chrome imposta il comportamento dei
download e legge i file veri.

## Alternative scartate
- **Timeline e decodifica in JS**: due implementazioni degli stessi formati,
  niente test in Rust, e la regola di attribuzione non sarebbe la stessa per
  la CLI futura.
- **Keyframe tenuti nella memoria del modulo**: 10 MB l'uno alla shell del
  kernel guest (e molti di più con Android), in una memoria lineare che non
  si restringe.
- **Keyframe scritti in OPFS durante la registrazione**: servirebbe
  un'interfaccia nuova in `vetro-machine` (il registratore è privato). Oggi i
  keyframe escono alla fine; si farà con i keyframe incrementali (ADR 0019).
- **Attribuzione per processo o per flusso**: senza tracer di syscall non si
  sa quale processo ha aperto un socket; l'euristica temporale è ciò che si
  può fare dall'esterno oggi, dichiarata come tale.

## Verifica
- `cargo test -p vetro-analysis`: `timeline` (causa entro la finestra,
  ingressi deboli, ordine allo stesso istante, console unita, JSON, limiti,
  righe della console, nomi dei tasti, effetti di rete dall'analisi) e
  `net::view` (lista e dettaglio rilette dal nostro parser JSON, multipart,
  protobuf).
- `cargo test -p vetro-wasm`: descrizione degli ingressi; registrazione,
  file del log, keyframe fuori e dentro, replay identico su un'altra
  macchina, salto con gli stessi registri, log di un'altra macchina
  rifiutato; cattura, HAR, pcapng e timeline dall'API.
- `tests/web/inspector.mjs` e `tests/web/replay.mjs` (Node, kernel M3):
  richieste di wget con corpi decodificati e attribuite al comando; replay
  identico (console, ispettore e timeline uguali) con JIT e interprete, salto
  con registri e memoria uguali, log ricomposto dall'archivio uguale al file.
- `tests/web/browser-analysis.mjs` (Chrome): wget nell'ispettore con il JSON
  decodificato e legato al comando nella timeline, scrittura di un file legata
  al suo comando, download di log/HAR/pcapng, replay identico, salto dalla
  timeline con registri e dump di memoria, log ricaricato e rigiocato.

## Conseguenze
- ABI 8: chi usa vetro-wasm aggiorna `web/node/vetro.mjs` (costanti
  `TIMELINE_INPUT`, `TIMELINE_EFFECT`, `RR_STATE`, `REPLAY_START`).
- Gli effetti sui file non compaiono nella timeline di un replay (il client
  del gestore è chiuso); rete e console sì, identici.
- Quando arriveranno gli hook TLS (M7) e i tracer (M8–M9), le richieste in
  chiaro diventano altri `HttpExchange` e altri effetti: la vista e la
  timeline non cambiano forma.
