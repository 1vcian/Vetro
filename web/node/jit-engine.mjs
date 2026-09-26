// Il motore JIT di vetro-wasm in JavaScript (ADR 0012 e 0013,
// docs/specs/jit.md, docs/specs/wasm.md). Corrisponde al trait
// `vetro_jit::Engine`:
//
// - compile(bytes): WebAssembly.Module, istanziato subito con
//   env.mem = la memoria di vetro-wasm, env.ld / env.st = gli export
//   vetro_jit_ld / vetro_jit_st di vetro-wasm (che fanno MMU, permessi e
//   bus) e, per il dispatcher, env.tbl = la tabella dei blocchi (funcref,
//   TABLE_SIZE voci);
// - place(id, count, base): mette gli export b0..b<count-1> del modulo nella
//   tabella dalla voce `base` (il dispatcher li chiama con call_indirect);
// - run(id, index, state): il codice d'uscita di `b<index>(state)`;
// - entry(id, index): mette `b<index>` nella tabella delle funzioni di
//   vetro-wasm, da cui Rust lo chiama direttamente (senza JS a ogni corsa);
// - reset(): scarta tutte le istanze e ricrea la tabella;
// - memory: è la memoria lineare di vetro-wasm, dove stanno `JitState`, la
//   cache dei salti, la TLB software e la RAM del guest.
//
// vetro-wasm lo chiama tramite gli import `vetro_jit.compile/entry/place/drop/reset`
// (crates/vetro-wasm/src/jit.rs). Nessuna dipendenza: solo l'API
// WebAssembly, uguale in Node e nel browser.

/** Voci della tabella dei blocchi (`vetro_jit::engine::TABLE_SIZE`). */
export const TABLE_SIZE = 1 << 18;

/**
 * Byte di moduli generati compilati fra un azzeramento e l'altro, al più.
 * Oltre, `compile` rifiuta il modulo e vetro-jit azzera il motore (come per
 * la tabella piena): V8 non restituisce errori quando lo spazio per il
 * codice compilato finisce (4 GiB), chiude il processo. Con Android il JIT
 * arrivava lì dopo mezz'ora (ADR 0028); 96 MiB di wasm sono qualche centinaio
 * di MiB di codice macchina.
 */
export const CODE_BUDGET = 96 << 20;

export class JitEngine {
  #vetro = null; // export dell'istanza di vetro-wasm
  #table = null;
  #instances = new Map(); // indice -> export del modulo generato
  #rt = {}; // export del modulo di runtime (import `rt.*` dei moduli)
  #next = 0;
  /** Byte compilati dall'ultimo azzeramento, e limite. */
  #since = 0;
  #budget;
  /** Voci della tabella delle funzioni di vetro-wasm date con `entry`, e quelle libere. */
  #entries = [];
  #free = [];
  /** Moduli compilati, byte e azzeramenti, per i benchmark. */
  stats = { modules: 0, bytes: 0, resets: 0, compileMs: 0, refused: 0 };

  constructor({ budget = CODE_BUDGET } = {}) {
    this.#budget = budget;
  }

  /** Da chiamare appena istanziato vetro-wasm (gli import servono prima). */
  attach(vetroExports) {
    this.#vetro = vetroExports;
  }

  #tbl() {
    this.#table ??= new WebAssembly.Table({ element: 'anyfunc', initial: TABLE_SIZE });
    return this.#table;
  }

  /** Compila e istanzia un modulo generato; restituisce il suo indice. */
  compile(bytes) {
    const v = this.#vetro;
    if (this.#since > 0 && this.#since + bytes.length > this.#budget) {
      this.stats.refused++;
      throw new RangeError(`limite del codice del JIT (${this.#budget} byte dall'ultimo azzeramento)`);
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
   * Installa il modulo di runtime (ADR 0024): i suoi export diventano gli
   * import `rt.*` dei moduli compilati dopo; resta anche dopo `reset`.
   */
  runtime(bytes) {
    const v = this.#vetro;
    const module = new WebAssembly.Module(bytes);
    const instance = new WebAssembly.Instance(module, {
      env: { mem: v.memory, ld: v.vetro_jit_ld, st: v.vetro_jit_st, vsync: v.vetro_jit_vsync, simd: v.vetro_jit_simd },
    });
    this.#rt = instance.exports;
  }

  /** Esegue il blocco `b<index>` del modulo `id` sul JitState all'indirizzo `state`. */
  run(id, index, state) {
    return this.#instances.get(id)[`b${index}`](state);
  }

  drop(id) {
    this.#instances.delete(id);
  }

  /** Mette `b0..b<count-1>` del modulo `id` nella tabella da `base`. */
  place(id, count, base) {
    const x = this.#instances.get(id);
    const t = this.#tbl();
    for (let i = 0; i < count; i++) t.set(base + i, x[`b${i}`]);
  }

  /** Scarta istanze e tabella: i moduli successivi usano una tabella nuova. */
  reset() {
    this.#instances.clear();
    this.#table = null;
    // Le voci date a Rust tengono vivi i loro moduli (il dispatcher tiene la
    // tabella dei blocchi, che tiene tutti i blocchi): si svuotano, così il
    // codice di prima si può liberare. Rust non le usa più (gli id dei moduli
    // nuovi sono diversi).
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
   * Mette `b<index>` del modulo `id` in una voce nuova della tabella delle
   * funzioni di vetro-wasm (`__indirect_function_table`, esportata ed
   * estendibile): da lì Rust la chiama come un puntatore a funzione, senza
   * passare da JS a ogni corsa. Restituisce la voce.
   */
  entry(id, index) {
    const t = this.#vetro.__indirect_function_table;
    const i = this.#free.length ? this.#free.pop() : t.grow(1);
    t.set(i, this.#instances.get(id)[`b${index}`]);
    this.#entries.push(i);
    return i;
  }

  /** Gli import `vetro_jit` da passare all'istanziazione di vetro-wasm. */
  imports() {
    return {
      compile: (ptr, len) => {
        // Copia: la memoria può crescere, e il modulo resta valido.
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
