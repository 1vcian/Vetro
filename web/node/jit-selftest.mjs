#!/usr/bin/env node
// Prova del ponte JIT di vetro-wasm (docs/specs/wasm.md): Rust chiama
// vetro_jit.compile/run (web/node/jit-engine.mjs), il modulo generato gira su
// JitState nella memoria di vetro-wasm e chiama env.ld / env.st, cioè gli
// export vetro_jit_ld / vetro_jit_st, che tornano all'Host in Rust.
//
// Il modulo di prova lo scrive questo file con un piccolo codificatore (il
// traduttore vero è dell'agente JIT, crates/vetro-jit). Il blocco b0 fa
//   x2 = ld(x0, 8) + x1;  st(x0 + 8, 8, x2);  pc += 12;  steps += 3
// e vetro_jit_selftest restituisce il valore scritto: 37 + 5 = 42.
//
//   node web/node/jit-selftest.mjs [--wasm FILE]

import { readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { copyIn, instantiate } from './vetro.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const i = process.argv.indexOf('--wasm');
const wasmPath = i >= 0 ? process.argv[i + 1] : join(root, 'target/wasm32-unknown-unknown/release/vetro_wasm.wasm');

// Codifica minima di un modulo WASM (spec 1.0, sezione 5).
const uleb = (n) => {
  const out = [];
  do {
    let b = n & 0x7f;
    n >>>= 7;
    if (n) b |= 0x80;
    out.push(b);
  } while (n);
  return out;
};
const sleb = (n) => {
  const out = [];
  for (;;) {
    const b = n & 0x7f;
    n >>= 7;
    if ((n === 0 && !(b & 0x40)) || (n === -1 && b & 0x40)) return [...out, b];
    out.push(b | 0x80);
  }
};
const vec = (items) => [...uleb(items.length), ...items.flat()];
const str = (s) => vec([...Buffer.from(s)]);
const section = (id, body) => [id, ...uleb(body.length), ...body];
const [I32, I64] = [0x7f, 0x7e];
const func = (params, results) => [0x60, ...vec(params), ...vec(results)];
const op = {
  get: (l) => [0x20, ...uleb(l)],
  i32: (v) => [0x41, ...sleb(v)],
  i64: (v) => [0x42, ...sleb(v)],
  load64: (off) => [0x29, 3, ...uleb(off)],
  store64: (off) => [0x37, 3, ...uleb(off)],
  add64: [0x7c],
  call: (f) => [0x10, ...uleb(f)],
  drop: [0x1a],
  end: [0x0b],
};
// Offset di JitState (docs/specs/jit.md).
const X0 = 0, X1 = 8, PC = 256, STEPS = 264;

function testModule() {
  const types = section(1, vec([func([I32, I64, I32], [I64]), func([I32, I64, I32, I64], [I32]), func([I32], [I32])]));
  const imports = section(
    2,
    vec([
      [...str('env'), ...str('mem'), 0x02, 0x00, 0x01], // memoria, min 1 pagina
      [...str('env'), ...str('ld'), 0x00, 0], // funzione 0, tipo 0
      [...str('env'), ...str('st'), 0x00, 1], // funzione 1, tipo 1
    ]),
  );
  const funcs = section(3, vec([[2]]));
  const exports = section(7, vec([[...str('b0'), 0x00, 2]]));
  const s = op.get(0);
  const body = [
    0, // nessun locale
    // st(state, x0 + 8, 8, ld(state, x0, 8) + x1)
    ...s,
    ...s, ...op.load64(X0), ...op.i64(8), ...op.add64,
    ...op.i32(8),
    ...s, ...s, ...op.load64(X0), ...op.i32(8), ...op.call(0),
    ...s, ...op.load64(X1), ...op.add64,
    ...op.call(1), ...op.drop,
    // pc += 12; steps += 3
    ...s, ...s, ...op.load64(PC), ...op.i64(12), ...op.add64, ...op.store64(PC),
    ...s, ...s, ...op.load64(STEPS), ...op.i64(3), ...op.add64, ...op.store64(STEPS),
    ...op.i32(0), // NEXT
    ...op.end,
  ];
  const code = section(10, vec([[...uleb(body.length), ...body]]));
  return new Uint8Array([0x00, 0x61, 0x73, 0x6d, 1, 0, 0, 0, ...types, ...imports, ...funcs, ...exports, ...code]);
}

const { exports: x, jit } = await instantiate(readFileSync(wasmPath));
const [p, n] = copyIn(x, testModule());
const r = x.vetro_jit_selftest(p, n);
x.vetro_free(p, n);
if (r !== 42n || jit.stats.modules !== 1) {
  console.error(`ERRORE: vetro_jit_selftest = ${r} (atteso 42), moduli compilati ${jit.stats.modules}`);
  process.exit(1);
}
console.log('ponte JIT: ok (Rust -> vetro_jit.compile/run -> modulo generato -> env.ld/env.st -> Rust)');
