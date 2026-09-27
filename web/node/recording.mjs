// Record & replay from JS (M10, ADR 0019 and 0023): the recorded log (or
// loaded from a file) stays in the vetro-wasm machine, its keyframes
// (full snapshots, ~10 MB each) are moved to an archive (OPFS in the
// Worker, memory in the tests) and come back only when a replay starts from them.
// It doesn't use Node APIs.
//
// The archive is a `SnapshotStore` (web/node/persist.mjs): `kf-<i>` for the
// keyframes and `log` for the log without the keyframe bytes (meta: number and
// sizes of the keyframes, start and end instructions). Every keyframe is written
// first, then the log: a log in the archive has its keyframes.

export class Recording {
  #m;
  #store;

  /** `machine`: Machine of vetro.mjs; `store`: SnapshotStore (opfs or memory). */
  constructor(machine, store) {
    this.#m = machine;
    this.#store = store;
    /** Metadata of the log in the archive ({ keyframes, sizes, startSteps, endSteps, events, savedAt }), or null. */
    this.meta = null;
  }

  /**
   * After `recordStop` or `logLoad`: moves the keyframes into the archive and
   * saves the log there (without their bytes). Returns the metadata.
   */
  async store() {
    const m = this.#m;
    const info = m.logInfo();
    if (!info) throw new Error('no log to save');
    const old = await this.#store.load('log').catch(() => null);
    await this.#store.remove('log');
    const sizes = [];
    for (let i = 0; i < info.keyframes; i++) {
      const bytes = m.logKeyframeTake(i);
      const k = m.logKeyframe(i);
      if (bytes.length) await this.#store.save(`kf-${i}`, { step: k.step }, bytes);
      sizes.push(bytes.length || k.size);
    }
    for (let i = info.keyframes; i < (old?.meta.keyframes ?? 0); i++) await this.#store.remove(`kf-${i}`);
    this.meta = {
      keyframes: info.keyframes,
      sizes,
      startSteps: info.startSteps,
      endSteps: info.endSteps,
      events: info.events,
      savedAt: new Date().toISOString(),
    };
    await this.#store.save('log', this.meta, m.logEncode());
    return this.meta;
  }

  /** At startup: reads back the archived log into the machine (keyframes out). null if there is none. */
  async restore() {
    const rec = await this.#store.load('log').catch(() => null);
    if (!rec) return null;
    try {
      this.#m.logLoad(rec.bytes);
    } catch {
      return null;
    }
    this.meta = rec.meta;
    return this.#m.logInfo();
  }

  /** Puts back into the machine the keyframe from which the replay towards `step` starts; returns its index (-1 none). */
  async ensureKeyframe(step) {
    const m = this.#m;
    const info = m.logInfo();
    if (!info) throw new Error('no log');
    const i = m.logKeyframeFor(Math.max(Number(step), info.startSteps));
    if (i >= 0 && !m.logKeyframe(i).present) {
      const rec = await this.#store.load(`kf-${i}`);
      if (!rec) throw new Error(`keyframe ${i} missing from the archive`);
      m.logKeyframePut(i, rec.bytes);
    }
    return i;
  }

  /** Removes from the machine the keyframes present (they are in the archive). */
  dropKeyframes() {
    const info = this.#m.logInfo();
    for (let i = 0; i < (info?.keyframes ?? 0); i++) if (this.#m.logKeyframe(i).present) this.#m.logKeyframeTake(i);
  }

  /** The complete log file, with all the keyframes (put back for the duration of the encoding). */
  async encodeFull() {
    const m = this.#m;
    const info = m.logInfo();
    if (!info) throw new Error('no log');
    for (let i = 0; i < info.keyframes; i++) {
      if (m.logKeyframe(i).present) continue;
      const rec = await this.#store.load(`kf-${i}`);
      if (!rec) throw new Error(`keyframe ${i} missing from the archive`);
      m.logKeyframePut(i, rec.bytes);
    }
    const bytes = m.logEncode();
    this.dropKeyframes();
    return bytes;
  }

  /** Loads a log file (throws if it is invalid) and archives it. */
  async load(bytes) {
    this.#m.logLoad(bytes);
    return this.store();
  }
}
