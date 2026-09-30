#!/usr/bin/env node
// Where the Android workload's time goes (M4, ADR 0040): digests a V8 CPU
// profile of tools/aosp/android-perf.mjs run with --profile (region functions
// named r<el>_<pc> in the modules' name section) and --samples (guest
// /proc/kallsyms and executable mappings, instruction-weighted PC samples).
//
//   node tools/aosp/perf-report.mjs PROFILE.cpuprofile [--base=target/aosp/perf] [--top=40] [--from=S] [--until=S]
//
// Prints:
//   - self time per category: region code, runtime (rt.*), host functions
//     called from regions (env.ld/st/resolve/simd...), the machine loop and
//     the JIT's per-run work, the interpreter, devices, WASM compilation, GC;
//   - the top functions by self time;
//   - region time (self + the rt/host calls under it) per guest code area:
//     kernel symbol (EL1) or library (EL0), with the instruction share from
//     the PC samples when present.

import { existsSync, readFileSync } from 'node:fs';

const file = process.argv[2];
const arg = (name, def) => {
  const a = process.argv.find((s) => s.startsWith(`--${name}=`));
  return a ? a.slice(name.length + 3) : def;
};
const base = arg('base', 'target/aosp/perf');
const top = Number(arg('top', 40));
// Window of the profile, in seconds from its start (e.g. one phase).
const from = Number(arg('from', 0)) * 1e6;
const until = Number(arg('until', Infinity)) * 1e6;
const p = JSON.parse(readFileSync(file, 'utf8'));
const byId = new Map(p.nodes.map((x) => [x.id, x]));
const parent = new Map();
for (const n of p.nodes) for (const c of n.children ?? []) parent.set(c, n.id);
const self = new Map();
let total = 0;
let clock = 0;
for (let i = 0; i < p.samples.length; i++) {
  const d = p.timeDeltas[i] ?? 0;
  clock += d;
  if (clock < from || clock > until) continue;
  self.set(p.samples[i], (self.get(p.samples[i]) ?? 0) + d);
  total += d;
}

/** Light demangling of Rust v0/legacy symbols: the identifiers joined by ::. */
function demangle(s) {
  if (!/^_R|^_ZN/.test(s)) return s;
  const ids = [];
  const re = /(\d+)(_?)([A-Za-z_][A-Za-z0-9_]*)/g;
  for (let at = 2; at < s.length; ) {
    re.lastIndex = at;
    const m = re.exec(s);
    if (!m) break;
    const n = Number(m[1]);
    const start = m.index + m[1].length + m[2].length;
    const id = s.slice(start, start + n);
    if (n > 0 && /^[A-Za-z_][A-Za-z0-9_]*$/.test(id) && !/^[0-9a-f]{16}$|^h[0-9a-f]{16}$/.test(id)) ids.push(id);
    at = start + Math.max(n, 1);
  }
  return ids.filter((x) => !/^Cs[0-9a-zA-Z_]+$/.test(x)).join('::') || s;
}

const region = /^r(\d)_([0-9a-f]+)$/;
const rt = /^(save|ld_slow|st_slow|nzcv|ld\d_\d|st\d_\d|ldp\d_\d|stp\d_\d|ldp_slow|stp_slow|finish|vsync|ldq\d|stq\d|ldq_slow|stq_slow|ldu\d|stu\d|simd|fp\d+|cr\d+)$/;

function category(name, url) {
  if (region.test(name)) return 'region code';
  if (rt.test(name)) return `rt (${name.replace(/\d+$/, 'N').replace(/^(ld|st|ldp|stp|ldq|stq|ldu|stu)N?_?N?$/, '$1*')})`;
  if (name === 'b0' && url.startsWith('wasm://')) return 'dispatcher';
  const d = demangle(name);
  if (/helper::exec/.test(d)) return 'env.simd (interpreter helper)';
  if (/SysHost.*::(ld|st)\b|jit_ld|jit_st|vetro_jit_ld|vetro_jit_st/.test(d)) return 'host ld/st (TLB miss)';
  if (/resolve|Cache.*lookup/.test(d)) return 'env.resolve / lookup';
  if (/vetro_mmu/.test(d)) return 'MMU (walks, TLB)';
  if (/vetro_jit::(sys|driver)/.test(d) || /SysJit/.test(d)) return 'JIT host (per run)';
  if (/vetro_jit::translate|vetro_jit::wasm/.test(d)) return 'JIT translation';
  if (/vetro_jit/.test(d)) return 'JIT other';
  if (/vetro_cpu/.test(d)) return 'interpreter';
  if (/vetro_machine::(dev|virtio|gic|timer|uart|rtc|display|input)|virtio|gic|Gic|Virtio/.test(d)) return 'devices';
  if (/vetro_machine/.test(d)) return 'machine';
  if (/vetro_snapshot|lzh/.test(d)) return 'snapshot';
  if (/vetro_net/.test(d)) return 'network';
  if (/vetro_wasm/.test(d)) return 'vetro-wasm glue';
  if (/^(memcpy|memset|memmove|dlmalloc|__rust|alloc|core::|_ZN4core|_ZN5alloc)/.test(d) || /^(core|alloc|std)::/.test(d)) return 'Rust runtime (memcpy, alloc)';
  if (name === '(garbage collector)') return 'GC';
  if (name === '(program)' || name === '(idle)' || name === '(root)') return name;
  if (/WebAssembly|compile|instantiate/i.test(name)) return 'WASM compilation (JS)';
  if (url.startsWith('wasm://')) return `wasm other (${d.slice(0, 40)})`;
  return `JS (${url.split('/').pop() || '?'})`;
}

