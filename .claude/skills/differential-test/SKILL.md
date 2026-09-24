---
name: differential-test
description: Come eseguire e scrivere un test differenziale di Vetro contro qemu-aarch64 (oracolo QEMU user mode), su Linux o su macOS via Docker.
---

# Test differenziale contro QEMU

## Eseguire
1. Oracolo disponibile?
   - Linux: `command -v qemu-aarch64` (altrimenti `apt-get install qemu-user`).
   - macOS: Docker attivo, poi
     `export VETRO_QEMU_AARCH64="$PWD/tools/oracle/qemu-aarch64-docker.sh"`.
2. `VETRO_REQUIRE_ORACLE=1 cargo test -p vetro-diff`
   Senza la variabile un oracolo assente produce `SKIP` e il test passa: non
   vale come verifica.

## Scrivere un caso
- Genera il programma con `vetro_diff::a64` (codifiche) ed
  `vetro_diff::elf::build(code, data)`. Per indirizzi di dati usa
  `elf::data_addr(n_istruzioni, offset)` con `a64::mov64`.
- Salva con `qemu::write_temp_elf(nome, &img)` ed esegui con
  `qemu::run(&q, &path, qemu::DEFAULT_TIMEOUT)`.
- Inizia sempre con `let Some(q) = qemu::locate_or_skip("nome") else { return };`.
- Confronta con il nostro interprete (da M1) i registri finali: in user mode
  il modo più semplice è far terminare il programma con un `write` dei
  registri su stdout, poi `exit`.
- Alla prima differenza: isola l'istruzione, scrivi un test mirato in
  `tests/isa`, correggi, e lascia il caso tra quelli permanenti.
