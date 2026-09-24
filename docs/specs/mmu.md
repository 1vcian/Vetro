# Spec — vetro-mmu

## Perimetro
Traduzione stage 1 AArch64 del regime EL1&0 (EL0 ed EL1), livello ARMv8.0
con i parametri della Cortex-A53 (ADR 0005). Riferimento: Arm ARM, D8
(VMSAv8-64) e lo pseudocodice `AArch64.TranslationTableWalk`,
`AArch64.TranslateAddressS1Off`, `AArch64.CheckPermission`: l'ordine dei
controlli è quello dello pseudocodice, quindi la priorità fra fault diversi è
architetturale. Niente stage 2, EL2, EL3, AArch32.

Dipendenze ammesse: `vetro-cpu` (solo `Access`, `MemFault`, `Memory`), nessun
crate esterno. Compila in `wasm32-unknown-unknown`.

## Interfaccia pubblica
- `MmuRegs { sctlr, tcr, ttbr0, ttbr1, mair }`: i registri `_EL1`, scritti dal
  sistema (MSR). Moduli `sctlr` e `tcr` con i bit usati.
- `trait PhysMemory { read, write, read_u64 }`: memoria fisica little-endian;
  gli errori sono `BusError::Decode` (nessuno risponde) o `BusError::Slave`.
  `read_u64` legge i descrittori (ha un'implementazione di default).
- `Mmu::new(pa_bits)`: `pa_bits` è PARange (`Mmu::PA_BITS_CORTEX_A53 = 40`).
  - `translate(&mut phys, va, access, el) -> Result<Translation, Fault>`: con
    TLB. `el` è il privilegio dell'accesso (0 per LDTR/STTR a EL1).
  - `walk(...)`: stessa cosa senza leggere né riempire il TLB (AT, debugger).
  - `tlbi(TlbiOp, xt)`, `tlb()`, `tlb_mut()`, `last_fault()`.
- `Translation`: `pa`, `level` (1-3; 0 a MMU spenta), `block_size`,
  `perms: Option<Perms>` (AP[2:1], UXN, PXN già combinati con le tabelle),
  `attr_index`, `mair_attr` (byte MAIR), `sh`, `ng`, `asid`; `par()` dà
  PAR_EL1 per un AT riuscito.
- `Fault { kind, va, access, el }`: `far()`, `esr(from_el)` (ESR_EL1 con EC
  0x20/0x21/0x24/0x25, IL = 1, WnR, EA, DFSC/IFSC), `par()`, `mem_fault()`.
  `from_el` è PSTATE.EL e decide "lower EL" / "same EL".
- `FaultKind` e codici FSC:

  | Variante | FSC |
  |---|---|
  | `AddressSize(l)` | `0b0000ll` |
  | `Translation(l)` | `0b0001ll` |
  | `AccessFlag(l)` | `0b0010ll` |
  | `Permission(l)` | `0b0011ll` |
  | `Alignment` | `0b100001` |
  | `External(_)` | `0b010000` |
  | `ExternalWalk(l, _)` | `0b0101ll` |
  | `Unimplemented(_)` | nessuno (`None`): limite di Vetro, non fault |

- `Tlb`: `flush_all`, `flush_va(va, asid)`, `flush_asid(asid)`,
  `flush_va_all_asids(va)`, `tlbi(op, xt)`, `len`.
- `TlbiOp` (definito in `vetro_cpu::sys` e riesportato qui): VMALLE1, VAE1, ASIDE1, VAAE1, VALE1, VAALE1 e varianti IS;
  `from_sys(op1, crn, crm, op2)` per il decoder della CPU, `is_broadcast()`.
- `VirtMemory { mmu, phys, el }`: implementa `vetro_cpu::Memory` sopra MMU e
  memoria fisica; `last_fault()` dà il `Fault` completo dell'ultimo accesso
  fallito (resta anche in `Mmu::last_fault()` dopo che l'adattatore è stato
  distrutto).
- `MmuBus { mmu, phys }` (ADR 0009): implementa `vetro_cpu::SysBus` per la
  modalità sistema della CPU. A ogni traduzione copia in `mmu.regs` i
  registri che la CPU passa (`TranslationRegs`: la CPU ne è l'unica
  proprietaria) e usa `translate_checked`; `at` usa `walk` (un abort
  esterno sul walk diventa `AtResult::Abort`, gli altri fault vanno in PAR);
  `tlbi` e `tlb_flush_all` agiscono sul TLB del core.
- `Mmu::translate_checked(phys, va, access, el, aligned)`: come
  `translate`, ma con `aligned = false` un accesso ai dati su memoria Device
  (anche a MMU spenta) dà `FaultKind::Alignment`, dopo i fault del walk e
  prima dei permessi (`AArch64.FirstStageTranslate`).

## Comportamento
- SCTLR.M = 0: identità; VA (senza tag, se c'è TBI) oltre PARange → address
  size fault di livello 0. Attributi: dati Device-nGnRnE (0x00), fetch 0xaa
  con SCTLR.I, 0x44 senza. Il TLB non si usa.
- Metà dello spazio: bit `AddrTop` (55 con TBI, 63 senza; il TBI si sceglie
  col bit 55). I bit fra AddrTop e 64-TxSZ devono essere tutti uguali al
  selettore, altrimenti translation fault di livello 0.
- TxSZ fuori da 16..=39 si riporta al limite (CONSTRAINED UNPREDICTABLE,
  scelta di QEMU). Livello iniziale e allineamento della prima tabella dallo
  pseudocodice (es. 48 bit → livello 0, 39 → 1, 30 → 2; 40 bit → livello 0
  con due voci).
- IPS limitato a PARange; i valori riservati valgono 48 prima del limite.
  Address size fault su base TTBR (livello 0), tabelle e uscita.
- AF = 0 → access flag fault (niente aggiornamento hardware in v8.0). L'AF
  viene prima dei permessi, l'address size dell'uscita prima dell'AF.
- Permessi: AP[2:1], APTable, UXN/PXN, UXNTable/PXNTable, SCTLR.WXN; una
  pagina scrivibile da EL0 non è eseguibile a EL1; a EL0 si può avere
  "solo esecuzione". Il permission fault riporta il livello della foglia.
- ASID: da TTBR1 se TCR.A1, altrimenti TTBR0; 8 bit se TCR.AS = 0.
- EPDx blocca solo i walk: una voce già nel TLB continua a valere.
- Granulo: TG 16 KiB e valori riservati valgono 4 KiB (la A53 non ha 16 KiB;
  scelta IMPLEMENTATION DEFINED, la stessa di QEMU); 64 KiB → `Unimplemented`.

## TLB
Corrispondenza diretta, 512 slot indicizzati dal numero di pagina da 4 KiB,
deterministica. Ogni voce ricorda il blocco intero (4 KiB, 2 MiB, 1 GiB):
una TLBI per VA dentro un blocco toglie tutte le voci del blocco. Solo walk
riusciti entrano nel TLB; i permessi e MAIR si rivalutano a ogni lookup
(l'architettura permette di tenerli in cache, ricalcolarli è più semplice e
comunque ammesso). Chiave: VA[55:0] e ASID (le voci globali valgono per ogni
ASID). Le varianti "last level" coincidono con le altre perché non c'è cache
dei livelli intermedi.

Doveri del sistema (con `MmuBus` li fa la CPU in modalità sistema):
- dopo una scrittura di SCTLR_EL1 o TCR_EL1 chiamare `tlb_mut().flush_all()`
  (come fa QEMU);
- le TLBI `...IS` vanno applicate al TLB di ogni core.

## Limiti noti
- Solo granulo 4 KiB; niente descrittori big-endian (SCTLR.EE = 1 →
  `Unimplemented`). SCTLR.E0E (dati a EL0) è affare della CPU.
- Niente FEAT_HAFDBS, PAN, TTST, LPA, HPD: sono oltre ARMv8.0.
- Il bit Contiguous è ignorato (ammesso: è un suggerimento).
- SCTLR.A è della CPU; il fault di allineamento su memoria Device lo dà
  `translate_checked` quando la CPU segnala un accesso non allineato.
- `esr` non conosce l'istruzione: ISV = 0 (come QEMU per gli abort stage 1)
  e CM = 0 (chi esegue DC su un indirizzo lo aggiunge).
- Accessi a cavallo di pagina: si traducono prima tutte le pagine, quindi un
  fault di traduzione o permesso non lascia scritture parziali; un abort
  esterno sul secondo pezzo sì. FAR = primo byte della pagina che fallisce
  (come QEMU).
- `VirtMemory` usa un solo privilegio per tutti gli accessi: per LDTR/STTR
  il sistema costruisce l'adattatore con `el = 0` e passa PSTATE.EL a
  `Fault::esr`.
- PAR_EL1: NS = 1 e bit 11 = 1 come QEMU.

## Test
`cargo test -p vetro-mmu`: tabelle costruite in una RAM di prova con
costanti del formato VMSAv8-64 indipendenti dal codice. Coprono pagine,
blocchi da 2 MiB e 1 GiB, TTBR1, TxSZ generici, TBI, MMU spenta, granuli,
fault a ogni livello, AF, address size, abort esterni, permessi EL0/EL1 con
attributi delle tabelle e WXN, codifiche ESR/FAR/PAR, ASID/nG, TLB e ogni
TLBI, adattatore `Memory`.
Il confronto con `qemu-system-aarch64` arriverà con l'avvio del kernel (M3).
