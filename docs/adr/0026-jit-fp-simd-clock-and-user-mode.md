# ADR 0026 — JIT: FP/SIMD in regions, clock, chaining in user mode

- Status: accepted (M4, 2026-09-26). Extends ADR 0012, 0013 and 0024.

## Context
ART, bionic, Skia and SwiftShader make heavy use of floating point and SIMD.
After ADR 0024 regions translated only the loads/stores of V registers,
DUP/INS/UMOV/SMOV and MOVI: every other SIMD/FP instruction closed the
region and was executed by the interpreter.

The profile with the per-class counters (below) on `tests/linux/c/fpsimd.c`
(conversions, float and double matrix products, NEON memcpy/strlen, TBL,
EXT, ADDV/UMAXV, FPCR/FPSR) showed:
- 76% of instructions still in the interpreter (31 of the 41 million);
- the JIT (21 MIPS) no faster than the interpreter (20 MIPS).

In addition:
- in the kernel boot, CNTVCT reads exited the regions
  (17 thousand);
- in user mode every region was a call into wasmtime (27 million
  for `gzip`), and exclusives stayed in the interpreter (6% of the exits of
  `awk`).

The interpreter's FP semantics (`vetro_cpu::simd::fp`) is software, bit for
bit like Arm. WASM has IEEE 754 with round to nearest even, but:
- it has no flags;
- it does not fix the NaN bits;
- it has no FMA;
- it does not check tininess before rounding, as Arm does for
  UFC.

## Decision

### Measurement: per-class counters
`vetro_jit::profile` counts, per class, the instructions the interpreter
executes with the JIT active, distinguishing those the JIT could translate
(cold code) from those that are missing. The SIMD/FP classes are fine-grained:
class, opcode, U, Q, size. It is enabled with `VETRO_JIT_PROFILE=1` in
`vetro run` and in `vetro boot` (`JitConfig::profile`, `SysJitConfig::profile`).
It also counts the instructions executed by `env.simd`.

### `env.simd`: the interpreter inside the region
Every SIMD/FP instruction without memory (integer, FP, cryptographic) is
translated. Those without an inline form call `rt.simd` → `env.simd`
(`vetro_jit::helper`): the interpreter (`vetro_cpu::simd::exec_dp`) executes it
on the V registers, FPCR and FPSR of `JitState`, without leaving the region.
- The semantics is the interpreter's by construction.
- General registers and NZCV stay in the region's variables: the caller
  passes the general register read and NZCV, and receives the written
  register or the new NZCV (`helper::io`).
- `JitState` carries FPCR and FPSR (offsets 424 and 428, previously padding).
- vetro-wasm exports `vetro_jit_simd` (ABI 11).
- One scratch CPU per thread; the 512 bytes of the V registers are copied.

### Inline integer SIMD with WASM SIMD
Only the forms that are exact by construction, with the semantics of
`vetro_cpu::simd::int` (with Q = 0 the upper half is zero):
- logical, BSL/BIT/BIF;
- additions and differences, comparisons, min/max, [SU]ABD/[SU]ABA,
  MUL/MLA/MLS, URHADD;
- ADDP, [SU]MAXP/[SU]MINP;
- ABS/NEG, CNT, NOT, REV, [SU]ADDLP/[SU]ADALP, XTN;
- reductions (ADDV, [SU]MAXV, [SU]MINV, [SU]ADDLV);
- shifts by immediate (SHL, [SU]SHR, [SU]SRA, [SU]SHLL, SHRN);
- ZIP/UZP/TRN, EXT, TBL/TBX (with `swizzle`: index - 16k for table k).

The saturating [SU]Q{ADD,SUB} at 8 and 16 bits, SQXTN and SQXTUN use WASM's
saturating operations and write FPSR.QC if a lane differs from the modular
result or from the truncation: there is saturation exactly when they
differ.

### Floating point: fast paths in the runtime
FMOV, FABS, FNEG and FCSEL (scalar) and vector FABS/FNEG are inline:
they are bits, even on NaNs.

The other operations have a runtime function `rt.fp<k>`. The region
calls it with the instruction word; the function reads the registers from
`JitState` and computes with WASM. It writes the result only if it is
certainly the interpreter's, that is if all these conditions hold:
- FPCR = 0;
- no NaN in input or output;
- no infinity from overflow;
- for products, quotients, FMA and narrowing conversions, a
  normal result greater than the smallest normal (Arm signals UFC
  by checking tininess before rounding);
- IXC already 1 in FPSR (cumulative flag), or an exact result
  verified exactly:
  - TwoSum for additions;
  - in single precision, product, quotient and square root rechecked in double;
  - conversions: the rounded value equal to the input.

If a condition is missing, the function calls `env.simd`.

Covered:
- scalar: FADD/FSUB/FMUL/FDIV/FMAX/FMIN/FMAXNM/FMINNM/FNMUL, FSQRT,
  FMADD and variants, FCMP/FCMPE, FCVT between S and D, FRINT[NPMZAIX],
  SCVTF/UCVTF, FCVT[NPMZA][SU];
- vector: the same binary ones, FMLA/FMLS and by-element FMUL/FMLA/FMLS,
  FADDP, FABD, FCMEQ/FCMGE/FCMGT (also with zero), FSQRT,
  FCVT[NPMZA][SU], SCVTF/UCVTF, FCVTL/FCVTN.

The FMA:
- in single precision is computed in double: the product is exact, the sum
  is rounded "to odd" (TwoSum, then the low bit) and then to even in
  single. This is the correct rounding of the FMA (Boldo and Melquiond).
