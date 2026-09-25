// Aiuti comuni ai test web in Node (tests/web): caricamento di vetro-wasm e
// del kernel guest di M3, una sessione con la console e i dischi.

import { existsSync, readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { instantiate, Machine } from '../../web/node/vetro.mjs';

export const root = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
export const wasmPath = process.env.VETRO_WASM ?? join(root, 'target/wasm32-unknown-unknown/release/vetro_wasm.wasm');
export const kernelDir = join(root, 'target/guest-kernel');

// Le stesse costanti di tests/boot/src/lib.rs.
export const SHELL_PROMPT = '# \x1b[6n';
export const PHASE_BUDGET = 6_000_000_000n;
export const QUANTUM = 1_000_000n;

/** Il POST JSON di `wget` verso il sinkhole dei test dell'ispettore e del replay. */
export const POST_JSON = `wget -q -O /dev/null --header 'Content-Type: application/json' --post-data '{"vetro":42,"nome":"prova"}' http://api.vetro.test/v1/eventi`;

export class Fail extends Error {}

export const normalize = (s) => s.replaceAll('\r\n', '\n').replaceAll('\r', '\n');

/** Il kernel guest (Image, initramfs) o un fallimento esplicito. */
export function guestKernel() {
  const image = join(kernelDir, 'Image');
  const initrd = join(kernelDir, 'initramfs.cpio.gz');
  if (!existsSync(image) || !existsSync(initrd)) {
    throw new Fail('target/guest-kernel mancante: esegui tools/guest-kernel/build.sh');
  }
  return { image: readFileSync(image), initrd: readFileSync(initrd) };
}

export async function loadVetro() {
  if (!existsSync(wasmPath)) throw new Fail(`${wasmPath} mancante: cargo build --release --target wasm32-unknown-unknown -p vetro-wasm`);
  return instantiate(readFileSync(wasmPath));
}

/**
 * Una macchina col kernel M3 e un copione alla console. Il guest avanza a
 * quanti di QUANTUM istruzioni con confini assoluti (multipli di QUANTUM):
 * un arresto `Blocked` (disco in attesa di dati) non sposta i confini, il
 * quanto riprende dopo che il DiskFeeder ha consegnato i blocchi. Il log si
 * guarda e l'ingresso si dà solo ai confini: così istruzioni e log sono gli
 * stessi con un disco locale e con uno via HTTP.
 */
export class Session {
  log = '';
  blocked = 0;

  /**
   * `restore`: byte di uno snapshot da ripristinare invece di caricare il
   * kernel (dopo `setup`, che aggiunge gli stessi dischi); `onQuantum`: si
   * chiama a ogni confine di quanto (dopo aver letto la console).
   */
  constructor(exports, kernel, { cmdline = 'console=ttyAMA0 vetro.noautotest', jit = true, machine = {}, setup = () => {}, restore = null, onQuantum = null } = {}) {
    this.m = new Machine(exports, machine);
    this.feeder = null;
    this.onQuantum = onQuantum;
    setup(this);
    if (restore) {
      const t0 = performance.now();
      this.m.snapshotRestore(restore);
      this.restoreMs = performance.now() - t0;
    }
    else this.m.loadLinux(kernel.image, kernel.initrd, cmdline);
    if (jit) this.m.setJit(16, 16);
  }

  #pull() {
    const out = this.m.consoleRead();
    if (out.length) this.log += Buffer.from(out.buffer, out.byteOffset, out.length).toString('latin1');
  }

  /** Un quanto fino al prossimo confine; restituisce l'arresto. */
  async quantum() {
    const target = (this.m.steps / QUANTUM + 1n) * QUANTUM;
    for (;;) {
      const stop = this.m.run(target - this.m.steps);
      this.#pull();
      if (stop !== 'Blocked') {
        this.onQuantum?.(this);
        return stop;
      }
      this.blocked++;
      if (!this.feeder || (await this.feeder.serve()) === 0) throw new Fail('disco in attesa senza blocchi da chiedere');
    }
  }

  /** Esegue finché `needle` compare dopo `from`; restituisce la posizione dopo. */
  async until(needle, from = 0) {
    const limit = this.m.steps + PHASE_BUDGET;
    for (;;) {
      const i = this.log.indexOf(needle, from);
      if (i >= 0) return i + needle.length;
      if (this.m.steps >= limit) throw new Fail(`${JSON.stringify(needle)} non arrivato:\n${this.tail()}`);
      const stop = await this.quantum();
      if (stop !== 'Budget') throw new Fail(`${stop} in attesa di ${JSON.stringify(needle)}:\n${this.tail()}`);
    }
  }

  /** Manda un comando alla shell e aspetta il prompt dopo; restituisce [inizio, fine]. */
  async command(cmd, from) {
    this.m.consoleWrite(`${cmd}\n`);
    const end = await this.until(SHELL_PROMPT, from);
    return [from, end];
  }

  text(from = 0, to = undefined) {
    return normalize(this.log.slice(from, to));
  }

  tail() {
    return normalize(this.log).split('\n').slice(-40).join('\n');
  }

  async poweroff(from) {
    this.m.consoleWrite('poweroff -f\n');
    const limit = this.m.steps + PHASE_BUDGET;
    let stop;
    do {
      stop = await this.quantum();
    } while (stop === 'Budget' && this.m.steps < limit);
    if (stop !== 'PowerOff') throw new Fail(`poweroff -f: ${stop}:\n${this.tail()}`);
    return from;
  }
}

/** Esegue `main` e traduce l'esito in codice d'uscita. */
export function run(main) {
  main().then(
    () => (process.exitCode = 0),
    (e) => {
      console.error(`ERRORE: ${e instanceof Fail ? e.message : e.stack ?? e}`);
      process.exit(1);
    },
  );
}

export function check(cond, msg) {
  if (!cond) throw new Fail(msg);
}

/**
 * Confronto col riferimento nativo (tests/boot/tests/web.rs, stessa API di
 * vetro-wasm compilata per l'host, interprete, disco locale): istruzioni e
 * log grezzo devono coincidere. Con VETRO_WEB_NATIVE=1 (tools/web-test.sh,
 * che esegue prima il riferimento) il confronto è obbligatorio; senza, si
 * dice che non è stato fatto.
 */
export function compareNative(name, steps, rawLog) {
  const dir = join(root, 'target/web-test');
  const stepsFile = join(dir, `native-${name}.steps`);
  if (process.env.VETRO_WEB_NATIVE !== '1') {
    console.log(`(confronto col nativo non fatto: VETRO_WEB_NATIVE=1 dopo cargo test --release -p vetro-boot-tests --test web)`);
    return;
  }
  check(existsSync(stepsFile), `${stepsFile} mancante: cargo test --release -p vetro-boot-tests --test web`);
  const nSteps = BigInt(readFileSync(stepsFile, 'latin1').trim());
  const nLog = readFileSync(join(dir, `native-${name}.log`)).toString('latin1');
  check(nSteps === steps, `${name}: ${steps} istruzioni, nativo ${nSteps}`);
  check(nLog === rawLog, `${name}: log diverso da quello nativo (target/web-test/native-${name}.log)`);
  console.log(`${name}: uguale al nativo (${steps} istruzioni, log byte per byte)`);
}
