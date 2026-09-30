// vetro-wasm's JIT engine in JavaScript (ADR 0012 and 0013,
// docs/specs/jit.md, docs/specs/wasm.md). It corresponds to the trait
// `vetro_jit::Engine`:
//
// - compile(bytes): WebAssembly.Module, instantiated right away with
//   env.mem = vetro-wasm's memory, env.ld / env.st = the exports
//   vetro_jit_ld / vetro_jit_st of vetro-wasm (which do MMU, permissions and
//   bus) and, for the dispatcher, env.tbl = the block table (funcref,
//   TABLE_SIZE entries);
// - place(id, count, base): puts the exports b0..b<count-1> of the module into the
//   table from entry `base` (the dispatcher calls them with call_indirect);
// - run(id, index, state): the exit code of `b<index>(state)`;
// - entry(id, index): puts `b<index>` into vetro-wasm's function table,
//   from which Rust calls it directly (without JS at every run);
// - reset(): discards all instances and recreates the table;
// - memory: it is vetro-wasm's linear memory, where `JitState`, the
//   jump cache, the software TLB and the guest RAM live.
//
// Background compilation (ADR 0038, off by default): after
// `startBackground()`, `compile` hands the module to a Worker
// (jit-compiler.mjs) and returns at once; until the Worker posts the compiled
// module back, `ready(id)` is 0 and vetro-jit runs those regions in the
// interpreter (same guest execution). `place` is deferred to the arrival. A
// module needed at once (`entry`: the dispatcher) is compiled here.
//
// vetro-wasm calls it through the imports `vetro_jit.compile/entry/place/drop/reset/ready`
// (crates/vetro-wasm/src/jit.rs). No dependencies: only the
// WebAssembly API, the same in Node and in the browser.

/** Entries of the block table (`vetro_jit::engine::TABLE_SIZE`). */
export const TABLE_SIZE = 1 << 18;

/**
 * At most this many bytes of live generated modules. Beyond it, `compile`
 * refuses the module and vetro-jit evicts the modules it did not enter since
 * the last eviction (ADR 0040), or resets the engine (as for a full table):
 * V8 returns no error when the space for compiled code runs out (4 GiB), it
 * kills the process. With Android the JIT got there after half an hour (ADR
 * 0028); 96 MiB of wasm are a few hundred MiB of machine code. A dropped
 * module leaves the table and stops counting: V8 frees its code with the
 * module.
 */
export const CODE_BUDGET = 96 << 20;

export class JitEngine {
  #vetro = null; // exports of the vetro-wasm instance
  #table = null;
  #instances = new Map(); // index -> exports of the generated module
  /** Live modules: index -> { bytes, places: [count, base][] } (ADR 0040). */
  #live = new Map();
  #rt = {}; // exports of the runtime module (imports `rt.*` of the modules)
  #next = 0;
  /** Bytes compiled since the last reset, and the limit. */
  #since = 0;
  #budget;
  /** Entries of vetro-wasm's function table handed out with `entry`, and the free ones. */
  #entries = [];
  #free = [];
  /** Background compilation (ADR 0038): the Worker, the generation (reset), id -> { bytes, places }. */
  #bg = null;
  #gen = 0;
  #pending = new Map();
  /** Compiled modules, bytes and resets, for the benchmarks. */
  stats = { modules: 0, bytes: 0, resets: 0, compileMs: 0, refused: 0, background: 0, workerMs: 0, instantiateMs: 0, forced: 0, stale: 0, dropped: 0 };

  constructor({ budget = CODE_BUDGET } = {}) {
    this.#budget = budget;
  }

  /** To be called right after instantiating vetro-wasm (the imports are needed first). */
  attach(vetroExports) {
    this.#vetro = vetroExports;
  }

