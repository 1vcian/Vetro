# ADR 0040 — JIT hot paths for Android: crypto, uaccess, regime switches, eviction

- Status: accepted (M4, 2026-09-30). Extends ADR 0013, 0024, 0026, 0036 and
  0038. Measurements: `docs/progress/M4.md` (2026-09-30).

## Context
On the M4 benchmarks the JIT runs at 150–800 MIPS; on Android it measured
4–17 MIPS in the browser and 20–37 MIPS in Node on the build VM (phases of
`tools/aosp/android-perf.mjs`). To find where the time goes, this ADR adds:

- **Region names** (`SysJitConfig::names`, bit 1 of
  `vetro_machine_set_jit_with`, `android-perf.mjs --names`): every region
  function is named `r<el>_<pc>` in its module's `name` section, so a V8 CPU
  profile attributes time to guest code and shows what each region calls
  (rt functions, `env.*` host functions). No other cost; with it the
  dispatcher also counts region calls (`dispatches`).
- **Guest samples** (`--samples`): the PC, EL and ASID after every quantum of
  1 M instructions (weighted by instructions), and at the end the guest's
  text symbols and executable mappings, for `tools/aosp/perf-report.mjs`
  (self time per category, time under each region, kernel functions and
  user libraries, top regions).
- The class profile (`--profile`) counted with a formatted string per
  interpreter step and `env.simd` call, a third of the time of a profiled
  run: words are now counted and grouped into classes for the report only.

The first profiles (Android 15 image 64fcd35, all phases, Node 22 on the
build VM) showed, besides the region code itself (28–32%):

| Item | Share | Why |
|---|---|---|
| dispatcher loop | 10–14% | a region call for every few guest instructions (calls, returns, short blocks) |
| `Cache::lookup` | 7–9% | every jump cache miss (17 M per 42 s of guest time) and every cold run: three lookups of a map with millions of entries, and an MMU fetch translation |
| `env.simd` (interpreter SIMD) | 7–10% | integer by-element, widening, narrowing and saturating forms of a media binary; AES/SHA/PMULL of userdata encryption and TLS before |
| run exits | 5–9% | 60% of the dispatcher runs ended with `YIELD`: the software PAN's TTBR0 writes at every kernel entry, exit and uaccess window |
| JIT resets and recompilation | 3–5% main thread + background | 8 full resets per run (96 MiB V8 code budget): every hot region back to the interpreter, 64 entries, recompiled |
| host loads/stores | 5–7% | 60 M per 42 s of guest time; LDTR/STTR (uaccess) always went to the host |

## Decision

### Cryptographic extension in regions
AESE, AESD, AESMC, AESIMC, SHA1C/P/M, SHA1H, SHA1SU0/SU1, SHA256H/H2,
SHA256SU0/SU1 and 64-bit PMULL/PMULL2 are runtime functions `rt.cr<k>` on
v128 values (`translate::crypto`), imported on first use, bit for bit the
interpreter's (no FPSR bit is touched):

- the S-box as 16 `i8x16.swizzle` of 16-byte slices of the table (an index
  outside 0..15 gives 0, so slice `h` answers only the bytes 16h..16h+15):
  table-based and exact, not constant-time (not needed in an emulator);
- ShiftRows as a shuffle; MixColumns with xtime and column rotations;
  InvMixColumns as MixColumns after the usual pre-multiplication;
- PMULL as three 32-bit carry-less products (Karatsuba), each with 16 integer
  multiplications of operands with holes every 4 bits (at most 8 terms per
  position: no carry reaches the next one);
- SHA rounds in i32 locals, renamed at translation instead of moved.

### More integer SIMD inline
`translate::vec2`: [SU]ADDL/ADDW/SUBL/SUBW/ABAL/ABDL/MLAL/MLSL/MULL (2),
(R)ADDHN/(R)SUBHN (2), MUL/MLA/MLS and [SU]MULL/MLAL/MLSL by element,
SQDMULH/SQRDMULH (vector and by element, 16 and 32 bits), [SU]HADD/SRHADD,
[SU]RSHR/RSRA, SRI/SLI, RSHRN (2) and SQ(R)SHRN/UQ(R)SHRN/SQ(R)SHRUN (2).
Exact by construction: `extmul`/`extend`; rounding shifts as
`(x >> s) + ((x >> (s-1)) & 1)`; WASM's saturating `narrow` with unsigned
sources first clamped to the signed maximum (`narrow_u` reads its input as
signed); SQRDMULH 16 with `q15mulr_sat_s`, the others on exact wider
products; QC exactly when the saturated result differs (widened back) or
both SQDMULH operands are the most negative value. Scalar forms stay with
`env.simd`.

