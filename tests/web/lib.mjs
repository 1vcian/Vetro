// Common helpers for the web tests in Node (tests/web): loading vetro-wasm and
// the M3 guest kernel, a session with the console and the disks.

import { existsSync, readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { instantiate, Machine } from '../../web/node/vetro.mjs';

export const root = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
export const wasmPath = process.env.VETRO_WASM ?? join(root, 'target/wasm32-unknown-unknown/release/vetro_wasm.wasm');
export const kernelDir = join(root, 'target/guest-kernel');

// The same constants as tests/boot/src/lib.rs.
export const SHELL_PROMPT = '# \x1b[6n';
export const PHASE_BUDGET = 6_000_000_000n;
export const QUANTUM = 1_000_000n;

/** The JSON POST by `wget` to the sinkhole of the inspector and replay tests. */
export const POST_JSON = `wget -q -O /dev/null --header 'Content-Type: application/json' --post-data '{"vetro":42,"nome":"prova"}' http://api.vetro.test/v1/eventi`;

export class Fail extends Error {}

export const normalize = (s) => s.replaceAll('\r\n', '\n').replaceAll('\r', '\n');

/** The guest kernel (Image, initramfs) or an explicit failure. */
export function guestKernel() {
  const image = join(kernelDir, 'Image');
  const initrd = join(kernelDir, 'initramfs.cpio.gz');
  if (!existsSync(image) || !existsSync(initrd)) {
    throw new Fail('target/guest-kernel missing: run tools/guest-kernel/build.sh');
  }
  return { image: readFileSync(image), initrd: readFileSync(initrd) };
}

export async function loadVetro(opts = {}) {
  if (!existsSync(wasmPath)) throw new Fail(`${wasmPath} missing: cargo build --release --target wasm32-unknown-unknown -p vetro-wasm`);
  return instantiate(readFileSync(wasmPath), opts);
}

/**
 * A machine with the M3 kernel and a script at the console. The guest advances in
 * quanta of QUANTUM instructions with absolute boundaries (multiples of QUANTUM):
 * a `Blocked` stop (disk waiting for data) doesn't move the boundaries, the
 * quantum resumes after the DiskFeeder has delivered the blocks. The log is
 * looked at and input is given only at the boundaries: so instructions and log are the
 * same with a local disk and with one over HTTP.
 */
export class Session {
  log = '';
  blocked = 0;

  /**
   * `restore`: bytes of a snapshot to restore instead of loading the
   * kernel (after `setup`, which adds the same disks); `onQuantum`: called
   * at every quantum boundary (after reading the console).
   */
  constructor(exports, kernel, { cmdline = 'console=ttyAMA0 vetro.noautotest', jit = true, machine = {}, setup = () => {}, restore = null, onQuantum = null, load = null } = {}) {
    this.m = new Machine(exports, machine);
    this.feeder = null;
    this.onQuantum = onQuantum;
    setup(this);
    if (restore) {
      const t0 = performance.now();
      this.m.snapshotRestore(restore);
      this.restoreMs = performance.now() - t0;
    }
    else if (load) load(this.m);
    else this.m.loadLinux(kernel.image, kernel.initrd, cmdline);
    if (jit) this.m.setJit(16, 16);
  }

  #pull() {
    const out = this.m.consoleRead();
    if (out.length) this.log += Buffer.from(out.buffer, out.byteOffset, out.length).toString('latin1');
  }

  /** One quantum up to the next boundary; returns the stop. */
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
      if (!this.feeder || (await this.feeder.serve()) === 0) throw new Fail('disk waiting with no blocks to request');
    }
  }

  /** Runs until `needle` appears after `from`; returns the position after it. */
  async until(needle, from = 0) {
    const limit = this.m.steps + PHASE_BUDGET;
    for (;;) {
      const i = this.log.indexOf(needle, from);
      if (i >= 0) return i + needle.length;
      if (this.m.steps >= limit) throw new Fail(`${JSON.stringify(needle)} did not arrive:\n${this.tail()}`);
      const stop = await this.quantum();
      if (stop !== 'Budget') throw new Fail(`${stop} while waiting for ${JSON.stringify(needle)}:\n${this.tail()}`);
    }
  }

  /** Sends a command to the shell and waits for the prompt after it; returns [start, end]. */
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

/** Runs `main` and turns the outcome into an exit code. */
export function run(main) {
  main().then(
    () => (process.exitCode = 0),
    (e) => {
      console.error(`ERROR: ${e instanceof Fail ? e.message : e.stack ?? e}`);
      process.exit(1);
    },
  );
}

export function check(cond, msg) {
  if (!cond) throw new Fail(msg);
}

/**
 * Comparison with the native reference (tests/boot/tests/web.rs, the same vetro-wasm
 * API compiled for the host, interpreter, local disk): instructions and
 * raw log must match. With VETRO_WEB_NATIVE=1 (tools/web-test.sh,
 * which runs the reference first) the comparison is mandatory; without it, we
 * say that it was not done.
 */
export function compareNative(name, steps, rawLog) {
  const dir = join(root, 'target/web-test');
  const stepsFile = join(dir, `native-${name}.steps`);
  if (process.env.VETRO_WEB_NATIVE !== '1') {
    console.log(`(comparison with native not done: VETRO_WEB_NATIVE=1 after cargo test --release -p vetro-boot-tests --test web)`);
    return;
  }
  check(existsSync(stepsFile), `${stepsFile} missing: cargo test --release -p vetro-boot-tests --test web`);
  const nSteps = BigInt(readFileSync(stepsFile, 'latin1').trim());
  const nLog = readFileSync(join(dir, `native-${name}.log`)).toString('latin1');
  check(nSteps === steps, `${name}: ${steps} instructions, native ${nSteps}`);
  check(nLog === rawLog, `${name}: log differs from the native one (target/web-test/native-${name}.log)`);
  console.log(`${name}: equal to native (${steps} instructions, log byte for byte)`);
}
