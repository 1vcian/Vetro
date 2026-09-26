// Il Worker dell'app: la macchina di vetro-wasm gira qui, a quanti, fuori
// dal thread della pagina. Riceve la configurazione e gli ingressi
// (tastiera, puntatore, tocco, console, tasto di accensione) come messaggi,
// e manda alla pagina l'uscita della console, i rettangoli cambiati dello
// scanout (ArrayBuffer trasferiti, niente copie), il cursore e le
// statistiche.
//
// Gli ingressi arrivano alla macchina fra un quanto e l'altro, cioè a un
// numero di istruzioni preciso: il registro `inputLog` (istruzione, evento)
// è quello che il replay di M10 rigiocherà.
//
// Tempo: il guest conta istruzioni (10 ns l'una). Con `realtime` il Worker
// non lascia correre il tempo del guest davanti all'orologio vero (dorme la
// differenza); senza, va alla massima velocità. Mentre un disco aspetta
// dati (`Blocked`) il tempo del guest è fermo (ADR 0014).
//
// Gestore dei file (M8, ADR 0020): con `files` la macchina ha virtio-vsock e
// il Worker tiene il client `GuestFiles` del demone `vetro-files` del guest;
// le richieste della pagina (messaggi `files`) sono ingressi come gli altri
// (registrati in `inputLog`), il client avanza fra una fetta e l'altra e
// risposte, eventi di inotify e stato tornano alla pagina (`files-reply`,
// `files-event`, `files-status`). Dopo il ripristino di uno snapshot il
// client è nuovo: le connessioni rimaste nello snapshot si chiudono.
//
// Ispettore di rete e timeline (M7, ADR 0023): con la rete la cattura è
// accesa dall'avvio; ogni ~0,7 s, se qualcosa è cambiato, il Worker manda
// alla pagina la lista delle richieste e la timeline (`analysis`); dettaglio,
// HAR e pcapng a richiesta (`inspect`). Gli ingressi dell'utente li annota
// vetro-wasm; qui si annotano i comandi del gestore dei file (non le letture
// del pannello) e, come effetti, i file cambiati visti dalle osservazioni.
//
// Record & replay (M10, ADR 0019 e 0023): i comandi della pagina (`rr`) si
// eseguono fra una fetta e l'altra. Alla fine di una registrazione (o dopo
// aver caricato un log) i keyframe vanno in OPFS (`vetro-recordings/`,
// `Recording` di web/node/recording.mjs) e il log resta lì per la sessione
// successiva. Un replay (anche il salto a un'istruzione) riparte dal
// keyframe più vicino: durante il replay gli ingressi della pagina si
// scartano, il gestore dei file è chiuso, niente tempo reale né snapshot in
// cache; al punto chiesto la macchina si ferma (`paused`: registri e
// memoria si leggono con `inspect`), alla fine del replay il verdetto
// (`replay-ended`: `Finished` = replay identico, `Diverged`) e la macchina
// continua libera.
//
// Persistenza (M6, ADR 0017):
// - le scritture del guest sui dischi vanno nell'overlay copy-on-write, che
//   si salva in OPFS (`vetro-overlays/`) fra una fetta e l'altra (al più
//   una volta al secondo, e sempre quando il guest si ferma in attesa) e si
//   riapplica alla sessione successiva; un overlay di un'altra immagine base
//   si scarta;
// - lo snapshot della macchina si salva in OPFS (`vetro-snapshots/`) la
//   prima volta che il guest è a riposo (avvio finito), e di nuovo quando è
//   a riposo e i dischi sono cambiati da allora, o a richiesta della pagina.
//   A riposo: `Idle`, oppure REST_NS di tempo del guest senza uscita sulla
//   console, senza cambi dello scanout, senza ingressi e senza attività dei
//   dischi (il kernel ha sempre un timer, quindi `Idle` da solo non basta). La chiave comprende la versione del formato,
//   gli hash di kernel e initramfs, la riga di comando, la configurazione
//   della macchina e l'identità dei dischi; i metadati tengono la
//   generazione dell'overlay di ogni disco al momento del salvataggio. Alla
//   sessione successiva lo snapshot si ripristina invece di avviare il
//   kernel, se gli overlay sono ancora a quella generazione.

