# Spec — vetro-cpu

## Perimetro
AArch64, ARMv8.0-A con le estensioni della Cortex-A53 (ADR 0005). Due
modalità (ADR 0009):
- **utente** (M1/M2): sempre EL0, eccezioni restituite al chiamante (il
  livello Linux di `vetro-cli`), memoria `Memory` senza MMU;
- **sistema** (M3): EL0 ed EL1 (niente EL2/EL3 né AArch32), MMU stage 1
  tramite `vetro-mmu`, eccezioni consegnate al guest attraverso VBAR_EL1,
  interrupt e registri della piattaforma tramite `CpuEnv`.

## Interfaccia pubblica
- `Cpu`: stato architetturale. `x[0..31]`, `sp` (stack pointer in uso:
  SP_EL0 in modalità utente), `pc`, `nzcv` (bit 31:28), `tpidr_el0`,
  `tpidrro_el0`, monitor esclusivo locale, `v`, `fpcr`, `fpsr`, `sys`
  (`SysState`: modalità, configurazione, PSTATE.{EL,SP,DAIF,IL}, SP non in
  uso, registri EL1, SError in attesa).
- `decode(u32) -> Insn`: funzione pura, indipendente dal livello di
  eccezione. `Insn` è la forma decodificata che userà anche il JIT (M4).
- `Cpu::step(&mut self, &mut impl Memory) -> Result<(), Exception>`
  (modalità utente): esegue un'istruzione. In caso di eccezione lo stato
  non cambia e `pc` punta all'istruzione colpevole, tranne per `Svc`, dove
  `pc` punta già all'istruzione successiva (come ELR per SVC).
- `trait Memory`: `read`, `write`, `fetch` su indirizzi virtuali, con errore
  `MemFault`; metodi di default `read_unpriv`/`write_unpriv` (LDTR/STTR) e
  `zero_block` (DC ZVA) che in modalità utente coincidono con `read`/`write`.
  Lo implementa `UserMemory` (regioni con permessi).

### Modalità sistema
- `Cpu::reset_system(SysConfig)`: stato di reset della Cortex-A53 senza
  EL2/EL3, come `qemu-system-aarch64 -M virt`: EL1h, DAIF = 1111, MMU
  spenta, SCTLR_EL1 = 0x00c50838, CPACR_EL1 = 0 (FP/SIMD trappati), OS lock
  attivo. PC e registri generali li imposta il caricatore.
- `SysConfig { psci: PsciConduit (Hvc | Smc | None), mpidr (default
  0x8000_0000), gicv3 (default vero) }`.
- `Cpu::step_system(&mut impl SysBus, &mut impl CpuEnv) -> SysEvent`:
  - `Executed`: un'istruzione eseguita;
  - `Exception { kind: Sync | Irq | Fiq | SError, esr, from_el }`: eccezione
    presa, PC al vettore, nessuna istruzione eseguita (informativo, per
    tracce e statistiche);
  - `WaitForInterrupt`: WFI eseguita, PC dopo la WFI;
  - `Hvc(imm)` / `Smc(imm)`: chiamata al conduit PSCI configurato, PC dopo
    l'istruzione, argomenti in x0-x7, risultato da scrivere in x0 (per una
    funzione sconosciuta QEMU risponde NOT_SUPPORTED = -1);
  - `Unimplemented { raw, what }`: limite di Vetro, stato invariato.
- `Cpu::take_exception(kind, esr, far, preferred)`: ingresso in
  un'eccezione a EL1, anche per la piattaforma (es. consegnare un
  Undefined dopo una HVC che non vuole gestire).
- `Cpu::sp_el(n)`, `set_sp_el(n, v)`, `pstate_spsr()`.
- `trait SysBus` (lo implementa `vetro_mmu::MmuBus`):
  `translate(&TranslationRegs, va, AccessReq { access, el, aligned })`
  → PA o `BusFault::{Abort { fsc, ea }, Unimplemented}`; `read_phys`,
  `write_phys` (un pezzo dentro una pagina); `at(...) -> AtResult`;
  `tlbi(TlbiOp, xt)`; `tlb_flush_all()`.
