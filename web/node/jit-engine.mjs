// Il motore JIT di vetro-wasm in JavaScript (ADR 0012, docs/specs/jit.md,
// docs/specs/wasm.md). Corrisponde al trait `vetro_jit::Engine`:
//
// - compile(bytes): WebAssembly.Module, istanziato subito con
//   env.mem = la memoria di vetro-wasm e env.ld / env.st = i suoi export
//   vetro_jit_ld / vetro_jit_st (che fanno MMU, permessi e bus);
// - run(id, index, state): il codice d'uscita di `b<index>(state)`;
// - memory: è la memoria lineare di vetro-wasm, dove sta `JitState`.
//
// vetro-wasm lo chiama tramite gli import `vetro_jit.compile/run/drop`
// (crates/vetro-wasm/src/jit.rs). Nessuna dipendenza: solo l'API
// WebAssembly, uguale in Node e nel browser.

export class JitEngine {
  #vetro = null; // export dell'istanza di vetro-wasm
  #instances = new Map(); // indice -> export del modulo generato
  #next = 0;
  /** Moduli compilati e byte, per i benchmark. */
  stats = { modules: 0, bytes: 0 };

  /** Da chiamare appena istanziato vetro-wasm (gli import servono prima). */
  attach(vetroExports) {
    this.#vetro = vetroExports;
  }

  /** Compila e istanzia un modulo generato; restituisce il suo indice. */
  compile(bytes) {
    const v = this.#vetro;
    const module = new WebAssembly.Module(bytes);
    const instance = new WebAssembly.Instance(module, {
      env: { mem: v.memory, ld: v.vetro_jit_ld, st: v.vetro_jit_st },
    });
    const id = this.#next++;
    this.#instances.set(id, instance.exports);
    this.stats.modules++;
    this.stats.bytes += bytes.length;
    return id;
  }

  /** Esegue il blocco `b<index>` del modulo `id` sul JitState all'indirizzo `state`. */
  run(id, index, state) {
    return this.#instances.get(id)[`b${index}`](state);
  }

  drop(id) {
    this.#instances.delete(id);
  }

  /** Gli import `vetro_jit` da passare all'istanziazione di vetro-wasm. */
  imports() {
    return {
      compile: (ptr, len) => {
        // Copia: la memoria può crescere, e il modulo resta valido.
        const bytes = new Uint8Array(this.#vetro.memory.buffer, ptr, len).slice();
        try {
          return this.compile(bytes);
        } catch (e) {
          console.error(`vetro_jit.compile: ${e}`);
          return -1;
        }
      },
      run: (id, index, state) => this.run(id, index, state),
      drop: (id) => this.drop(id),
    };
  }
}
