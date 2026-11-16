# ADR 0051 — Guest CPU speed: more than 100 M instructions per second of guest time

- Status: accepted (M4, 2026-11-16). Extends ADR 0019 (every host input
  through `Machine::input`), 0026 (the regions' clock), 0042 (the clock of
  several cores), 0040 and 0044 (the guest against real time in the page).
  Interfaces: `docs/specs/platform.md` ("Timer"), `snapshot.md`, `replay.md`,
  `wasm.md`, `jit.md`. Measurements: below and `docs/progress/M4.md`.

## Context
The machine's time is its instruction count: the generic timer's counter
advances 5 ticks every 8 instructions (62.5 MHz over a nominal 100 MHz), so
one second of guest time is 100 M instructions per core. That is what makes
a run reproducible, and it stays.

It is also a speed limit. The page keeps guest time from running ahead of
real time (a guest that is ahead waits), so a host able to run more than
100 M instructions per second holds a busy guest back. Until now no host
got there with Android. With the browser's GPU drawing (ADR 0047), saves
out of the way (ADR 0046) and the code cache no longer churning (the
compile threshold at 256), it does: on a hosted runner, the slowest machine
we measure on, the Worker is idle 11–13% of the time while an app opens.
On the owner's Mac a shell loop in the Linux guest runs three times faster
than the clock allows.

## Decision
The number of instructions in a second of guest time becomes a setting of
the running machine.

1. **Speed and bias.** The machine has a guest CPU speed `k` (1..=64, 1 by
   default) and a clock bias. Its clock is `steps + bias`; a *nominal step*
   (10 ns of guest time, 5/8 of a counter tick) is `cores × k` steps of
   the clock. At speed 1 with one core and no bias this is the machine as
   it was, instruction for instruction: the reference compared with QEMU
   does not change.
2. **A change keeps the time.** When the speed changes, the bias is set so
   that the nominal steps so far stay what they are: CNTPCT, CNTVCT and
   guest time go on from their value, and from that instruction on they
   need `k` instructions per core per nominal step. Deadlines are kept in
   counter ticks, so they need nothing.
3. **It is an input.** `Machine::input(Input::Speed(k))`, recorded with its
   instruction number and applied again by a replay: the run stays
   reproducible. Rejected out of range, and while the cores run in
   parallel (stop them first, as for a snapshot).
4. **Snapshots.** An optional `CLK ` section (speed, bias) after `MACH`,
   written only when they are not the default: a snapshot of a machine that
   never changed speed is byte for byte the file it was, older snapshots
   read as speed 1, and the configuration hash does not include the speed.
   **The published prebuilt snapshots stay valid**: the speed is raised
   after the restore.
5. **The JIT.** The host gives the regions the clock (`time_base`) and its
   divisor (`TIME_DIV` = cores × speed). The regions of one core did not
   divide (the divisor was always 1 for them): now every region that reads
   the counter does.
6. **Cores in parallel.** The shared clock is the machine's clock, bias
   included, and each core divides it by the rate; the instruction count
   comes back from it when the cores return to turns.
7. **vetro-wasm ABI 19**: `vetro_speed`, `vetro_speed_set`.

### The app chooses by itself
The Worker adjusts the speed to the host, with no choice for the user
(`web/app/worker.mjs`). In windows of one second it measures what the host
really executes: the instructions run (the clock's steps without the idle
time it skipped, `Machine::perf`) over the time spent running them. The
speed becomes the power of two that covers 85% of that rate (up to 16): the
guest's clock then allows what the host can do, the host is not left
waiting, and guest time stays between about half of real time and real
time while the guest works. A window with little work (under 20 M
instructions, or 150 ms of running) says nothing and changes nothing, so an
idle guest keeps its speed. Raising is at once, lowering after two windows
that agree. `?speed=N` fixes it (1 = as before). No adjustment during a
replay (the changes come from the log) or with the cores in parallel.

Two things in the page had to follow the speed:
- the quantum's bounds (`QUANTUM`, `MIN_QUANTUM`) are stretches of guest
  time, so in instructions they are multiplied by the speed: unscaled, an
  idle guest at speed 16 needed 1600 slices for a second of its time, each
  with its bookkeeping and messages, and the page stopped answering;
- the analysis timeline stamped inputs with `instructions / 100`
  microseconds, while frames and effects carry guest time: inputs now carry
  the machine's guest time too (`UserInput::at`), and the timeline rebuilt
  from a record log follows the log's speed changes (`GuestClock`). Found by
  `tests/web/browser-analysis.mjs` (the request was tied to the wrong
  command).

