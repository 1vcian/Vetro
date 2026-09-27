#!/usr/bin/env node
// M10: record & replay via vetro-wasm (ABI 8, ADR 0019 and 0023) on the M3
// guest kernel with the network:
//   - recording from the shell (keyframe every 10 M instructions): DHCP, a
//     JSON POST with wget to the sinkhole, a command; halfway registers and
//     memory (at the address of VBAR_EL1) are taken for the comparison;
//   - the log goes through a file; the keyframes leave the machine towards
//     the archive (`Recording` of web/node/recording.mjs, the same as the app's
//     Worker, here with the archive in memory instead of OPFS) and come back
//     only when needed; the log reassembled from the archive equals the file;
//   - replay on a new machine from the initial keyframe: it ends with the
//     recorded state ("identical replay", same instructions), the console is
//     the same byte for byte, the replay's inspector and timeline are
//     those of the recording (same requests, same HAR);
//   - jump to the instruction taken halfway on another machine: same
//     registers and same memory;
//   - a keyframe put back with a changed byte: the replay is not identical;
//   - with and without JIT in the replay (the recording with the JIT).
//
//   node tests/web/replay.mjs [--no-jit]

import { DEV, Machine } from '../../web/node/vetro.mjs';
import { SnapshotStore } from '../../web/node/persist.mjs';
import { Recording } from '../../web/node/recording.mjs';
import { check, guestKernel, loadVetro, normalize, POST_JSON, run, Session, SHELL_PROMPT } from './lib.mjs';

const jit = !process.argv.includes('--no-jit');
const KEYFRAMES = 10_000_000;
const QUANTUM = 1_000_000;

/** A machine like the session's, without a kernel: it starts from a keyframe. */
function fresh(exports, useJit) {
  const m = new Machine(exports, { devices: DEV.DEFAULT });
  if (useJit) m.setJit(16, 16);
  return m;
}

/** Loads the log and takes out all the keyframes (returned as an array). */
function loadOut(m, file) {
  m.logLoad(file);
  const info = m.logInfo();
  const kfs = [];
  for (let i = 0; i < info.keyframes; i++) kfs.push(m.logKeyframeTake(i));
  return { info, kfs };
}

function consoleText(m) {
  return Buffer.from(m.consoleRead()).toString('latin1');
}

