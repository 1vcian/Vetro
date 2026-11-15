# ADR 0049 — Region chaining: tail calls between regions, the dispatcher only on a miss

- Status: accepted (M4, 2026-11-15). The code was written on
  `jit/transitions` on 2026-10-02 (its comments said ADR 0046, a number main
  then gave to deferred snapshots) and measured when it was rebased.
  Extends ADR 0024 (regions), 0036, 0041 (jump cache, block table) and 0026
  (inline memory fast paths). Interfaces: `docs/specs/jit.md`. Measurements:
  below and `docs/progress/M4.md` (2026-11-15).

## Context
After ADR 0041 and 0045 the Worker's profile on Android's live path had no
single hot spot left in the guest's own code: region code 17–25%, **the
dispatcher loop 19–20%**, other host code 20–25%, the interpreter 8–12%
(`docs/progress/M4.md`, 2026-10-02). A region is called every ~19 guest
instructions, and every one of those calls went back to the dispatcher: a
return, the jump cache lookup, the limit check, an indirect call.

## Decision
A region that ends with `NEXT` does the dispatcher's work itself and goes on
with a WebAssembly tail call.

1. **The dispatcher's checks, inline.** After writing back the registers,
   `pc` and `steps`, the region looks up the jump cache entry for `pc` and
   `ctx`, checks `steps` + the entry's maximum steps <= `limit` (read from
   `JitState`, so an abort or a shorter quantum still stops the run), and
   writes `entry`. They are exactly the dispatcher's checks: what runs and
   the instruction count do not change. On a miss, or beyond the limit, it
   returns `NEXT` as before.
2. **Where the engine shares a table's dispatch data** among the instances
   that import it (wasmtime; V8 13 and later), every region module imports
   the block table (`env.tbl`) and chains to **any** region with
   `return_call_indirect` (`Chaining::Shared`).
3. **Elsewhere** (V8 12: each importing instance gets its own copy of the
   table, 5 MiB for `TABLE_SIZE` entries, ADR 0041) a module defines its own
   table 0 with its regions and chains **within the module**
   (`Chaining::Own(base)`); for a region of another module it returns the new
   exit code `CHAIN` with the jump cache word, and the dispatcher calls that
   table entry at once, without a lookup.
4. The JS engine decides (`JitEngine.sharedTables()`): the V8 version in
   Node; in a browser a probe (four instantiations of a module importing a
   `TABLE_SIZE` table take microseconds when shared, milliseconds with a copy
   each). `sharedTable: true|false` forces the kind (tests).
5. With it, inline LDP/STP of W and X registers and LDR/STR of Q (aligned,
   within the page, one TLB entry) instead of two accesses or a runtime call.
6. Counters, only with `SysJitConfig::names` (the app's `?jitcount=1`,
   `live-path.mjs --jit-count`): tail calls, `CHAIN` returns and `NEXT` exits
   by kind, without the class profile's ~15% cost.

### The browser
Tail calls are standard WebAssembly (Chrome 112, Firefox 121, Safari 18.2).
The page probes them like SIMD (`TAIL_CALL_PROBE` in `web/app/launch.mjs`):
without them it says so and does not start the machine.

## Alternatives
- Larger regions (more blocks per module): fewer exits, but compile time and
  code size grow with every block whether hot or not, and a write to a code
  page throws away more.
- A direct `return_call` to a known next region: the target would be fixed
  at translation time, and every invalidation would have to find and patch
  its callers. Going through the jump cache keeps invalidation as it is
  (ADR 0041): a stale target simply misses.

## Verification
- `sys_parity.rs`: the same programs with the shared table, within the
  module and through the dispatcher give the interpreter's state and steps;
  a loop calling into the next page chains (counters), also after an
  invalidation; inline pairs and Qs at page ends and with unaligned bases.
- `tools/wasm-boot.sh --jit` (Mac, Node 22): the guest kernel's boot is the
  same 284786912 instructions and the same log with the native interpreter,
  the interpreter in V8, the JIT, the JIT with Worker compilation and the
  threads build; two cores 560802421, as natively.
- `tools/guest-bench.mjs`: `jitSteps`, runs, resolves, calls, faults and
  every other counter are identical to main's.

## Measurements
`tools/guest-bench.mjs` (BusyBox guest, steady state: the second run of each
command), Apple M2 Pro, main (8f78463) against this branch, runs interleaved;
MIPS, the range over the runs:

| | Node 26.4 (V8 14.6, shared table), 4 runs each | | Node 22.20 (V8 12.4, own table + `CHAIN`), 2 runs each | |
|---|---|---|---|---|
| | main | chaining | main | chaining |
| sha256sum (61 M instr.) | 1235–1304 | 1338–1396 | 1136–1147 | 1169–1249 |
| gzip (301 M) | 773–797 | 906–973 | 622 | 786–788 |
| shell loop (301 M) | 444–467 | 510–542 | 233–234 | 251–253 |
| FP awk (1775 M) | 479–494 | 750–762 | 294–302 | 432–436 |
| boot to the prompt (274 M) | 1.43–1.48 s | 1.48–1.52 s | 1.58–1.64 s | 1.65 s |

Steady state, medians: +8%, +21%, +17% and +55% with the shared table; +6%,
+27%, +8% and +46% without. The boot is 3–4% slower: the modules are larger
(11.06 → 13.59 MB of wasm for the same 7384 blocks, +23%: every `NEXT` exit
carries the lookup) and cold code is mostly compilation.

**Not measured: Android.** The build VM is off. Android's live path runs
much more cold code than these loops, so the larger modules count against
the gain there (compilation was 5–8% of the Worker, the code budget is
finite). The numbers above are the BusyBox guest only.

## Maintenance burden
3/5. One more exit kind and two chaining kinds to keep equal to the
dispatcher's checks: a change to the jump cache word, the limit or the
contexts must be made in three places (the dispatcher, `Shared`, `Own`).
`sys_parity.rs` runs all three against the interpreter. The table-sharing
probe is a timing test: a wrong "shared" on an engine that copies would cost
5 MiB per module, a wrong "not shared" only loses the cross-module tail calls.

## Consequences
- `docs/specs/jit.md`: exit code 5 `CHAIN`, the chaining rules, the counters'
  offsets in `JitState` (total area 197728 bytes).
- ABI 18 (`docs/specs/wasm.md`): the import `vetro_jit.shared`, provided by
  `JitEngine.imports()` to the machine's instance and to the core Workers'
  alike, and the new counters at the end of `vetro_jit_stats`.
- Browsers without WebAssembly tail calls can no longer run the machine (the
  page says so).
- Open: the Android measurement (`live-path.mjs --jit-count`, before/after,
  repeated), and from it whether cold code needs a threshold before a module
  is compiled with chaining.
