# ADR 0017 — Overlay copy-on-write persistente e cache degli snapshot nel browser

- Stato: accettata (M6, seconda parte, 2026-09-26). Estende l'ADR 0014
  (dischi dal browser) e l'ADR 0015 (snapshot).

## Contesto
Le immagini dei dischi arrivano con HTTP Range e sono in sola lettura; le
scritture del guest finivano in un `CowBackend` in memoria e si perdevano a
ogni ricarica della pagina. M6 chiede che restino fra una sessione e
l'altra, e che dal secondo avvio la macchina riparta da uno snapshot invece
di riavviare il kernel. Due problemi:

1. **Dove e come conservare le scritture.** Nel browser c'è OPFS
   (`FileSystemSyncAccessHandle` nel Worker, scritture sincrone a un
   offset); la CLI ha i file. Lo stesso formato deve andare bene per tutti e
   due (`vetro boot --disk=base.img --overlay=FILE`).
2. **Coerenza fra snapshot e disco.** Lo snapshot contiene la RAM, e con lei
   la cache delle pagine e lo stato del filesystem del guest: vale solo con
   il disco com'era nel momento del salvataggio. Se la sessione continua e
   il guest scrive ancora, ripristinare lo snapshot della volta prima sopra
   il disco nuovo rovina il filesystem; ripristinarlo col disco vecchio
   perde le scritture.

## Decisione

