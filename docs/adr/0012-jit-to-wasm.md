# ADR 0012 — JIT a blocchi verso WebAssembly (M4)

- Stato: accettata (M4, 2026-09-25). Fissa anche la soglia di M4 lasciata
  aperta da M3.

## Contesto
L'interprete è corretto: M1-M3 sono verdi contro QEMU. Nativo fa circa 50
MIPS, e nel browser sarà più lento. M4 chiede:
- prestazioni lavorabili;
- gli stessi test verdi col JIT;
- nessuna differenza tra interprete e JIT sul set differenziale.

Nel browser il codice generato deve essere WebAssembly, perché è l'unico
codice che una pagina può creare ed eseguire. Il JIT deve comunque girare
anche fuori dal browser, nei test e in CI.

## Decisione
- **Unità di traduzione: il blocco base.** È una sequenza di istruzioni già
  decodificate da `vetro_cpu::decode`, dentro una pagina da 4 KiB. Finisce:
  - dopo un salto, un'eccezione sincrona (SVC, BRK, HVC...) o un'istruzione
    di sistema;
  - prima della prima istruzione che il traduttore non copre (quella la
    esegue l'interprete, e si riparte dal JIT dopo);
  - dopo al massimo 64 istruzioni.

  Ogni blocco diventa una funzione WASM esportata. Più blocchi stanno in un
  modulo, per ammortizzare la compilazione.
- **Un solo ABI per tutti i motori** (`docs/specs/jit.md`):
  - lo stato della CPU sta in una struttura `JitState` in una memoria
    lineare condivisa con l'host;
  - gli accessi alla memoria del guest passano da funzioni importate
    dall'host (`ld`/`st`), che fanno MMU, permessi e bus;
  - il blocco restituisce un codice d'uscita.

  Lo stesso modulo gira in V8 (browser e Node) e in wasmtime (test nativi).
- **Motori, dietro il trait `vetro_jit::Engine`:**
  - wasmtime per i test e per `vetro --jit`: crate `vetro-jit-native`,
    l'unico con dipendenze esterne, mai nel core;
  - JavaScript `WebAssembly` per `vetro-wasm` (browser, Node).
- **Precisione.**
  - Prima di ogni accesso alla memoria il blocco salva in `JitState` il PC
    dell'istruzione e il numero di istruzioni già eseguite. Un fault a metà
    blocco lascia quindi lo stato esattamente come l'interprete, e la
    consegna dell'eccezione resta al codice che c'è già.
  - Il numero di istruzioni resta l'orologio del guest (ADR 0010 e 0011): lo
    stesso programma ha la stessa traccia di tempo con e senza JIT.
- **Invalidazione.**
  - I blocchi sono indicizzati per pagina: virtuale in modalità utente,
    fisica in modalità sistema, insieme a EL e alle opzioni di traduzione.
  - Ogni scrittura su una pagina che contiene blocchi li invalida, che venga
    dal guest (`st`) o dal kernel emulato. Se la scrittura cade nel blocco in
    corso, questo esce subito dopo.
  - In modalità sistema un blocco vale solo se la traduzione VA→PA del suo
    inizio è ancora quella con cui è stato compilato: si controlla a ogni
    ingresso, con la cache delle traduzioni recenti della MMU.
- **Parità.** Il set differenziale gira con interprete e con JIT, e i
  risultati devono essere identici. Il set comprende:
  - i programmi casuali di `vetro-diff`;
  - i test per istruzione;
  - BusyBox, LTP rapido e RISU;
  - l'avvio del kernel.

  Il JIT non ha un comportamento suo: ciò che non sa tradurre, lo esegue
  l'interprete.

## Soglia di M4
L'avvio del kernel guest con lo stesso copione di `tests/boot` (fino a
`poweroff -f`) sotto il JIT in Node (V8) deve durare al massimo quanto lo
stesso avvio con l'interprete nativo, misurato nello stesso job della CI. Oggi
l'interprete nativo impiega circa 2 s su M2 Pro.

## Conseguenze
- `vetro-cpu` espone quello che serve al traduttore (decodifica, semantica
  delle istruzioni coperte) senza duplicarlo. Il traduttore genera WASM per
  gruppi di istruzioni, e i gruppi non coperti restano all'interprete.
- `UserMemory` e la `Board` segnalano le scritture sulle pagine sorvegliate.
- La CI aggiunge i test col JIT (wasmtime) e il benchmark in Node.
