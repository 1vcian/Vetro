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

import { DEV, INPUT, instantiate, Machine } from '../node/vetro.mjs';
import { BlobSource, DiskFeeder, MemoryCache, OpfsCache, RangeSource } from '../node/disk.mjs';

const QUANTUM = 1_000_000;
const SLICE_MS = 12;
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

async function openDisk(d, i) {
  const source = d.file ? new BlobSource(d.file) : await new RangeSource(d.url).open();
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
  return index;
}

async function start(c) {
  cfg = c;
  status('carico vetro-wasm');
  const wasm = await (await fetch(c.wasmUrl)).arrayBuffer();
  ({ exports } = await instantiate(wasm));
  const kernel = await bytesOf(c.kernel, 'il kernel');
  const initrd = await bytesOf(c.initrd, "l'initramfs");
  let devices = DEV.GPU | DEV.KEYBOARD;
  devices |= c.pointer === 'multitouch' ? DEV.MULTITOUCH : DEV.TABLET;
  if (c.net) devices |= DEV.NET;
  m = new Machine(exports, { ramSize: BigInt(c.ramMiB) << 20n, devices, width: c.width, height: c.height });
  feeder = new DiskFeeder(m);
  for (const [i, d] of (c.disks ?? []).entries()) await openDisk(d, i);
  m.loadLinux(kernel, initrd, c.cmdline);
  if (c.jit) m.setJit(16, 16);
  post({ type: 'started', pointer: c.pointer });
  running = true;
  loop().catch((e) => {
    running = false;
    post({ type: 'error', text: e.stack ?? String(e) });
  });
}

function apply(msg) {
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
    default:
      inputLog.pop();
      break;
  }
}

let lastUpdates = -1;
let lastCursor = -1;
let lastCursorResource = -1;

/** Console, fotogramma, cursore. */
function flush() {
  const out = m.consoleRead();
  if (out.length) post({ type: 'console', bytes: out }, [out.buffer]);
  const updates = m.displayUpdates();
  if (updates !== lastUpdates) {
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
}

async function loop() {
  const t0 = performance.now();
  let paused = 0;
  let lastStats = 0;
  let steps0 = m.steps;
  let wall0 = t0;
  for (;;) {
    while (inbox.length) apply(inbox.shift());
    const slice = performance.now();
    let stop;
    for (;;) {
      stop = m.run(QUANTUM);
      if (stop === 'Blocked') {
        const w = performance.now();
        await feeder.serve();
        paused += performance.now() - w;
        continue;
      }
      if (stop !== 'Budget' || performance.now() - slice > SLICE_MS) break;
      if (cfg.realtime && Number(m.guestNs) / 1e6 > performance.now() - t0 - paused + 20) break;
    }
    flush();
    const now = performance.now();
    if (now - lastStats > 500) {
      const mips = Number(m.steps - steps0) / ((now - wall0) * 1000);
      post({
        type: 'stats',
        steps: Number(m.steps),
        guestSecs: Number(m.guestNs) / 1e9,
        mips,
        disks: feeder.disks.map((d, i) => ({ ...m.diskStats(i), http: d.source.stats })),
        feeder: feeder.stats,
        jit: m.jitStats(),
        inputs: inputLog.length,
      });
      lastStats = now;
      steps0 = m.steps;
      wall0 = now;
    }
    if (stop === 'Idle') {
      status('il guest aspetta un ingresso');
      await new Promise((ok) => (wake = ok));
      wake = null;
      continue;
    }
    if (stop !== 'Budget') {
      running = false;
      post({ type: 'stopped', reason: stop, steps: Number(m.steps) });
      return;
    }
    // Tempo reale: il guest non corre davanti all'orologio.
    const ahead = cfg.realtime ? Number(m.guestNs) / 1e6 - (performance.now() - t0 - paused) : 0;
    await sleep(Math.max(0, Math.min(ahead, 50)));
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
  inbox.push(msg);
  wake?.();
};
