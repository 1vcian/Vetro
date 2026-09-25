# ADR 0025 — CI selettiva in base ai file cambiati

Stato: accettato (2026-09-26)

## Contesto
Il giro completo della CI dura circa 50 minuti: LTP completo due volte
(interprete e JIT) sul runner arm64, avvio del kernel sotto QEMU e sotto
Vetro, kselftest (circa 20 minuti da soli), test web e sito. Una modifica a
un file JavaScript o a un documento non tocca niente di quello che LTP o i
kselftest verificano, ma li rifaceva tutti; anche la pubblicazione del sito
aspettava quei 50 minuti.

## Decisione
Un job `changes` (tools/ci-changes.sh) confronta il push con il commit
precedente (o la PR con la sua base) e decide quali job servono:
- `native`, `wasm`: codice Rust, test Rust, toolchain;
- `linux`: CPU, MMU, JIT, vetro-cli (syscall Linux), test linux/diff/isa,
  LTP, RISU, binari guest, oracolo;
- `boot`: qualunque crate, tests/boot, kernel guest e i suoi strumenti;
  i kselftest solo se cambia il kernel guest (configurazione, initramfs,
  kselftest, script di costruzione);
- job `boot` ridotto (kernel dalla cache, WebAssembly in Node, test web) se
  cambia solo la parte web;
- `pages`/`deploy`: ciò che finisce nel sito; si pubblica se nessun job è
  fallito (quelli saltati non bloccano).
Tutto gira comunque: ogni notte (schedule), a mano (workflow_dispatch), sul
primo push di un branch, se cambia la CI stessa, o con `[ci full]` nel
messaggio di commit.

## Conseguenze
- La regola d'oro resta: una milestone si chiude su un giro completo verde
  (quello notturno o uno con `[ci full]`), non su uno parziale.
- Il rischio è una dipendenza non prevista fra aree: il giro notturno lo
  scopre entro un giorno. Le categorie sono larghe apposta (per esempio ogni
  crate fa scattare `boot`).
