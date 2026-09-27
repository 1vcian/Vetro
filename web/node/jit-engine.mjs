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
// vetro-wasm calls it through the imports `vetro_jit.compile/entry/place/drop/reset`
// (crates/vetro-wasm/src/jit.rs). No dependencies: only the
// WebAssembly API, the same in Node and in the browser.

/** Entries of the block table (`vetro_jit::engine::TABLE_SIZE`). */
export const TABLE_SIZE = 1 << 18;

/**
 * At most this many bytes of generated modules compiled between two resets.
 * Beyond it, `compile` refuses the module and vetro-jit resets the engine (as
 * for a full table): V8 returns no error when the space for compiled code
 * runs out (4 GiB), it kills the process. With Android the JIT got there after
 * half an hour (ADR 0028); 96 MiB of wasm are a few hundred MiB of machine
 * code.
 */
export const CODE_BUDGET = 96 << 20;

export class JitEngine {
  #vetro = null; // exports of the vetro-wasm instance
  #table = null;
  #instances = new Map(); // index -> exports of the generated module
  #rt = {}; // exports of the runtime module (imports `rt.*` of the modules)
  #next = 0;
  /** Bytes compiled since the last reset, and the limit. */
  #since = 0;
  #budget;
  /** Entries of vetro-wasm's function table handed out with `entry`, and the free ones. */
  #entries = [];
  #free = [];
  /** Compiled modules, bytes and resets, for the benchmarks. */
  stats = { modules: 0, bytes: 0, resets: 0, compileMs: 0, refused: 0 };

  constructor({ budget = CODE_BUDGET } = {}) {
    this.#budget = budget;
  }

  /** To be called right after instantiating vetro-wasm (the imports are needed first). */
  attach(vetroExports) {
    this.#vetro = vetroExports;
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
    const t0 = performance.now();
    const module = new WebAssembly.Module(bytes);
    const instance = new WebAssembly.Instance(module, {
      env: { mem: v.memory, tbl: this.#tbl(), ld: v.vetro_jit_ld, st: v.vetro_jit_st, resolve: v.vetro_jit_resolve },
      rt: this.#rt,
    });
    this.stats.compileMs += performance.now() - t0;
    const id = this.#next++;
    this.#instances.set(id, instance.exports);
    this.stats.modules++;
    this.stats.bytes += bytes.length;
    this.#since += bytes.length;
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

  drop(id) {
    this.#instances.delete(id);
  }

  /** Puts `b0..b<count-1>` of module `id` into the table from `base`. */
  place(id, count, base) {
    const x = this.#instances.get(id);
    const t = this.#tbl();
    for (let i = 0; i < count; i++) t.set(base + i, x[`b${i}`]);
  }

  /** Discards instances and table: the following modules use a new table. */
  reset() {
    this.#instances.clear();
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
    };
  }
}
