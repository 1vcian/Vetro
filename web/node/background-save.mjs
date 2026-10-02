// Deferred machine snapshots (M6, ADR 0046): the machine's thread only copies,
// another thread compresses and writes. Uses no Node or DOM API beyond
// messages: it runs in the app's Workers and in Node tests (in one thread).
//
// - `BackgroundSave` (machine side): starts the deferred save
//   (`Machine.snapshotBackground`: the state of this instant, the RAM trapped
//   copy-on-write) and, between the machine's slices, hands the raw stream
//   (`Machine.snapshotPump`) to the saver in transferred pieces, at most
//   WINDOW bytes unacknowledged, as many pieces as `pump(pieces)` allows.
// - `SaveJob` (saver side): the `SnapshotAssembler` over a
//   `SnapshotStore.saveTarget` file; the snapshot replaces the previous one
//   only once complete.
//
// Messages, machine to saver: `begin` { key, meta, plan }, `data` { bytes },
// `end`, `abort`; saver to machine: `ack` { bytes }, `done` { size, ms },
// `error` { message }.

import { SnapshotAssembler } from './vetro.mjs';

/** Raw stream bytes per piece. */
export const PIECE = 4 << 20;
/** Raw stream bytes handed over and not yet taken by the saver. */
export const WINDOW = 16 << 20;

/** The machine side of one deferred save. */
export class BackgroundSave {
  #m;
  #send;
  #inFlight = 0;
  #given = false;
  #settle;
  /** Bytes of the raw stream handed over. */
  sent = 0;
  /** Raw stream length (from the plan). */
  total = 0;
  /** Wall time spent in `snapshotPump` and in copying (ms): the machine thread's cost. */
  pumpMs = 0;
  /** Time spent starting the save (ms). */
  beginMs = 0;
  /** Most bytes of guest pages kept at once. */
  keptMax = 0;
  /** Resolves with the saver's `done` ({ size, ms }), rejects with its error. */
  done;

  /**
   * Starts the save of `m` now. `send(msg, transfer)` reaches the saver;
   * its replies come back through `reply`.
   */
  constructor(m, send, key, meta) {
    this.#m = m;
    this.#send = send;
    this.done = new Promise((ok, ko) => (this.#settle = { ok, ko }));
    // Not an unhandled rejection if the caller only looks at `active`.
    this.done.catch(() => {});
    const t0 = performance.now();
    const plan = m.snapshotBackground();
    this.beginMs = performance.now() - t0;
    this.total = Number(new DataView(plan.buffer, plan.byteOffset + 8, 8).getBigUint64(0, true));
    send({ type: 'begin', key, meta, plan }, [plan.buffer]);
  }

  /** True until everything has been handed over and the saver answered. */
  get active() {
    return !!this.#settle;
  }

  /** True if the saver can take more now. */
  get hungry() {
    return !this.#given && this.#inFlight < WINDOW;
  }

  /**
   * Between two slices: hands over up to `pieces` pieces while the window
   * has room. Returns the bytes handed over now.
   */
  pump(pieces = Infinity) {
    if (!this.#settle || this.#given || pieces <= 0) return 0;
    let n = 0;
    const t0 = performance.now();
    try {
      for (let k = 0; k < pieces && this.#inFlight < WINDOW; k++) {
        const bytes = this.#m.snapshotPump(PIECE);
        if (!bytes) {
          this.#given = true;
          this.#send({ type: 'end' });
          break;
        }
        this.#inFlight += bytes.length;
        this.sent += bytes.length;
        n += bytes.length;
        this.#send({ type: 'data', bytes }, [bytes.buffer]);
      }
      this.keptMax = Math.max(this.keptMax, this.#m.snapshotKept);
    } catch (e) {
      this.#fail(e);
    } finally {
      this.pumpMs += performance.now() - t0;
    }
    return n;
  }

  /** A message from the saver. */
  reply(msg) {
    if (!this.#settle) return;
    if (msg.type === 'ack') this.#inFlight -= msg.bytes;
    else if (msg.type === 'done') {
      const s = this.#settle;
      this.#settle = null;
      s.ok(msg);
    } else if (msg.type === 'error') this.#fail(new Error(msg.message));
  }

  /** Gives up (the previous snapshot stays). */
  cancel(why = 'cancelled') {
    this.#fail(new Error(why));
  }

  #fail(e) {
    if (!this.#settle) return;
    const s = this.#settle;
    this.#settle = null;
    this.#m.snapshotCancelBackground();
    this.#send({ type: 'abort' });
    s.ko(e);
  }
}

/**
 * The saver side: one save at a time over `store` (a SnapshotStore), with the
 * vetro-wasm exports `x` (its own instance). `reply(msg)` answers.
 */
export class SaveJob {
  #x;
  #store;
  #reply;
  #job = null;
  /** Messages run in order (begin opens the file asynchronously). */
  #queue = Promise.resolve();

  constructor(x, store, reply) {
    this.#x = x;
    this.#store = store;
    this.#reply = reply;
  }

  handle(msg) {
    this.#queue = this.#queue.then(() => this.#run(msg)).catch((e) => this.#error(e));
    return this.#queue;
  }

  async #run(msg) {
    switch (msg.type) {
      case 'begin': {
        await this.#drop();
        const target = await this.#store.saveTarget(msg.key);
        this.#job = { target, asm: new SnapshotAssembler(this.#x, msg.plan, target.file), key: msg.key, meta: msg.meta, t0: performance.now(), ms: 0 };
        break;
      }
      case 'data': {
        const j = this.#job;
        if (!j) return;
        const t = performance.now();
        j.asm.push(msg.bytes);
        j.ms += performance.now() - t;
        this.#reply({ type: 'ack', bytes: msg.bytes.length });
        break;
      }
      case 'end': {
        const j = this.#job;
        if (!j) return;
        const t = performance.now();
        const size = j.asm.finish();
        j.ms += performance.now() - t;
        j.asm.free();
        this.#job = null;
        await j.target.finish(j.meta, size);
        this.#reply({ type: 'done', size, ms: j.ms, wallMs: performance.now() - j.t0 });
        break;
      }
      case 'abort':
        await this.#drop();
        break;
    }
  }

  async #drop() {
    const j = this.#job;
    this.#job = null;
    if (j) {
      j.asm.free();
      await j.target.abort();
    }
  }

  async #error(e) {
    await this.#drop().catch(() => {});
    this.#reply({ type: 'error', message: String(e?.message ?? e) });
  }
}
