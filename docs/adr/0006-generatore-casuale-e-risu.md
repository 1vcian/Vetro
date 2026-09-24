# ADR 0006 — Programmi casuali per M1 generati in Rust; RISU da M2

- Stato: accettata (M1, 2026-09-24). Modifica il criterio di uscita di M1.

## Contesto
Il piano chiede, per M1, 200 programmi casuali generati da RISU con registri
finali uguali a `qemu-aarch64`. RISU però non è solo un generatore: il suo
harness (`risu`) è un programma C che esegue il blocco di test e confronta
lo stato dentro un gestore di SIGILL con `ucontext`. Farlo girare come
"apprendista" dentro Vetro richiede segnali, syscall e un binario Linux
completo: è lavoro di M2.

## Decisione
- In M1 i programmi casuali li genera `vetro-diff::random`, con lo stesso
  principio di RISU: bit casuali dentro le maschere delle classi di
  istruzioni intere (non a partire dal nostro decoder), campi corretti solo
  dove serve (registri riservati, indirizzi validi, salti in avanti), casi
  CONSTRAINED UNPREDICTABLE esclusi.
- Ogni programma: prologo che imposta registri e flag casuali, corpo
  casuale, epilogo che scrive su stdout x0–x30, SP, NZCV e il blocco di
  memoria. L'uscita di Vetro e quella di QEMU si confrontano byte per byte.
- In una parte dei programmi il corpo termina con un'istruzione che il
  nostro decoder dichiara UNDEFINED: QEMU deve morire di SIGILL nello stesso
  punto. Così anche gli errori del decoder in senso "troppo restrittivo"
  emergono.
- Semi deterministici e riproducibili (`VETRO_DIFF_SEED`), numero di casi
  configurabile (`VETRO_DIFF_CASES`, default 250; di notte molti di più).
- RISU vero entra in M2, quando Vetro esegue binari Linux con segnali.

## Nuovo criterio di uscita di M1
Suite per istruzione verde (valori attesi verificati anche contro QEMU) e
almeno 200 programmi casuali di `vetro-diff::random` identici a
`qemu-aarch64 -cpu cortex-a53`, con `VETRO_REQUIRE_ORACLE=1`.
