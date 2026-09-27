# ADR 0036 — Responsive input and lighter graphics for Android in the app

- Status: accepted (M5/M6, 2026-09-27). Builds on ADR 0014 (disk waits and
  guest time), 0028 (AOSP in the browser), 0031 (prebuilt snapshot, nightly
  job), 0035 (device profiles). Measurements: `docs/progress/M5.md`
  (2026-09-27, "Responsiveness").

## Context
The owner reported that clicks on the phone "seem to do nothing". Measured in
headless Chrome on the build VM (`tests/web/android-responsiveness.mjs`, a
new diagnostic that taps the test app `tests/apps/tocco`, which flips colour
at every touch, and times the press to the frame that changes it):

- **The first tap after an install waited for a snapshot.** The Worker saves
  the "app installed" snapshot right after `am start` returns, exactly when
  the user taps the app; the save stops the guest for 15–25 s (1 GB, fast
  level; 60 s on a loaded machine). The nightly job's click took 18.5 s on
  the GitHub runner and 30.7 s on the build VM, almost all of it the save.
- **The guest is slow and busy after a start.** Restored from a snapshot the
  guest runs at 5–17 MIPS in Chrome on the (shared) VM, guest time at
  0.05–0.14 of real time, and its own load average is 24–28 (SystemUI,
  system_server, microG, phone, statementservice): a tap takes 2–5 s of
  wall time to reach the screen, 4–15 s under heavy host load. Most of this
  is guest work per frame and CPU speed (the JIT is another agent's work).
- **The Worker added latency of its own.** Quanta were 1M instructions: at
  10 MIPS one quantum is 100 ms, and the page's messages only get in between
  slices, so an input waited 10–240 ms. Between slices it slept with
  `setTimeout(0)`, which browsers clamp to at least 4 ms once timeouts nest
  (a quarter of a 12 ms slice). With real time on, a WFI jumped to its timer
  deadline inside a quantum, so guest time could run ahead of the real clock
  by a whole timer period and an input then waited for the clock.
- Presenting a frame is cheap: `putImageData` of the changed rectangle took
  0.2–4 ms (max 30 ms under host load), one copy out of the module's memory
  in the Worker, transferred.

## Decision
1. **A WFI stops at the end of its quantum** (`Machine::run`): a timer
   deadline beyond the quantum's end stops time there and the WFI resumes in
   the next quantum. The guest sees nothing different (nothing happens in a
   WFI but interrupts; unit test with small and large quanta), replay is
   unchanged (the browser replay tests are identical), and the host's clock
   is overtaken by at most one quantum: an input wakes the guest at its
   instruction, not at the next timer.
2. **Worker scheduling:** quanta sized from the measured speed (about 3 ms,
   20k–1M instructions); with real time the guest waits when it is more than
   4 ms ahead of the clock, and an input ends the wait at once; between
   slices a `MessageChannel` message to itself instead of `setTimeout(0)`
   (`scheduler.yield()` is not used: its continuation is meant to run ahead
   of other tasks, which could keep the page's inputs waiting). Inputs still
   reach the machine between quanta at a recorded instruction count
   (`inputLog`, the machine's recording).
3. **Automatic snapshots wait for a pause:** the "app installed" (and home
   screen) snapshot is saved after 4 s without user input, at most 60 s after
   it was requested; the Save button is immediate. While saving, a note on the
   screen says the machine is saving and will answer when done.
4. **A touch ripple drawn by the page** (a CSS animation at the press point):
   the user sees at once that the press arrived, whatever the guest's delay.
5. **Lighter graphics by default** (no AOSP rebuild):
   - the web app selects a new starter profile, **`light`: 960x600 at
     180 dpi**, the same 853x533 dp layout as the image's 1280x800 at 240 dpi
     with 56% of the pixels (the HWC's per-pixel BGRA conversion, the
     composition and every app frame scale with them). `default` stays the
     image's own machine, the CLI's default, offered as "Full resolution".
     Both have a prebuilt home-screen snapshot for the default image
     (`PREBUILT_PROFILES`); `prebuilt-key.mjs` computes the app's profile key;
     the nightly job uses the app's default;
   - after adb connects (and in the prebuilt snapshot): window and
     transition animation scales 0, animator duration scale 0.5, window blurs
     disabled (`ANDROID_GRAPHICS.light`; idempotent settings in `/data`); a
     "full animations and blurs" box (`graphics=full`) restores Android's
     values.
6. **Frames stay drawn on the page's thread.** The Worker is the bottleneck
   (it runs the guest); moving `putImageData` into it with an
   `OffscreenCanvas` would take time from the guest to save the page thread
   work it does in parallel, and the tests read the canvas pixels. The one
   copy out of the module's memory is needed because the guest keeps writing
   there.
7. **The nightly job enforces M6's 15 s** (the second start's first frame)
   and reports three tap latencies after the second start.

## Rejected
- `OffscreenCanvas`/`transferControlToOffscreen` for presenting (above).
- Coalesced pointer events: more touch points for a guest that already
  cannot keep up; Android interpolates moves itself.
- 720x450 as the default: a third of the pixels, but 135 dpi text is blurry
  when the canvas is scaled to the page; it can be a user profile file.
- Resizing the display at run time instead of a new prebuilt snapshot: the
  ranchu HWC takes the mode from DRM at start.

## Consequences
- A new prebuilt snapshot per image version for the `light` profile (made with
  `tools/aosp/prebuilt-snapshot.mjs --profile=light`), next to the
  full-resolution one.
- Page tests that read the canvas size must not assume 1280x800 for Android.
