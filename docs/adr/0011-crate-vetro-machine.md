# ADR 0011 — `vetro-machine`: la macchina completa in un crate a sé

- Stato: accettata (M3, 2026-09-25).

## Contesto
In M3 i pezzi della modalità sistema sono pronti e provati ciascuno per
conto suo:
- la CPU a EL0/EL1 con `SysBus` e `CpuEnv` (ADR 0009);
- la MMU con `MmuBus` sopra una `PhysMemory`;
- la piattaforma virt (GICv3, timer, PL011, PL031, virtio-mmio, device tree);
- il caricatore del kernel.

Manca chi li mette insieme. `vetro-platform` non dipende dalla CPU, e deve
restare così, perché i suoi dispositivi si provano senza CPU. Il caricatore
stava in `vetro-cli`, che è nativo. Il browser (M5) avrà bisogno della stessa
macchina.

## Decisione
- Nuovo crate `vetro-machine`, compilabile per wasm32 e senza dipendenze
  esterne. Dipende da `vetro-cpu`, `vetro-mmu` e `vetro-platform`, e contiene:
  - `boot`: il caricatore, spostato da `vetro-cli`, che lo ri-esporta;
  - `Board`: RAM, piattaforma e contatore. Implementa `PhysMemory` (prima la
    RAM, poi il bus MMIO a 1/2/4/8 byte; nessuna risposta = decode error) e
    `CpuEnv` (timer generico, ICC_* del GIC con il solo gruppo 1, linea IRQ);
  - `Machine`: CPU, MMU e `Board`, con `load_linux` (come
    `-kernel/-initrd/-append` di QEMU) e `run(budget)`, che si ferma per
    quanto esaurito, PowerOff, Reset, Idle o Unimplemented;
  - PSCI 1.1 via HVC, con le risposte di `target/arm/psci.c` per una CPU.
- **Tempo deterministico.** Il tempo è il numero di istruzioni: CNTPCT
  avanza di 5 ogni 8 istruzioni, cioè 62,5 MHz su 100 MHz nominali, lo
  stesso passo del livello user mode (ADR 0010). Una WFI senza interrupt
  pronti salta alla prossima scadenza del timer; senza scadenze la macchina
  è `Idle` e aspetta un ingresso.
- **Linee di interrupt.** Si aggiornano solo quando qualcosa può averle
  cambiate: un accesso MMIO, un registro del timer, l'ingresso della console
  o il superamento della prossima scadenza del timer (in cache). Non a ogni
  istruzione.
- **Device tree come QEMU virt**, per la parte che conta per Linux:
  - `rng-seed` e `kaslr-seed` in `/chosen`, derivati da `MachineConfig::seed`;
  - dimensione totale di 1 MiB (QEMU non compatta il suo DTB, e Linux lo
    riserva tutto);
  - nessun `model`.
- **Oracolo di M3:** `qemu-system-aarch64 -M virt,gic-version=3,its=off
  -cpu cortex-a53 -m 1G`, cioè la stessa piattaforma di Vetro (GICv3 senza
  ITS). Il log di riferimento è versionato in
  `guest/kernel/reference/qemu-boot.log`.

## Differenze note con l'oracolo
Sono elencate in `tests/boot/src/lib.rs` (`KNOWN_DIFFERENCES`,
`MEMORY_LINES`):
- il GICv3 di QEMU dichiara gli LPI anche senza ITS, e Linux stampa "ITS: No
  ITS available";
- Vetro non esegue AArch32 (ADR 0005), quindi manca la riga "32-bit EL0
  Support";
- il DTB di QEMU descrive dispositivi in più (PCIe, fw-cfg, flash, GPIO,
  PMU): nelle righe di memoria cambiano pochi KiB, e si confrontano senza
  numeri;
- l'ordine di qualche initcall asincrono dipende dai tempi reali di QEMU:
  il confronto è per insieme di righe.

## Conseguenze
- `vetro-cli boot` e il test `tests/boot/tests/vetro.rs` usano
  `vetro-machine`; lo userà anche il browser.
- Una CPU sola. SMP (CPU_ON, redistributori multipli) è un'estensione di
  `Machine`, da fare quando servirà ad Android.
