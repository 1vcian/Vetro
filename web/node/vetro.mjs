// Caricatore di vetro-wasm: istanzia il modulo con i suoi import e avvolge
// l'API C di docs/specs/wasm.md. Non usa API di Node: va bene anche nel
// browser (i byte del .wasm li passa chi chiama).

import { JitEngine } from './jit-engine.mjs';

export const ABI_VERSION = 2;
/** Codici di vetro_run. */
export const STOP = ['Budget', 'PowerOff', 'Reset', 'Idle', 'Unimplemented'];

const utf8 = new TextDecoder();
const toUtf8 = new TextEncoder();

/** Istanzia vetro-wasm dai byte del .wasm: { exports, jit }. */
export async function instantiate(wasmBytes) {
  const jit = new JitEngine();
  let exports = null;
  const imports = {
    vetro_host: {
      panic: (ptr, len) => {
        const msg = utf8.decode(new Uint8Array(exports.memory.buffer, ptr, len));
        console.error(`vetro-wasm: panic: ${msg}`);
      },
    },
    vetro_jit: jit.imports(),
  };
  const { instance } = await WebAssembly.instantiate(wasmBytes, imports);
  exports = instance.exports;
  jit.attach(exports);
  const abi = exports.vetro_abi_version();
  if (abi !== ABI_VERSION) throw new Error(`vetro-wasm: API ${abi}, attesa ${ABI_VERSION}`);
  return { exports, jit };
}

/** Copia `bytes` in un buffer nuovo della memoria del modulo: [ptr, len]. */
export function copyIn(x, bytes) {
  if (bytes.length === 0) return [0, 0];
  const ptr = x.vetro_alloc(bytes.length);
  if (ptr === 0) throw new Error(`vetro_alloc(${bytes.length}) fallita`);
  // Vista presa dopo l'allocazione: la memoria può essere cresciuta.
  new Uint8Array(x.memory.buffer, ptr, bytes.length).set(bytes);
  return [ptr, bytes.length];
}

/** Una macchina di vetro-wasm. */
export class Machine {
  #x;
  #vm;
  #buf;
  #cap = 64 * 1024;

  /** ramSize/nowSecs/seed: BigInt, 0n = i valori di MachineConfig::default. */
  constructor(x, { ramSize = 0n, nowSecs = 0n, seed = 0n } = {}) {
    this.#x = x;
    this.#vm = x.vetro_machine_new(ramSize, nowSecs, seed);
    this.#buf = x.vetro_alloc(this.#cap);
  }

  #message() {
    const x = this.#x;
    const ptr = x.vetro_message_ptr(this.#vm);
    return utf8.decode(new Uint8Array(x.memory.buffer, ptr, x.vetro_message_len(this.#vm)));
  }

  /** Kernel, initramfs (o null) e riga di comando; lancia in caso di errore. */
  loadLinux(image, initrd, cmdline) {
    const x = this.#x;
    const bufs = [copyIn(x, image), copyIn(x, initrd ?? new Uint8Array()), copyIn(x, toUtf8.encode(cmdline))];
    const code = x.vetro_load_linux(this.#vm, ...bufs.flat());
    for (const [p, n] of bufs) x.vetro_free(p, n);
    if (code !== 0) throw new Error(`vetro_load_linux: codice ${code}: ${this.#message()}`);
  }

  /** Esegue al più `budget` istruzioni; restituisce il motivo dell'arresto. */
  run(budget) {
    const x = this.#x;
    const code = x.vetro_run(this.#vm, BigInt(budget));
    if (STOP[code] === 'Unimplemented') {
      const pc = x.vetro_unimplemented_pc(this.#vm).toString(16);
      const raw = (x.vetro_unimplemented_raw(this.#vm) >>> 0).toString(16).padStart(8, '0');
      return `Unimplemented { pc: 0x${pc}, raw: 0x${raw}, what: ${JSON.stringify(this.#message())} }`;
    }
    return STOP[code] ?? `codice ${code}`;
  }

  /** L'uscita della console dall'ultima lettura (Uint8Array). */
  consoleRead() {
    const x = this.#x;
    const parts = [];
    let total = 0;
    for (;;) {
      const n = x.vetro_console_read(this.#vm, this.#buf, this.#cap);
      if (n === 0) break;
      parts.push(new Uint8Array(x.memory.buffer, this.#buf, n).slice());
      total += n;
    }
    if (parts.length === 1) return parts[0];
    const out = new Uint8Array(total);
    let at = 0;
    for (const p of parts) {
      out.set(p, at);
      at += p.length;
    }
    return out;
  }

  /** Scrive sulla console, come dalla tastiera. */
  consoleWrite(text) {
    const x = this.#x;
    const [p, n] = copyIn(x, toUtf8.encode(text));
    x.vetro_console_write(this.#vm, p, n);
    x.vetro_free(p, n);
  }

  /**
   * Attiva il JIT della modalità sistema (ADR 0013): `threshold` ingressi
   * prima di tradurre un blocco, `batch` blocchi per modulo. Il risultato
   * non cambia, solo la velocità.
   */
  setJit(threshold = 16, batch = 16) {
    this.#x.vetro_machine_set_jit(this.#vm, threshold, batch);
  }

  /** Contatori del JIT (`SysJitStats`), o null senza JIT. */
  jitStats() {
    const x = this.#x;
    const names = ['jitSteps', 'runs', 'resolves', 'calls', 'blocks', 'modules', 'reused', 'invalidatedPages', 'faults',
      'svcs', 'stops', 'epochs', 'tlbFlushes', 'tlbFills', 'resets'];
    const p = x.vetro_alloc(8 * names.length);
    const n = x.vetro_jit_stats(this.#vm, p, names.length);
    const v = new BigUint64Array(x.memory.buffer, p, names.length);
    const out = n ? Object.fromEntries(names.map((k, i) => [k, Number(v[i])])) : null;
    x.vetro_free(p, 8 * names.length);
    return out;
  }

  /** Istruzioni eseguite (BigInt). */
  get steps() {
    return this.#x.vetro_steps(this.#vm);
  }

  /** Tempo del guest in ns (BigInt). */
  get guestNs() {
    return this.#x.vetro_guest_ns(this.#vm);
  }

  free() {
    this.#x.vetro_free(this.#buf, this.#cap);
    this.#x.vetro_machine_free(this.#vm);
  }
}
