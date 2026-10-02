// One guest core in a Worker of its own (ADR 0042, step 3): the threads
// build of vetro-wasm instantiated again on the machine's shared memory, with
// its own stack, thread-local storage and JIT engine (V8's tables belong to
// one agent), running `vetro_core_run` until the cores stop. A browser module
// Worker or a Node `worker_threads` Worker.
//
// Protocol: in { module, memory, core, stackTop, tls, jit, jitBudget, budget }
// (jit: { threshold, batch } or null; jitBudget: its engine's code budget) -> out { type: 'ready' }, then once the cores
// stop (`vetro_parallel_request_stop` or the machine powered off)
// { type: 'done', stop, executed, jit } or { type: 'error', error }. In
// { type: 'ping' } at any time -> out { type: 'pong' } from the event loop.
//
// A WFI waits in Rust (Atomics.wait, allowed in Workers), and the stop
// request arrives through the shared memory. The loop still returns to the
// Worker's event loop every YIELD_MS: V8 runs its tasks for this isolate
// there, the ones that free the code of dropped JIT modules included.
// Without them the dead code piled up until "Commit wasm code space
// Allocation failed" (Node, four cores, after a few minutes of Android).

import { JitEngine } from './jit-engine.mjs';

const STOP = ['Budget', 'PowerOff', 'Reset', 'Idle', 'Unimplemented', 'Blocked'];
const YIELD_MS = 50;

/** A turn of the event loop (a macrotask: setTimeout(0) would be clamped). */
function turn() {
  return new Promise((ok) => {
    const ch = new MessageChannel();
    ch.port1.onmessage = () => {
      ch.port1.close();
      ok();
    };
    ch.port2.postMessage(0);
  });
}

async function run({ module, memory, core, stackTop, tls, jit, jitBudget, budget }, post) {
  const engine = new JitEngine(jitBudget ? { budget: jitBudget } : {});
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
  let last = performance.now();
  while (!x.vetro_core_stopped(core)) {
    stop = STOP[x.vetro_core_run(core, BigInt(budget))] ?? 'Unknown';
    if (stop !== 'Budget' && stop !== 'Idle') break;
    if (performance.now() - last >= YIELD_MS) {
      await turn();
      last = performance.now();
    }
  }
  const executed = Number(x.vetro_core_executed(core));
  x.vetro_core_drop_jit(core);
  post({ type: 'done', stop, executed, jit: engine.stats });
}

async function onMessage(m, post) {
  try {
    await run(m, post);
  } catch (e) {
    post({ type: 'error', error: String(e?.stack ?? e) });
  }
}

/** The first message starts the core; the others are pings. */
function listen(post) {
  let started = false;
  return (m) => {
    if (m?.type === 'ping') post({ type: 'pong' });
    else if (!started) {
      started = true;
      onMessage(m, post);
    }
  };
}

if (typeof self !== 'undefined' && typeof self.postMessage === 'function' && typeof process === 'undefined') {
  const on = listen((m) => self.postMessage(m));
  self.onmessage = (e) => on(e.data);
} else {
  const { parentPort } = await import('node:worker_threads');
  const on = listen((r) => parentPort.postMessage(r));
  parentPort.on('message', on);
}