const ms = (us) => (us / 1000).toFixed(0);
const pct = (us) => ((100 * us) / total).toFixed(1);

// 1. Categories.
const cats = new Map();
const funcs = new Map();
for (const [id, t] of self) {
  const n = byId.get(id);
  const { functionName: name, url } = n.callFrame;
  const c = category(name || '(anon)', url || '');
  cats.set(c, (cats.get(c) ?? 0) + t);
  const k = region.test(name) ? 'r* (region code)' : `${demangle(name || '(anon)').slice(0, 90)}`;
  funcs.set(k, (funcs.get(k) ?? 0) + t);
}
console.log(`total ${(total / 1e6).toFixed(1)} s sampled`);
console.log('\n== self time per category');
for (const [k, v] of [...cats].sort((a, b) => b[1] - a[1])) console.log(`${ms(v).padStart(9)} ms ${pct(v).padStart(5)}%  ${k}`);
// --cat=REGEX: the functions of the matching categories.
const catRe = arg('cat', null);
if (catRe) {
  const rx = new RegExp(catRe);
  const inCat = new Map();
  for (const [id, t] of self) {
    const n = byId.get(id);
    const { functionName: name, url } = n.callFrame;
    if (!rx.test(category(name || '(anon)', url || ''))) continue;
    const k = demangle(name || '(anon)').slice(0, 110);
    inCat.set(k, (inCat.get(k) ?? 0) + t);
  }
  console.log(`\n== functions in categories /${catRe}/`);
  for (const [k, v] of [...inCat].sort((a, b) => b[1] - a[1]).slice(0, top)) console.log(`${ms(v).padStart(9)} ms ${pct(v).padStart(5)}%  ${k}`);
}
console.log(`\n== top ${top} functions (self)`);
for (const [k, v] of [...funcs].sort((a, b) => b[1] - a[1]).slice(0, top)) console.log(`${ms(v).padStart(9)} ms ${pct(v).padStart(5)}%  ${k}`);

// 2. Time under each region (self + callees), and which callees.
const under = new Map(); // region node id -> {pc, el, self, callees: Map}
function regionOf(id) {
  for (let cur = id; cur !== undefined; cur = parent.get(cur)) {
    const n = byId.get(cur);
    if (region.test(n.callFrame.functionName)) return cur;
  }
  return undefined;
}
const calleeCats = new Map();
let inRegions = 0;
for (const [id, t] of self) {
  const r = regionOf(id);
  if (r === undefined) continue;
  inRegions += t;
  const [, el, pc] = region.exec(byId.get(r).callFrame.functionName);
  const key = `${el} ${pc}`;
  const e = under.get(key) ?? { el: Number(el), pc: BigInt(`0x${pc}`), t: 0, self: 0 };
  e.t += t;
  if (id === r) e.self += t;
  under.set(key, e);
  if (id !== r) {
    const n = byId.get(id);
    const c = category(n.callFrame.functionName || '(anon)', n.callFrame.url || '');
    calleeCats.set(c, (calleeCats.get(c) ?? 0) + t);
  }
}
console.log(`\n== under regions: ${ms(inRegions)} ms (${pct(inRegions)}%); callees by category`);
for (const [k, v] of [...calleeCats].sort((a, b) => b[1] - a[1]).slice(0, 20)) console.log(`${ms(v).padStart(9)} ms ${pct(v).padStart(5)}%  ${k}`);

