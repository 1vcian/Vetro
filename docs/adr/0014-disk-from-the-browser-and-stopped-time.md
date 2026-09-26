# ADR 0014 — Dischi con i dati dal browser: `Stop::Blocked` e tempo del guest fermo

- Stato: accettata (M5, 2026-09-25). Estende l'ADR 0011 (`Machine::run`).

## Contesto
Nel browser le immagini dei dischi (AOSP: GiB) non stanno in memoria e non
si possono leggere in modo sincrono: arrivano con `fetch` e HTTP Range, a
pezzi, oppure da un `File` scelto dall'utente (`Blob.slice`, asincrono), e
si tengono in una cache OPFS per i riavvii. La macchina invece è sincrona:
`Machine::run` esegue istruzioni e serve i dispositivi virtio in mezzo.

`VirtioBlk` sa già lasciare in sospeso una richiesta quando il backend
risponde `BlockError::NotReady` e riprovarla al servizio successivo. Ma se
nel frattempo il guest continua a girare, il momento in cui la richiesta si
completa (e quindi l'interrupt, e tutto quello che segue) dipende dalla
rete: lo stesso avvio darebbe istruzioni e log diversi a ogni esecuzione,
contro il principio del determinismo (e il replay di M10).

## Decisione
- **La macchina si ferma.** Dopo ogni servizio virtio `Board` guarda se un
  `VirtioBlk` ha una richiesta in sospeso (`has_pending`). Se sì,
  `Machine::run` restituisce il nuovo `Stop::Blocked` prima di eseguire
  un'altra istruzione, e continua a restituirlo (senza eseguire niente) a
  ogni chiamata finché la richiesta resta in sospeso. Il contatore delle
  istruzioni, cioè il tempo del guest, non avanza.
- **L'host consegna e richiama `run`.** I dati arrivano al backend da
  `Machine::device`, che segna il dispositivo da servire: al `run`
  successivo il servizio ripete la richiesta da capo (è idempotente) allo
  stesso numero di istruzioni e la completa. Da lì in poi l'esecuzione è
  identica a quella con un disco sempre pronto.
- **WFI.** Se la richiesta in sospeso nasce nel servizio fatto dentro una
  WFI, la WFI si riprende al `run` successivo (`wfi_pending`), così anche il
  salto alla prossima scadenza del timer resta lo stesso.
- **Chi guarda la macchina non la tocca.** Le letture dell'host che non
  sono ingressi (immagine dello scanout, contatori, blocchi chiesti) non
  passano da `Machine::device` e non fanno servire i dispositivi; anche una
  consegna di blocchi fatta mentre la macchina non è ferma (lettura
  anticipata) non segna il dispositivo. Solo gli ingressi veri (tasti,
  puntatore, console, GPIO, dati di un disco atteso) cambiano quando il
  guest vede qualcosa.
- **Quanti con confini fissi.** Chi confronta esecuzioni (i test) tratta
  `Blocked` come trasparente: serve i blocchi e continua lo stesso quanto
  fino al suo confine, poi guarda la console e dà gli ingressi. Così gli
  ingressi arrivano agli stessi numeri di istruzione con un disco locale e
  con uno via HTTP.
- **In vetro-wasm** (`docs/specs/wasm.md`): `HostDisk` (blocchi allineati
  da `block_size` byte, lista dei richiesti, cache con limite facoltativo)
  sotto un `CowBackend` (scritture del guest in memoria; M6 le renderà
  persistenti); codice d'arresto 5 `BLOCKED`, `vetro_disk_wanted`,
  `vetro_disk_fill`, `vetro_disk_fail`. Il JS (`web/node/disk.mjs`) cerca
  i blocchi in cache (OPFS con `FileSystemSyncAccessHandle` nel Worker),
  poi nella sorgente, unendo i blocchi contigui in una richiesta.
- La richiesta dei blocchi è per interrogazione (export
  `vetro_disk_wanted` dopo `BLOCKED`) e non per import chiamato da Rust:
  `run` si ferma comunque, il JS sa già quando chiedere, e il codice si
  prova uguale sul target nativo.

## Conseguenze
- Stesse istruzioni e stesso log con un disco locale, via HTTP con cache
  vuota, dalla cache, con blocchi da 4 KiB o 64 KiB, con e senza lettura
  anticipata, col JIT e con l'interprete, e uguali al riferimento nativo
  con disco locale (`tests/web/boot-disk.mjs` e `tests/boot/tests/web.rs`,
  182,9 milioni di istruzioni in tutti i casi).
- Il tempo reale speso ad aspettare la rete non esiste per il guest: niente
  timeout di I/O nel guest per una rete lenta (un blocco che non arriva mai
  si dichiara fallito con `vetro_disk_fail`, e il guest riceve IOERR).
- Il controllo dopo ogni servizio virtio scorre i 32 slot (downcast): costo
  trascurabile rispetto al servizio stesso.
- `vetro-cli` (dischi da file, sempre pronti) non vede mai `Blocked`.
