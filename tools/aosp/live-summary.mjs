#!/usr/bin/env node
// A compact text summary of a tools/aosp/live-path.mjs run, for CI notices
// (a notice is the only part of a run that can be read without signing in).
//
//   node tools/aosp/live-summary.mjs measurements-first.json [actions|profiles]
//
// actions: one line per action (press to first frame, to the screen changed,
// to the awaited outcome, to settled; guest MIPS; guest seconds in wall
// seconds), the install time, the median MIPS of the 5 s timeline and the
// error, if the run ended with one. profiles: for each Worker CPU profile the
// 14 functions with the most self time.

import { readFileSync } from 'node:fs';

const r = JSON.parse(readFileSync(process.argv[2], 'utf8'));
const what = process.argv[3] ?? 'actions';
const ms = (v) => (v === null || v === undefined ? '-' : `${(v / 1000).toFixed(1)}s`);
const num = (v, d = 1) => (v === null || v === undefined ? '-' : v.toFixed(d));

if (what === 'actions') {
  console.log(`url ${r.url}, ${r.visit} visit${r.error ? `, ERROR: ${String(r.error).split('\n')[0]}` : ''}`);
  for (const a of r.actions ?? []) {
    console.log(`${a.name}: frame ${ms(a.frameMs)}, changed ${ms(a.changedMs)}, done ${ms(a.doneMs)}, settled ${ms(a.settledMs)}, ${num(a.mips)} MIPS, guest ${num(a.guestS)}s in ${num(a.wallS)}s`);
  }
  if (r.install) console.log(`${r.install.app} installed in ${ms(r.install.ms)}`);
  const mips = (r.timeline ?? []).map((t) => t.mips).filter((v) => typeof v === 'number').sort((a, b) => a - b);
  if (mips.length) console.log(`timeline: ${mips.length} samples, MIPS median ${num(mips[mips.length >> 1])}, max ${num(mips.at(-1))}`);
  if (r.gpu) console.log(`graphics: ${JSON.stringify(r.gpu).slice(0, 300)}`);
} else {
  for (const [k, p] of Object.entries(r)) {
    if (!k.startsWith('profile_')) continue;
    console.log(`${k.slice(8)} (${p.secs}s, guest ${num(p.guestS)}s):`);
    for (const t of p.top.slice(0, 14)) console.log(`  ${t.pct}% ${t.fn}`);
  }
}
