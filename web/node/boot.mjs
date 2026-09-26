#!/usr/bin/env node
// Avvio del kernel guest di M3 sotto vetro-wasm in Node (V8), con lo stesso
// copione di tests/boot/tests/vetro.rs: marcatore di /init, autotest senza
// errori, un comando a prompt completo, `poweroff -f` fino a PowerOff.
// Stampa tempo reale e istruzioni, e scrive il log (normalizzato come nel
// test nativo) in target/guest-kernel/node-boot.log. Esce con 0 solo se
// tutto va bene.
//
//   node web/node/boot.mjs [--wasm FILE] [--kernel DIR] [--expect-steps N]
//                          [--jit [--jit-threshold N] [--jit-batch N]]
//
// Con --jit gira col JIT della modalità sistema (ADR 0013, moduli compilati
// da V8): istruzioni e log devono essere gli stessi dell'interprete, e il
// log va in target/guest-kernel/node-boot-jit.log.
//
// Node 22, nessuna dipendenza. Il .wasm si costruisce con tools/wasm-boot.sh
// (cargo build --release --target wasm32-unknown-unknown -p vetro-wasm).

import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { performance } from 'node:perf_hooks';
import { instantiate, Machine } from './vetro.mjs';

// Le stesse costanti di tests/boot/src/lib.rs.
const BOOT_MARKER = 'VETRO-BOOT-OK';
const AUTOTEST_OK = 'VETRO-AUTOTEST-FINE: ok';
const AUTOTEST_END = 'VETRO-AUTOTEST-FINE';
const SHELL_PROMPT = '# \x1b[6n';
// Come tests/boot/tests/vetro.rs: istruzioni concesse a ogni fase e quanto.
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

class Fail extends Error {}

/** Ultime righe della console, per i messaggi d'errore. */
let tailOf = () => '';

const normalize = (s) => s.replaceAll('\r\n', '\n').replaceAll('\r', '\n');

async function main() {
  const t0 = performance.now();
  const { exports, jit: engine } = await instantiate(readFileSync(wasmPath));
  const image = readFileSync(join(kernelDir, 'Image'));
  const initrd = readFileSync(join(kernelDir, 'initramfs.cpio.gz'));
  const m = new Machine(exports);
  m.loadLinux(image, initrd, CMDLINE);
  if (jit) m.setJit(jitThreshold, jitBatch);
  const tLoad = performance.now();

  // Il log come stringa latin1: un carattere per byte, posizioni come in Rust.
  let log = '';
  const pull = () => {
    const out = m.consoleRead();
    if (out.length) log += Buffer.from(out.buffer, out.byteOffset, out.length).toString('latin1');
  };
  tailOf = () => normalize(log).split('\n').slice(-40).join('\n');
  const until = (needle, from) => {
    const limit = m.steps + PHASE_BUDGET;
    for (;;) {
      const i = log.indexOf(needle, from);
      if (i >= 0) return i + needle.length;
      if (m.steps >= limit) throw new Fail(`${JSON.stringify(needle)} non arrivato entro ${PHASE_BUDGET} istruzioni`);
      const stop = m.run(QUANTUM);
      pull();
      if (stop === 'Idle') throw new Fail(`guest inattivo in attesa di ${JSON.stringify(needle)}`);
      if (stop !== 'Budget') throw new Fail(`${stop} in attesa di ${JSON.stringify(needle)}`);
    }
  };

  const at = until(BOOT_MARKER, 0);
  const tInit = performance.now();
  const bootNs = m.guestNs;
  // Fino alla fine della riga: un quanto può finire a metà.
  const atEnd = until(AUTOTEST_END, at);
  const end = until('\n', atEnd);
  const line = log.slice(atEnd - AUTOTEST_END.length, end).trimEnd();
  if (line !== AUTOTEST_OK) throw new Fail(`autotest con errori: ${JSON.stringify(line)}`);
  // Ingresso solo a prompt completo, come sotto QEMU.
  const prompt = until(SHELL_PROMPT, end);
  m.consoleWrite('echo VETRO-SHELL-$((6*7))\n');
  const out = until('VETRO-SHELL-42', prompt);
  until(SHELL_PROMPT, out);
  m.consoleWrite('poweroff -f\n');
  const limit = m.steps + PHASE_BUDGET;
  let stop;
  for (;;) {
    stop = m.run(QUANTUM);
    pull();
    if (stop !== 'Budget' || m.steps >= limit) break;
  }
  if (stop !== 'PowerOff') throw new Fail(`poweroff -f non ha spento la macchina: ${stop}`);
  const t1 = performance.now();

  writeFileSync(join(kernelDir, jit ? 'node-boot-jit.log' : 'node-boot.log'), normalize(log), 'latin1');
  const steps = m.steps;
  const secs = (t1 - tLoad) / 1000;
  console.log(
    `Vetro in Node ${process.version}: /init a ${(Number(bootNs) / 1e9).toFixed(2)} s di guest, ` +
      `spento a ${(Number(m.guestNs) / 1e9).toFixed(2)} s (${steps} istruzioni)`,
  );
  console.log(
    `tempo reale: caricamento ${((tLoad - t0) / 1000).toFixed(2)} s, /init ${((tInit - tLoad) / 1000).toFixed(2)} s, ` +
      `esecuzione ${secs.toFixed(2)} s (${(Number(steps) / secs / 1e6).toFixed(1)} MIPS)`,
  );
  if (jit) {
    console.log(`JIT (soglia ${jitThreshold}, ${jitBatch} blocchi per modulo): ${JSON.stringify(m.jitStats())}`);
    console.log(`motore JS: ${JSON.stringify(engine.stats)}`);
  }
  // Riga da leggere per gli script (tools/wasm-boot.sh).
  console.log(`VETRO-NODE-BOOT steps=${steps} ms=${Math.round(t1 - tLoad)}`);
  if (expectSteps !== null && BigInt(expectSteps) !== steps) {
    throw new Fail(`istruzioni: ${steps} in Node, attese ${expectSteps} (la macchina è deterministica)`);
  }
  m.free();
}
main().then(
  () => (process.exitCode = 0),
  (e) => {
    console.error(`ERRORE: ${e instanceof Fail ? e.message : e.stack ?? e}`);
    const t = tailOf();
    if (t) console.error(`ultime righe della console:\n${t}`);
    process.exit(1);
  },
);