  /**
   * Turns on background compilation (ADR 0038): modules compiled from now on
   * are compiled by a Worker. In Node a `worker_threads` Worker, in the
   * browser a module Worker. Resolves when the Worker is running.
   */
  async startBackground() {
    if (this.#bg) return;
    const url = new URL('./jit-compiler.mjs', import.meta.url);
    let port;
    if (typeof Worker === 'function' && typeof process === 'undefined') {
      const w = new Worker(url, { type: 'module' });
      port = { post: (m) => w.postMessage(m), stop: () => w.terminate() };
      w.onmessage = (e) => this.#arrived(e.data);
    } else {
      const { Worker: NodeWorker } = await import('node:worker_threads');
      const w = new NodeWorker(url);
      w.on('message', (m) => this.#arrived(m));
      // The Worker must not keep Node alive.
      w.unref();
      port = { post: (m) => w.postMessage(m), stop: () => w.terminate() };
    }
    this.#bg = port;
  }

  /** Stops the Worker (modules still compiling are compiled here when needed). */
  stopBackground() {
    this.#bg?.stop();
    this.#bg = null;
    for (const id of [...this.#pending.keys()]) this.#force(id);
  }

  #instantiate(module) {
    const v = this.#vetro;
    return new WebAssembly.Instance(module, {
      env: { mem: v.memory, tbl: this.#tbl(), ld: v.vetro_jit_ld, st: v.vetro_jit_st, resolve: v.vetro_jit_resolve },
      rt: this.#rt,
    });
  }

  /** A module from the Worker: instantiated and placed, unless dropped or from before a reset. */
  #arrived({ id, gen, module, ms, error }) {
    const p = this.#pending.get(id);
    if (!p || gen !== this.#gen) {
      this.stats.stale++;
      return;
    }
    if (error) {
      console.error(`vetro_jit background compile: ${error}`);
      this.#force(id);
      return;
    }
    const t0 = performance.now();
    this.#install(id, this.#instantiate(module), p);
    this.stats.instantiateMs += performance.now() - t0;
    this.stats.workerMs += ms;
    this.stats.background++;
  }

  #install(id, instance, p) {
    this.#pending.delete(id);
    this.#instances.set(id, instance.exports);
    for (const [count, base] of p.places) this.place(id, count, base);
  }

  /** Compiles a module still in the Worker here, now (it is needed at once). */
  #force(id) {
    const p = this.#pending.get(id);
    if (!p) return;
    const t0 = performance.now();
    this.#install(id, this.#instantiate(new WebAssembly.Module(p.bytes)), p);
    this.stats.compileMs += performance.now() - t0;
    this.stats.forced++;
  }

  /** 1 if module `id` can run, 0 while the Worker compiles it. */
  ready(id) {
    return this.#pending.has(id) ? 0 : 1;
  }

  #tbl() {
    this.#table ??= new WebAssembly.Table({ element: 'anyfunc', initial: TABLE_SIZE });
    return this.#table;
  }

