// Record & replay dal JS (M10, ADR 0019 e 0023): il log registrato (o
// caricato da un file) resta nella macchina di vetro-wasm, i suoi keyframe
// (snapshot completi, ~10 MB l'uno) si spostano in un archivio (OPFS nel
// Worker, memoria nei test) e rientrano solo quando un replay parte da loro.
// Non usa API di Node.
//
// L'archivio è uno `SnapshotStore` (web/node/persist.mjs): `kf-<i>` per i
// keyframe e `log` per il log senza i byte dei keyframe (meta: numero e
// dimensioni dei keyframe, istruzioni di partenza e fine). Si scrive prima
// ogni keyframe, poi il log: un log nell'archivio ha i suoi keyframe.

export class Recording {
  #m;
  #store;

  /** `machine`: Machine di vetro.mjs; `store`: SnapshotStore (opfs o memory). */
  constructor(machine, store) {
    this.#m = machine;
    this.#store = store;
    /** Metadati del log nell'archivio ({ keyframes, sizes, startSteps, endSteps, events, savedAt }), o null. */
    this.meta = null;
  }

  /**
   * Dopo `recordStop` o `logLoad`: sposta i keyframe nell'archivio e ci
   * salva il log (senza i loro byte). Restituisce i metadati.
   */
  async store() {
    const m = this.#m;
    const info = m.logInfo();
    if (!info) throw new Error('nessun log da salvare');
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

  /** All'avvio: rilegge il log dell'archivio nella macchina (keyframe fuori). null se non c'è. */
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

  /** Rimette nella macchina il keyframe da cui parte il replay verso `step`; restituisce il suo indice (-1 nessuno). */
  async ensureKeyframe(step) {
    const m = this.#m;
    const info = m.logInfo();
    if (!info) throw new Error('nessun log');
    const i = m.logKeyframeFor(Math.max(Number(step), info.startSteps));
    if (i >= 0 && !m.logKeyframe(i).present) {
      const rec = await this.#store.load(`kf-${i}`);
      if (!rec) throw new Error(`keyframe ${i} assente dall'archivio`);
      m.logKeyframePut(i, rec.bytes);
    }
    return i;
  }

  /** Toglie dalla macchina i keyframe presenti (sono nell'archivio). */
  dropKeyframes() {
    const info = this.#m.logInfo();
    for (let i = 0; i < (info?.keyframes ?? 0); i++) if (this.#m.logKeyframe(i).present) this.#m.logKeyframeTake(i);
  }

  /** Il file completo del log, con tutti i keyframe (rimessi per il tempo della codifica). */
  async encodeFull() {
    const m = this.#m;
    const info = m.logInfo();
    if (!info) throw new Error('nessun log');
    for (let i = 0; i < info.keyframes; i++) {
      if (m.logKeyframe(i).present) continue;
      const rec = await this.#store.load(`kf-${i}`);
      if (!rec) throw new Error(`keyframe ${i} assente dall'archivio`);
      m.logKeyframePut(i, rec.bytes);
    }
    const bytes = m.logEncode();
    this.dropKeyframes();
    return bytes;
  }

  /** Carica un file di log (lancia se non è valido) e lo archivia. */
  async load(bytes) {
    this.#m.logLoad(bytes);
    return this.store();
  }
}