- `trait CpuEnv` (lo implementa la piattaforma): `irq_line()`, `fiq_line()`
  (default falso), `read_sysreg(EnvReg)`, `write_sysreg(EnvReg, v)`.
  `EnvReg`: CNTFRQ/CNTPCT/CNTVCT, CNTP_{CTL,CVAL,TVAL}, CNTV_{CTL,CVAL,TVAL},
  ICC_{PMR,IAR0/1,EOIR0/1,HPPIR0/1,BPR0/1,AP0R0,AP1R0,DIR,RPR,SGI0R,SGI1R,
  ASGI1R,CTLR,SRE,IGRPEN0/1}_EL1. La CPU controlla l'accesso (livello,
  CNTKCTL_EL1, sola lettura/scrittura); la piattaforma dà la semantica e il
  tempo, così il contatore resta deterministico.
- `sysreg::SysReg::lookup(op0, op1, crn, crm, op2)`: catalogo dei registri
  modellati.

## Eccezioni in modalità utente
| `Exception` | Segnale Linux in user mode |
|---|---|
| `Svc(imm)` | syscall |
| `Breakpoint(imm)` | SIGTRAP |
| `Undefined(raw)` | SIGILL |
| `Unimplemented { raw, what }` | SIGILL, ma è un nostro limite: va segnalato |
| `DataAbort { addr, write }`, `InstructionAbort { addr }` | SIGSEGV |
| `Alignment { addr }`, `PcAlignment { addr }` | SIGBUS |
| `SpAlignment` | mai in modalità utente |

In modalità utente le istruzioni di sistema restano come in M1: ERET, HVC,
SMC, MSR immediato, SYS di EL1 e MSR TPIDRRO_EL0 → `Undefined`; WFI e WFE →
NOP; MRS/MSR del canale di debug di EL0 (MDCCSR_EL0, DBGDTR*_EL0) →
`Undefined`, come QEMU user che (come Linux) accende MDSCR_EL1.TDCC; MRS/MSR
di registri diversi da NZCV, TPIDR_EL0, TPIDRRO_EL0, FPCR, FPSR, DCZID_EL0,
CTR_EL0 → `Unimplemented`. LDTR/STTR accedono come LDR/STR.

## Modalità sistema: comportamento
- **Ordine di un passo**: FIQ, IRQ, SError non mascherati (in quest'ordine,
  ELR = PC); PC disallineato (EC 0x22, FAR = PC); fetch tradotto
  (Instruction Abort, EC 0x20/0x21); PSTATE.IL (EC 0x0E); trap FP/SIMD da
  CPACR_EL1.FPEN (EC 0x07, ISS 0x1e00000); istruzione.
- **Ingresso** (`AArch64.TakeException`): SPSR_EL1 = PSTATE (NZCV, IL,
  DAIF, M), ELR_EL1, ESR_EL1 per le sincrone e SError, FAR_EL1 per abort e
  PC disallineato (altrimenti invariato, come QEMU), EL1h, DAIF = 1111,
  IL = 0. Vettore = VBAR_EL1 + {0x000 EL1t, 0x200 EL1h, 0x400 da EL0} +
  {0x000 sync, 0x080 IRQ, 0x100 FIQ, 0x180 SError}.
- **ERET**: SPSR con M ∈ {EL0t, EL1t, EL1h} e AArch64; altrimenti ritorno
  illegale (EL/SP invariati, IL = 1, PC = ELR). NZCV e DAIF sempre da SPSR.
  Il monitor esclusivo si azzera; a ELR si applica il TBI del nuovo livello.
- **Salti**: il bersaglio passa da `AArch64.BranchAddr` (TBI da TCR_EL1).
- **ESR** (verificate con QEMU): Unknown 0x02000000; SVC/HVC/SMC/BRK con
  imm16; WFI da EL0 con nTWI = 0: 0x07e00000; trap MSR/MRS/SYS (EC 0x18) con
  ISS dalla codifica; abort con EA (1 = slave error), CM (AT), WnR, FSC;
  allineamento FSC 0x21; SP disallineato EC 0x26.
