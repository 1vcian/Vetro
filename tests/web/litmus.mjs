#!/usr/bin/env node
// ADR 0042: the litmus tests of the parallel cores in V8 (Node), with the
// threads build of vetro-wasm: core 1 in a Worker of its own, the JIT of each
// core on its own JS engine, the guest RAM reached through the software TLB
// (the browser's fast paths, which the native tests on wasmtime do not have).
// The same programs as crates/vetro-machine/tests/litmus.rs:
//
// - SB+dmbs: never both loads 0;
// - MP+dmbs, MP+rel+acq: never the flag without the data;
// - the exclusive counter: no lost increment (the store-exclusive is an
//   atomic compare-and-exchange inside the region).
//
//   node tests/web/litmus.mjs [--wasm FILE]   (default: the threads build)

import { existsSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { instantiate, Machine } from '../../web/node/vetro.mjs';
import { check, Fail, root, run } from './lib.mjs';

const R = 0x4000_0000;
const args = process.argv.slice(2);
const wasmPath = args.includes('--wasm') ? args[args.indexOf('--wasm') + 1]
  : join(root, 'target/wasm32-unknown-unknown/release/vetro_wasm_threads.wasm');

// Encodings from tools/a64asm.sh (see litmus.rs for the layout).
const SB = [
  0xd2a80014, // mov x20, #0x40000000        // =1073741824
  0xd2a10001, // mov x1, #0x8000000          // =134217728
  0x52800042, // mov w2, #0x2                // =2
  0xb9000022, // str w2, [x1]
  0xd2800060, // mov x0, #0x3                // =3
  0xf2b88000, // movk x0, #0xc400, lsl #16
  0xd2800021, // mov x1, #0x1                // =1
  0x10000062, // adr x2, 0x28 <both>
  0xd2800003, // mov x3, #0x0                // =0
  0xd4000002, // hvc #0
  0xd2a80014, // mov x20, #0x40000000        // =1073741824
  0xd53800ad, // mrs x13, MPIDR_EL1
  0x92401dad, // and x13, x13, #0xff
  0xd2800009, // mov x9, #0x0                // =0
  0x91404295, // add x21, x20, #0x10, lsl #12 // =0x10000
  0x91408296, // add x22, x20, #0x20, lsl #12 // =0x20000
  0x9140c297, // add x23, x20, #0x30, lsl #12 // =0x30000
  0x91410298, // add x24, x20, #0x40, lsl #12 // =0x40000
  0x91414299, // add x25, x20, #0x50, lsl #12 // =0x50000
  0xf944029a, // ldr x26, [x20, #0x800]
  0x8b090eaa, // add x10, x21, x9, lsl #3
  0xc85ffd4b, // ldaxr x11, [x10]
  0x9100056b, // add x11, x11, #0x1
  0xc80cfd4b, // stlxr w12, x11, [x10]
  0x35ffffac, // cbnz w12, 0x54 <loop+0x4>
  0xc8dffd4b, // ldar x11, [x10]
  0xf100097f, // cmp x11, #0x2
  0x54ffffcb, // b.lt 0x64 <loop+0x14>
  0xeb1a013f, // cmp x9, x26
  0x54000200, // b.eq 0xb4 <done>
  0x8b090ece, // add x14, x22, x9, lsl #3
  0x8b090eef, // add x15, x23, x9, lsl #3
  0xd2800030, // mov x16, #0x1               // =1
  0xb50000cd, // cbnz x13, 0x9c <core1>
  0xf90001d0, // str x16, [x14]
  0xd5033bbf, // dmb ish
  0xf94001f1, // ldr x17, [x15]
  0xf8297b11, // str x17, [x24, x9, lsl #3]
  0x14000005, // b 0xac <next>
  0xf90001f0, // str x16, [x15]
  0xd5033bbf, // dmb ish
  0xf94001d1, // ldr x17, [x14]
  0xf8297b31, // str x17, [x25, x9, lsl #3]
  0x91000529, // add x9, x9, #0x1
  0x17ffffe8, // b 0x50 <loop>
  0xb500008d, // cbnz x13, 0xc4 <park>
  0xd2800100, // mov x0, #0x8                // =8
  0xf2b08000, // movk x0, #0x8400, lsl #16
  0xd4000002, // hvc #0
  0xd503207f, // wfi
  0x17ffffff, // b 0xc4 <park>
];
const MP_DMB = [
  0xd2a80014, // mov x20, #0x40000000        // =1073741824
  0xd2a10001, // mov x1, #0x8000000          // =134217728
  0x52800042, // mov w2, #0x2                // =2
  0xb9000022, // str w2, [x1]
  0xd2800060, // mov x0, #0x3                // =3
  0xf2b88000, // movk x0, #0xc400, lsl #16
  0xd2800021, // mov x1, #0x1                // =1
  0x10000062, // adr x2, 0x28 <both>
  0xd2800003, // mov x3, #0x0                // =0
  0xd4000002, // hvc #0
  0xd2a80014, // mov x20, #0x40000000        // =1073741824
  0xd53800ad, // mrs x13, MPIDR_EL1
  0x92401dad, // and x13, x13, #0xff
  0xd2800009, // mov x9, #0x0                // =0
  0x91404295, // add x21, x20, #0x10, lsl #12 // =0x10000
  0x91408296, // add x22, x20, #0x20, lsl #12 // =0x20000
  0x9140c297, // add x23, x20, #0x30, lsl #12 // =0x30000
  0x91410298, // add x24, x20, #0x40, lsl #12 // =0x40000
  0x91414299, // add x25, x20, #0x50, lsl #12 // =0x50000
  0xf944029a, // ldr x26, [x20, #0x800]
  0x8b090eaa, // add x10, x21, x9, lsl #3
  0xc85ffd4b, // ldaxr x11, [x10]
  0x9100056b, // add x11, x11, #0x1
  0xc80cfd4b, // stlxr w12, x11, [x10]
  0x35ffffac, // cbnz w12, 0x54 <loop+0x4>
  0xc8dffd4b, // ldar x11, [x10]
  0xf100097f, // cmp x11, #0x2
  0x54ffffcb, // b.lt 0x64 <loop+0x14>
  0xeb1a013f, // cmp x9, x26
  0x54000200, // b.eq 0xb4 <done>
  0x8b090ece, // add x14, x22, x9, lsl #3
  0x8b090eef, // add x15, x23, x9, lsl #3
  0xd2800030, // mov x16, #0x1               // =1
  0xb50000ed, // cbnz x13, 0xa0 <writer>
  0xf94001d1, // ldr x17, [x14]
  0xd5033bbf, // dmb ish
  0xf94001f2, // ldr x18, [x15]
  0xf8297b11, // str x17, [x24, x9, lsl #3]
  0xf8297b32, // str x18, [x25, x9, lsl #3]
  0x14000004, // b 0xac <next>
  0xf90001f0, // str x16, [x15]
  0xd5033bbf, // dmb ish
  0xf90001d0, // str x16, [x14]
  0x91000529, // add x9, x9, #0x1
  0x17ffffe8, // b 0x50 <loop>
  0xb500008d, // cbnz x13, 0xc4 <park>
  0xd2800100, // mov x0, #0x8                // =8
  0xf2b08000, // movk x0, #0x8400, lsl #16
  0xd4000002, // hvc #0
  0xd503207f, // wfi
  0x17ffffff, // b 0xc4 <park>
];
const MP_RELACQ = [
  0xd2a80014, // mov x20, #0x40000000        // =1073741824
  0xd2a10001, // mov x1, #0x8000000          // =134217728
  0x52800042, // mov w2, #0x2                // =2
  0xb9000022, // str w2, [x1]
  0xd2800060, // mov x0, #0x3                // =3
  0xf2b88000, // movk x0, #0xc400, lsl #16
  0xd2800021, // mov x1, #0x1                // =1
  0x10000062, // adr x2, 0x28 <both>
  0xd2800003, // mov x3, #0x0                // =0
  0xd4000002, // hvc #0
  0xd2a80014, // mov x20, #0x40000000        // =1073741824
  0xd53800ad, // mrs x13, MPIDR_EL1
  0x92401dad, // and x13, x13, #0xff
  0xd2800009, // mov x9, #0x0                // =0
  0x91404295, // add x21, x20, #0x10, lsl #12 // =0x10000
  0x91408296, // add x22, x20, #0x20, lsl #12 // =0x20000
  0x9140c297, // add x23, x20, #0x30, lsl #12 // =0x30000
  0x91410298, // add x24, x20, #0x40, lsl #12 // =0x40000
  0x91414299, // add x25, x20, #0x50, lsl #12 // =0x50000
  0xf944029a, // ldr x26, [x20, #0x800]
  0x8b090eaa, // add x10, x21, x9, lsl #3
  0xc85ffd4b, // ldaxr x11, [x10]
  0x9100056b, // add x11, x11, #0x1
  0xc80cfd4b, // stlxr w12, x11, [x10]
  0x35ffffac, // cbnz w12, 0x54 <loop+0x4>
  0xc8dffd4b, // ldar x11, [x10]
  0xf100097f, // cmp x11, #0x2
  0x54ffffcb, // b.lt 0x64 <loop+0x14>
  0xeb1a013f, // cmp x9, x26
  0x540001c0, // b.eq 0xac <done>
  0x8b090ece, // add x14, x22, x9, lsl #3
  0x8b090eef, // add x15, x23, x9, lsl #3
  0xd2800030, // mov x16, #0x1               // =1
  0xb50000cd, // cbnz x13, 0x9c <writer>
  0xc8dffdd1, // ldar x17, [x14]
  0xf94001f2, // ldr x18, [x15]
  0xf8297b11, // str x17, [x24, x9, lsl #3]
  0xf8297b32, // str x18, [x25, x9, lsl #3]
  0x14000003, // b 0xa4 <next>
  0xf90001f0, // str x16, [x15]
  0xc89ffdd0, // stlr x16, [x14]
  0x91000529, // add x9, x9, #0x1
  0x17ffffea, // b 0x50 <loop>
  0xb500008d, // cbnz x13, 0xbc <park>
  0xd2800100, // mov x0, #0x8                // =8
  0xf2b08000, // movk x0, #0x8400, lsl #16
  0xd4000002, // hvc #0
  0xd503207f, // wfi
  0x17ffffff, // b 0xbc <park>
];
const ATOM = [
  0xd2a80014, // mov x20, #0x40000000        // =1073741824
  0xd2a10001, // mov x1, #0x8000000          // =134217728
  0x52800042, // mov w2, #0x2                // =2
  0xb9000022, // str w2, [x1]
  0xd2800060, // mov x0, #0x3                // =3
  0xf2b88000, // movk x0, #0xc400, lsl #16
  0xd2800021, // mov x1, #0x1                // =1
  0x10000062, // adr x2, 0x28 <both>
  0xd2800003, // mov x3, #0x0                // =0
  0xd4000002, // hvc #0
  0xd2a80014, // mov x20, #0x40000000        // =1073741824
  0xd53800ad, // mrs x13, MPIDR_EL1
  0x92401dad, // and x13, x13, #0xff
  0xf944029a, // ldr x26, [x20, #0x800]
  0x9141828a, // add x10, x20, #0x60, lsl #12 // =0x60000
  0x91404295, // add x21, x20, #0x10, lsl #12 // =0x10000
  0xd2800009, // mov x9, #0x0                // =0
  0x8b0d0d4e, // add x14, x10, x13, lsl #3
  0xf90005c9, // str x9, [x14, #0x8]
  0xc85f7d4b, // ldxr x11, [x10]
  0x9100056b, // add x11, x11, #0x1
  0xc80c7d4b, // stxr w12, x11, [x10]
  0x35ffffac, // cbnz w12, 0x4c <loop+0x4>
  0x91000529, // add x9, x9, #0x1
  0xeb1a013f, // cmp x9, x26
  0x54ffff21, // b.ne 0x48 <loop>
  0xc85ffeab, // ldaxr x11, [x21]
  0x9100056b, // add x11, x11, #0x1
  0xc80cfeab, // stlxr w12, x11, [x21]
  0x35ffffac, // cbnz w12, 0x68 <loop+0x20>
  0xb50000ed, // cbnz x13, 0x94 <park>
  0xc8dffeab, // ldar x11, [x21]
  0xf100097f, // cmp x11, #0x2
  0x54ffffcb, // b.lt 0x7c <loop+0x34>
  0xd2800100, // mov x0, #0x8                // =8
  0xf2b08000, // movk x0, #0x8400, lsl #16
  0xd4000002, // hvc #0
  0xd503207f, // wfi
  0x17ffffff, // b 0x94 <park>
];

const words = (bytes) => Array.from(new BigUint64Array(bytes.buffer, bytes.byteOffset, bytes.length / 8), Number);

/** Runs `code` (N at R+0x800) on two cores in parallel up to the power-off. */
async function runParallel(x, code, n, jit) {
  const m = new Machine(x, { ramSize: 1n << 20n, devices: 0, cpus: 2 });
  try {
    const prog = new Uint8Array(new Uint32Array(code).buffer);
    check(m.loadRaw(R, prog, BigInt(R)), 'program outside RAM');
    check(m.loadRaw(R + 0x800, new Uint8Array(new BigUint64Array([BigInt(n)]).buffer)), 'N outside RAM');
    if (jit) m.setJit(1, 1);
    await m.startParallel({ jit: jit ? { threshold: 1, batch: 1 } : null });
    let stop;
    for (let i = 0; ; i++) {
      stop = m.run(1 << 20);
      if (stop !== 'Budget' && stop !== 'Idle') break;
      if (i > 200_000) throw new Fail('the program did not finish');
    }
    const reports = await m.stopParallel();
    if (process.env.VETRO_LITMUS_DEBUG) console.log(JSON.stringify(m.jitStats()), JSON.stringify(reports.map((r) => r.jit)));
    check(stop === 'PowerOff', `stop ${stop}`);
    return (pa, count) => words(m.readPhys(pa, 8 * count));
  } finally {
    m.free();
  }
}

run(async () => {
  if (!existsSync(wasmPath)) throw new Fail(`${wasmPath} missing: tools/wasm-threads.sh`);
  const { exports: x } = await instantiate(readFileSync(wasmPath));
  const N = 4000;
  for (const jit of [false, true]) {
    const tag = jit ? 'JIT' : 'interpreter';
    // SB+dmbs.
    const seen = [0, 0, 0, 0];
    for (let k = 0; k < 3; k++) {
      const read = await runParallel(x, SB, N, jit);
      const r0 = read(R + 0x4_0000, N);
      const r1 = read(R + 0x5_0000, N);
      for (let i = 0; i < N; i++) seen[r0[i] * 2 + r1[i]]++;
    }
    console.log(`SB+dmbs (${tag}): r0r1 = 00: ${seen[0]}, 01: ${seen[1]}, 10: ${seen[2]}, 11: ${seen[3]}`);
    check(seen[0] === 0, `SB+dmbs: both loads saw 0 (${tag})`);
    for (const [name, code] of [['MP+dmbs', MP_DMB], ['MP+rel+acq', MP_RELACQ]]) {
      const mp = [0, 0, 0, 0];
      for (let k = 0; k < 3; k++) {
        const read = await runParallel(x, code, N, jit);
        const flag = read(R + 0x4_0000, N);
        const data = read(R + 0x5_0000, N);
        for (let i = 0; i < N; i++) mp[flag[i] * 2 + data[i]]++;
      }
      console.log(`${name} (${tag}): flag,data = 00: ${mp[0]}, 01: ${mp[1]}, 10: ${mp[2]}, 11: ${mp[3]}`);
      check(mp[2] === 0, `${name}: the flag without the data (${tag})`);
    }
    // The core Worker returns to its event loop while its core runs (V8
    // frees dead JIT code in tasks there): a ping is answered mid-run.
    {
      const m = new Machine(x, { ramSize: 1n << 20n, devices: 0, cpus: 2 });
      try {
        check(m.loadRaw(R, new Uint8Array(new Uint32Array(ATOM).buffer), BigInt(R)), 'program outside RAM');
        check(m.loadRaw(R + 0x800, new Uint8Array(new BigUint64Array([1n << 40n]).buffer)), 'N outside RAM');
        if (jit) m.setJit(1, 1);
        await m.startParallel({ jit: jit ? { threshold: 1, batch: 1 } : null });
        // Core 0 starts core 1 (CPU_ON), then both count for hours.
        for (let i = 0; i < 20; i++) m.run(1 << 20);
        const t0 = performance.now();
        const answered = await Promise.race([m.pingCores().then(() => true), new Promise((ok) => setTimeout(() => ok(false), 10_000))]);
        console.log(`core Worker ping (${tag}): ${answered ? `answered in ${(performance.now() - t0).toFixed(0)} ms` : 'no answer in 10 s'}`);
        check(answered, `the core Worker never returns to its event loop (${tag})`);
        await m.stopParallel();
      } finally {
        m.free();
      }
    }
    const A = jit ? 2_000_000 : 200_000;
    const read = await runParallel(x, ATOM, A, jit);
    const total = read(R + 0x6_0000, 1)[0];
    console.log(`exclusive counter (${tag}): ${total} of ${2 * A}`);
    check(total === 2 * A, `lost increments (${tag}): ${total} of ${2 * A}`);
  }
});
