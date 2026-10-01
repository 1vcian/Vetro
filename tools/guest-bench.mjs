#!/usr/bin/env node
// M4 steady-state benchmarks in the guest (ADR 0024, 0026, 0041): boots the
// M3 guest kernel under vetro-wasm in Node (V8) with the JIT, then runs each
// command twice at the shell prompt and reports the MIPS of the second run
// (code already compiled). Guest instructions per command are deterministic;
// only the time changes between builds.
//
//   node tools/guest-bench.mjs [--wasm FILE] [--kernel DIR] [--rounds N]
//
// Needs target/guest-kernel (tools/guest-kernel/build.sh).

import { readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { performance } from 'node:perf_hooks';
import { instantiate, Machine } from '../web/node/vetro.mjs';

const PROMPT = '# \x1b[6n';
const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const args = process.argv.slice(2);
const opt = (name, def) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : def;
};
const wasmPath = opt('--wasm', join(root, 'target/wasm32-unknown-unknown/release/vetro_wasm.wasm'));
const kernelDir = opt('--kernel', join(root, 'target/guest-kernel'));
const rounds = Number(opt('--rounds', '2'));

const COMMANDS = [
  ['sha256sum', 'sha256sum /bin/busybox > /dev/null'],
  ['gzip', 'gzip -c /bin/busybox > /dev/null'],
  ['shell loop', 'i=0; while [ $i -lt 20000 ]; do i=$((i+1)); done'],
  ['FP awk', "awk 'BEGIN { s = 0; for (i = 1; i < 300000; i++) s += sqrt(i) / i; print s }' > /dev/null"],
];

const { exports } = await instantiate(readFileSync(wasmPath));
const m = new Machine(exports);
m.loadLinux(readFileSync(join(kernelDir, 'Image')), readFileSync(join(kernelDir, 'initramfs.cpio.gz')), 'console=ttyAMA0');
m.setJit(64, 16);
let log = '';
const until = (needle, from) => {
  for (;;) {
    const i = log.indexOf(needle, from);
    if (i >= 0) return i + needle.length;
    const stop = m.run(1_000_000);
    const out = m.consoleRead();
    if (out.length) log += Buffer.from(out.buffer, out.byteOffset, out.length).toString('latin1');
    if (stop !== 'Budget') throw new Error(`${stop} while waiting for ${JSON.stringify(needle)}`);
  }
};
const t0 = performance.now();
let at = until(PROMPT, 0);
console.log(`boot to the prompt: ${((performance.now() - t0) / 1000).toFixed(2)} s, ${m.steps} instructions`);
for (const [name, cmd] of COMMANDS) {
  for (let r = 1; r <= rounds; r++) {
    const s0 = m.steps;
    const t = performance.now();
    m.consoleWrite(`${cmd}; echo VETRO-BENCH-$((40+2))\n`);
    at = until("VETRO-BENCH-42", at);
    at = until(PROMPT, at);
    const ms = performance.now() - t;
    const n = Number(m.steps - s0);
    if (r === rounds) console.log(`${name.padEnd(12)} ${(n / 1e6).toFixed(0).padStart(6)} M instr  ${(ms / 1000).toFixed(2).padStart(6)} s  ${(n / ms / 1000).toFixed(0).padStart(5)} MIPS`);
  }
}
console.log(`JIT: ${JSON.stringify(m.jitStats())}`);
