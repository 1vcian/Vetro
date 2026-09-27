# ADR 0010 — M2 LTP: oracle environment, native oracle where QEMU is wrong, virtual time

- Status: accepted (M2, 2026-09-25). Refines the M2 exit criterion and
  supplements ADR 0003 (QEMU oracle).

## Context
The M2 criterion compares a selection of LTP tests run on Vetro and on
`qemu-aarch64 -cpu cortex-a53`: same exit code, same counts of
TPASS/TFAIL/TBROK/TCONF. On the CI linux job (arm64 host) three
problems emerged that do not concern Vetro's correctness:

1. QEMU user mode passes many requests through to the host: `uname -r`, the CPUs of
   `sched_getaffinity`, resource limits. Many LTP tests decide what to
   try based on the kernel version or the number of CPUs, so
   Vetro and QEMU were running different tests.
2. On some tests QEMU departs from the Linux kernel: `futex_waitv` and
   `F_CREATED_QUERY` not implemented, the order of `mmap` checks, `clone`
   flags, and so on. Imitating these errors would be a step backwards in
   fidelity.
3. Vetro's virtual time counted one nanosecond per instruction. An
   `alarm(5)` cost 5·10⁹ instructions, and a loop made only of syscalls
   (which on the host cost real microseconds but few instructions) barely
   advanced time.

## Decision
- **Oracle environment.** The LTP harness asks QEMU, with BusyBox, for the
  kernel version (`uname -r`) and the available CPUs (`nproc`), and passes them
  to Vetro (`Config::release`, `Config::cpus`). Vetro's default values
  stay fixed (one CPU, `6.6.0-vetro`), so that execution is
  reproducible and because in the browser there is no host to imitate.
- **Native oracle where QEMU is wrong.** `tools/ltp/qemu-divergent.txt`
  lists the tests on which QEMU differs from Linux, each with the native outcome,
  QEMU's outcome and the reason. Every entry is verified by running the binary
  natively and under QEMU in the same environment, as an unprivileged user. For
  these tests the oracle is the native execution of the static binary on a
  Linux aarch64 host (the CI linux job); on other hosts they are skipped, with a
  warning. The real kernel is a stronger oracle than QEMU: the exception does not
  weaken the rule "QEMU is the oracle", it makes it stricter where QEMU
  is not enough.
- **Repeated oracle.** Vetro is deterministic, the oracle is not: the
  timing tests (`nanosleep01`, `futex_wait05`, etc.) measure real time
  and on a loaded host QEMU sometimes gets them wrong. If the oracle's outcome
  differs from Vetro's, the harness reruns it up to two times and it is enough
  for one of the runs to match. Vetro's outcome is never repeated. The
  tests that measure real time (`tools/ltp/timing.txt`, those of the
  `tst_timer_test` library) run alone, after the parallel phase.
- **Host limits.** Vetro uses one host descriptor for every guest
  file, so it raises its own soft limit (`raise_fd_limit`). The oracle
  receives the original limit, because QEMU passes it to the guest.
- **QEMU outcome.** QEMU prints "uncaught target signal" even when a
  child process of the test dies, which is expected in many cases. The signal counts
  only if the main process did not exit normally.
- **Exclusions.** `tools/ltp/skip.txt` remains for the tests that cannot be
  compared, each with its reason (for now `fork14`, which asks for 16 TB of
  virtual space).
- **Virtual time.** 10 ns per instruction (a nominal 100 MHz CPU,
  close to the interpreter's real speed) and 1 µs per syscall (the
  typical cost on a real kernel). It stays deterministic, because it depends only on the
  executed sequence.

## Consequences
- The M2 criterion is: all the tests of the LTP selection, excluding those in
  `skip.txt` with a reason, have the same outcome on Vetro and on the oracle (QEMU,
  or native for those in `qemu-divergent.txt`) in the CI linux job.
- The two lists are part of the criterion: adding an entry requires the
  native/QEMU verification and the reason, and must be reviewed at every QEMU update.
- Limit of the repeated oracle: if the oracle is unstable on a test, Vetro
  can match one of its rare outcomes and pass anyway. This is accepted for
  real-time cases only; a test that always diverges remains a failure.
- The time constants are visible to programs, for example in the number of
  iterations of a loop that lasts one second. Changing them changes the traces, but not
  correctness.