import { DEV, INOTIFY, INPUT, instantiate, Machine, TIMELINE_EFFECT, TIMELINE_INPUT } from '../node/vetro.mjs';
import { Recording } from '../node/recording.mjs';
import { BlobSource, DiskFeeder, MemoryCache, OpfsCache, RangeSource } from '../node/disk.mjs';
import { DiskOverlay, fromBase64, opfsFile, sha256Hex, SnapshotStore, snapshotKey, staleReason, toBase64 } from '../node/persist.mjs';

const QUANTUM = 1_000_000;
const SLICE_MS = 12;
/** Salvataggio degli overlay al più ogni tanti ms mentre il guest lavora. */
const PERSIST_MS = 1000;
/** Tempo del guest senza attività dopo cui il guest è a riposo (1,5 s). */
const REST_NS = 1_500_000_000n;
/** Coda della console tenuta per lo snapshot (la pagina la rimostra). */
const CONSOLE_TAIL = 64 * 1024;
const EV_SYN = 0;
const EV_REL = 2;
const REL_WHEEL = 8;

let m = null;
let exports = null;
let feeder = null;
let cfg = null;
let running = false;
const inbox = [];
let wake = null;
const inputLog = [];
/** Overlay persistente di ogni disco (o null). */
let overlays = [];
let store = null;
let snapKey = null;
/** Metadati dell'ultimo snapshot salvato o ripristinato in questa sessione. */
let lastSnapshot = null;
let saveRequested = false;
let consoleTail = [];
let consoleTailLen = 0;
/** Client del gestore dei file (GuestFiles) e ultimo stato mandato alla pagina. */
let files = null;
let filesKey = '';
/** Cartella osservata per ogni wd (per gli effetti sui file della timeline). */
const watchPaths = new Map();
/** 'live', 'replay' (rifà il log), 'paused' (fermo al punto chiesto). */
let mode = 'live';
/** Istruzione a cui fermarsi durante il replay (BigInt) o null. */
let target = null;
/** Comandi di registrazione e replay, eseguiti fra una fetta e l'altra. */
const control = [];
let recording = null;
/** Finestra di attribuzione della timeline (µs, 0 = quella di vetro-analysis). */
let windowUs = 0;
let lastAnalysis = -1n;
let lastAnalysisAt = 0;
let ignoredNotice = false;
/** Riferimento del tempo reale (si azzera quando il tempo del guest salta). */
const clock = { t0: 0, g0: 0n, paused: 0 };

const post = (msg, transfer = []) => postMessage(msg, transfer);
const sleep = (ms) => new Promise((ok) => setTimeout(ok, ms));
const status = (text) => post({ type: 'status', text });

/** I byte di un file scelto o di un URL. */
async function bytesOf(src, what) {
  if (!src) return null;
  if (src.file) return new Uint8Array(await src.file.arrayBuffer());
  status(`scarico ${what}: ${src.url}`);
  const res = await fetch(src.url);
  if (!res.ok) throw new Error(`${src.url}: stato ${res.status}`);
  return new Uint8Array(await res.arrayBuffer());
}

