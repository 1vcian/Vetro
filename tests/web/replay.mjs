#!/usr/bin/env node
// M10: record & replay via vetro-wasm (ABI 8, ADR 0019 e 0023) sul kernel
// guest di M3 con la rete:
//   - registrazione dalla shell (keyframe ogni 10 M istruzioni): DHCP, un
//     POST JSON con wget verso il sinkhole, un comando; a metà si prendono
//     registri e memoria (all'indirizzo di VBAR_EL1) per il confronto;
//   - il log passa da un file; i keyframe escono dalla macchina verso
//     l'archivio (`Recording` di web/node/recording.mjs, lo stesso del Worker
//     dell'app, qui con l'archivio in memoria al posto di OPFS) e rientrano
//     solo quando servono; il log ricomposto dall'archivio è uguale al file;
//   - replay su una macchina nuova dal keyframe iniziale: finisce con lo
//     stato registrato ("replay identico", stesse istruzioni), la console è
//     la stessa byte per byte, l'ispettore e la timeline del replay sono
//     quelli della registrazione (stesse richieste, stesso HAR);
//   - salto all'istruzione presa a metà su un'altra macchina: stessi
//     registri e stessa memoria;
//   - un keyframe rimesso con un byte cambiato: il replay non è identico;
//   - con e senza JIT nel replay (la registrazione col JIT).
//
//   node tests/web/replay.mjs [--no-jit]

import { DEV, Machine } from '../../web/node/vetro.mjs';
import { SnapshotStore } from '../../web/node/persist.mjs';
import { Recording } from '../../web/node/recording.mjs';
import { check, guestKernel, loadVetro, normalize, POST_JSON, run, Session, SHELL_PROMPT } from './lib.mjs';

const jit = !process.argv.includes('--no-jit');
const KEYFRAMES = 10_000_000;
const QUANTUM = 1_000_000;

/** Una macchina come quella della sessione, senza kernel: parte da un keyframe. */
function fresh(exports, useJit) {
  const m = new Machine(exports, { devices: DEV.DEFAULT });
  if (useJit) m.setJit(16, 16);
  return m;
}

