// One guest core in a Worker of its own (ADR 0042, step 3): the threads
// build of vetro-wasm instantiated again on the machine's shared memory, with
// its own stack, thread-local storage and JIT engine (V8's tables belong to
// one agent), running `vetro_core_run` until the cores stop. A browser module
// Worker or a Node `worker_threads` Worker.
//
// Protocol: in { module, memory, core, stackTop, tls, jit, budget } (jit:
// { threshold, batch } or null) -> out { type: 'ready' }, then once the cores
// stop (`vetro_parallel_request_stop` or the machine powered off)
// { type: 'done', stop, executed, jit } or { type: 'error', error }.
//
// The loop never returns to the event loop while the core runs: a WFI waits
// in Rust (Atomics.wait, allowed in Workers), and the stop request arrives
// through the shared memory.

import { JitEngine } from './jit-engine.mjs';

const STOP = ['Budget', 'PowerOff', 'Reset', 'Idle', 'Unimplemented', 'Blocked'];

function run({ module, memory, core, stackTop, tls, jit, budget }, post) {
  const engine = new JitEngine();
  let x = null;
  const imports = {
    env: { memory },
    vetro_host: {
      panic: (ptr, len) => {
        const msg = new TextDecoder().decode(new Uint8Array(memory.buffer, ptr >>> 0, len).slice());
        console.error(`vetro-wasm (core Worker): panic: ${msg}`);
      },
      snapshot_read: () => {
        throw new Error('snapshot_read in a core Worker');
      },
      snapshot_write: () => {
        throw new Error('snapshot_write in a core Worker');
      },
      // Devices are serviced by whichever core touches them; WebGL lives
      // with the page's Worker, so 3D batches from here run nowhere.
      gl_execute: () => {},
    },
    vetro_jit: engine.imports(),
  };
  const instance = new WebAssembly.Instance(module, imports);
  x = { ...instance.exports, memory };
  // This thread's stack and thread-local storage, before any Rust code runs.
  x.__stack_pointer.value = stackTop;
  x.__wasm_init_tls(tls);
  engine.attach(x);
  x.vetro_core_set_jit(core, jit ? jit.threshold : 0, jit ? jit.batch : 0);
  post({ type: 'ready' });
  let stop = 'Budget';
  while (!x.vetro_core_stopped(core)) {
    stop = STOP[x.vetro_core_run(core, BigInt(budget))] ?? 'Unknown';
    if (stop !== 'Budget' && stop !== 'Idle') break;
  }
  const executed = Number(x.vetro_core_executed(core));
  x.vetro_core_drop_jit(core);
  post({ type: 'done', stop, executed, jit: engine.stats });
}

function onMessage(m, post) {
  try {
    run(m, post);
  } catch (e) {
    post({ type: 'error', error: String(e?.stack ?? e) });
  }
}

if (typeof self !== 'undefined' && typeof self.postMessage === 'function' && typeof process === 'undefined') {
  self.onmessage = (e) => onMessage(e.data, (m) => self.postMessage(m));
} else {
  const { parentPort } = await import('node:worker_threads');
  parentPort.once('message', (m) => onMessage(m, (r) => parentPort.postMessage(r)));
}
