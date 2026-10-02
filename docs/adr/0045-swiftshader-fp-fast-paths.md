# ADR 0045 — SwiftShader's FP/SIMD in regions: FPCR modes, half precision, exact IXC

- Status: accepted (M4, 2026-10-02). Extends ADR 0026 and 0041.
  Measurements: `docs/progress/M4.md` (2026-10-02, "SwiftShader's FP").

## Context
On the live site a tap takes 8–11 s to its first frame, the app drawer up
to ~100 s, Settings up to 165 s: while the user acts the guest runs at
0.3–0.5x real time (ADR 0044). A Worker CPU profile during activity is
dominated by the interpreter's FP code (`fpinsn::exec`, `fp::round`,
`unpack_ahp`, `jit::helper::exec`): the guest's software Vulkan
(SwiftShader, under ANGLE and hwui) runs FP/SIMD that leaves the regions'
fast paths (ADR 0026) for `env.simd`.

ADR 0026's fast paths applied only with FPCR = 0, with IXC already 1 or a
result verified exact, and only to some forms.

## Measurement
`VETRO_JIT_PROFILE` (and the app's `?jitprofile=1`, read by
`tools/aosp/live-path.mjs --jit-profile` per action) now also counts every
FP instruction run by the interpreter or by `env.simd` with its state:
FPCR\[26:19\] (FZ16, Stride, RMode, FZ, DN, AHP), whether IXC was already 1,
the flags the instruction raised (env.simd only) and the classes of its
input lanes (NaN, denormal, infinity, zero). The report groups them by
instruction class and state ("FP by state"), so a miss shows its reason.

What it showed on the live path (build VM, headless Chrome 154 without a
GPU, the app served from the VM against R2, light profile, image f08b79e,
the published warmed prebuilt; `?jitprofile=1` run, all actions summed):

- **FPCR is always 0 and IXC always 1.** SwiftShader does not set FZ, DN or
  a rounding mode; no fast path missed because of FPCR or a clear IXC.
- **No half-precision conversion at all** (no FCVTL/FCVTN with halves, no
  scalar FCVT with H), no FRECPE/FRSQRTE/FRECPS/FRSQRTS, no FADDP.
- `env.simd`: 31.8 M calls, against 230 M interpreter steps and ~10 G guest
  instructions in the session:

| Share of env.simd | Instruction | Why it missed |
|---|---|---|
| 43.4% | FMUL .4s | fast path exists; a lane with a zero factor gives an exact zero, which the "result above the smallest normal" check rejected |
| 32.4% | FRINTI .4s | no vector FRINT fast path |
| 6.6% | FMUL .2s by element | zero lanes, denormal inputs with exact (tiny) products |
| 5.7% | FCVTZS .4s, #fbits | no fixed-point conversion fast path |
| 4.4% | FMLA .4s by element | zero products rejected by the same check |
| 1.9% | FMUL .4s by element | zero lanes |
| 1.0% | FDIV .4s | zero dividends |
| 1.0% | FSUB .4s | an infinite input (∞ − x is exact) rejected by the finite check |
| 0.8% | FMUL s (scalar) | denormal inputs (exact tiny products) |
| 0.7% | FRINTM .4s | no vector FRINT fast path |
| 0.7% | FMLA .4s | zero products |
| 0.5% | UQXTN .8b | not inline |

  In the interpreter (cold code and exits), LD2 of one lane was 3.0% of the
  steps (not translated: only LD1 of one lane was).
- A Worker CPU profile (30 s from the press, before this ADR) gave the
  FP/SIMD helpers (`fpinsn::exec`, `fp::*`, `helper::exec`) **4.8%** of the
  time while the app drawer opened, **1.6%** for Settings and 0% while the
  catalog app opened; the rest: region code 17–25%, the dispatcher loop 19%,
  other host code 20–25%, the interpreter 8–12%, `Cache::lookup` 5–7%,
  WASM compilation 5–8%. The FP helpers are not what makes these steps slow
  on this path (the owner's profile that motivated this work came from his
  browser; it was not reproduced here). While Minesweeper opened, 46% of the
  Worker went to the snapshot compression (`vetro_snapshot::lz::compress`)
  of the save that follows the install.

## Decision: what the measurement asked for
The top items take exact fast paths: exact zero lanes and exact (rechecked)
single products, zero products in FMLA/FMLS, infinite sums, vector FRINT*,
FCVTZS/FCVTZU with fraction bits, scalar exact tiny products, UQXTN, LD2..LD4
of one lane and LD2R..LD4R. The other changes below are general (FPCR modes,
half precision, IXC raised by the fast path): they were written before the
measurement, which then showed that this guest does not use them; they stay
because they are exact, tested, and cost nothing when unused.

## Decision: details