async function openDisk(d, i, sources) {
  const source = sources[i] ?? (d.file ? new BlobSource(d.file) : await new RangeSource(d.url).open());
  sources[i] = source;
  let cache = null;
  if (d.url) {
    if (cfg.opfs) {
      try {
        cache = await OpfsCache.open(source.key, d.blockSize, Math.ceil(source.size / d.blockSize));
      } catch (e) {
        status(`OPFS non disponibile (${e.message ?? e}): cache in memoria`);
      }
    }
    cache ??= new MemoryCache();
  }
  const index = feeder.add(source, { cache, blockSize: d.blockSize, readOnly: d.readOnly, readahead: d.readahead ?? 1 });
  status(`disco ${i}: ${d.url ?? d.file.name}, ${(source.size / 2 ** 20).toFixed(1)} MiB, blocchi da ${d.blockSize >> 10} KiB`);
  overlays[index] = null;
  if (cfg.persist && cfg.opfs && !d.readOnly) {
    try {
      const file = await opfsFile('vetro-overlays', `${(await sha256Hex(source.key)).slice(0, 32)}.cow`);
      const o = DiskOverlay.open(m, index, file, source.key);
      overlays[index] = o;
      const info = o.info;
      if (o.opened.code === 'Mismatch' || o.opened.code === 'Corrupt') status(`disco ${i}: ${o.opened.message}`);
      else if (o.opened.code === 'Loaded') status(`disco ${i}: overlay persistente, ${info.clusters} cluster scritti nelle sessioni precedenti`);
    } catch (e) {
      status(`disco ${i}: overlay persistente non disponibile (${e.message ?? e}): scritture solo in memoria`);
    }
  }
  return index;
}

function devicesOf(c) {
  let devices = DEV.GPU | DEV.KEYBOARD;
  devices |= c.pointer === 'multitouch' ? DEV.MULTITOUCH : DEV.TABLET;
  if (c.net) devices |= DEV.NET;
  if (c.files) devices |= DEV.VSOCK;
  return devices;
}

/** Macchina nuova con i dischi (e i loro overlay). */
async function build(c, sources) {
  for (const o of overlays) o?.close();
  overlays = [];
  m?.free();
  m = new Machine(exports, { ramSize: BigInt(c.ramMiB) << 20n, devices: devicesOf(c), width: c.width, height: c.height });
  feeder = new DiskFeeder(m);
  for (const [i, d] of (c.disks ?? []).entries()) await openDisk(d, i, sources);
}

/** Salva gli overlay cambiati; restituisce se ha scritto qualcosa. */
function persistOverlays() {
  let wrote = false;
  for (const o of overlays) if (o?.persist()) wrote = true;
  return wrote;
}

const generations = () => overlays.map((o) => (o ? o.generation : null));

/** Snapshot della macchina in OPFS, insieme agli overlay (salvati prima). */
async function saveSnapshot(why) {
  persistOverlays();
  const t0 = performance.now();
  const bytes = m.snapshotSave();
  const saveMs = performance.now() - t0;
  const meta = {
    steps: String(m.steps),
    generations: generations(),
    console: toBase64(joinTail()),
    savedAt: new Date().toISOString(),
    why,
  };
  const t1 = performance.now();
  await store.save(snapKey, meta, bytes);
  lastSnapshot = meta;
  const writeMs = performance.now() - t1;
  post({ type: 'snapshot', why, steps: Number(m.steps), size: bytes.length, saveMs, writeMs, generations: meta.generations });
}

function joinTail() {
  const out = new Uint8Array(consoleTailLen);
  let at = 0;
  for (const c of consoleTail) {
    out.set(c, at);
    at += c.length;
  }
  return out;
}

function keepTail(bytes) {
  consoleTail.push(bytes.slice());
  consoleTailLen += bytes.length;
  while (consoleTailLen - consoleTail[0].length >= CONSOLE_TAIL) consoleTailLen -= consoleTail.shift().length;
}