  /** Compiles and instantiates a generated module; returns its index. */
  compile(bytes) {
    const v = this.#vetro;
    if (this.#since > 0 && this.#since + bytes.length > this.#budget) {
      this.stats.refused++;
      throw new RangeError(`JIT code limit (${this.#budget} bytes since the last reset)`);
    }
    const id = this.#next++;
    if (this.#bg) {
      this.#pending.set(id, { bytes, places: [] });
      this.#bg.post({ id, gen: this.#gen, bytes });
    } else {
      const t0 = performance.now();
      const instance = new WebAssembly.Instance(new WebAssembly.Module(bytes), {
        env: { mem: v.memory, tbl: this.#tbl(), ld: v.vetro_jit_ld, st: v.vetro_jit_st, resolve: v.vetro_jit_resolve },
        rt: this.#rt,
      });
      this.stats.compileMs += performance.now() - t0;
      this.#instances.set(id, instance.exports);
    }
    this.stats.modules++;
    this.stats.bytes += bytes.length;
    this.#since += bytes.length;
    this.#live.set(id, { bytes: bytes.length, places: [] });
    return id;
  }

  /**
   * Installs the runtime module (ADR 0024): its exports become the
   * imports `rt.*` of the modules compiled afterwards; it stays even after `reset`.
   */
  runtime(bytes) {
    const v = this.#vetro;
    const module = new WebAssembly.Module(bytes);
    const instance = new WebAssembly.Instance(module, {
      env: { mem: v.memory, ld: v.vetro_jit_ld, st: v.vetro_jit_st, vsync: v.vetro_jit_vsync, simd: v.vetro_jit_simd },
    });
    this.#rt = instance.exports;
  }

  /** Runs block `b<index>` of module `id` on the JitState at address `state`. */
  run(id, index, state) {
    return this.#instances.get(id)[`b${index}`](state);
  }

  /**
   * Module `id` is no longer used (vetro-jit evicted it, ADR 0040): its table
   * entries are cleared, so that nothing keeps it alive, and its bytes no
   * longer count against the budget.
   */
  drop(id) {
    this.#instances.delete(id);
    this.#pending.delete(id);
    const l = this.#live.get(id);
    if (!l) return;
    this.#live.delete(id);
    this.#since -= l.bytes;
    this.stats.dropped++;
    const t = this.#table;
    if (t) for (const [count, base] of l.places) for (let i = 0; i < count; i++) t.set(base + i, null);
  }

  /** Puts `b0..b<count-1>` of module `id` into the table from `base`. */
  place(id, count, base) {
    const p = this.#pending.get(id);
    if (p) {
      p.places.push([count, base]);
      return;
    }
    this.#live.get(id)?.places.push([count, base]);
    const x = this.#instances.get(id);
    const t = this.#tbl();
    for (let i = 0; i < count; i++) t.set(base + i, x[`b${i}`]);
  }

  /** Discards instances and table: the following modules use a new table. */
  reset() {
    this.#instances.clear();
    this.#pending.clear();
    this.#live.clear();
    this.#gen++;
    this.#table = null;
    // The entries handed to Rust keep their modules alive (the dispatcher
    // holds the block table, which holds every block): clearing them lets the
    // old code be freed. Rust no longer uses them (new modules have different
    // ids).
    const t = this.#vetro?.__indirect_function_table;
    for (const i of this.#entries) {
      t?.set(i, null);
      this.#free.push(i);
    }
    this.#entries = [];
    this.#since = 0;
    this.stats.resets++;
  }

  /**
   * Puts `b<index>` of module `id` into a new entry of vetro-wasm's
   * function table (`__indirect_function_table`, exported and
   * growable): from there Rust calls it as a function pointer, without
   * going through JS at every run. Returns the entry.
   */
  entry(id, index) {
    this.#force(id);
    const t = this.#vetro.__indirect_function_table;
    const i = this.#free.length ? this.#free.pop() : t.grow(1);
    t.set(i, this.#instances.get(id)[`b${index}`]);
    this.#entries.push(i);
    return i;
  }

  /** The `vetro_jit` imports to pass when instantiating vetro-wasm. */
  imports() {
    return {
      compile: (ptr, len) => {
        // Copy: the memory can grow, and the module stays valid.
        const bytes = new Uint8Array(this.#vetro.memory.buffer, ptr >>> 0, len).slice();
        try {
          return this.compile(bytes);
        } catch (e) {
          if (!(e instanceof RangeError)) console.error(`vetro_jit.compile: ${e}`);
          return -1;
        }
      },
      runtime: (ptr, len) => {
        const bytes = new Uint8Array(this.#vetro.memory.buffer, ptr >>> 0, len).slice();
        try {
          this.runtime(bytes);
          return 0;
        } catch (e) {
          console.error(`vetro_jit.runtime: ${e}`);
          return -1;
        }
      },
      entry: (id, index) => this.entry(id, index),
      drop: (id) => this.drop(id),
      place: (id, count, base) => this.place(id, count, base),
      reset: () => this.reset(),
      ready: (id) => this.ready(id),
    };
  }
}