- in double precision it is the "Emulation of FMA" algorithm by Boldo and
  Melquiond (IEEE TC 2008):
  1. Dekker's exact product with Veltkamp splitting;
  2. TwoSum;
  3. sum of the errors rounded to odd;
  4. final sum.

  It holds without overflows or tiny values, guaranteed by the exponents:
  factors in [2^-400, 2^400), addend zero or in [2^-800, 2^800).

A region module imports only the `rt.fp<k>` it uses (importing them all
cost ~1.5 KB per module, 1.5 MB in the boot in V8).

### SIMD loads/stores
- LD1/ST1 of 1-4 registers, LD1R, single-lane LD1/ST1, LD2..LD4/ST2..ST4
  (multiple structures, permutations with two levels of `shuffle`).
- 8- or 16-byte accesses instead of per-element: same bytes. If a
  wide access fails and the per-element ones do not (alignment), the fault
  exit lets the interpreter decide.
- Loads read everything before writing the registers.
- Stores write in order: redone by the interpreter after a fault,
  they rewrite the same bytes.

### CNTPCT/CNTVCT in regions
`JitState` carries `time_base` (machine instructions at the start of the
dispatcher run), CNTVOFF and `time_ok`. The region computes
`counter(time_base + steps)` like `Machine::counter`: CNTPCT = s / 8 × 5 +
(s mod 8) × 5 / 8, CNTVCT = CNTPCT − CNTVOFF.
- The machine provides the clock before every run (`SysJitDyn::set_time`);
  without it, the region exits and the interpreter reads it.
- At EL0 it is translated only with CNTKCTL_EL1.EL0PCTEN/EL0VCTEN at 1. The
  two bits enter the region parameters and the branch cache context
  (`ctx = epoch << 7 | parameters`).

### User mode: chaining and exclusives
`JitCpu` uses the engine's table and the system mode dispatcher.
- Each address space has a context for the branch cache. The
  context changes when the space's pages are invalidated: the entries of
  a space remain good when the scheduler returns to it.
- Missing entries are written by the host (`Host::resolve`) if the region
  is already compiled.
- LDXR/STXR and variants are translated in user mode too, with the monitor
  in `JitState` (`from_cpu`/`to_cpu` copy it).

### Second tier: no
A second tier for hot regions was tried:
- a counter per region in a module global variable;
- on the n-th entry the host recompiles the region with up to 256
  instructions, and the branch cache restarts.

Measurements in V8 (M3 kernel + BusyBox, commands in the guest on the second
round, boot of `tests/boot`):

| | boot | modules | FP `awk` | shell loop |
|---|---|---|---|---|
| without second tier | 1.61-1.68 s | 10.1 MB | 270 MIPS | 220 MIPS |
| second tier at 2000 entries | 1.76-1.80 s | 11.0 MB | 269 MIPS | 231 MIPS |
| second tier at 200 entries | 1.98 s | 13.4 MB | 283 MIPS | 224 MIPS |
| all regions of 256 | 1.68 s | 12.0 MB | 318 MIPS | 245 MIPS |

At most +5% at steady state, for a boot slower by 6-18%: the profile does not
justify it, and the code does not go in.
- The gain of large regions comes from transitions between lukewarm
  regions, not from the few hot regions.
- Larger regions for everyone (+11-18% on branchy code, +19% of compiled
  bytes) remain a choice to revisit with a real Android workload.

## Verification
- `tests/linux/c/fpsimd.c` (`tests/linux/tests/fpsimd.rs`) against QEMU, with
  the interpreter and with the JIT.
- `vetro-jit-native/tests/parity.rs`:
  - random V registers, FPCR and FPSR with special values;
  - programs three-quarters SIMD/FP, including V register loads/stores;
  - exclusive pairs in user mode;
  - chaining after a page invalidation.
- `vetro-jit-native/tests/fp_limits.rs`:
  - every instruction with a fast path on 470 thousand combinations of
    special values, FPCR and FPSR;
  - random FMAs;
  - the double rounding that rounding to odd avoids (in
    single and double precision);
  - the tiny product rounded to the smallest normal;
  - the fast paths actually used (`helper::calls` does not grow).
- `sys_parity.rs`: MRS CNTPCT/CNTVCT, random CNTKCTL, clock sometimes
  absent.
- `tests/diff`: the random SIMD/FP programs with the JIT (as before), plus
  `random_fp_fast_paths_match_qemu` (seeds `fpfast-`: normal values, FPCR at
  zero, IXC at 1 half the time) against QEMU.
- Every fast path has a deliberately introduced bug that a test finds:
  - sum without TwoSum, tininess threshold, rounding to odd;
  - BIT as BIF, QC never written, MAXP as MINP;
  - permutations of LD2..LD4/ST2..ST4, offset of multiple LD1, index of
    single-lane LD1;
  - counter without the instruction index;
  - context not changed after an invalidation, monitor not copied back.
- During the work the test that counts the calls to `env.simd` found a
  real bug: IXC combined with AND as 0x10 instead of as a boolean, and
  the fast path with IXC at 1 never triggered.

## Consequences
- vetro-wasm ABI 11 (`vetro_jit_simd`); `JitState` of 992 bytes (FPCR,
  FPSR, clock); `docs/specs/jit.md` and `docs/specs/wasm.md` updated.
- `vetro-cpu` exports:
  - `simd::exec_dp`;
  - the types `simd::MovKind` and `simd::Post`.
- `SysJitDyn` has `set_time` and the profile methods; `Machine` uses them.
- In user mode the limit is now memory: every load/store is a
  call to the host (`UserMemory`). The next step is a software TLB
  with the guest memory in the engine's memory.