### Il file dell'overlay (`vetro_snapshot::overlay`)
- Intestazione di 4 KiB (magia `VETROCOW`, versione, cluster da 4 KiB,
  dimensione del disco, **generazione**, numero di slot, **identità della
  base**, somma di controllo), poi slot di 16 + 4096 byte: indice del
  cluster (o libero), controllo (`hash64` dei dati legato all'indice), dati.
- **Un cluster, uno slot, riscritto sul posto.** Niente registro da
  compattare: il file è grande quanto i cluster vivi. Un cluster tolto
  (dopo un ripristino) libera il suo slot, che il prossimo cluster nuovo
  riusa.
- **Ordine delle scritture**: prima gli slot, poi (dopo un flush)
  l'intestazione con generazione e numero di slot. Un'interruzione prima
  dell'intestazione lascia gli slot nuovi fuori dal conto; uno slot scritto a
  metà ha il controllo sbagliato e si ignora (quel cluster torna quello
  della base); un'intestazione rovinata scarta l'overlay.
- **Identità della base**: una stringa dell'host, confrontata esattamente,
  più la dimensione del disco. Browser: URL, dimensione ed `ETag` (o
  `Last-Modified`), la stessa chiave della cache dei blocchi; file scelto
  dall'utente: nome, dimensione, data di modifica. CLI: nome del file,
  dimensione, data di modifica in ns. Un overlay di un'altra base **si
  scarta** (con un messaggio) e il file si riscrive da capo: applicato a
  un'altra immagine sarebbe un filesystem rovinato. Niente hash del
  contenuto: leggere GiB di immagine a ogni avvio non si può.
- Il modulo non fa I/O. `Overlay::load` legge il file intero (i cluster
  stanno comunque in memoria nel `CowBackend`) e `Overlay::update`/`sync`
  restituiscono le scritture da fare (`Patches`: troncamento facoltativo,
  poi coppie offset/byte), che il JS applica con
  `FileSystemSyncAccessHandle` e la CLI con `write_at`. Il formato e
  l'allocazione degli slot quindi sono in un posto solo, e due sessioni
  uguali producono file uguali byte per byte.

### Chi sa che cosa è cambiato
- `CowBackend` (vetro-platform) tiene l'insieme dei cluster scritti dal
  guest dall'ultima `take_dirty`, e dà `cluster`, `clusters`, `load_cluster`
  (i cluster di un overlay caricato non contano come scritture). È
  contabilità dell'host: non entra negli snapshot, e `restore_state` la
  svuota.
- Dopo un ripristino i cluster in memoria sono quelli dello snapshot: chi
  persiste fa un **confronto completo** (`Overlay::sync`: riscrive i
  cluster diversi, toglie quelli che non ci sono più, lascia gli uguali). Se
  snapshot e file coincidono (il caso normale, vedi sotto) non scrive niente.
- La persistenza si fa **fra un quanto e l'altro**, fuori da
  `Machine::device`: il guest non vede niente, e il momento non cambia
  l'esecuzione.

### vetro-wasm, ABI 6
`vetro_overlay_open` (contenuto del file e identità; codici `LOADED`, `NEW`,
`MISMATCH`, `CORRUPT`, `NO_DISK`), `vetro_overlay_take` / `_ptr` / `_clear`
(le scritture codificate), `vetro_overlay_info` (generazione, cluster, slot,
slot rovinati, lunghezza del file). `vetro_snapshot_restore` segna gli
overlay aperti per il confronto completo. Dettagli in `docs/specs/wasm.md`.

### Cache degli snapshot nel browser
- **Chiave**: SHA-256 di versione del formato degli snapshot
  (`vetro_snapshot_version`), SHA-256 di kernel e initramfs, riga di
  comando, RAM, risoluzione, dispositivi, e per ogni disco identità della
  base, dimensione, sola lettura. Non entrano JIT, tempo reale e dimensione
  dei blocchi della cache (non cambiano lo stato del guest). Due file in
  OPFS (`vetro-snapshots/<chiave>.snap` e `.json`); i metadati si scrivono
  dopo i byte e fanno da segno di snapshot completo.
- **Snapshot e overlay si salvano insieme**: prima gli overlay (tutte le
  scritture del guest nel file), poi lo snapshot, con nei metadati la
  **generazione** dell'overlay di ogni disco in quel momento.
- **Al ripristino** lo snapshot vale solo se ogni overlay è ancora alla
  generazione salvata. Se il disco è andato avanti dopo lo snapshot
  (scritture della sessione dopo l'ultimo salvataggio), lo snapshot si
  lascia stare e la macchina **si avvia da zero con l'overlay**: le
  scritture non si perdono mai, al più si perde la ripartenza veloce, e al
  prossimo riposo si salva uno snapshot nuovo. Lo snapshot contiene anche i
  cluster (ADR 0015), quindi anche senza overlay persistente è coerente da
  solo.
- **Quando si salva**: la prima volta che il guest è **a riposo** (avvio
  finito), poi a riposo quando la generazione degli overlay è cambiata dallo
  snapshot, e a richiesta (pulsante "Salva stato"). A riposo = `Stop::Idle`,
  oppure 1,5 s di tempo del guest senza uscita sulla console, senza cambi
  dello scanout, senza ingressi e senza attività dei dischi: il kernel ha
  sempre un timer in corso, quindi `Idle` da solo non arriva mai al prompt.
  Non si salva alla chiusura della pagina (il Worker muore senza preavviso).
- Col ripristino la pagina rimostra la coda della console salvata nei
  metadati (64 KiB), senza rispondere di nuovo alle richieste del terminale
  (`ESC[6n`): la risposta l'aveva già data la sessione salvata.

## Conseguenze
- Scritture del guest persistenti fra le sessioni, nel browser e nella CLI,
  con lo stesso formato; cambiare l'immagine base scarta l'overlay.
- Dal secondo avvio il kernel guest di M3 è pronto in 0,69 s dall'apertura
  della pagina (ripristino 263 ms, snapshot di 11,7 MiB) contro 2,2 s da
  zero; in Node/V8 salvataggio 117 ms e ripristino 148–353 ms di uno
  snapshot di 10,1 MiB al prompt (`docs/progress/M6.md`).
- Gli overlay stanno interi in memoria (nel `CowBackend`) e il file si
  legge intero all'apertura: per Android (centinaia di MiB scritti) andrà
  misurato; il formato permette di leggere i cluster a richiesta senza
  cambiarlo.
- Il riposo è euristico: un guest che scrive sempre sulla console (o
  Android con l'animazione della home) non si salva da solo finché non si
  calma; c'è il pulsante. Per Android si deciderà il segnale di "avvio
  finito" (per esempio `sys.boot_completed` via adb) quando ci sarà.
- La persistenza non è sincrona col FLUSH del guest: una scrittura già
  confermata al guest si perde se la pagina si chiude prima del salvataggio
  successivo (al più 1 s nel browser, un quanto nella CLI). Un'interruzione
  a metà di una riscrittura sul posto fa tornare quel cluster a quello della
  base (controllo sbagliato). Legare la conferma del FLUSH al salvataggio
  vorrebbe dire fermare il guest (`Blocked`) a ogni FLUSH: da valutare con
  Android, misurando.