### FPCR modes
The fast paths need only FPCR.RMode = round to nearest (WASM's rounding).
The other bits do not change their results:
- DN changes only NaN results, and no fast path writes a NaN (each one
  checks for NaN inputs or results already);
- AHP and FZ16 concern only half precision (the half conversions below
  require AHP = 0; FZ16 does not exist on ARMv8.0);
- FZ flushes denormal inputs (IDC) and tiny results (UFC): with FZ set
  every function checks (`G::fz_guard`) that no input lane that counts is
  a denormal and, for sums and the exact narrowing paths, that no result
  is; products, quotients, FMA and inexact narrowings already require a
  result above the smallest normal, and the emulated double FMA's
  exponent ranges exclude denormals.

### Half precision
FCVTL/FCVTL2 (half to single), FCVTN/FCVTN2 (single to half) and scalar
FCVT Sd, Hn / Hd, Sn in `rt.fp<k>`, with integer WASM SIMD:
- half to single is exact for every half that is not a NaN (no flush on
  ARMv8.0, so a denormal half is a value like the others): the bits
  shifted by 13 are a single whose product by 2^112 is exact, infinities
  apart, then the sign;
- single to half rounds to nearest even on the bits:
  `(abs − (112 << 23) + 0xfff + ((abs >> 13) & 1)) >> 13` for magnitudes
  in [2^-14, 65520) (a normal half after rounding: never tiny, no
  overflow), ±0 and ±infinity exact; inexact exactly when the 13 low
  mantissa bits are not zero.

### IXC raised by the fast path
Where a function knows exactly whether its result is inexact it raises
FPSR.IXC itself instead of falling back when IXC is 0: sums (TwoSum),
single products, quotients, roots and FMA (rechecked in double), FCVT
D→S, FCVTN, FCVT to integer (scalar and vector), FRINTX, SCVTF/UCVTF from
a general register (the significant bits of |x|: 64 − clz − ctz), the half
conversions. Double products, quotients, roots and FMA still need IXC = 1.

### Exact zeros, exact products, infinities (measured top items)
- vector FMUL/FDIV and by-element FMUL: a lane is accepted if its result is
  above the smallest normal, or (single products) the product rechecked in
  double is exact (tiny and zero results included: no UFC without FZ), or
  (double, quotients) a zero with a zero factor or dividend; single products
  then raise IXC themselves when inexact;
- FMLA/FMLS single (vector and by element): a lane with a zero factor and
  finite operands is the accumulator, exactly;
- vector FADD/FSUB: a lane with an infinite input and an infinite result is
  exact (∞ − ∞ is a NaN, rejected);
- scalar single FMUL/FNMUL/FDIV: an exact result (rechecked in double) is
  accepted whatever its size;
- vector FCVTZS/FCVTZU with fraction bits: × 2^fbits (exact, or an infinity
  the range check rejects), then the FCVTZ path;
- UQXTN(2) inline: the input clamped (unsigned) to the narrow maximum before
  WASM's `narrow_u`, QC as for SQXTUN;
- LD2..LD4 of one lane and LD2R..LD4R in regions: all loads first, then the
  registers (a fault leaves them unchanged).

### More forms
- vector FRINTN/P/M/Z/A/X/I (I and X under RMode = nearest);
- scalar pairwise FADDP, FMAXP, FMINP, FMAXNMP, FMINNMP;
- FMAXV, FMINV, FMAXNMV, FMINNMV (.4s): without NaNs the four agree and
  WASM's max/min order zeros like Arm;
- scalar by-element FMUL, FMLA, FMLS (single: round to odd in double;
  double: the emulated FMA).

## Verification
- `fp_limits.rs` `zero_products_stay_in_the_fast_path` and
  `infinities_and_exact_tiny_products_stay_in_the_fast_path` (no
  `env.simd`), and the special-value table for every new form; broken on
  purpose, each is found: the zero-factor requirement, the accumulator's
  finiteness, the fraction-bits exponent, the FZ guard on exact tiny
  products. `parity.rs`: `single_structure_loads_match_the_interpreter`
  (lane, replicate, writeback by immediate and register, a fault on the
  second element, registers wrapping past V31; red with a wrong element
  offset) and UQXTN in `saturating_simd_sets_qc_like_the_interpreter` (now
  also asserting no `env.simd`; red without the clamp).
- `vetro-jit-native/tests/fp_limits.rs`: the new forms on the special
  values (now with half values and the half rounding boundaries: 65504,
  65520, 2^-14, ties) under seven FPCR values (FZ, DN, AHP, rounding
  modes and a mix) and both IXC states; `percorsi_veloci_usati` also with
  FZ | DN; `fast_paths_raise_ixc` (inexact results with IXC at 0, no
  `env.simd`). Broken on purpose, each is found: no FZ guard, no IXC
  raised (scalar and FRINTX), the half rounding's tie bit, its overflow
  bound, its inexact mask, the half NaN bound, the by-element index.
- `tests/diff`: `random_swiftshader_fp_match_qemu` (seeds `ssfp-`): short
  programs of the forms above with moderate values, halves over their
  range, exact and tie mantissas, and the FPCR modes; interpreter, JIT and
  QEMU identical. The broken tie bit is found there too.
- The nightly differential configuration on the build VM.

## Consequences
- `docs/specs/jit.md`: the `rt.fp<k>` conditions.
- No ABI change: new `rt.fp<k>` functions are appended (imports are by
  name and optional, ADR 0041).