async function start(c) {
  cfg = c;
  const t0 = performance.now();
  const times = {};
  status('carico vetro-wasm');
  const wasm = await (await fetch(c.wasmUrl)).arrayBuffer();
  ({ exports } = await instantiate(wasm));
  times.wasm = performance.now() - t0;
  const kernel = await bytesOf(c.kernel, 'il kernel');
  const initrd = await bytesOf(c.initrd, "l'initramfs");
  times.files = performance.now() - t0 - times.wasm;
  const sources = [];
  await build(c, sources);
  let restored = null;
  if (c.snapshot && c.opfs) {
    try {
      store = await SnapshotStore.opfs();
      const t1 = performance.now();
      snapKey = await snapshotKey({
        format: m.snapshotVersion,
        kernel: await sha256Hex(kernel),
        initrd: initrd ? await sha256Hex(initrd) : null,
        cmdline: c.cmdline,
        ramMiB: c.ramMiB,
        width: c.width,
        height: c.height,
        devices: devicesOf(c),
        disks: sources.map((s, i) => ({ identity: s.key, size: Math.floor(s.size / 512) * 512, readOnly: !!c.disks[i].readOnly })),
      });
      times.key = performance.now() - t1;
      const t2 = performance.now();
      const rec = await store.load(snapKey);
      times.read = performance.now() - t2;
      const stale = rec && staleReason(rec.meta, overlays);
      if (rec && stale) status(`snapshot non usato: ${stale}`);
      if (rec && !stale) {
        const t3 = performance.now();
        try {
          m.snapshotRestore(rec.bytes);
          times.restore = performance.now() - t3;
          restored = rec;
        } catch (e) {
          status(`snapshot non usato: ${e.message}`);
          // Con 'Corrupt' la macchina va scartata: si rifà da capo.
          if (e.code === 'Corrupt') await build(c, sources);
        }
      }
    } catch (e) {
      status(`cache degli snapshot non disponibile (${e.message ?? e})`);
      store = null;
    }
  }
  if (restored) {
    lastSnapshot = restored.meta;
    const tail = fromBase64(restored.meta.console ?? '');
    if (tail.length) keepTail(tail);
    times.total = performance.now() - t0;
    post({ type: 'restored', steps: Number(m.steps), size: restored.bytes.length, times, savedAt: restored.meta.savedAt, console: tail }, [tail.buffer]);
  } else {
    m.loadLinux(kernel, initrd, c.cmdline);
    times.total = performance.now() - t0;
    post({ type: 'cold', times });
  }
  if (c.jit) m.setJit();
  if (c.files) openFiles();
  if (c.net) m.capture(true);
  try {
    recording = new Recording(m, c.opfs ? await SnapshotStore.opfs('vetro-recordings') : SnapshotStore.memory());
    const info = await recording.restore();
    if (info && !info.sameMachine) status('registrazione salvata di una macchina configurata diversamente: non si può rigiocare qui');
  } catch (e) {
    status(`archivio delle registrazioni non disponibile (${e.message ?? e}): in memoria`);
    recording = new Recording(m, SnapshotStore.memory());
  }
  postRr();
  post({ type: 'started', pointer: c.pointer, restored: !!restored });
  running = true;
  loop().catch((e) => {
    running = false;
    post({ type: 'error', text: e.stack ?? String(e) });
  });
}

function apply(msg) {
  if (mode !== 'live') {
    // Durante il replay gli ingressi vengono dal log.
    if (!ignoredNotice) status('replay in corso: gli ingressi della pagina non arrivano al guest');
    ignoredNotice = true;
    return;
  }
  inputLog.push([Number(m.steps), msg]);
  switch (msg.type) {
    case 'serial':
      m.consoleWrite(msg.text);
      break;
    case 'key':
      m.key(msg.code, msg.down);
      break;
    case 'abs':
      m.pointerMove(msg.x, msg.y);
      break;
    case 'button':
      m.pointerButton(msg.code, msg.down);
      break;
    case 'wheel':
      m.inputEvents(INPUT.POINTER, [[EV_REL, REL_WHEEL, msg.delta], [EV_SYN, 0, 0]]);
      break;
    case 'touch':
      m.touch(msg.slot, msg.down ? [msg.x, msg.y] : null);
      break;
    case 'power':
      m.gpio(msg.down);
      break;
    case 'resize':
      m.displayResize(msg.width, msg.height);
      break;
    case 'files':
      filesRequest(msg);
      break;
    default:
      inputLog.pop();
      break;
  }
}

