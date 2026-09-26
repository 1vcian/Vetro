# Spec — vetro-mmu

## Scope
AArch64 stage 1 translation of the EL1&0 regime (EL0 and EL1), ARMv8.0 level
with the Cortex-A53 parameters (ADR 0005). Reference: Arm ARM, D8
(VMSAv8-64) and the pseudocode `AArch64.TranslationTableWalk`,
`AArch64.TranslateAddressS1Off`, `AArch64.CheckPermission`: the order of the
checks is that of the pseudocode, so the priority between different faults is
architectural. No stage 2, EL2, EL3, AArch32.

Allowed dependencies: `vetro-cpu` (only `Access`, `MemFault`, `Memory`), no
external crates. It compiles for `wasm32-unknown-unknown`.

## Public interface
- `MmuRegs { sctlr, tcr, ttbr0, ttbr1, mair }`: the `_EL1` registers, written
  by the system (MSR). Modules `sctlr` and `tcr` with the bits in use.
- `trait PhysMemory { read, write, read_u64 }`: little-endian physical memory;
  errors are `BusError::Decode` (nobody answers) or `BusError::Slave`.
  `read_u64` reads the descriptors (it has a default implementation).
- `Mmu::new(pa_bits)`: `pa_bits` is PARange (`Mmu::PA_BITS_CORTEX_A53 = 40`).
  - `translate(&mut phys, va, access, el) -> Result<Translation, Fault>`: with
    the TLB. `el` is the privilege of the access (0 for LDTR/STTR at EL1).
  - `walk(...)`: the same without reading or filling the TLB (AT, debugger).
  - `tlbi(TlbiOp, xt)`, `tlb()`, `tlb_mut()`, `last_fault()`.
- `Translation`: `pa`, `level` (1-3; 0 with the MMU off), `block_size`,
  `perms: Option<Perms>` (AP[2:1], UXN, PXN already combined with the tables),
  `attr_index`, `mair_attr` (MAIR byte), `sh`, `ng`, `asid`; `par()` gives
  PAR_EL1 for a successful AT.
- `Fault { kind, va, access, el }`: `far()`, `esr(from_el)` (ESR_EL1 with EC
  0x20/0x21/0x24/0x25, IL = 1, WnR, EA, DFSC/IFSC), `par()`, `mem_fault()`.
  `from_el` is PSTATE.EL and decides "lower EL" / "same EL".
- `FaultKind` and FSC codes:

  | Variant | FSC |
  |---|---|
  | `AddressSize(l)` | `0b0000ll` |
  | `Translation(l)` | `0b0001ll` |
  | `AccessFlag(l)` | `0b0010ll` |
  | `Permission(l)` | `0b0011ll` |
  | `Alignment` | `0b100001` |
  | `External(_)` | `0b010000` |
  | `ExternalWalk(l, _)` | `0b0101ll` |
  | `Unimplemented(_)` | none (`None`): a Vetro limitation, not a fault |

- `Tlb`: `flush_all`, `flush_va(va, asid)`, `flush_asid(asid)`,
  `flush_va_all_asids(va)`, `tlbi(op, xt)`, `len`.
- `TlbiOp` (defined in `vetro_cpu::sys` and re-exported here): VMALLE1, VAE1, ASIDE1, VAAE1, VALE1, VAALE1 and IS variants;
  `from_sys(op1, crn, crm, op2)` for the CPU decoder, `is_broadcast()`.
- `VirtMemory { mmu, phys, el }`: implements `vetro_cpu::Memory` on top of the
  MMU and physical memory; `last_fault()` gives the full `Fault` of the last
  failed access (it also stays in `Mmu::last_fault()` after the adapter has
  been destroyed).
- `MmuBus { mmu, phys }` (ADR 0009): implements `vetro_cpu::SysBus` for the
  CPU's system mode. On every translation it copies into `mmu.regs` the
  registers the CPU passes (`TranslationRegs`: the CPU is their sole
  owner) and uses `translate_checked`; `at` uses `walk` (an external
  abort on the walk becomes `AtResult::Abort`, the other faults go into PAR);
  `tlbi` and `tlb_flush_all` act on the core's TLB.
- `Mmu::translate_checked(phys, va, access, el, aligned)`: like
  `translate`, but with `aligned = false` a data access to Device memory
  (also with the MMU off) gives `FaultKind::Alignment`, after the walk faults
  and before the permissions (`AArch64.FirstStageTranslate`).

