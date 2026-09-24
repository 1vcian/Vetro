# Vetro — regole per tutti gli agenti

Vetro è un emulatore di sistema ARM64 completo, in Rust, che gira in WebAssembly
nel browser e fa girare Android (AOSP + microG) con strumenti di analisi
dall'esterno. Il piano, le milestone e lo stato stanno in `docs/PLAN.md`; il
diario di lavoro in `docs/progress/`. Leggili a inizio sessione.

"Vetro" è il nome ufficiale. Il prodotto non si presenta mai come "Android" (marchio Google).

## Principi

- **Fedeltà prima della velocità.** Prima corretto e verificato, poi veloce.
- **L'oracolo è QEMU.** Ogni comportamento della CPU si confronta con
  `qemu-aarch64` / `qemu-system-aarch64`.
- **Determinismo dal primo giorno.** Orologio, casualità, input e tempi dei
  dispositivi passano da un unico punto registrabile (serve al replay di M10).

## Regola d'oro

Nessuna milestone è completa finché il suo comando di uscita non passa in CI.
Non dichiarare fatto ciò che un test non conferma. Un test saltato (`SKIP`) non
è un test passato. Se un test è rosso, si lavora finché diventa verde o finché
se ne capisce il motivo, scritto in `docs/progress/`.

## Comandi

```sh
tools/ci.sh                          # tutti i controlli della CI in locale
cargo test --workspace               # test nativi
cargo test -p vetro-diff             # oracolo QEMU e programmi casuali (ADR 0006)
cargo test -p vetro-isa-tests        # test per istruzione (anche contro QEMU)
VETRO_DIFF_SEED=<seme> VETRO_DIFF_CASES=1 cargo test -p vetro-diff --test random   # riproduce un caso
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
cargo build --target wasm32-unknown-unknown --workspace --exclude vetro-cli --exclude vetro-diff --exclude vetro-isa-tests --exclude vetro-linux-tests
```

Oracolo su macOS (QEMU user mode esiste solo su Linux; serve Docker attivo):

```sh
export VETRO_QEMU_AARCH64="$PWD/tools/oracle/qemu-aarch64-docker.sh"
```

Codifiche per i test: `printf 'add x0, x1, #1\n' | tools/a64asm.sh` (assembler
vero, rifiuta le rilocazioni). Mai scrivere codifiche a mano nei test.

`VETRO_REQUIRE_ORACLE=1` trasforma lo skip dell'oracolo in un fallimento
(attivo in CI). Usalo anche in locale prima di dichiarare chiuso un lavoro.

## Toolchain

Rust nightly fissato in `rust-toolchain.toml` (i thread WASM richiedono
build-std, vedi `docs/adr/0002`). Non cambiarlo senza un ADR.

## Regole non negoziabili

- Ogni correzione di bug entra con un test che fallirebbe senza la correzione.
- Ogni istruzione o syscall nuova arriva con il suo caso mirato o differenziale.
- Una singola differenza con QEMU nel set differenziale blocca il rilascio.
- Le prestazioni si misurano, non si stimano.
- **Mai ricompilare AOSP in CI.** Le immagini si costruiscono sulla macchina
  Linux dedicata e si caricano come artefatti versionati.

## Confini e collaborazione

- Ogni area ha un proprietario (vedi `.claude/agents/` e `docs/PLAN.md`,
  sezione "Squadra di agenti"). Un agente scrive solo nelle sue cartelle.
- Le interfacce tra crate stanno in `docs/specs/`. Cambiarle è una decisione
  architetturale: prima l'ADR in `docs/adr/`, poi il codice.
- Le decisioni non ovvie diventano un ADR numerato, così non si rimettono in
  discussione.
- Branch `area/descrizione` (es. `cpu/decoder-simd`). Una PR per unità di
  lavoro, sempre con il test che dimostra il criterio.
- Commit che citano la milestone, es. `M1: decoder ADD/SUB immediato`.

## Fine sessione

Aggiungi tre righe in `docs/progress/Mx.md`: fatto, manca, bloccato.

## Licenze

Codice nostro: PolyForm Noncommercial 1.0.0 (`LICENSE.md`, vedi ADR 0004):
niente uso commerciale. Il kernel Linux è GPL-2.0: per ogni immagine
distribuita si pubblicano anche i sorgenti del kernel usato.