/** Operazioni del gestore dei file chieste dalla pagina. */
const FILE_OPS = {
  stat: (a) => files.stat(a.path),
  list: (a) => files.list(a.path),
  read: (a) => files.read(a.path, a.offset ?? 0, a.length ?? null),
  write: (a) => files.writeFile(a.path, a.bytes, a.mode ?? 0o644),
  mkdir: (a) => files.mkdir(a.path, a.mode ?? 0o755),
  create: (a) => files.create(a.path, a.mode ?? 0o644),
  delete: (a) => files.delete(a.path, { recursive: !!a.recursive }),
  rename: (a) => files.rename(a.path, a.to),
  watch: (a) => files.watch(a.path).then((wd) => {
    watchPaths.set(wd, a.path);
    return wd;
  }),
  unwatch: (a) => files.unwatch(a.wd),
  // SQL nel guest con il motore vero, come il proprietario del database (ADR 0021).
  sql: (a) => files.sql(a.path, a.sql, a.params ?? [], { expect: a.expect ?? null, readonly: !!a.readonly }),
};

/** I comandi del gestore dei file che sono azioni dell'utente (timeline). */
const FILE_COMMANDS = {
  write: (a) => `salva ${a.path}`,
  mkdir: (a) => `nuova cartella ${a.path}`,
  create: (a) => `nuovo file ${a.path}`,
  delete: (a) => `cancella ${a.path}`,
  rename: (a) => `rinomina ${a.path} → ${a.to}`,
  sql: (a) => (a.readonly ? null : `SQL su ${a.path}: ${a.sql.length > 80 ? `${a.sql.slice(0, 80)}…` : a.sql}`),
};

/** Gli eventi di inotify che cambiano file, con il nome nella timeline. */
const FILE_CHANGES = [
  [INOTIFY.CREATE, 'creato'], [INOTIFY.CLOSE_WRITE, 'scritto'], [INOTIFY.MOVED_TO, 'spostato qui'],
  [INOTIFY.MOVED_FROM, 'spostato via'], [INOTIFY.DELETE, 'cancellato'],
];

function fileEffect(e) {
  const change = FILE_CHANGES.find(([bit]) => e.mask & bit);
  if (!change || e.name.startsWith('.vetro-tmp.')) return;
  const dir = watchPaths.get(e.wd) ?? `wd ${e.wd}`;
  const path = e.name ? `${dir.replace(/\/$/, '')}/${e.name}` : dir;
  m.timelineEffect(TIMELINE_EFFECT.FILE, `${change[1]}${e.mask & INOTIFY.ISDIR ? ' (cartella)' : ''} ${path}`);
}

function openFiles() {
  files = m.files();
  watchPaths.clear();
  files.onEvent = (event) => {
    fileEffect(event);
    post({ type: 'files-event', event });
  };
}

function filesRequest(msg) {
  const reply = (r) => post({ type: 'files-reply', id: msg.id, ...r }, r.result?.data ? [r.result.data.buffer] : []);
  const op = FILE_OPS[msg.op];
  if (!files || !op) return reply({ ok: false, error: files ? `operazione ${msg.op} sconosciuta` : 'gestore dei file spento' });
  const label = FILE_COMMANDS[msg.op]?.(msg.args);
  if (label) m.timelineInput(TIMELINE_INPUT.FILES, label);
  op(msg.args).then((result) => reply({ ok: true, result }), (e) => reply({ ok: false, error: e.message, code: e.code }));
}

/** Fa avanzare il gestore dei file; manda lo stato alla pagina se è cambiato. */
function pumpFiles() {
  if (!files) return;
  files.pump();
  const st = files.status();
  const key = `${st.state}/${st.generation}`;
  if (key !== filesKey) {
    filesKey = key;
    post({ type: 'files-status', status: st });
  }
}

let lastUpdates = -1;
let lastCursor = -1;
let lastCursorResource = -1;

