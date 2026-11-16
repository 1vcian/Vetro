#!/usr/bin/env node
// A compact text summary of a tools/aosp/live-path.mjs run, for CI notices
// (a notice is the only part of a run that can be read without signing in).
//
//   node tools/aosp/live-summary.mjs measurements-first.json [actions|profiles]
//
// actions: one line per action (press to first frame, to the screen changed,
// to the awaited outcome, to settled; guest MIPS; guest seconds in wall
// seconds), the install time, the median MIPS of the 5 s timeline and the
// error, if the run ended with one, and the snapshots saved (in the
// background or in the foreground, with the time the machine's thread spent).
// profiles: for each Worker CPU profile the self time by kind of code and
// the 22 functions with the most.

import { readFileSync } from 'node:fs';

const r = JSON.parse(readFileSync(process.argv[2], 'utf8'));
const what = process.argv[3] ?? 'actions';
const ms = (v) => (v === null || v === undefined ? '-' : `${(v / 1000).toFixed(1)}s`);
const num = (v, d = 1) => (v === null || v === undefined ? '-' : v.toFixed(d));

if (what === 'actions') {
  console.log(`url ${r.url}, ${r.visit} visit${r.error ? `, ERROR: ${String(r.error).split('\n')[0]}` : ''}`);
  for (const a of r.actions ?? []) {
    console.log(`${a.name}: frame ${ms(a.frameMs)}, changed ${ms(a.changedMs)}, done ${ms(a.doneMs)}, settled ${ms(a.settledMs)}, ${num(a.mips)} MIPS, guest ${num(a.guestS)}s in ${num(a.wallS)}s${a.speed > 1 ? `, speed ×${a.speed}` : ''}`);
    // The JIT's counters for the actions that keep the guest busy (the
    // others are the idle loop's).
    const j = a.jit;
    if (j && a.steps > 2e8 && /drawer|Settings|open |start /.test(a.name)) {
      const M = (v) => `${num((v ?? 0) / 1e6)}M`;
      const k = (v) => `${num((v ?? 0) / 1e3, 0)}k`;
      const per = (x, y) => (y ? num(x / y) : '-');
      const exits = ['exitInPage', 'exitCallOut', 'exitOut', 'exitRet', 'exitBr', 'exitBlr', 'exitLimit'].filter((e) => j[e]).map((e) => `${e.slice(4)} ${M(j[e])}`).join(', ');
      console.log(`  jit: ${M(a.steps)} instructions, ${num(100 * j.jitSteps / a.steps, 0)}% in regions; ${M(j.calls)} region calls (${per(j.jitSteps, j.calls)} instr each), ` +
        `${M(j.runs)} runs, ${M(j.resolves)} resolves, ${M(j.jcProbes)} jump cache probes, ${M(j.memoHits)} memo hits; chains ${M(j.chains)}, CHAIN returns ${M(j.chainCalls)}, dispatches ${M(j.dispatches)}`);
      console.log(`  jit: ${exits ? `NEXT exits: ${exits}; ` : ''}faults ${k(j.faults)}, svcs ${k(j.svcs)}, yields ${k(j.yields)}, stops ${k(j.stops)}; TLB fills ${k(j.tlbFills)}, host loads ${k(j.hostLds)}, host stores ${k(j.hostSts)}, ` +
        `TLB flushes ${k(j.tlbFlushes)}, epochs ${k(j.epochs)} (regs ${k(j.epochsRegs)}, tlbi ${k(j.epochsTlbi)}, code ${k(j.epochsCode)}), regime switches ${k(j.regimeSwitches)}, base switches ${k(j.baseSwitches)}; ` +
        `+${j.modules} modules, +${j.blocks} blocks, +${num((j.wasmBytes ?? 0) / 2 ** 20)} MiB wasm, ${j.reused} reused, ${j.invalidatedPages} pages invalidated, ${j.evictedModules} modules evicted`);
    }
  }
  if (r.install) console.log(`${r.install.app} installed in ${ms(r.install.ms)}`);
  const mips = (r.timeline ?? []).map((t) => t.mips).filter((v) => typeof v === 'number').sort((a, b) => a - b);
  if (mips.length) console.log(`timeline: ${mips.length} samples, MIPS median ${num(mips[mips.length >> 1])}, max ${num(mips.at(-1))}`);
  // The guest CPU speed over the session (ADR 0051): how long at each.
  const speeds = {};
  for (const t of r.timeline ?? []) speeds[t.speed ?? 1] = (speeds[t.speed ?? 1] ?? 0) + 1;
  if (Object.keys(speeds).some((k) => k !== '1')) console.log(`guest CPU speed, 5 s samples at each: ${Object.entries(speeds).map(([k, n]) => `×${k}: ${n}`).join(', ')}`);
  // The snapshots saved in the session: in the background (ADR 0046: the
  // machine's thread only starts the save and copies) or in the foreground
  // (the machine stops for the whole time).
  for (const s of r.snapshots ?? []) {
    const how = s.background
      ? `in the background: machine thread ${num(s.beginMs, 0)} ms to start + ${num(s.pumpMs, 0)} ms of copies, saver ${ms(s.writeMs)}, ${num((s.raw ?? 0) / 2 ** 20, 0)} MiB raw`
      : `in the foreground: the machine stopped for ${ms((s.saveMs ?? 0) + (s.writeMs ?? 0))}`;
    console.log(`snapshot "${s.why}": ${num((s.size ?? 0) / 2 ** 20, 0)} MiB, ${how}`);
  }
  if (r.gpu) console.log(`graphics: ${JSON.stringify(r.gpu).slice(0, 300)}`);
} else {
  // Rust symbols (v0 mangling) are long: their identifiers are enough here.
  // Outside an identifier, `s…_` (a disambiguator) and `B…_` (a back
  // reference) are skipped; an identifier is its length, then its characters.
  const idents = (sym) => {
    const out = [];
    for (let i = 2; i < sym.length;) {
      const c = sym[i];
      if (c === 's' || c === 'B') {
        const j = sym.indexOf('_', i);
        if (j < 0) break;
        i = j + 1;
      } else if (c >= '0' && c <= '9') {
        let j = i;
        while (sym[j] >= '0' && sym[j] <= '9') j++;
        const n = Number(sym.slice(i, j));
        if (sym[j] === '_') j++;
        out.push(sym.slice(j, j + n));
        i = j + n;
      } else i++;
    }
    return out;
  };
  const short = (fn) => {
    const [sym, where] = fn.split(' ');
    if (!sym.startsWith('_R') || !where?.startsWith('vetro_wasm')) return fn;
    const ids = idents(sym);
    return ids.length > 6 ? `${ids.slice(0, 6).join('::')}::…::${ids.at(-1)}` : ids.join('::');
  };
  for (const [k, p] of Object.entries(r)) {
    if (!k.startsWith('profile_')) continue;
    console.log(`${k.slice(8)} (${p.secs}s, guest ${num(p.guestS)}s):`);
    for (const c of (p.categories ?? []).slice(0, 12)) console.log(`  ${c.pct}% ${c.category}`);
    console.log('  top functions:');
    for (const t of p.top.slice(0, 22)) console.log(`    ${t.pct}% ${short(t.fn)}`);
  }
}
