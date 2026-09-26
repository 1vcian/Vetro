# ADR 0009 — Modalità sistema della CPU: stato EL1 nella CPU, MMU e piattaforma dietro trait

- Stato: accettata (M3, 2026-09-25)

## Contesto
In M3 la CPU deve eseguire il kernel: EL0 ed EL1, eccezioni con i vettori
di VBAR_EL1, registri di sistema, MMU stage 1, interrupt dal GIC e timer
generico. Tre vincoli:

1. la modalità utente di M1/M2 (`Cpu::step` con `UserMemory`, eccezioni
   restituite al livello Linux) deve restare identica e senza costi;
2. `vetro-mmu` dipende già da `vetro-cpu` (`Access`, `MemFault`,
   `Memory`): la CPU non può dipendere dalla MMU;
3. la piattaforma (GIC, timer, PSCI) sta in `vetro-platform`, che la CPU non
   deve conoscere; il tempo deve entrare da un solo punto registrabile.

## Decisione
- **Due modalità, un interprete.** `Cpu` ha un campo `sys: SysState`
  (PSTATE oltre NZCV, SP per livello, registri EL1). `Default` è la modalità
  utente: `Cpu::step` non lo guarda. `Cpu::reset_system(SysConfig)` porta la
  CPU nello stato di reset della Cortex-A53 senza EL2/EL3;
  `Cpu::step_system(&mut impl SysBus, &mut impl CpuEnv) -> SysEvent` esegue
  un passo in modalità sistema riusando lo stesso `execute` dell'interprete.
- **Decoder unico e indipendente dal livello.** `decode` riconosce anche
  ERET, HVC, SMC, WFI/WFE, MSR immediato, SYS (TLBI, AT, cache), LDTR/STTR
  (`unpriv`) e tutti i registri di sistema modellati. In modalità utente
  queste istruzioni danno esattamente l'esito di prima (Undefined, NOP o
  `Unimplemented`); il generatore casuale di `tests/diff` non le produce.
- **Memoria: trait `SysBus` definito in `vetro-cpu`, implementato da
  `vetro-mmu` (`MmuBus`).** La CPU è l'unica proprietaria di SCTLR, TCR,
  TTBR0/1 e MAIR e li passa a ogni traduzione (`TranslationRegs`); la MMU
  traduce (con TLB) e fa gli accessi fisici. La CPU divide gli accessi per
  pagina, controlla SCTLR.A e costruisce ESR/FAR. `TlbiOp` passa in
  `vetro-cpu` e `vetro-mmu` lo riesporta.
- **Piattaforma: trait `CpuEnv`.** Linee IRQ/FIQ (a livello, lette prima di
  ogni istruzione) e MRS/MSR dei registri che non stanno nella CPU (`EnvReg`:
  CNT* del timer generico e ICC_* del GICv3). La CPU fa i controlli di
  accesso (EL0, CNTKCTL, sola lettura/scrittura), la piattaforma la
  semantica. Il contatore arriva solo da qui.
- **Eventi al chiamante.** WFI (`WaitForInterrupt`), HVC/SMC del conduit
  PSCI configurato (`Hvc`, `Smc`, con PC già avanzato) e i limiti di Vetro
  (`Unimplemented`, stato invariato). Tutto il resto (eccezioni sincrone,
  IRQ, FIQ, SError) si consegna al guest.
- **Oracolo della modalità sistema.** Valori ID, maschere e sindromi si
  verificano con una sonda bare-metal (`tests/isa/system/probe.S`) eseguita
  su `qemu-system-aarch64 -M virt,gic-version=3 -cpu cortex-a53`; l'uscita
  registrata è versionata e il test la confronta con Vetro riga per riga, così
  la CI non ha bisogno di `qemu-system`.
- **Architettura prima di QEMU quando divergono** in casi che il kernel
  Linux non osserva: fault di allineamento su memoria Device con MMU accesa e
  per DC ZVA su Device, controllo SCTLR.SA/SA0, sindrome della trap di
  MSR DAIFSet. Ogni differenza è scritta in `docs/specs/cpu.md` e coperta da
  un test.
- **ID_AA64PFR0_EL1** dichiara EL0/EL1 solo AArch64 (QEMU: anche AArch32),
  perché Vetro non implementa AArch32 (ADR 0005).

## Conseguenze
- La piattaforma costruisce per ogni passo (o blocco di passi) un `MmuBus`
  sopra la propria memoria fisica e implementa `CpuEnv`; gestisce PSCI e WFI.
- Il JIT (M4) potrà riusare lo stesso `SysState` e gli stessi trait.
- Aggiungere un registro di sistema: una riga in `SysReg::lookup`, i
  controlli in `sysreg_access`, la semantica in `sysreg_read/write`, una
  riga nella sonda.