/** Console, fotogramma, cursore; restituisce se il guest ha mostrato qualcosa. */
function flush() {
  let active = false;
  const out = m.consoleRead();
  if (out.length) {
    active = true;
    keepTail(out);
    post({ type: 'console', bytes: out }, [out.buffer]);
  }
  const updates = m.displayUpdates();
  if (updates !== lastUpdates) {
    active = true;
    lastUpdates = updates;
    const size = m.displaySize();
    if (!size) {
      post({ type: 'frame', off: true });
    } else {
      const rect = m.displayTakeDirty();
      if (rect) {
        const pixels = m.displayCopy(rect);
        post({ type: 'frame', width: size.width, height: size.height, rect, pixels }, [pixels.buffer]);
      }
    }
  }
  const c = m.cursor();
  if (c && c.updates !== lastCursor) {
    lastCursor = c.updates;
    const msg = { type: 'cursor', ...c };
    if (c.resource !== lastCursorResource || c.updates === 1) {
      msg.image = m.cursorImage();
      lastCursorResource = c.resource;
    }
    post(msg, msg.image ? [msg.image.buffer] : []);
  }
  return active;
}

/** Azzera il riferimento del tempo reale (all'inizio e quando il tempo del guest salta). */
function resetClock() {
  clock.t0 = performance.now();
  clock.g0 = m.guestNs;
  clock.paused = 0;
}

/** Stato di registrazione e replay per la pagina. */
function postRr(extra = {}) {
  if (!m) return;
  post({ type: 'rr', status: m.rrStatus(), info: m.logInfo(), meta: recording?.meta ?? null, mode, steps: Number(m.steps),
    target: target === null ? null : Number(target), ...extra });
}

/** Lista dell'ispettore e timeline, se sono cambiate (al più ogni 700 ms, o subito con `force`). */
function postAnalysis(force = false) {
  const now = performance.now();
  if (!force && now - lastAnalysisAt < 700) return;
  lastAnalysisAt = now;
  const v = m.timelineVersion();
  if (!force && v === lastAnalysis) return;
  lastAnalysis = v;
  post({ type: 'analysis', requests: m.inspectRequests(), timeline: m.timeline(windowUs), capture: m.captureStats() });
}

/** Al punto chiesto del replay: ferma e manda registri e stato. */
function pause() {
  mode = 'paused';
  target = null;
  status(`replay fermo all'istruzione ${m.steps}: registri e memoria nel pannello Registrazione`);
  post({ type: 'paused', steps: Number(m.steps), registers: m.registersText() });
  postAnalysis(true);
  postRr();
}

/** Fine del replay (identico o no): la macchina continua libera. */
function replayEnded(st) {
  mode = 'live';
  target = null;
  ignoredNotice = false;
  resetClock();
  if (cfg.files) openFiles();
  post({ type: 'replay-ended', status: st, steps: Number(m.steps) });
  postAnalysis(true);
  postRr();
}

async function startReplay(step, stopAt) {
  if (m.rrStatus().state === 'Recording') {
    m.recordStop();
    await recording.store();
  }
  await recording.ensureKeyframe(step);
  try {
    m.replayStart(step);
  } finally {
    recording.dropKeyframes();
  }
  // Il client del gestore dei file l'ha già tolto vetro-wasm (le sue
  // operazioni sono nel log): qui si rifiutano le richieste in corso.
  files?.close();
  files = null;
  filesKey = '';
  post({ type: 'files-status', status: { state: 'None', pending: 0, generation: 0, maxChunk: 0, selinux: false } });
  mode = 'replay';
  target = stopAt ? BigInt(step) : null;
  lastUpdates = -1;
  resetClock();
  post({ type: 'replay-started', from: Number(m.steps), target: stopAt ? Number(step) : null });
  if (target !== null && m.steps >= target) pause();
}

