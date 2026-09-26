# ADR 0010 — LTP di M2: ambiente dell'oracolo, oracolo nativo dove QEMU sbaglia, tempo virtuale

- Stato: accettata (M2, 2026-09-25). Precisa il criterio di uscita di M2 e
  integra l'ADR 0003 (oracolo QEMU).

## Contesto
Il criterio di M2 confronta una selezione di test LTP eseguiti su Vetro e su
`qemu-aarch64 -cpu cortex-a53`: stesso codice d'uscita, stessi conteggi di
TPASS/TFAIL/TBROK/TCONF. Sul job linux della CI (host arm64) sono emersi tre
problemi che non riguardano la correttezza di Vetro:

1. QEMU user mode passa all'host molte richieste: `uname -r`, le CPU di
   `sched_getaffinity`, i limiti di risorse. Molti test LTP decidono cosa
   provare in base alla versione del kernel o al numero di CPU, quindi
   Vetro e QEMU eseguivano test diversi.
2. Su alcuni test QEMU si discosta dal kernel Linux: `futex_waitv` e
   `F_CREATED_QUERY` non implementati, ordine dei controlli di `mmap`, flag di
   `clone`, eccetera. Imitare questi errori sarebbe un passo indietro nella
   fedeltà.
3. Il tempo virtuale di Vetro contava un nanosecondo per istruzione. Un
   `alarm(5)` costava 5·10⁹ istruzioni, e un ciclo fatto di sole syscall
   (che sull'host costano microsecondi reali ma poche istruzioni) non faceva
   quasi avanzare il tempo.

## Decisione
- **Ambiente dell'oracolo.** L'harness LTP chiede a QEMU, con BusyBox, la
  versione del kernel (`uname -r`) e le CPU disponibili (`nproc`), e le passa
  a Vetro (`Config::release`, `Config::cpus`). I valori predefiniti di Vetro
  restano fissi (una CPU, `6.6.0-vetro`), perché l'esecuzione sia
  riproducibile e perché nel browser non c'è un host da imitare.
- **Oracolo nativo dove QEMU sbaglia.** `tools/ltp/qemu-divergent.txt`
  elenca i test su cui QEMU differisce da Linux, ciascuno con l'esito nativo,
  quello di QEMU e il motivo. Ogni voce è verificata eseguendo il binario in
  nativo e sotto QEMU nello stesso ambiente, da utente non privilegiato. Per
  questi test l'oracolo è l'esecuzione nativa del binario statico su un host
  Linux aarch64 (il job linux della CI); sugli altri host sono saltati, con un
  avviso. Il kernel reale è un oracolo più forte di QEMU: l'eccezione non
  indebolisce la regola "QEMU è l'oracolo", la rende più stretta dove QEMU
  non basta.
- **Oracolo ripetuto.** Vetro è deterministico, l'oracolo no: i test di
  temporizzazione (`nanosleep01`, `futex_wait05`, ecc.) misurano tempi reali
  e su un host carico QEMU a volte li sbaglia. Se l'esito dell'oracolo è
  diverso da quello di Vetro, l'harness lo riesegue fino a due volte e basta
  che una delle esecuzioni coincida. L'esito di Vetro non si ripete mai. I
  test che misurano tempi reali (`tools/ltp/timing.txt`, quelli della
  libreria `tst_timer_test`) girano da soli, dopo la fase in parallelo.
- **Limiti dell'host.** Vetro usa un descrittore dell'host per ogni file del
  guest, quindi alza il proprio limite soft (`raise_fd_limit`). L'oracolo
  riceve il limite originale, perché QEMU lo passa al guest.
- **Esito di QEMU.** QEMU stampa "uncaught target signal" anche quando muore
  un processo figlio del test, cosa attesa in molti casi. Il segnale conta
  solo se il processo principale non è uscito normalmente.
- **Esclusioni.** `tools/ltp/skip.txt` resta per i test che non si possono
  confrontare, ciascuno con il motivo (per ora `fork14`, che chiede 16 TB di
  spazio virtuale).
- **Tempo virtuale.** 10 ns per istruzione (una CPU nominale da 100 MHz,
  vicina alla velocità reale dell'interprete) e 1 µs per syscall (il costo
  tipico su un kernel vero). Resta deterministico, perché dipende solo dalla
  sequenza eseguita.

## Conseguenze
- Il criterio di M2 è: tutti i test della selezione LTP, esclusi quelli di
  `skip.txt` con motivo, hanno lo stesso esito su Vetro e sull'oracolo (QEMU,
  o il nativo per quelli di `qemu-divergent.txt`) nel job linux della CI.
- Le due liste sono parte del criterio: aggiungere una voce richiede la
  verifica nativo/QEMU e il motivo, e va rivisto a ogni aggiornamento di QEMU.
- Limite dell'oracolo ripetuto: se l'oracolo è instabile su un test, Vetro
  può coincidere con un suo esito raro e passare lo stesso. Si accetta per i
  soli casi di tempi reali; un test che diverge sempre resta un fallimento.
- Le costanti di tempo sono visibili ai programmi, ad esempio nel numero di
  giri di un ciclo che dura un secondo. Cambiarle cambia le tracce, ma non la
  correttezza.
