# ADR 0023 — Network inspector, input→effects timeline and record & replay in the browser

- Status: accepted (M7 and M10, web part, 2026-09-26). Extends ADR 0016
  (network analysis) and ADR 0019 (record & replay); vetro-wasm moves
  to ABI 8.
- Number: 0021 is already used by two branches in progress (SQLite and
  preferences editing, AOSP image) and 0022 is likely for their reordering; if
  the numbers are free at merge time, it can be renumbered without touching
  the substance.

## Context
Network analysis (ADR 0016) and record & replay (ADR 0019) exist in the
machine and in the CLI, but not in the page. M7 asks for a network inspector
and the input→effects timeline; M10 recording and replay in the browser with
jumping to a moment and the view of registers and memory. Constraints:
`vetro-analysis` without dependencies and deterministic, no change to
`vetro-machine`, `vetro-net` and the JIT (other owners), the machine runs in a
Worker and the page must not change the execution by looking.

## Decision

### Network inspector in vetro-wasm
- vetro-wasm's `Vm` holds the capture: on every `vetro_run` it moves the
  frames of `Machine::net_tap_take` into a `Capture` (at most 64 MiB, then it
  counts the dropped ones). Turning on, clearing and reading do not touch the
  guest.
- The analysis is that of `vetro_analysis::net` (redone only if frames have
  arrived). List and detail pass to JS as JSON produced by Rust
  (`vetro_analysis::net::view`: camelCase fields, bodies with the decoder's
  rendering, the structure for JSON/form/multipart and the bytes in base64 up
  to 256 KiB), so the page decodes nothing again and the format is tested in
  Rust. HAR and pcapng are those of ADR 0016.
- A single **result buffer** per machine (`vetro_result_ptr`): JSON,
  HAR, pcapng, log, keyframes and registers all come out of it, valid until
  the next result.

### Timeline: model in vetro-analysis, declared heuristic
- `vetro_analysis::timeline`: user inputs (`UserInput`: instruction,
  kind, text, *weak* or command) and effects (`Effect`: http, dns, tls,
  file, console) in **guest time in µs** (`instructions / 100`, the same
  as the captured frames).
- **Attribution**: an effect belongs to the last input preceding it within
  a window (3 s of guest time, chosen by the page). For network and files
  only command inputs count (Enter, click, tap, file manager command, power
  button): a character typed in the middle of a line does not cause a
  request; for console output the character counts too (the echo). At the
  same instant arrival order applies: output read at the end of a quantum
  comes before the inputs given at that boundary. It is a heuristic, not
  causality: the guest may make requests on its own within the window. True
  causality will come from the syscall and Binder tracers (M8–M9), which can
  replace the rule without changing the model.
- **Inputs**: vetro-wasm describes them itself from the `Input` it passes to
  `Machine::input` (pressed keys, buttons, new touches, console lines
  reconstructed by `LineEditor`, power button, resolution; not movements,
  releases, automatic terminal replies, vsock and host network). The same
  description applies to the events of a log: the timeline of a replay is
  rebuilt from the log. File manager commands are annotated by JS (they are
  vsock traffic, they cannot be recognised from the `Input`); panel reads are
  not inputs.
- **Effects**: network from the capture (HTTP request at the start, DNS
  questions, TLS ClientHello); files from the inotify events of the file
  manager's watches (annotated by the Worker: created, written, moved,
  deleted); console when JS reads it (precision: the quantum, ≤ 10 ms of guest
  time), merged until something else arrives.

### Record & replay in the browser
- Recording and replay are those of the machine (ADR 0019). The finished log
  (or one loaded from a file) stays in the `Vm`; the **keyframes are moved
  out** one at a time (`vetro_log_keyframe_take`) and come back only when a
  replay starts from them (`vetro_log_keyframe_put`). In the Worker they go to
  OPFS (`vetro-recordings/`: `kf-<i>` and `log` without their bytes, with the
  same `SnapshotStore` as snapshots; keyframes first, then the log), so the
  module's memory does not hold dozens of ~10 MB snapshots and the recording
  survives a page reload.
- The **downloaded file** is the complete log (`Log::encode` with all the
  keyframes put back for the duration of the encoding): on its own it is
  enough to redo the session on a machine configured the same way, and
  loading it archives it again.
- **Replay and jump** start from the nearest keyframe (`vetro_replay_start`);
  jumping to an instruction is the same start followed by the usual
  `vetro_run` calls with the quantum limited up to there. `Machine::goto` is
  not used: it stops on `Blocked`, while in the browser disks are served
  asynchronously between one quantum and the next. The result is the same
  (quantum boundaries irrelevant, ADR 0014/0015/0019).
- During replay the page sends no inputs (the Worker drops them; the
  machine would ignore them anyway), the file manager client is closed
  (its operations are in the log) and reopens at the end, no real time
  nor cached snapshots. At the end: "identical replay" (`Finished`, same
  fingerprint) or the difference; then the machine continues freely.
- View of the state at the point reached: `Machine::registers_text`,
  `read_virt` (translation with the current tables, RAM only, no effect
  on devices), `translate`, `read_phys`.

### Exports
Blob and `<a download>` in the page: on the published app (GitHub Pages) the
browser downloads the file; the Chrome test sets the download behaviour and
reads the real files.

## Rejected alternatives
- **Timeline and decoding in JS**: two implementations of the same formats,
  no tests in Rust, and the attribution rule would not be the same for
  the future CLI.
- **Keyframes kept in module memory**: 10 MB each at the guest kernel's
  shell (and many more with Android), in a linear memory that does not
  shrink.
- **Keyframes written to OPFS during recording**: it would need a new
  interface in `vetro-machine` (the recorder is private). Today the
  keyframes come out at the end; it will be done with incremental keyframes
  (ADR 0019).
- **Attribution per process or per flow**: without syscall tracers there is
  no way to know which process opened a socket; the temporal heuristic is
  what can be done from outside today, declared as such.

## Verification
- `cargo test -p vetro-analysis`: `timeline` (cause within the window,
  weak inputs, order at the same instant, merged console, JSON, limits,
  console lines, key names, network effects from the analysis) and
  `net::view` (list and detail reread by our JSON parser, multipart,
  protobuf).
- `cargo test -p vetro-wasm`: input description; recording, log file,
  keyframes out and in, identical replay on another machine, jump with the
  same registers, log from another machine rejected; capture, HAR, pcapng
  and timeline from the API.
- `tests/web/inspector.mjs` and `tests/web/replay.mjs` (Node, M3 kernel):
  wget requests with decoded bodies and attributed to the command; identical
  replay (same console, inspector and timeline) with JIT and interpreter, jump
  with the same registers and memory, log reassembled from the archive equal
  to the file.
- `tests/web/browser-analysis.mjs` (Chrome): wget in the inspector with the
  decoded JSON and tied to the command in the timeline, a file write tied to
  its command, download of log/HAR/pcapng, identical replay, jump from the
  timeline with registers and memory dump, log reloaded and replayed.

## Consequences
- ABI 8: users of vetro-wasm update `web/node/vetro.mjs` (constants
  `TIMELINE_INPUT`, `TIMELINE_EFFECT`, `RR_STATE`, `REPLAY_START`).
- File effects do not appear in the timeline of a replay (the file manager
  client is closed); network and console do, identical.
- When the TLS hooks (M7) and the tracers (M8–M9) arrive, the plaintext
  requests become more `HttpExchange`s and more effects: the view and the
  timeline do not change shape.
