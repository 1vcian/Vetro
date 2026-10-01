#!/usr/bin/env node
// Boot of the M3 guest kernel under vetro-wasm in Node (V8), with the same
// script as tests/boot/tests/vetro.rs: /init marker, autotest without
// errors, a command at a complete prompt, `poweroff -f` until PowerOff.
// Prints real time and instructions, and writes the log (normalised as in the
// native test) to target/guest-kernel/node-boot.log. Exits with 0 only if
// everything goes well.
//
//   node web/node/boot.mjs [--wasm FILE] [--kernel DIR] [--expect-steps N]
//                          [--jit [--jit-threshold N] [--jit-batch N] [--jit-background]]
//                          [--cpus N]
//
// With --jit it runs with the system-mode JIT (ADR 0013, modules compiled
// by V8): instructions and log must be the same as the interpreter's, and the
// log goes to target/guest-kernel/node-boot-jit.log. --jit-background
// compiles the modules in a Worker (ADR 0038): still the same instructions
// and log; the loop then yields to the event loop after every quantum.
// --cpus N boots N guest cores (ADR 0042, deterministic turns): the log goes
// to node-boot-smpN[-jit].log.
//
// Node 22, no dependencies. The .wasm is built with tools/wasm-boot.sh
// (cargo build --release --target wasm32-unknown-unknown -p vetro-wasm).

import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { performance } from 'node:perf_hooks';
import { instantiate, Machine } from './vetro.mjs';

// The same constants as tests/boot/src/lib.rs.
const BOOT_MARKER = 'VETRO-BOOT-OK';
const AUTOTEST_OK = 'VETRO-AUTOTEST-FINE: ok';
const AUTOTEST_END = 'VETRO-AUTOTEST-FINE';
const SHELL_PROMPT = '# \x1b[6n';
// Like tests/boot/tests/vetro.rs: instructions granted to each phase, and the quantum.
const PHASE_BUDGET = 6_000_000_000n;
const QUANTUM = 1_000_000;
const CMDLINE = 'console=ttyAMA0';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const args = process.argv.slice(2);
const opt = (name, def) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : def;
};
const wasmPath = opt('--wasm', join(root, 'target/wasm32-unknown-unknown/release/vetro_wasm.wasm'));
const kernelDir = opt('--kernel', join(root, 'target/guest-kernel'));
const expectSteps = opt('--expect-steps', null);
const jit = args.includes('--jit');
const jitThreshold = Number(opt('--jit-threshold', '64'));
const jitBatch = Number(opt('--jit-batch', '16'));
const background = jit && args.includes('--jit-background');
/** ADR 0042: guest cores, in turns on this thread (deterministic). */
const cpus = Number(opt('--cpus', '1'));
/** After each quantum with --jit-background: lets the Worker's modules arrive. */
const tick = background ? () => new Promise((ok) => setImmediate(ok)) : () => null;

class Fail extends Error {}

/** Last lines of the console, for error messages. */
let tailOf = () => '';

const normalize = (s) => s.replaceAll('\r\n', '\n').replaceAll('\r', '\n');

async function main() {
  const t0 = performance.now();
  const { exports, jit: engine } = await instantiate(readFileSync(wasmPath));
  const image = readFileSync(join(kernelDir, 'Image'));
  const initrd = readFileSync(join(kernelDir, 'initramfs.cpio.gz'));
  const m = new Machine(exports, { cpus });
  m.loadLinux(image, initrd, CMDLINE);
  if (background) await engine.startBackground();
  if (jit) m.setJit(jitThreshold, jitBatch);
  const tLoad = performance.now();

  // The log as a latin1 string: one character per byte, positions as in Rust.
  let log = '';
  const pull = () => {
    const out = m.consoleRead();
    if (out.length) log += Buffer.from(out.buffer, out.byteOffset, out.length).toString('latin1');
  };
  tailOf = () => normalize(log).split('\n').slice(-40).join('\n');
  const until = async (needle, from) => {
    const limit = m.steps + PHASE_BUDGET;
    for (;;) {
      const i = log.indexOf(needle, from);
      if (i >= 0) return i + needle.length;
      if (m.steps >= limit) throw new Fail(`${JSON.stringify(needle)} did not arrive within ${PHASE_BUDGET} instructions`);
      const stop = m.run(QUANTUM);
      pull();
      await tick();
      if (stop === 'Idle') throw new Fail(`guest idle while waiting for ${JSON.stringify(needle)}`);
      if (stop !== 'Budget') throw new Fail(`${stop} while waiting for ${JSON.stringify(needle)}`);
    }
  };

  const at = await until(BOOT_MARKER, 0);
  const tInit = performance.now();
  const bootNs = m.guestNs;
  // Up to the end of the line: a quantum may end halfway.
  const atEnd = await until(AUTOTEST_END, at);
  const end = await until('\n', atEnd);
  const line = log.slice(atEnd - AUTOTEST_END.length, end).trimEnd();
  if (line !== AUTOTEST_OK) throw new Fail(`autotest with errors: ${JSON.stringify(line)}`);
  // Input only at a complete prompt, as under QEMU.
  const prompt = await until(SHELL_PROMPT, end);
  m.consoleWrite('echo VETRO-SHELL-$((6*7))\n');
  const out = await until('VETRO-SHELL-42', prompt);
  await until(SHELL_PROMPT, out);
  m.consoleWrite('poweroff -f\n');
  const limit = m.steps + PHASE_BUDGET;
  let stop;
  for (;;) {
    stop = m.run(QUANTUM);
    pull();
    await tick();
    if (stop !== 'Budget' || m.steps >= limit) break;
  }
  if (stop !== 'PowerOff') throw new Fail(`poweroff -f did not turn the machine off: ${stop}`);
  const t1 = performance.now();

  const smp = cpus > 1 ? `-smp${cpus}` : '';
  writeFileSync(join(kernelDir, jit ? `node-boot${smp}-jit.log` : `node-boot${smp}.log`), normalize(log), 'latin1');
  const steps = m.steps;
  const secs = (t1 - tLoad) / 1000;
  console.log(
    `Vetro in Node ${process.version}: /init at ${(Number(bootNs) / 1e9).toFixed(2)} s of guest time, ` +
      `powered off at ${(Number(m.guestNs) / 1e9).toFixed(2)} s (${steps} instructions)`,
  );
  console.log(
    `real time: load ${((tLoad - t0) / 1000).toFixed(2)} s, /init ${((tInit - tLoad) / 1000).toFixed(2)} s, ` +
      `run ${secs.toFixed(2)} s (${(Number(steps) / secs / 1e6).toFixed(1)} MIPS)`,
  );
  if (jit) {
    console.log(`JIT (threshold ${jitThreshold}, ${jitBatch} blocks per module): ${JSON.stringify(m.jitStats())}`);
    console.log(`JS engine: ${JSON.stringify(engine.stats)}`);
  }
  // Line to be read by scripts (tools/wasm-boot.sh).
  console.log(`VETRO-NODE-BOOT steps=${steps} ms=${Math.round(t1 - tLoad)}`);
  if (expectSteps !== null && BigInt(expectSteps) !== steps) {
    throw new Fail(`instructions: ${steps} in Node, expected ${expectSteps} (the machine is deterministic)`);
  }
  m.free();
}
main().then(
  () => (process.exitCode = 0),
  (e) => {
    console.error(`ERROR: ${e instanceof Fail ? e.message : e.stack ?? e}`);
    const t = tailOf();
    if (t) console.error(`last lines of the console:\n${t}`);
    process.exit(1);
  },
);