/** Carica il log e porta fuori tutti i keyframe (restituiti come array). */
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

  // Registrazione.
  const s = new Session(exports, kernel, { jit, machine: { devices: DEV.DEFAULT } });
  s.m.capture(true);
  let at = await s.until(SHELL_PROMPT);
  s.m.recordStart(KEYFRAMES);
  check(s.m.rrStatus().state === 'Recording', 'registrazione non partita');
  const logFrom = s.log.length;
  [, at] = await s.command('udhcpc -i eth0 -n -q', at);
  [, at] = await s.command(POST_JSON, at);
  const mid = s.m.steps;
  const regs = s.m.registersText();
  const vbar = BigInt(`0x${/vbar_el1\s+([0-9a-f]+)/.exec(regs)[1]}`);
  const mem = s.m.readVirt(vbar, 256);
  check(mem.bytes && mem.bytes.some((b) => b !== 0), `memoria a VBAR_EL1 ${vbar.toString(16)}: ${JSON.stringify(mem)}`);
  [, at] = await s.command('echo VETRO-FINE-$((6*7))', at);
  check(s.m.recordStop(), 'registrazione non finita');
  const st = s.m.rrStatus();
  check(st.state === 'Idle' && st.hasLog && st.events === 3 && st.keyframes >= 3 && st.endSteps === Number(s.m.steps), `stato dopo la registrazione: ${JSON.stringify(st)}`);
  const recorded = s.log.slice(logFrom);
  const reqs = s.m.inspectRequests().requests;
  const liveTimeline = s.m.timeline(0);
  const file = s.m.logEncode();
  check(Buffer.from(file.subarray(0, 8)).toString() === 'VETROREC', 'intestazione del log');
  const events = s.m.logEvents();
  check(events.some((e) => e.user && e.label === `Invio: ${POST_JSON}`), 'il comando fra gli eventi del log');
  console.log(`registrazione: ${st.events} eventi, ${st.keyframes} keyframe, ${Number(s.m.steps) - st.startSteps} istruzioni, ` +
    `log ${(file.length / 2 ** 20).toFixed(1)} MiB (eventi ${s.m.logInfo().eventsBytes} byte)`);

  // L'archivio dei keyframe (in memoria, come OPFS nel Worker).
  const store = SnapshotStore.memory();
  for (const replayJit of [jit, !jit]) {
    // Replay dal keyframe iniziale su una macchina nuova.
    const m = fresh(exports, replayJit);
    const rec = new Recording(m, store);
    const meta = await rec.load(file);
    const info = m.logInfo();
    check(info.sameMachine && info.startSteps === st.startSteps && info.endSteps === st.endSteps, `log riletto: ${JSON.stringify(info)}`);
    check(meta.keyframes === st.keyframes && [...Array(meta.keyframes).keys()].every((i) => !m.logKeyframe(i).present), 'keyframe non spostati');
    let err = null;
    try { m.replayStart(0); } catch (e) { err = e; }
    check(err?.code === 'KeyframeMissing', `senza keyframe: ${err}`);
    check(await rec.ensureKeyframe(0) === 0, 'il primo keyframe');
    m.capture(true);
    m.replayStart(0);
    rec.dropKeyframes();
    check(Number(m.steps) === info.startSteps, `partenza a ${m.steps}`);
    let out = '';
    let status;
    while ((status = m.rrStatus()).state === 'Replaying') {
      m.run(QUANTUM);
      out += consoleText(m);
    }
    check(status.state === 'Finished', `replay: ${status.state} ${status.message}`);
    check(Number(m.steps) === info.endSteps, `replay finito a ${m.steps}, registrazione a ${info.endSteps}`);
    check(normalize(out) === normalize(recorded), `console del replay diversa:\n${normalize(out).slice(-400)}\n---\n${normalize(recorded).slice(-400)}`);
    const rreqs = m.inspectRequests().requests;
    check(JSON.stringify(rreqs) === JSON.stringify(reqs), 'ispettore del replay diverso da quello della registrazione');
    const tl = m.timeline(0);
    const http = tl.effects.find((e) => e.kind === 'http');
    check(http && tl.inputs[http.cause]?.label === `Invio: ${POST_JSON}`, `timeline del replay: ${JSON.stringify(http)}`);
    check(liveTimeline.effects.some((e) => e.kind === 'http' && e.atUs === http.atUs), 'richiesta a un altro istante nel replay');
    const again = await rec.encodeFull();
    check(Buffer.compare(again, file) === 0, 'il log ricomposto dall\'archivio è diverso dal file');
    m.free();

    // Salto all'istruzione presa a metà, su una macchina nuova col log
    // riletto dall'archivio (come dopo aver ricaricato la pagina).
    const g = fresh(exports, replayJit);
    const grec = new Recording(g, store);
    check((await grec.restore())?.events === 3, 'log non riletto dall\'archivio');
    const k = await grec.ensureKeyframe(Number(mid));
    g.replayStart(Number(mid));
    grec.dropKeyframes();
    const from = Number(g.steps);
    while (g.steps < mid) g.run(Math.min(QUANTUM, Number(mid - g.steps)));
    check(g.steps === mid, `salto: ${g.steps} invece di ${mid}`);
    check(g.registersText() === regs, `registri diversi al salto:\n${g.registersText()}\n---\n${regs}`);
    check(Buffer.compare(g.readVirt(vbar, 256).bytes, mem.bytes) === 0, 'memoria diversa al salto');
    check(g.translate(vbar) !== null && g.readVirt(0x10n, 4).fault === 0x10n, 'traduzione e indirizzo non mappato');
    console.log(`replay${replayJit ? ' (JIT)' : ' (interprete)'}: identico (${info.endSteps - info.startSteps} istruzioni, ` +
      `console e ispettore uguali); salto a ${mid} dal keyframe ${k} (${from}): stessi registri e memoria`);
    g.free();
  }

  // Un keyframe alterato: il replay non è identico.
  const bad = fresh(exports, jit);
  const lo = loadOut(bad, file);
  const k0 = lo.kfs[0].slice();
  // Un byte della RAM verso la fine dello snapshot (dopo le intestazioni).
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
  check(refused || bst.state === 'Diverged', `keyframe alterato: ${bst.state}`);
  console.log(`keyframe alterato: ${refused ? `rifiutato (${refused.message.slice(0, 80)})` : `divergenza (${bst.message.slice(0, 100)})`}`);
});
