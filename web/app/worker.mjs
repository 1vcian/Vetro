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
//
// Vetro's AOSP image (M5/M6, ADR 0028), with `config.android`:
// - the version's manifest.json (R2 or a local server) gives the image hashes
//   (snapshot key) and their URLs; boot, vendor_boot and init_boot are
//   downloaded (checked with the sha256, kept in OPFS, `vetro-images/`) only
//   for a cold boot; the disk is the `web/disk.json` map (LayoutSource: super
//   and userdata from the sparse files with HTTP Range), with the block cache
//   in OPFS;
// - the boot phases (BootProgress) go to the page (`progress`);
// - after sys.boot_completed the ADB client (web/node/adb.mjs) connects to
//   adbd (TCP 5555 in the guest, GuestSocket), keeps the screen on, watches
//   for the home screen (launcher focused and drawn on the scanout) and
//   serves the page's requests (`adb`: shell, devices, install of a dropped
//   APK and opening it); the requests are inputs (recorded in `inputLog`);
// - the snapshot is saved ANDROID_HOME_NS of guest time after the home screen
//   is drawn, after an APK install and on request; no console idle heuristic
//   (Android always writes). The snapshot is the unit of persistence: it
//   also holds the guest's disk writes (the copy-on-write layer), so there is
//   no separate overlay (hundreds of MiB more to write and read back). The
//   next session resumes from the last snapshot: writes made after it are
//   lost, like going back to the last saved state; without a snapshot the
//   first boot runs again. Snapshots go to and come from OPFS in chunks.

import { DEV, INOTIFY, INPUT, instantiate, Machine, TIMELINE_EFFECT, TIMELINE_INPUT } from '../node/vetro.mjs';
import { Recording } from '../node/recording.mjs';
import { BlobSource, DiskFeeder, LayoutSource, MemoryCache, OpfsCache, RangeSource } from '../node/disk.mjs';
import { AdbClient } from '../node/adb.mjs';
import { apkInfo } from '../node/apk.mjs';
import { ANDROID_PARAMS, BootProgress, gridColors, HOME_MIN_COLORS, HOME_QUERY, isHome } from '../node/android.mjs';
import { DiskOverlay, fromBase64, opfsFile, sha256Hex, SnapshotStore, snapshotKey, staleReason, toBase64 } from '../node/persist.mjs';