A first version of the adjustment looked at how long a busy guest had been
held back; it took an idle guest whose WFI ended with the quantum for a
busy one, drove the speed to the maximum at the prompt, and then needed
seconds to come down when work started.

### What the guest sees
A CPU that runs more or fewer instructions per second from time to time,
as with frequency scaling. Its timers, sleeps and timeouts are in guest
time and do not change. Work driven by time (ticks, vsync, polling) costs
fewer instructions relative to the rest when the speed is higher.

When the host cannot keep the speed that was set, guest time runs behind
real time while the guest is busy, as it already did at speed 1 on a slow
host; the automatic adjustment keeps that within about a factor of two.

## Alternatives
- **A faster clock for everyone** (say 400 MHz, a constant): new prebuilt
  snapshots, a different reference against QEMU, and on a slower host
  guest time would run at a quarter of real time whenever the guest works.
- **A `MachineConfig` parameter**: part of the configuration hash, so the
  published snapshots would not restore, and no adjustment to the host
  while running.
- **Guest time from the real clock** when the host is fast: not
  reproducible; ADR 0019 rules it out.

## Verification
- `vetro-machine`: `speed_change_keeps_the_counter_and_scales_it` (the
  counter and guest time go on at a change, `steps_at` is the first
  instruction at which the counter is there, with a positive and a negative
  bias, out-of-range speeds change nothing);
  `timer_deadline_follows_the_speed` (a WFI wakes after `k` times the
  instructions, from the start and from the middle of the wait; the
  snapshot carries the section, one at speed 1 does not have it and loads
  as speed 1); the input's encoding in the record log's round trip.
- `vetro-jit-native` `sistema_interprete_e_jit_identici`: one-core cases
  now read a counter divided by 4 as well; red without the division in the
  regions (seed 100).
- The Linux guest's boot in Node (`web/node/boot.mjs --speed N`): the
  interpreter and the JIT give the same instruction count and log at speed
  1 (284786912, as before), 4 (659758564) and 16 (2199749034). The first
  try at speed 4 with the JIT ended in the guest kernel's RCU stall
  warnings: that was the missing division.
- `tests/web/browser-speed.mjs` (step 20 of `tools/web-test.sh`): the app in
  headless Chrome; at `&speed=1` a shell loop cannot finish before its
  instructions' worth of real time; with the automatic speed it is over
  sooner, the stats show a speed above 1, `sleep 2` still takes two
  seconds, and a second session restores the snapshot saved at the raised
  speed.

## Measurements
- Mac (Apple M2 Pro), headless Chrome, the Linux guest, a shell loop of
  60 000 iterations (`tests/web/browser-speed.mjs`): 9.58 s at speed 1 (the
  clock's limit), 3.47 s with the automatic speed (×4 while the loop runs,
  the host doing 365–420 M instructions per second; ×8 by its end);
  `sleep 2` 2.05 s.
- Android on hosted runners (`android-run/exp-speed2`, three runs with the
  automatic speed and two at speed 1; `docs/progress/M4.md`, 2026-11-16):
  ×1 while an app opens (the host executes about 30 M instructions per
  second of the guest's new code there, so the clock is not its limit), ×2
  with the phone at rest. The times overlap: Settings on screen in
  10.3–13.0 s against 9.3–10.1 s, an installed app in 9.6–12.8 s against
  11.1–12.0 s. Nothing gained and nothing clearly lost, as expected; the
  gain is for a host faster than the clock, and that is not measured with
  Android yet.

## Maintenance burden
2/5. Two fields and three conversions in the machine (`clock`, `rate`,
`steps_at`), one optional snapshot section, one input. Whoever converts
between instructions and guest time must go through them: a new direct use
of `steps` as time would be wrong at any speed but 1, and only the tests
above would notice.

## Consequences
- A record log with a speed change has input tag 10: a build before this
  ADR does not read it.
- `vetro_guest_ns` and the page's "guest time" are nominal steps, no longer
  instructions; the page's stats show the speed when it is not 1.
- Whatever in the page or in the tools counts instructions as time (a
  quantum, a keyframe interval, a threshold) means `speed` times less guest
  time at a higher speed: the record keyframes (every 20 M instructions)
  are one such, left as they are (more keyframes while recording at a high
  speed).
- Open: the automatic adjustment with the cores in parallel (the machine
  refuses a change there today); the adjustment's constants were chosen by
  reasoning, not tuned on measurements.