run(async () => {
  const { exports } = await loadVetro();
  const kernel = guestKernel();

  // Recording.
  const s = new Session(exports, kernel, { jit, machine: { devices: DEV.DEFAULT } });
  s.m.capture(true);
  let at = await s.until(SHELL_PROMPT);
  s.m.recordStart(KEYFRAMES);
  check(s.m.rrStatus().state === 'Recording', 'recording not started');
  const logFrom = s.log.length;
  [, at] = await s.command('udhcpc -i eth0 -n -q', at);
  [, at] = await s.command(POST_JSON, at);
  const mid = s.m.steps;
  const regs = s.m.registersText();
  const vbar = BigInt(`0x${/vbar_el1\s+([0-9a-f]+)/.exec(regs)[1]}`);
  const mem = s.m.readVirt(vbar, 256);
  check(mem.bytes && mem.bytes.some((b) => b !== 0), `memory at VBAR_EL1 ${vbar.toString(16)}: ${JSON.stringify(mem)}`);
  [, at] = await s.command('echo VETRO-FINE-$((6*7))', at);
  check(s.m.recordStop(), 'recording not finished');
  const st = s.m.rrStatus();
  check(st.state === 'Idle' && st.hasLog && st.events === 3 && st.keyframes >= 3 && st.endSteps === Number(s.m.steps), `state after the recording: ${JSON.stringify(st)}`);
  const recorded = s.log.slice(logFrom);
  const reqs = s.m.inspectRequests().requests;
  const liveTimeline = s.m.timeline(0);
  const file = s.m.logEncode();
  check(Buffer.from(file.subarray(0, 8)).toString() === 'VETROREC', 'log header');
  const events = s.m.logEvents();
  check(events.some((e) => e.user && e.label === `Enter: ${POST_JSON}`), 'the command among the log events');
  console.log(`recording: ${st.events} events, ${st.keyframes} keyframes, ${Number(s.m.steps) - st.startSteps} instructions, ` +
    `log ${(file.length / 2 ** 20).toFixed(1)} MiB (events ${s.m.logInfo().eventsBytes} bytes)`);

  // The keyframe archive (in memory, like OPFS in the Worker).
  const store = SnapshotStore.memory();
  for (const replayJit of [jit, !jit]) {
    // Replay from the initial keyframe on a new machine.
    const m = fresh(exports, replayJit);
    const rec = new Recording(m, store);
    const meta = await rec.load(file);
    const info = m.logInfo();
    check(info.sameMachine && info.startSteps === st.startSteps && info.endSteps === st.endSteps, `log read back: ${JSON.stringify(info)}`);
    check(meta.keyframes === st.keyframes && [...Array(meta.keyframes).keys()].every((i) => !m.logKeyframe(i).present), 'keyframes not moved');
    let err = null;
    try { m.replayStart(0); } catch (e) { err = e; }
    check(err?.code === 'KeyframeMissing', `without keyframe: ${err}`);
    check(await rec.ensureKeyframe(0) === 0, 'the first keyframe');
    m.capture(true);
    m.replayStart(0);
    rec.dropKeyframes();
    check(Number(m.steps) === info.startSteps, `start at ${m.steps}`);
    let out = '';
    let status;
    while ((status = m.rrStatus()).state === 'Replaying') {
      m.run(QUANTUM);
      out += consoleText(m);
    }
    check(status.state === 'Finished', `replay: ${status.state} ${status.message}`);
    check(Number(m.steps) === info.endSteps, `replay finished at ${m.steps}, recording at ${info.endSteps}`);
    check(normalize(out) === normalize(recorded), `console del replay diversa:\n${normalize(out).slice(-400)}\n---\n${normalize(recorded).slice(-400)}`);
    const rreqs = m.inspectRequests().requests;
    check(JSON.stringify(rreqs) === JSON.stringify(reqs), "the replay's inspector differs from the recording's");
    const tl = m.timeline(0);
    const http = tl.effects.find((e) => e.kind === 'http');
    check(http && tl.inputs[http.cause]?.label === `Enter: ${POST_JSON}`, `timeline del replay: ${JSON.stringify(http)}`);
    check(liveTimeline.effects.some((e) => e.kind === 'http' && e.atUs === http.atUs), 'request at another instant in the replay');
    const again = await rec.encodeFull();
    check(Buffer.compare(again, file) === 0, 'the log reassembled from the archive differs from the file');
    m.free();

    // Jump to the instruction taken halfway, on a new machine with the log
    // reread from the archive (as after reloading the page).
    const g = fresh(exports, replayJit);
    const grec = new Recording(g, store);
    check((await grec.restore())?.events === 3, 'log not reread from the archive');
    const k = await grec.ensureKeyframe(Number(mid));
    g.replayStart(Number(mid));
    grec.dropKeyframes();
    const from = Number(g.steps);
    while (g.steps < mid) g.run(Math.min(QUANTUM, Number(mid - g.steps)));
    check(g.steps === mid, `jump: ${g.steps} instead of ${mid}`);
    check(g.registersText() === regs, `registers differ at the jump:\n${g.registersText()}\n---\n${regs}`);
    check(Buffer.compare(g.readVirt(vbar, 256).bytes, mem.bytes) === 0, 'memory differs at the jump');
    check(g.translate(vbar) !== null && g.readVirt(0x10n, 4).fault === 0x10n, 'translation and unmapped address');
    console.log(`replay${replayJit ? ' (JIT)' : ' (interpreter)'}: identical (${info.endSteps - info.startSteps} instructions, ` +
      `console and inspector equal); jump to ${mid} from keyframe ${k} (${from}): same registers and memory`);
    g.free();
  }

  // An altered keyframe: the replay is not identical.
  const bad = fresh(exports, jit);
  const lo = loadOut(bad, file);
  const k0 = lo.kfs[0].slice();
  // A byte of RAM towards the end of the snapshot (after the headers).
  k0[k0.length - 100] ^= 0x55;
  let refused = null;
  try {
    bad.logKeyframePut(0, k0);
    bad.replayStart(0);
    while (bad.rrStatus().state === 'Replaying') bad.run(QUANTUM);
  } catch (e) {
    refused = e;
  }
  const bst = bad.rrStatus();
  check(refused || bst.state === 'Diverged', `altered keyframe: ${bst.state}`);
  console.log(`altered keyframe: ${refused ? `refused (${refused.message.slice(0, 80)})` : `divergence (${bst.message.slice(0, 100)})`}`);
});