- **Accessi da EL0**: CTR_EL0 (UCT), DC ZVA (DZE; DCZID.DZP lo riflette),
  DAIF e MSR DAIFSet/Clr (UMA), DC CVAU/CVAC/CIVAC e IC IVAU (UCI) → trap
  EC 0x18; CNT* secondo CNTKCTL_EL1 (EL0PCTEN, EL0VCTEN, EL0PTEN, EL0VTEN;
  CNTFRQ leggibile con uno dei primi due) → trap; TPIDRRO_EL0 e
  PMUSERENR_EL0 sola lettura; tutto il resto di EL1 → UNDEFINED.
- **A EL1**: scrittura di un registro a sola lettura o lettura di uno a sola
  scrittura → UNDEFINED; MRS/MSR SP_EL0 con SPSel = 0 → UNDEFINED; HVC/SMC
  fuori dal conduit PSCI → UNDEFINED (niente EL2/EL3).
- **Registri** (valori e maschere di QEMU `-cpu cortex-a53`, vedi
  `sys/id.rs`): MIDR 0x410fd034, REVIDR 0x100, MPIDR da `SysConfig`, CTR
  0x84448004, CLIDR 0x0a200023, CCSIDR per CSSELR 0/1/2, spazio degli ID
  (CRm 1..7) con i valori di QEMU e zero per le codifiche riservate.
  SCTLR_EL1 (bit MTE azzerati; scrittura svuota il TLB), TCR_EL1 (svuota
  il TLB), TTBR0/1, MAIR, CONTEXTIDR, CPACR, TPIDR_EL1, PAR, CNTKCTL, ESR,
  FAR, ELR, SPSR: tutti i 64 bit. VBAR_EL1: azzerati solo i bit [4:0].
  CSSELR: 4 bit. ACTLR, AMAIR, AFSR0/1, MDCCINT: RAZ/WI. IMPLEMENTATION
  DEFINED della A53 (L2CTLR, L2ECTLR, L2ACTLR, CPUACTLR, CPUECTLR,
  CPUMERRSR, L2MERRSR): RAZ/WI; CBAR_EL1 = `SysConfig::cbar` (0x0800_0000,
  il GICD della virt), sola lettura. Codifiche MRS/MSR che la A53 non ha
  (estensioni successive come FPMR, ZCR, SMCR; codifiche libere) →
  UNDEFINED, come QEMU; la PMU, che la A53 ha, → `Unimplemented` finché
  Vetro non la modella. Debug (op0 = 2, come `debug_cp_reginfo` di QEMU,
  verificati dalla sonda): MDSCR, OSLAR/OSLSR (OSLK al reset), OSDLR
  (1 bit), DBGBVR/BCR 0..5, DBGWVR 0..3 (bit [1:0] a zero), DBGWCR 0..3,
  MDRAR = 0: solo memoria, nessuna eccezione di debug. OSDTRRX_EL1,
  OSDTRTX_EL1, OSECCR_EL1, MDCCINT_EL1: RAZ/WI. Canale di debug di EL0:
  MDCCSR_EL0 (sola lettura, 0), DBGDTR_EL0 e DBGDTRRX/TX_EL0 (RAZ/WI);
  da EL0 accessibili con MDSCR_EL1.TDCC = 0, in trap (EC 0x18) con
  TDCC = 1; a EL1 TDCC non conta. DBGCLAIMSET_EL1 legge 0xff e accende i
  bit [7:0] scritti; DBGCLAIMCLR_EL1 legge i bit di CLAIM e spegne quelli
  scritti. DBGPRCR_EL1, DBGAUTHSTATUS_EL1, DBGVCR32_EL2, breakpoint e
  watchpoint oltre il numero della A53 e il resto di op0 = 2: UNDEFINED,
  come QEMU.
- **Memoria**: gli accessi passano da `SysBus` con il privilegio del livello
  corrente (0 per LDTR/STTR). Accessi a cavallo di pagina: prima si
  traducono tutte le pagine. SCTLR.A → fault di allineamento prima della
  traduzione; accesso ai dati disallineato su memoria Device (anche a MMU
  spenta, dove i dati sono Device) e DC ZVA su Device → fault di
  allineamento dopo il walk e prima dei permessi. SCTLR.EE/E0E = 1 →
  `Unimplemented`.