const QUANTUM = 1_000_000;
const SLICE_MS = 12;
/** Salvataggio degli overlay al più ogni tanti ms mentre il guest lavora. */
const PERSIST_MS = 1000;
/** Tempo del guest senza attività dopo cui il guest è a riposo (1,5 s). */
const REST_NS = 1_500_000_000n;
/** Coda della console tenuta per lo snapshot (la pagina la rimostra). */
const CONSOLE_TAIL = 64 * 1024;
/** Guest time after the home screen is drawn before the Android snapshot. */
const ANDROID_HOME_NS = 5_000_000_000n;
/** At most this much guest time from the launcher being focused to the home screen drawn. */
const HOME_DRAW_NS = 300_000_000_000n;
/** How often (guest time) adb is asked whether the home screen is up. */
const HOME_POLL_NS = 5_000_000_000n;
/** If the home screen does not come within this long after sys.boot_completed, the snapshot is saved anyway. */
const HOME_GIVE_UP_NS = 3000_000_000_000n;
/** Wait (guest time) before trying to connect to adbd again. */
const ADB_RETRY_NS = 5_000_000_000n;
/** adb command that keeps the screen on and wakes it (after connecting). */
const ANDROID_WAKE = 'svc power stayon true; settings put system screen_off_timeout 2147483647; input keyevent KEYCODE_WAKEUP; wm dismiss-keyguard';
/** Android disk blocks kept in memory (64 MiB): the rest is in OPFS. */
const ANDROID_MAX_BLOCKS = 64;
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
/** Why the requested snapshot is saved (for the page). */
let saveWhy = 'richiesta';
/** Android state (config.android), or null with the test kernel. */
let android = null;
let startT0 = 0;
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
  const source = sources[i] ?? (d.file ? new BlobSource(d.file) : d.layout ? await new LayoutSource(d.layout).open() : await new RangeSource(d.url).open());
  sources[i] = source;
  let cache = null;
  if (d.url || d.layout) {
    if (cfg.opfs) {
      try {
        cache = await OpfsCache.open(source.key, d.blockSize, Math.ceil(source.size / d.blockSize));
      } catch (e) {
        status(`OPFS non disponibile (${e.message ?? e}): cache in memoria`);
      }
    }
    cache ??= new MemoryCache();
  }
  const index = feeder.add(source, { cache, blockSize: d.blockSize, maxBlocks: d.maxBlocks ?? 0, readOnly: d.readOnly, readahead: d.readahead ?? 1 });
  status(`disk ${i}: ${d.url ?? d.layout ?? d.file.name}, ${(source.size / 2 ** 20).toFixed(1)} MiB, ${d.blockSize >> 10} KiB blocks`);
  overlays[index] = null;
  // With Android the unit of persistence is the snapshot (which also holds
  // the guest's writes): no separate overlay (see the top of the file).
  if (cfg.persist && cfg.opfs && !d.readOnly && !android) {
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
  const meta = {
    steps: String(m.steps),
    generations: generations(),
    console: toBase64(joinTail()),
    savedAt: new Date().toISOString(),
    why,
  };
  if (android) meta.progress = android.progress.events;
  // In chunks, straight to OPFS: with Android it is hundreds of MiB, which
  // whole would not fit in the module's memory (ADR 0028).
  const t0 = performance.now();
  let writeMs = 0;
  const memory = m.memoryBytes;
  const size = await store.saveStream(snapKey, meta, (write) => m.snapshotSaveTo((b, at) => {
    const tw = performance.now();
    write(b, at);
    writeMs += performance.now() - tw;
  }));
  const saveMs = performance.now() - t0 - writeMs;
  lastSnapshot = meta;
  post({ type: 'snapshot', why, steps: Number(m.steps), size, saveMs, writeMs, generations: meta.generations, memory: Math.max(memory, m.memoryBytes) });
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
  startT0 = t0;
  const times = {};
  status('carico vetro-wasm');
  const wasm = await (await fetch(c.wasmUrl)).arrayBuffer();
  ({ exports } = await instantiate(wasm));
  times.wasm = performance.now() - t0;
  if (c.android) android = await prepareAndroid(c);
  const kernel = android ? null : await bytesOf(c.kernel, 'il kernel');
  const initrd = android ? null : await bytesOf(c.initrd, "l'initramfs");
  times.files = performance.now() - t0 - times.wasm;
  const sources = [];
  await build(c, sources);
  let restored = null;
  if (c.snapshot && c.opfs) {
    try {
      store = await SnapshotStore.opfs();
      const t1 = performance.now();
      const common = {
        format: m.snapshotVersion,
        ramMiB: c.ramMiB,
        width: c.width,
        height: c.height,
        devices: devicesOf(c),
        disks: sources.map((s, i) => ({ identity: s.key, size: Math.floor(s.size / 512) * 512, readOnly: !!c.disks[i].readOnly })),
      };
      snapKey = await snapshotKey(android
        ? { ...common, android: android.manifest.version, images: android.images.map((f) => f.sha256), params: android.params }
        : { ...common, kernel: await sha256Hex(kernel), initrd: initrd ? await sha256Hex(initrd) : null, cmdline: c.cmdline });
      times.key = performance.now() - t1;
      const t2 = performance.now();
      const meta = await store.loadMeta(snapKey);
      times.read = performance.now() - t2;
      const stale = meta && staleReason(meta, overlays);
      if (meta && stale) status(`snapshot not used: ${stale}`);
      if (meta && !stale) {
        status(`restoring the snapshot (${(meta.size / 2 ** 20).toFixed(0)} MiB)`);
        const t3 = performance.now();
        try {
          // In chunks from OPFS: only the part before the RAM goes into the
          // module's memory (ADR 0028).
          const reader = await store.openReader(snapKey);
          let readMs = 0;
          try {
            m.snapshotRestoreStream(reader.size, (view, at) => {
              const tr = performance.now();
              reader.readAt(view, at);
              readMs += performance.now() - tr;
            });
          } finally {
            reader.close();
          }
          times.readSnapshot = readMs;
          times.restore = performance.now() - t3 - readMs;
          restored = { meta, size: meta.size };
        } catch (e) {
          status(`snapshot non usato: ${e.message}`);
          // Con 'Corrupt' la macchina va scartata: si rifà da capo.
          if (e.code === 'Corrupt' || e.code === 'Memory') await build(c, sources);
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
    if (android) {
      for (const ev of restored.meta.progress ?? []) android.progress.events.push(ev);
      android.progress.index = android.progress.events.length - 1;
      android.bootedNs = m.guestNs;
      if (android.progress.phase === 'home') android.homeNs = android.focusNs = m.guestNs;
      android.savedBoot = true;
    }
    times.total = performance.now() - t0;
    post({ type: 'restored', steps: Number(m.steps), size: restored.size, times, savedAt: restored.meta.savedAt, console: tail,
      progress: android?.progress.events ?? null, memory: m.memoryBytes }, [tail.buffer]);
  } else if (android) {
    const [boot, vendorBoot, initBoot] = await androidImages(android);
    times.images = performance.now() - t0 - times.wasm - times.files;
    const desc = m.loadAndroid({ boot, vendorBoot, initBoot, params: android.params });
    status(`Android images loaded: ${desc.split(';')[0]}`);
    times.total = performance.now() - t0;
    post({ type: 'cold', times, android: desc });
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

// ---- Vetro's AOSP image (ADR 0028) ----------------------------------------------

/**
 * Reads the version's manifest.json and prepares the configuration: the
 * disks (the map next to the manifest) and the image URLs.
 */
async function prepareAndroid(c) {
  const url = new URL(c.android.manifest, location.href).href;
  status(`Android image: ${url}`);
  const res = await fetch(url);
  if (!res.ok) throw new Error(`${url}: status ${res.status}`);
  const manifest = await res.json();
  const file = (path) => {
    const f = manifest.files.find((x) => x.path === path);
    if (!f) throw new Error(`manifest without ${path}`);
    return { ...f, url: new URL(path, url).href };
  };
  const images = ['boot.img', 'vendor_boot.img', 'init_boot.img'].map(file);
  const layout = new URL(c.android.layout ?? 'web/disk.json', url).href;
  c.disks = [{ layout, blockSize: c.android.blockSize ?? 1 << 20, maxBlocks: ANDROID_MAX_BLOCKS, readOnly: false, readahead: 1 }];
  return {
    manifest,
    images,
    params: c.android.params ?? ANDROID_PARAMS,
    progress: new BootProgress(),
    bootedNs: null,
    homeNs: null,
    focusNs: null,
    focusDetail: '',
    homePollNs: 0n,
    homeQuery: false,
    savedBoot: false,
    adb: null,
    adbReady: false,
    adbRetryNs: 0n,
    ops: [],
    busy: false,
  };
}

/** The three boot images: from OPFS if there, otherwise downloaded and checked. */
async function androidImages(a) {
  const dir = cfg.opfs ? await navigator.storage.getDirectory().then((r) => r.getDirectoryHandle('vetro-images', { create: true })).catch(() => null) : null;
  const out = [];
  for (const f of a.images) {
    let bytes = null;
    if (dir) {
      try {
        const h = await (await dir.getFileHandle(`${f.sha256}.img`)).createSyncAccessHandle();
        try {
          if (h.getSize() === f.size) {
            bytes = new Uint8Array(f.size);
            h.read(bytes, { at: 0 });
          }
        } finally {
          h.close();
        }
      } catch {}
    }
    if (!bytes) {
      status(`downloading ${f.path} (${(f.size / 2 ** 20).toFixed(0)} MiB)`);
      const res = await fetch(f.url);
      if (!res.ok) throw new Error(`${f.url}: status ${res.status}`);
      bytes = new Uint8Array(await res.arrayBuffer());
      const got = await sha256Hex(bytes);
      if (got !== f.sha256) throw new Error(`${f.path}: sha256 ${got}, the manifest says ${f.sha256}`);
      if (dir) {
        try {
          const h = await (await dir.getFileHandle(`${f.sha256}.img`, { create: true })).createSyncAccessHandle();
          h.truncate(0);
          h.write(bytes, { at: 0 });
          h.flush();
          h.close();
        } catch {}
      }
    }
    out.push(bytes);
  }
  return out;
}

const latin1 = new TextDecoder('latin1');

/** The boot phases read from the console output. */
function androidConsole(bytes) {
  for (const ev of android.progress.feed(latin1.decode(bytes), Number(m.guestNs) / 1e9)) {
    post({ type: 'progress', ...ev, wallMs: performance.now() - startT0 });
  }
}

/** Connection to adbd, the page's requests, the snapshot after the boot. */
function androidTick() {
  const a = android;
  if (a.progress.phase === 'booted' && a.bootedNs === null) {
    a.bootedNs = m.guestNs;
    post({ type: 'booted', guestSecs: Number(m.guestNs) / 1e9, wallMs: performance.now() - startT0 });
  }
  if (a.bootedNs === null || mode !== 'live') return;
  if (!a.adb && m.guestNs >= a.adbRetryNs) {
    const sock = m.connectGuest(5555);
    const adb = new AdbClient(sock);
    a.adb = adb;
    post({ type: 'adb-status', state: 'connecting' });
    adb.connect().then(async (banner) => {
      // A virtual machine in the page: the screen stays on.
      await adb.shell(ANDROID_WAKE);
      a.adbReady = true;
      const devices = await adb.devices();
      post({ type: 'adb-status', state: 'ready', banner, devices });
    }).catch((e) => {
      sock.release();
      if (a.adb === adb) a.adb = null;
      a.adbReady = false;
      a.adbRetryNs = m.guestNs + ADB_RETRY_NS;
      post({ type: 'adb-status', state: 'waiting', error: String(e.message ?? e) });
    });
  }
  a.adb?.pump();
  if (a.adb?.lost && a.adbReady) {
    const why = a.adb.lost;
    a.adbReady = false;
    a.adb = null;
    a.adbRetryNs = m.guestNs + ADB_RETRY_NS;
    post({ type: 'adb-status', state: 'waiting', error: `connection closed (${why})` });
  }
  // The home screen: the focused window becomes the launcher, and the scanout
  // shows it (FallbackHome can stay for tens of seconds of guest time first).
  if (a.focusNs !== null && a.homeNs === null) {
    const size = m.displaySize();
    const px = size && m.displayPixels();
    const colors = px ? gridColors(px, size.width, size.height) : 0;
    if (colors >= HOME_MIN_COLORS || m.guestNs - a.focusNs >= HOME_DRAW_NS) {
      a.homeNs = m.guestNs;
      for (const ev of a.progress.mark('home', Number(m.guestNs) / 1e9)) {
        post({ type: 'progress', ...ev, wallMs: performance.now() - startT0, detail: a.focusDetail, colors, focusGuestSecs: Number(a.focusNs) / 1e9 });
      }
    }
  }
  if (a.adbReady && a.focusNs === null && !a.homeQuery && !a.busy && m.guestNs >= a.homePollNs) {
    a.homeQuery = true;
    a.adb.shell(HOME_QUERY).then((r) => {
      if (isHome(r.stdout) && a.focusNs === null) {
        a.focusNs = m.guestNs;
        a.focusDetail = r.stdout.trim();
      }
    }).catch(() => {}).finally(() => {
      a.homeQuery = false;
      a.homePollNs = m.guestNs + HOME_POLL_NS;
    });
  }
  if (a.adbReady && !a.busy && !a.homeQuery && a.ops.length) runAdbOp(a.ops.shift());
  const homeReady = a.homeNs !== null && m.guestNs - a.homeNs >= ANDROID_HOME_NS;
  if (store && !a.savedBoot && (homeReady || m.guestNs - a.bootedNs >= HOME_GIVE_UP_NS)) {
    a.savedBoot = true;
    if (!lastSnapshot) {
      saveRequested = true;
      saveWhy = homeReady ? 'home screen' : 'boot finished (home screen not seen)';
    }
  }
}

/** An ADB request from the page (one at a time). */
function runAdbOp(msg) {
  const a = android;
  a.busy = true;
  inputLog.push([Number(m.steps), { type: 'adb', op: msg.op, name: msg.name, cmd: msg.cmd }]);
  const reply = (r) => post({ type: 'adb-reply', id: msg.id, ...r });
  const t0 = performance.now();
  let p;
  switch (msg.op) {
    case 'shell':
      m.timelineInput(TIMELINE_INPUT.OTHER, `adb shell ${msg.cmd}`);
      p = a.adb.shell(msg.cmd);
      break;
    case 'devices':
      p = a.adb.devices();
      break;
    case 'install':
      p = adbInstall(new Uint8Array(msg.bytes), msg);
      break;
    default:
      p = Promise.reject(new Error(`unknown adb operation ${msg.op}`));
  }
  p.then((result) => reply({ ok: true, result, ms: performance.now() - t0 }), (e) => reply({ ok: false, error: String(e.message ?? e), ms: performance.now() - t0 }))
    .finally(() => (a.busy = false));
}

/** Installs an APK with adb (push + pm install) and opens its main activity. */
async function adbInstall(bytes, msg) {
  const adb = android.adb;
  const info = await apkInfo(bytes);
  m.timelineInput(TIMELINE_INPUT.OTHER, `install ${info.package}`);
  post({ type: 'adb-progress', id: msg.id, text: `installing ${info.package} (${(bytes.length / 1024).toFixed(0)} KiB)` });
  const t0 = performance.now();
  const output = await adb.install(bytes, { name: `${info.package}.apk` });
  const installMs = performance.now() - t0;
  let component = info.launcher ? `${info.package}/${info.launcher}` : null;
  if (!component) {
    const r = await adb.shell(`cmd package resolve-activity --brief -a android.intent.action.MAIN -c android.intent.category.LAUNCHER ${info.package} | tail -n 1`);
    component = r.stdout.trim().includes('/') ? r.stdout.trim() : null;
  }
  let start = null;
  if (component && msg.open !== false) {
    post({ type: 'adb-progress', id: msg.id, text: `opening ${component}` });
    const r = await adb.shell(`am start -W -n ${component}`);
    start = `${r.stdout}${r.stderr}`.trim();
  }
  if (store && msg.save !== false) {
    saveRequested = true;
    saveWhy = 'app installed';
  }
  return { info, output, component, start, installMs, openMs: performance.now() - t0 - installMs };
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
    if (android) androidConsole(out);
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
    if (android) androidTick();
    const now = performance.now();
    // With Android there is no separate overlay (see the top of the file).
    if (!android && (stop !== 'Budget' || now - lastPersist > PERSIST_MS)) {
      if (persistOverlays()) activity();
      lastPersist = now;
    }
    // Snapshot: la prima volta che il guest è a riposo (avvio finito), poi a
    // riposo se i dischi sono cambiati, o a richiesta. Android: see androidTick.
    const rest = !android && (stop === 'Idle' || (!rested && m.guestNs - activeNs >= REST_NS));
    if (rest && stop !== 'Idle') {
      rested = true;
      if (persistOverlays()) activity();
    }
    if (store && mode === 'live' && (saveRequested || (rest && (!lastSnapshot || String(generations()) !== String(lastSnapshot.generations))))) {
      const why = saveRequested ? saveWhy : lastSnapshot ? 'dischi cambiati' : 'avvio finito';
      saveRequested = false;
      saveWhy = 'richiesta';
      status(`saving the snapshot (${why})`);
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
        memory: m.memoryBytes,
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
  if (msg.type === 'adb') {
    if (!android) return post({ type: 'adb-reply', id: msg.id, ok: false, error: 'adb needs the Android image' });
    android.ops.push(msg);
    if (!android.adbReady) post({ type: 'adb-progress', id: msg.id, text: 'waiting for adbd (end of the boot)' });
    wake?.();
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