// 3. Guest code areas.
const syms = [];
if (existsSync(`${base}.kallsyms`)) {
  for (const l of readFileSync(`${base}.kallsyms`, 'utf8').split('\n')) {
    const m = /^([0-9a-f]+) [tTwW] (\S+)/.exec(l);
    if (m && m[1] !== '0000000000000000') syms.push([BigInt(`0x${m[1]}`), m[2]]);
  }
  syms.sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0));
}
const maps = [];
if (existsSync(`${base}.maps`)) {
  // android-perf.mjs --samples: `ps -A -o PID,NAME`, then
  // `/proc/<pid>/maps:<line>` for every executable mapping.
  const names = new Map();
  for (const l of readFileSync(`${base}.maps`, 'utf8').split('\n')) {
    const ps = /^\s*(\d+)\s+(\S+)\s*$/.exec(l);
    if (ps) {
      names.set(ps[1], ps[2]);
      continue;
    }
    const m = /^\/proc\/(\d+)\/maps:([0-9a-f]+)-([0-9a-f]+) \S+ [0-9a-f]+ \S+ \d+\s*(.*)$/.exec(l);
    if (m) maps.push([BigInt(`0x${m[2]}`), BigInt(`0x${m[3]}`), (m[4] || '[anon]').split('/').pop(), names.get(m[1]) ?? m[1]]);
  }
}
function kernelSym(pc) {
  let lo = 0;
  let hi = syms.length - 1;
  if (hi < 0 || pc < syms[0][0]) return '[kernel]';
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (syms[mid][0] <= pc) lo = mid;
    else hi = mid - 1;
  }
  return syms[lo][1];
}
function userLib(pc) {
  const libs = new Set();
  for (const [a, b, name] of maps) if (pc >= a && pc < b) libs.add(name);
  if (libs.size === 0) return '[user ?]';
  return [...libs].sort().slice(0, 3).join('|') + (libs.size > 3 ? '|...' : '');
}
const area = (el, pc) => (el === 1 ? `k ${kernelSym(pc)}` : `u ${userLib(pc)}`);
const areas = new Map();
for (const e of under.values()) {
  const a = area(e.el, e.pc);
  const x = areas.get(a) ?? { t: 0, self: 0, samples: 0 };
  x.t += e.t;
  x.self += e.self;
  areas.set(a, x);
}
// Instruction-weighted PC samples (android-perf.mjs --samples).
let nSamples = 0;
if (existsSync(`${base}.samples`)) {
  for (const l of readFileSync(`${base}.samples`, 'utf8').split('\n')) {
    const m = /^(\d+) \S+ (\d) [0-9a-f]+ ([0-9a-f]+)$/.exec(l);
    if (!m) continue;
    const a = area(Number(m[2]) >= 1 ? 1 : 0, BigInt(`0x${m[3]}`));
    const x = areas.get(a) ?? { t: 0, self: 0, samples: 0 };
    x.samples += Number(m[1]);
    nSamples += Number(m[1]);
    areas.set(a, x);
  }
}
console.log(`\n== guest code areas: time under regions (self in the region's own code), share of guest instructions (${nSamples} samples)`);
console.log('     time ms  self ms  %time  %instr  area');
for (const [k, v] of [...areas].sort((a, b) => b[1].t - a[1].t).slice(0, top)) {
  const ins = nSamples ? ((100 * v.samples) / nSamples).toFixed(1) : '-';
  console.log(`${ms(v.t).padStart(11)} ${ms(v.self).padStart(8)} ${pct(v.t).padStart(6)} ${String(ins).padStart(7)}  ${k}`);
}
// The hottest regions themselves (--regions=N).
const nRegions = Number(arg('regions', 0));
if (nRegions) {
  console.log(`\n== top ${nRegions} regions: time under the region (self), area`);
  for (const e of [...under.values()].sort((a, b) => b.t - a.t).slice(0, nRegions)) {
    console.log(`${ms(e.t).padStart(9)} ms (${ms(e.self).padStart(7)} self) ${pct(e.t).padStart(5)}%  r${e.el}_${e.pc.toString(16)}  ${area(e.el, e.pc)}`);
  }
}
// Coarse: kernel vs user, and by library for user.
const coarse = new Map();
for (const [k, v] of areas) {
  const c = k.startsWith('k ') ? 'kernel (EL1)' : k;
  const x = coarse.get(c) ?? { t: 0, samples: 0 };
  x.t += v.t;
  x.samples += v.samples;
  coarse.set(c, x);
}
console.log('\n== kernel vs user libraries');
for (const [k, v] of [...coarse].sort((a, b) => b[1].t - a[1].t).slice(0, 25)) {
  const ins = nSamples ? ((100 * v.samples) / nSamples).toFixed(1) : '-';
  console.log(`${ms(v.t).padStart(11)} ms ${pct(v.t).padStart(6)}% time ${String(ins).padStart(6)}% instr  ${k}`);
}