/** Un comando di registrazione o replay. */
async function rr(cmd) {
  try {
    switch (cmd.op) {
      case 'record-start':
        if (mode !== 'live') throw new Error('prima finisci il replay');
        m.recordStart(cmd.keyframeEvery);
        status(`registrazione in corso (keyframe ogni ${cmd.keyframeEvery / 1e6} M istruzioni)`);
        break;
      case 'record-stop': {
        if (!m.recordStop()) throw new Error('nessuna registrazione in corso');
        const meta = await recording.store();
        status(`registrazione finita: ${meta.events} ingressi, ${meta.keyframes} keyframe salvati`);
        break;
      }
      case 'load-log': {
        const meta = await recording.load(new Uint8Array(cmd.bytes));
        status(`log caricato: ${meta.events} ingressi, ${meta.keyframes} keyframe`);
        break;
      }
      case 'replay':
        await startReplay(cmd.step ?? 0, !!cmd.pause);
        break;
      case 'continue':
        if (mode === 'paused') {
          mode = 'replay';
          target = cmd.step !== undefined ? BigInt(cmd.step) : null;
        }
        break;
    }
  } catch (e) {
    status(`${cmd.op}: ${e.message ?? e}`);
    post({ type: 'rr-error', op: cmd.op, message: String(e.message ?? e) });
  }
  postRr();
}

async function loop() {
  resetClock();
  const guestMs = () => Number(m.guestNs - clock.g0) / 1e6;
  let lastStats = 0;
  let lastPersist = performance.now();
  // Ultima attività del guest (tempo del guest) e riposo già usato.
  let activeNs = m.guestNs;
  let rested = false;
  const activity = () => {
    activeNs = m.guestNs;
    rested = false;
  };
  let steps0 = m.steps;
  let wall0 = performance.now();
  for (;;) {
    while (control.length) await rr(control.shift());
    if (mode === 'paused') {
      while (inbox.length) apply(inbox.shift());
      if (!control.length) await new Promise((ok) => (wake = ok));
      wake = null;
      continue;
    }
    if (inbox.length) activity();
    while (inbox.length) apply(inbox.shift());
    pumpFiles();
    const slice = performance.now();
    const realtime = cfg.realtime && mode === 'live';
    let stop;
    for (;;) {
      const budget = target === null ? QUANTUM : Math.min(QUANTUM, Number(target - m.steps));
      stop = budget > 0 ? m.run(budget) : 'Budget';
      if (stop === 'Blocked') {
        activity();
        const w = performance.now();
        await feeder.serve();
        clock.paused += performance.now() - w;
        continue;
      }
      if (mode === 'replay') {
        const st = m.rrStatus();
        if (st.state !== 'Replaying') {
          flush();
          replayEnded(st);
          break;
        }
        if (target !== null && m.steps >= target) {
          flush();
          pause();
          break;
        }
      }
      if (stop !== 'Budget' || performance.now() - slice > SLICE_MS) break;
      if (realtime && guestMs() > performance.now() - clock.t0 - clock.paused + 20) break;
    }
    if (mode === 'paused') continue;
    if (flush()) activity();
    pumpFiles();
    const now = performance.now();
    if (stop !== 'Budget' || now - lastPersist > PERSIST_MS) {
      if (persistOverlays()) activity();
      lastPersist = now;
    }
    // Snapshot: la prima volta che il guest è a riposo (avvio finito), poi a
    // riposo se i dischi sono cambiati, o a richiesta.
    const rest = stop === 'Idle' || (!rested && m.guestNs - activeNs >= REST_NS);
    if (rest && stop !== 'Idle') {
      rested = true;
      if (persistOverlays()) activity();
    }
    if (store && mode === 'live' && (saveRequested || (rest && (!lastSnapshot || String(generations()) !== String(lastSnapshot.generations))))) {
      const why = saveRequested ? 'richiesta' : lastSnapshot ? 'dischi cambiati' : 'avvio finito';
      saveRequested = false;
      await saveSnapshot(why).catch((e) => status(`snapshot non salvato: ${e.message ?? e}`));
    }
    // Dopo un replay il contatore può tornare indietro.
    if (m.steps < steps0) {
      steps0 = m.steps;
      wall0 = now;
    }
    if (now - lastStats > 500) {
      const mips = Number(m.steps - steps0) / ((now - wall0) * 1000);
      post({
        type: 'stats',
        steps: Number(m.steps),
        guestSecs: Number(m.guestNs) / 1e9,
        mips,
        disks: feeder.disks.map((d, i) => ({ ...m.diskStats(i), http: d.source.stats, overlay: overlays[i]?.info ?? null })),
        feeder: feeder.stats,
        jit: m.jitStats(),
        inputs: inputLog.length,
      });
      if (mode !== 'live' || m.rrStatus().state === 'Recording') postRr();
      lastStats = now;
      steps0 = m.steps;
      wall0 = now;
    }
    postAnalysis();
    if (stop === 'Idle' && mode === 'live') {
      status('il guest aspetta un ingresso');
      if (!inbox.length && !saveRequested && !control.length) await new Promise((ok) => (wake = ok));
      wake = null;
      continue;
    }
    if (stop !== 'Budget') {
      running = false;
      post({ type: 'stopped', reason: stop, steps: Number(m.steps) });
      return;
    }
    // Tempo reale: il guest non corre davanti all'orologio.
    const ahead = realtime ? guestMs() - (performance.now() - clock.t0 - clock.paused) : 0;
    await sleep(Math.max(0, Math.min(ahead, 50)));
  }
}