### LDTR/STTR through the EL0 software TLB
Linux's uaccess is LDTR/STTR. `JitState::utlb` (offset 988, former padding)
is 1 when the EL0 TLB groups were filled under the current TTBR0/TTBR1 bases:
their entries then give what an access with EL0 permissions gives now, and
`rt.ldt_<n>`/`rt.stt_<n>` use them; otherwise the host, with `SIZE_UNPRIV`.
The host fills the EL0 tables after a successful EL0-permission access under
the same condition (with software PAN the uaccess window has the process's
own TTBR0, the one its EL0 code filled the TLB under).

### Regime switches within the run
A region's MSR TTBR0/TTBR1 still ends the region with `YIELD`, and now also
writes `exit_detail` = 3. `SysJit::run` then moves the table bases into the
`Cpu`, resyncs the contexts and the TLB groups of the current EL
(`sync_bases`: nothing else can change within a run) and goes on; a `YIELD`
after unmasking interrupts still returns to the machine. The groups' lists
are emptied in place (they were freed and regrown twice per syscall).

### Cheaper lookups
One hash lookup per `Cache::lookup` (the entry API), and the fetch
translation remembered per entry with the jump cache context and a
generation of partial TLBIs: within a context it cannot change (the jump
cache's own guarantee), so a jump cache miss for a known region does not use
the MMU.

### Eviction instead of resets
Modules take table chunks of `batch` entries. The host marks a module when
it looks up one of its regions; every 1024 modules the jump cache gets a new
epoch (so every region entered is looked up again). When the engine refuses
a module (V8 code budget) or the table is full, the modules not marked since
the last eviction go (the older half if all were), their regions leave the
cache, the cold block entries below the threshold are pruned, and the marks
are cleared; a chunk is reused once nothing refers to its module. In the JS
engine a dropped module leaves the table (entries cleared) and its bytes stop
counting against `CODE_BUDGET`, which becomes a live budget. A full reset is
the last resort.

A first policy cleared the marks every 1024 modules: most modules then
looked unused at the budget, and `pm install` compiled 39% more code.

### Region modules import only what they call
All `rt.*` imports are in order of first use (before, the 43 fixed ones
came first in every module).

## Not done, and why
- **Region chaining without the dispatcher**: regions cannot import the
  block table, because V8 (Node 22) still gives every importing instance a
  dispatch table as large as the table (measured: 5 MiB per instance for 2¹⁸
  entries). Direct calls between regions of the same module and page would be
  possible; not measured yet.
- **Larger code budget**: the live budget stays 96 MiB; eviction frees V8's
  code as modules die. A larger one trades memory for recompilation (see the
  measurements).

## Verification
Targeted tests, each red when the point is broken on purpose (tried):
- `parity.rs`: `crypto_matches_the_interpreter_without_env_simd` (19 crypto
  forms, S-box slice, ShiftRows lane, InvMixColumns, carry-less mask, SHA
  rotations), `more_integer_simd_matches_the_interpreter_without_env_simd`
  (66 encodings: rounding bit, saturation clamp, QC, lane selection,
  by-element index);
- `sys_parity.rs`: `ldtr_sttr_at_el1_use_the_el0_tlb_of_the_same_tables`
  (EL0 code, an SVC handler with LDTR/STTR, an EL1-only page, a second table),
  `msr_ttbr0_in_a_region_switches_the_regime` (resync, registers, DAIF vs
  TTBR yield), `eviction_keeps_the_execution` (an engine with a tiny budget
  whose dropped entries trap);
- `tests/diff`: 64-bit PMULL in the crypto-focused programs, interpreter,
  JIT and QEMU identical.
Plus the existing suites and the nightly differential configuration on the
build VM (see `docs/progress/M4.md`).

## Consequences
- JIT ABI (`docs/specs/jit.md`): `JitState::utlb`, `exit_detail` 3, the
  `rt.cr<k>`, `rt.ldt_<n>`, `rt.stt_<n>` functions, all rt imports optional,
  table chunks and eviction, `JitState::dispatches` (offset 1016).
- vetro-wasm (`docs/specs/wasm.md`, additive, ABI unchanged): bit 1 of
  `vetro_machine_set_jit_with`, more `vetro_jit_stats` counters,
  `vetro_jit.drop` clears the module's table entries and budget.
- `vetro-cpu` exports `simd::CryptoOp`.