## Behaviour
- SCTLR.M = 0: identity; VA (without tag, if there is TBI) beyond PARange →
  level 0 address size fault. Attributes: data Device-nGnRnE (0x00), fetch
  0xaa with SCTLR.I, 0x44 without. The TLB is not used.
- Half of the address space: bit `AddrTop` (55 with TBI, 63 without; TBI is
  selected by bit 55). The bits between AddrTop and 64-TxSZ must all equal
  the selector, otherwise level 0 translation fault.
- TxSZ outside 16..=39 is clamped to the limit (CONSTRAINED UNPREDICTABLE,
  QEMU's choice). Starting level and alignment of the first table from the
  pseudocode (e.g. 48 bits → level 0, 39 → 1, 30 → 2; 40 bits → level 0
  with two entries).
- IPS limited to PARange; reserved values count as 48 before the limit.
  Address size fault on the TTBR base (level 0), tables and output.
- AF = 0 → access flag fault (no hardware update in v8.0). AF
  comes before the permissions, output address size before AF.
- Permissions: AP[2:1], APTable, UXN/PXN, UXNTable/PXNTable, SCTLR.WXN; a
  page writable from EL0 is not executable at EL1; at EL0 one can have
  "execute only". The permission fault reports the level of the leaf.
- ASID: from TTBR1 if TCR.A1, otherwise TTBR0; 8 bits if TCR.AS = 0.
- EPDx blocks only walks: an entry already in the TLB stays valid.
- Granule: TG 16 KiB and reserved values count as 4 KiB (the A53 has no
  16 KiB; IMPLEMENTATION DEFINED choice, the same as QEMU); 64 KiB →
  `Unimplemented`.

## TLB
Direct-mapped, 512 slots indexed by the 4 KiB page number,
deterministic. Each entry records the whole block (4 KiB, 2 MiB, 1 GiB):
a TLBI by VA inside a block removes all entries of the block. Only successful
walks enter the TLB; permissions and MAIR are re-evaluated on every lookup
(the architecture allows caching them, recomputing them is simpler and
still allowed). Key: VA[55:0] and ASID (global entries apply to every
ASID). The "last level" variants coincide with the others because there is
no cache of intermediate levels.

Duties of the system (with `MmuBus` the CPU does them in system mode):
- after a write to SCTLR_EL1 or TCR_EL1 call `tlb_mut().flush_all()`
  (as QEMU does);
- the `...IS` TLBIs must be applied to the TLB of every core.

## Known limitations
- 4 KiB granule only; no big-endian descriptors (SCTLR.EE = 1 →
  `Unimplemented`). SCTLR.E0E (data at EL0) is the CPU's business.
- No FEAT_HAFDBS, PAN, TTST, LPA, HPD: they are beyond ARMv8.0.
- The Contiguous bit is ignored (allowed: it is a hint).
- SCTLR.A belongs to the CPU; the alignment fault on Device memory is given
  by `translate_checked` when the CPU signals a misaligned access.
- `esr` does not know the instruction: ISV = 0 (like QEMU for stage 1
  aborts) and CM = 0 (whoever executes DC on an address adds it).
- Page-crossing accesses: all pages are translated first, so a
  translation or permission fault leaves no partial writes; an external
  abort on the second piece does. FAR = first byte of the failing page
  (like QEMU).
- `VirtMemory` uses a single privilege for all accesses: for LDTR/STTR
  the system builds the adapter with `el = 0` and passes PSTATE.EL to
  `Fault::esr`.
- PAR_EL1: NS = 1 and bit 11 = 1 like QEMU.

## Tests
`cargo test -p vetro-mmu`: tables built in a test RAM with
VMSAv8-64 format constants independent of the code. They cover pages,
2 MiB and 1 GiB blocks, TTBR1, generic TxSZ, TBI, MMU off, granules,
faults at every level, AF, address size, external aborts, EL0/EL1
permissions with table attributes and WXN, ESR/FAR/PAR encodings, ASID/nG,
TLB and every TLBI, `Memory` adapter.
The comparison with `qemu-system-aarch64` will come with the kernel boot (M3).

## Snapshot (M6, ADR 0015)

`Mmu` and `Tlb` implement `vetro_snapshot::Snapshot`: translation registers
and TLB entries (observable state). On restore the cache of recent
translations starts empty again, the slot generations grow and
`Tlb::flushes` grows (the JIT discards its software TLB). PARange is
checked.