const enc = new TextEncoder();

/** Letture della pagina: dettaglio, esportazioni, registri, memoria, finestra della timeline. */
async function inspect(msg) {
  switch (msg.op) {
    case 'request':
      return { result: m.inspectRequest(msg.index) };
    case 'har': {
      const bytes = enc.encode(m.inspectHar(msg.epochUs ?? 0));
      return { result: bytes, transfer: [bytes.buffer] };
    }
    case 'pcapng': {
      const bytes = m.inspectPcapng(msg.epochUs ?? 0);
      return { result: bytes, transfer: [bytes.buffer] };
    }
    case 'log': {
      const bytes = await recording.encodeFull();
      return { result: bytes, transfer: [bytes.buffer] };
    }
    case 'events':
      return { result: m.logEvents() };
    case 'registers':
      return { result: { steps: Number(m.steps), text: m.registersText() } };
    case 'memory': {
      const va = BigInt(msg.va);
      const r = m.readVirt(va, msg.length);
      if (!r.bytes) return { result: { va: msg.va, fault: `0x${r.fault.toString(16)}` } };
      return { result: { va: msg.va, bytes: r.bytes, pa: m.translate(va)?.toString(16) ?? null } };
    }
    case 'window':
      windowUs = msg.us;
      postAnalysis(true);
      return { result: true };
    case 'capture-clear':
      m.captureClear();
      postAnalysis(true);
      return { result: true };
    case 'timeline-clear':
      m.timelineClear();
      postAnalysis(true);
      return { result: true };
    default:
      throw new Error(`lettura ${msg.op} sconosciuta`);
  }
}

onmessage = (e) => {
  const msg = e.data;
  if (msg.type === 'start') {
    if (running) return;
    start(msg.config).catch((err) => post({ type: 'error', text: err.stack ?? String(err) }));
    return;
  }
  if (!m) return;
  if (msg.type === 'rr') {
    control.push(msg);
    wake?.();
    return;
  }
  if (msg.type === 'inspect') {
    // Letture che non toccano il guest: subito, fra una fetta e l'altra.
    inspect(msg).then(
      ({ result, transfer = [] }) => post({ type: 'inspect-reply', id: msg.id, ok: true, result }, transfer),
      (err) => post({ type: 'inspect-reply', id: msg.id, ok: false, error: String(err.message ?? err) }),
    );
    return;
  }
  if (msg.type === 'save') {
    // Non è un ingresso del guest: si salva fra due fette.
    if (store) saveRequested = true;
    else status('cache degli snapshot non attiva');
    wake?.();
    return;
  }
  inbox.push(msg);
  wake?.();
};
