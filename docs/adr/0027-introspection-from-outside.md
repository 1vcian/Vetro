# ADR 0027 — Introspezione del guest dall'esterno: profilo del kernel, agganci nella macchina

- Stato: accettata (M7–M9, base comune, 2026-09-26). Estende l'ADR 0011
  (ciclo della macchina), 0013 e 0024 (JIT di sistema), 0019 (replay).

## Contesto
Gli hook TLS di M7 (SSL_read/SSL_write), il decoder Binder di M8 e lo
scripting di M9 hanno bisogno delle stesse cose: sapere quali processi
girano, dove sono mappate le loro librerie, che cosa chiamano (syscall,
ioctl binder) e fermarsi "invisibilmente" all'ingresso di una funzione
utente. Il guest non deve poterlo scoprire: niente agenti, moduli, BRK o
pagine modificate. Il determinismo (e il replay di M10) non deve cambiare.

## Decisione

### Profilo del kernel: simboli + BTF, senza tabelle per versione
- **Simboli**: `System.map` se c'è, altrimenti la tabella kallsyms dentro
  l'`Image`, trovata senza simboli (tabella dei token dalle cifre `0`..`9`,
  verificata dall'indice; marcatori, nomi, offset in entrambi gli ordini
  del kernel). `kallsyms_relative_base` nel file vale 0 con le
  rilocazioni RELA (`--no-apply-dynamic-relocs`, GCC/BFD): si prende
  dall'addendo della R_AARCH64_RELATIVE giusta (quella la cui base rende
  zero i posti da rilocare); con RELR (GKI) c'è già nel file.
- **Tipi**: parser BTF nostro (`vetro_analysis::introspect::btf`), dal
  blob nell'`Image` (GKI: `CONFIG_DEBUG_INFO_BTF`) o da un file staccato.
- **Kernel di prova 6.18**: niente BTF (vorrebbe `BPF_SYSCALL`, che cambia
  il kernel). `tools/guest-kernel/build.sh` compila in una cartella a parte
  `vmlinux` con la stessa `.config` più `DEBUG_KERNEL` +
  `DEBUG_INFO_DWARF5` (con `DEBUG_MISC` e `RCU_TRACE` spenti, e un
  controllo che nient'altro cambi) e ne estrae con `pahole` il BTF staccato
  `target/guest-kernel/vmlinux.btf`. L'`Image` che si avvia non cambia
  (verificato: stessi byte).
- **KASLR**: il nostro avvio Android usa `nokaslr` (e il kernel di prova
  non ha `RANDOMIZE_BASE`); comunque lo spostamento si ricava da VBAR_EL1
  meno `vectors`.
- **Traduzione**: walker delle tabelle AArch64 (4 KiB, livelli da TxSZ,
  blocchi) su `PhysMem` in sola lettura; lo spazio utente di un processo è
  quello del kernel con TTBR0 = fisico di `mm->pgd`. `vmemmap` (per la page
  cache) si calibra con la pila di un thread (`stack_vm_area->pages[0]`
  contro il fisico di `task->stack`), senza costanti di versione.

### Agganci nella macchina (`vetro_machine::hooks`)
- Un solo `Tracer` (trait `Any`) riceve `Event::SyscallEnter`,
  `SyscallExit` e `Breakpoint` con una `GuestView` in sola lettura
  (registri e RAM): non può cambiare lo stato.
- **Syscall**: l'interprete esegue sempre SVC (il JIT chiude i blocchi
  prima) ed ERET (non tradotta). Ingresso = eccezione sincrona da EL0 con
  EC 0x15; uscita = primo passaggio EL1→EL0 dello stesso thread, con chiave
  SP_EL1 (cima della pila del kernel del thread, uguale all'ingresso e al
  ritorno). Il PC di ritorno distingue il ritorno normale da execve,
  segnali, syscall da ripetere. Il processo si ricava da `__entry_task`
  per CPU (+ TPIDR_EL1). `exit`/`exit_group` si registrano all'ingresso.
- **Punti d'arresto**: prima di ogni passo dell'interprete a EL0 un filtro
  a 64 bit e una mappa per indirizzo (con filtro facoltativo sul TTBR0 del
  processo). L'evento arriva solo se l'istruzione è stata eseguita (o è una
  SVC): un'eccezione in mezzo (fault del fetch, IRQ) non lo duplica.
  Registri di prima dell'istruzione, RAM di dopo.
- **JIT**: `SysJit::set_stops(indirizzi)` (aggancio minimo nel JIT): le
  regioni non contengono quegli indirizzi (la scoperta si ferma prima, una
  regione non comincia lì), così li esegue sempre l'interprete. Cambiare
  l'insieme dimentica blocchi e voci della cache dei salti (nuova epoca),
  senza azzerare il motore. Prova di parità dedicata.
- Il costo con gli agganci accesi è solo nei passi che cambiano EL o hanno
  un punto al PC; niente negli snapshot né nei log.

### Dipendenze
`vetro-machine` dipende da `vetro-analysis` (che resta senza dipendenze e
compila in wasm): la macchina dà la `GuestView` come `PhysMem` e ospita il
tracciatore pronto (`introspect::SyscallTracer`) e `Machine::linux`.

## Alternative scartate
- **BRK nel codice del guest**: il guest lo vedrebbe (checksum, lettura del
  proprio codice) e cambierebbe la RAM, quindi il replay.
- **Tabelle di offset scritte a mano per versione**: si rompono a ogni
  kernel; il BTF c'è nel GKI e per il kernel di prova costa una build.
- **Ricostruire il kernel di prova con BTF**: cambierebbe `Image`, log e
  confronti con QEMU.
- **Uscita della syscall al ritorno a `svc_pc + 4`**: perde execve e
  segnali; la chiave SP_EL1 li copre.

## Verifica
- `vetro-analysis`: test unitari di BTF (unioni anonime, byte arbitrari),
  kallsyms (tabella finta, RELA), walker, ELF (file e memoria con
  GNU_HASH), binder, strace.
- `vetro-jit-native`, `sys_parity::punti_di_fermata_restano_all_interprete`:
  150 programmi con tre fermate a caso, identici all'interprete e con ogni
  passo sulle fermate fatto dall'interprete (fallisce senza il controllo).
- `tests/boot/tests/introspect.rs` (kernel di prova, release):
  `ps` = processi dalla memoria (29); `/proc/<pid>/maps` = testo
  ricostruito; `/proc/<pid>/fd`; cmdline; simboli di vetro-dev dalla page
  cache = file sull'host (e il file intero); syscall di `cat` come utente
  501 = `qemu-aarch64 -strace` (16, in ordine); punti d'arresto su
  `open`/`ioctl` = syscall `openat`/`ioctl` del processo con gli stessi
  argomenti; ingresso di BusyBox una volta per exec; stessa console e
  stesso stato finale senza agganci; replay col JIT e con gli agganci:
  stesse syscall e stessi punti d'arresto, replay identico.
  `profili_dei_kernel`: kallsyms del 6.18 = System.map (18834 simboli);
  GKI 6.6 da `boot.img`: 107807 simboli, BTF con binder_proc,
  binder_transaction.

## Conseguenze
- Hook TLS (M7), decoder Binder (M8), scripting (M9) si scrivono come
  `Tracer` o sopra `SyscallTracer`/`Linux`.
- La prova lunga su Android (processi con system_server/zygote, prima
  transazione binder) non è ancora fatta: vedi `docs/progress/M7.md`.