- **AT S1E{0,1}{R,W}**: walk senza TLB, risultato in PAR_EL1; un abort
  esterno sul walk si prende come Data Abort (CM = 1, WnR = 1), come QEMU.
- **TLBI**: le 12 operazioni EL1 (anche IS) vanno a `SysBus::tlbi`.
- **Cache**: IC IALLU/IALLUIS, IC IVAU, DC IVAC/CVAC/CVAU/CIVAC e
  set/way non fanno nulla (come QEMU); DC ZVA azzera.
- **WFE**: sempre NOP, mai trappata (come QEMU, che non attende).

## Differenze note rispetto a QEMU (scelta: l'architettura)
- Accesso disallineato a memoria Device con MMU accesa e DC ZVA su Device:
  Vetro dà il fault di allineamento, QEMU 10.0 no (a MMU spenta entrambi sì).
- SCTLR_EL1.SA/SA0: Vetro controlla l'allineamento dello SP (EC 0x26), QEMU
  no. Linux imposta SA0: un programma utente con SP disallineato riceve
  SIGBUS come su hardware.
- Trap di MSR DAIFSet/DAIFClr da EL0: ISS con Op1 = 3, Op2 = 6/7; QEMU 10
  li scambia.
- ID_AA64PFR0_EL1: EL0/EL1 = 1 (solo AArch64); QEMU dichiara anche AArch32.
- STXR disallineata: Vetro dà sempre il fault di allineamento; QEMU solo se
  il monitor corrisponde.

## Limiti noti della modalità sistema
- Niente eccezioni di debug (breakpoint e watchpoint hardware, single step
  di MDSCR.SS, PSTATE.SS/D), niente PMU (solo PMUSERENR_EL0), niente trace.
- DC CVAC/CVAU/CIVAC/IVAC e IC IVAU non traducono l'indirizzo (come QEMU):
  nessun abort su un indirizzo non mappato.
- Il TBI si applica ai salti di registro e a ERET solo in modalità sistema;
  in modalità utente il PC conserva il tag (come in M1).
- Una sola CPU; le TLBI IS vanno replicate dalla piattaforma se ce ne
  saranno di più. Big-endian non supportato.

## Invarianti
- Nessuna dipendenza da `std::fs`, `std::process`, thread: compila in
  `wasm32-unknown-unknown`.
- Comportamenti CONSTRAINED UNPREDICTABLE: si replica la scelta di QEMU
  quando è economico, e i generatori casuali li escludono.
- Esclusive: il monitor ricorda indirizzo, dimensione e valore letto; STXR
  riesce se indirizzo e dimensione coincidono e la memoria contiene ancora
  quel valore (stesso modello di QEMU user mode). ERET azzera il monitor.
- La modalità utente non legge né scrive `sys` (test
  `sys::tests::modalita_utente_invariata`).

## Test
- `tests/isa`: casi per istruzione con valori attesi, verificati anche
  contro QEMU quando l'oracolo è presente; `tests/system_probe.rs` esegue
  la sonda bare-metal `system/probe.S` e confronta l'uscita con quella di
  `qemu-system-aarch64` registrata in `system/probe.expected`
  (rigenerabile con `system/build.sh`, serve Docker).
- `tests/diff`: programmi casuali confrontati con QEMU (ADR 0006).
- `cargo test -p vetro-cpu`: `sys::tests` (eccezioni, ESR, trap da EL0,
  FP, abort, allineamento, IRQ/FIQ/SError, AT/TLBI, registri) su un bus di
  prova.
- `cargo test -p vetro-mmu --test system`: programma bare-metal con tabelle
  vere (VBAR, MMU, AT, LDTR, SVC da EL0), TLB e TLBI, Instruction Abort,
  Device.

## Snapshot (M6, ADR 0015)

`Cpu`, `SysState` e `SysConfig` implementano `vetro_snapshot::Snapshot`
(`src/snapshot.rs`): tutti i campi, monitor esclusivo e SError in attesa
compresi. La CPU non ha stato nascosto.
