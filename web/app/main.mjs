// La pagina di Vetro: sceglie kernel, initramfs e disco, avvia la macchina
// nel Worker (worker.mjs), mostra lo scanout di virtio-gpu e il cursore,
// la console seriale, e manda al guest tastiera, mouse/tocco e il tasto di
// accensione. Nessun bundler né dipendenza: moduli ES serviti così come
// sono (tools/web-serve.mjs).
//
// Parametri dell'URL per precompilare ed eventualmente avviare:
//   ?kernel=URL&initrd=URL&disk=URL&cmdline=...&pointer=multitouch&webgpu=1&autostart=1
//   &snapshot=0 (niente cache degli snapshot) &persist=0 (dischi non persistenti)
//   &files=/tmp,/root (radici del gestore dei file) &nofiles=1 (senza gestore)
//
// Gestore dei file (M8, ADR 0020): pannello accanto allo schermo con
// l'albero delle radici (`window.vetroFiles.setRoots([...])`: oggi a mano,
// con Android le imposterà il rilevamento dell'app in primo piano).
//
// Analisi (M7, M10, ADR 0023): sotto lo schermo i pannelli dell'ispettore
// di rete, della timeline input→effetti e della registrazione/replay
// (analysis.mjs); `window.vetroAnalysis` per i test.
//
// Schermo: a scanout spento un messaggio spiega che il guest non disegna e
// un pulsante digita nella console `timeout 30 vetro-dev drm-hold` (motivo
// di prova, vedi DEMO_COMMAND).
//
// Persistenza (M6, ADR 0017): il Worker salva in OPFS lo snapshot della
// macchina e l'overlay dei dischi; al secondo avvio riparte dallo snapshot.
// Lo stato si legge anche da `window.vetroState` (per i test nel browser).
//
// Vetro's AOSP image (M5/M6, ADR 0028): `?os=android` (or the "System"
// selector) and `&manifest=URL` (default: the version published on R2). The
// panel next to the screen shows the boot phases read from the console and
// the home screen, the adb status and the place to drop an APK (also on the
// screen), which the Worker installs with the ADB client and opens; an
// `adb shell` line. `window.vetroAndroid` for tests.

import { absAxis, BUTTONS, evdevCode } from './keymap.mjs';
import { keyToBytes, Terminal } from './terminal.mjs';
import { Canvas2DRenderer, WebGpuRenderer } from './display.mjs';
import { FilePanel } from './files.mjs';
import { AnalysisPanels } from './analysis.mjs';
import { PHASES } from '../node/android.mjs';

/** The version of Vetro's AOSP image published on R2 (ADR 0022, 0028, 0030). */
export const DEFAULT_MANIFEST = 'https://pub-06e88fdd7f374fffb06844d60083f2ae.r2.dev/aosp/android-15.0.0_r36-BP1A.250505.005.D1-bd09e2f/manifest.json';
/** Guest RAM with AOSP in the browser (ADR 0028). */
export const ANDROID_RAM_MIB = 2048;

const $ = (id) => document.getElementById(id);
const form = $('setup');
const screen = $('screen');
const cursorCanvas = $('cursor');
const consoleEl = $('console');
const statusEl = $('status');
let worker = null;
let renderer = null;
let fb = { width: 0, height: 0 };
let cursor = null;
let pointerKind = 'tablet';
/** Stato visibile ai test: come è partita la macchina, snapshot salvati, dischi. */
const vetroState = (window.vetroState = { boot: null, snapshots: [], disks: [], stopped: null });
let startedAt = 0;

// ---- Gestore dei file --------------------------------------------------------

const DEFAULT_ROOTS = ['/tmp', '/root', '/etc'];
const ANDROID_ROOTS = ['/data/local/tmp', '/sdcard/Download'];
let rpcId = 0;
const rpcPending = new Map();
/** Un'operazione del gestore dei file nel Worker: Promise del risultato. */
function rpc(op, args) {
  return new Promise((ok, ko) => {
    if (!worker) return ko(new Error('macchina spenta'));
    const id = ++rpcId;
    rpcPending.set(id, { ok, ko });
    worker.postMessage({ type: 'files', id, op, args });
  });
}
const filePanel = new FilePanel({
  box: $('files-box'),
  status: $('files-status'),
  roots: $('files-roots'),
  tree: $('files-tree'),
  path: $('files-path'),
  info: $('files-info'),
  mode: $('files-mode'),
  save: $('files-save'),
  reload: $('files-reload'),
  content: $('files-content'),
  message: $('files-message'),
}, rpc);
let pendingRoots = DEFAULT_ROOTS;
let rootsFromUrl = false;
/** Per i test e per chi imposta le radici (l'app in primo piano). */
window.vetroFiles = {
  panel: filePanel,
  setRoots: (roots) => {
    pendingRoots = roots;
    return filePanel.status.state === 'Ready' ? filePanel.setRoots(roots) : Promise.resolve();
  },
  state: () => filePanel.snapshot(),
};

const setStatus = (t) => {
  statusEl.textContent = t;
  statusEl.title = t;
};

const panels = new AnalysisPanels({ post: (msg, transfer = []) => worker?.postMessage(msg, transfer), setStatus });

// ---- Console ---------------------------------------------------------------

// Durante la ripresa della coda della console di uno snapshot il terminale
// non risponde (la risposta a ESC[6n l'aveva già data la sessione salvata:
// ripeterla sarebbe un ingresso in più per il guest).
let replaying = false;
const term = new Terminal({ onReply: (s) => !replaying && worker?.postMessage({ type: 'serial', text: s }) });
let consoleDirty = false;

function renderConsole() {
  consoleDirty = false;
  const atBottom = consoleEl.scrollTop + consoleEl.clientHeight >= consoleEl.scrollHeight - 4;
  consoleEl.textContent = term.text();
  if (atBottom) consoleEl.scrollTop = consoleEl.scrollHeight;
}

consoleEl.addEventListener('keydown', (e) => {
  const bytes = keyToBytes(e);
  if (bytes === null) return;
  e.preventDefault();
  worker?.postMessage({ type: 'serial', text: bytes });
});
consoleEl.addEventListener('paste', (e) => {
  e.preventDefault();
  const text = e.clipboardData.getData('text').replaceAll('\r\n', '\r').replaceAll('\n', '\r');
  worker?.postMessage({ type: 'serial', text });
});

// ---- Schermo ---------------------------------------------------------------

function placeCursor() {
  if (!cursor || !cursor.resource || !fb.width) {
    cursorCanvas.hidden = true;
    return;
  }
  const k = screen.clientWidth / fb.width;
  cursorCanvas.hidden = false;
  cursorCanvas.style.width = `${64 * k}px`;
  cursorCanvas.style.height = `${64 * k}px`;
  cursorCanvas.style.left = `${(cursor.x - cursor.hotX) * k}px`;
  cursorCanvas.style.top = `${(cursor.y - cursor.hotY) * k}px`;
}
new ResizeObserver(placeCursor).observe(screen);

function onFrame(msg) {
  if (msg.off) {
    $('screen-off').hidden = false;
    renderer.clear();
    return;
  }
  if (!vetroState.firstFrame) vetroState.firstFrame = performance.now() - startedAt;
  $('screen-off').hidden = true;
  if (msg.width !== fb.width || msg.height !== fb.height) {
    fb = { width: msg.width, height: msg.height };
    renderer.resize(fb.width, fb.height);
    placeCursor();
  }
  renderer.draw(msg.rect, msg.pixels);
}

// Scanout spento: col kernel di prova il guest non disegna finché un
// programma non usa il DRM. Il pulsante digita nella console un comando del
// guest di prova che disegna il motivo noto di `vetro-dev drm-hold`
// (tests/boot/tests/devices.rs) e lo tiene finché arriva una riga su stdin
// (Invio nella console) o per 30 s di tempo del guest (`timeout` di
// BusyBox): la shell torna libera in ogni caso, e chiuso il DRM lo scanout
// si spegne di nuovo.
const DEMO_COMMAND = 'timeout 30 vetro-dev drm-hold';
$('screen-demo').addEventListener('click', () => {
  worker?.postMessage({ type: 'serial', text: `${DEMO_COMMAND}\r` });
  consoleEl.focus();
});

function onCursor(msg) {
  cursor = msg;
  if (msg.image) {
    cursorCanvas.getContext('2d').putImageData(new ImageData(msg.image, 64, 64), 0, 0);
  }
  placeCursor();
}

// ---- Ingressi --------------------------------------------------------------

const send = (msg) => worker?.postMessage(msg);
const held = new Set();

screen.addEventListener('keydown', (e) => {
  const code = evdevCode(e.code);
  if (code === undefined) return;
  e.preventDefault();
  // L'autorepeat lo fa il guest (EV_REP): le ripetizioni del browser no.
  if (e.repeat || held.has(code)) return;
  held.add(code);
  send({ type: 'key', code, down: true });
});
screen.addEventListener('keyup', (e) => {
  const code = evdevCode(e.code);
  if (code === undefined) return;
  e.preventDefault();
  held.delete(code);
  send({ type: 'key', code, down: false });
});
screen.addEventListener('blur', () => {
  for (const code of held) send({ type: 'key', code, down: false });
  held.clear();
});

function abs(e) {
  const r = screen.getBoundingClientRect();
  return [absAxis((e.clientX - r.left) / r.width), absAxis((e.clientY - r.top) / r.height)];
}

// Contatti del touchscreen: pointerId -> slot (0..9).
const slots = new Map();
function slotFor(id) {
  if (!slots.has(id)) {
    const used = new Set(slots.values());
    let s = 0;
    while (used.has(s)) s++;
    if (s > 9) return null;
    slots.set(id, s);
  }
  return slots.get(id);
}

screen.addEventListener('pointerdown', (e) => {
  screen.focus();
  screen.setPointerCapture(e.pointerId);
  e.preventDefault();
  const [x, y] = abs(e);
  if (pointerKind === 'multitouch') {
    const slot = slotFor(e.pointerId);
    if (slot !== null) send({ type: 'touch', slot, x, y, down: true });
    return;
  }
  send({ type: 'abs', x, y });
  const code = BUTTONS[e.button];
  if (code) send({ type: 'button', code, down: true });
});
screen.addEventListener('pointermove', (e) => {
  const [x, y] = abs(e);
  if (pointerKind === 'multitouch') {
    if (slots.has(e.pointerId)) send({ type: 'touch', slot: slots.get(e.pointerId), x, y, down: true });
    return;
  }
  send({ type: 'abs', x, y });
});
const release = (e) => {
  if (pointerKind === 'multitouch') {
    if (slots.has(e.pointerId)) {
      send({ type: 'touch', slot: slots.get(e.pointerId), x: 0, y: 0, down: false });
      slots.delete(e.pointerId);
    }
    return;
  }
  const code = BUTTONS[e.button];
  if (code) send({ type: 'button', code, down: false });
};
screen.addEventListener('pointerup', release);
screen.addEventListener('pointercancel', release);
screen.addEventListener('contextmenu', (e) => e.preventDefault());
screen.addEventListener('wheel', (e) => {
  e.preventDefault();
  if (pointerKind !== 'multitouch' && e.deltaY) send({ type: 'wheel', delta: e.deltaY < 0 ? 1 : -1 });
}, { passive: false });

const power = $('power');
power.addEventListener('pointerdown', () => send({ type: 'power', down: true }));
power.addEventListener('pointerup', () => send({ type: 'power', down: false }));
power.addEventListener('pointerleave', (e) => e.buttons && send({ type: 'power', down: false }));

// ---- Avvio -----------------------------------------------------------------

function source(urlField, fileField) {
  const f = form.elements[fileField].files[0];
  if (f) return { file: f };
  const u = form.elements[urlField].value.trim();
  return u ? { url: new URL(u, location.href).href } : null;
}

function fmtStats(s) {
  const parts = [
    `${(s.steps / 1e6).toFixed(0)} M istruzioni`,
    `guest ${s.guestSecs.toFixed(2)} s`,
    `${s.mips.toFixed(1)} MIPS`,
  ];
  if (s.memory) parts.push(`memory ${(s.memory / 2 ** 20).toFixed(0)} MiB`);
  for (const [i, d] of s.disks.entries()) {
    const ov = d.overlay ? ` (persistente, gen. ${d.overlay.generation})` : '';
    parts.push(`vd${String.fromCharCode(97 + i)}: ${d.fills} blocchi, ${d.http.requests} letture, cow ${d.dirtyClusters}${ov}`);
  }
  if (s.feeder.served) parts.push(`attesa disco ${(s.feeder.waitMs / 1000).toFixed(1)} s`);
  if (s.jit) parts.push(`JIT ${s.jit.modules} moduli`);
  return parts.join(' · ');
}

// ---- AOSP: boot phases and adb ---------------------------------------------------

const osValue = () => form.elements.os.value;

function showOs() {
  const android = osValue() === 'android';
  $('android-fields').hidden = !android;
  $('linux-fields').hidden = android;
  const el = form.elements;
  if (android) {
    el.ramMiB.value = String(ANDROID_RAM_MIB);
    el.pointer.value = 'multitouch';
    el.net.checked = true;
    if (!el.manifestUrl.value) el.manifestUrl.value = DEFAULT_MANIFEST;
    if (!rootsFromUrl) pendingRoots = ANDROID_ROOTS;
  } else {
    el.ramMiB.value = '1024';
    el.pointer.value = 'tablet';
    if (!rootsFromUrl) pendingRoots = DEFAULT_ROOTS;
  }
}
for (const r of form.elements.os) r.addEventListener('change', showOs);

/** AOSP state for tests: phases, home screen, adb, installs. */
const androidState = { phases: [], booted: null, adb: { state: 'none' }, installs: [] };
let adbId = 0;
const adbPending = new Map();

/** An ADB request to the Worker: a Promise of the result. */
function adbRequest(op, args = {}, transfer = []) {
  return new Promise((ok, ko) => {
    if (!worker) return ko(new Error('machine off'));
    const id = ++adbId;
    adbPending.set(id, { ok, ko, op });
    worker.postMessage({ type: 'adb', id, op, ...args }, transfer);
  });
}

async function installApk(bytes, name = 'app.apk') {
  $('apk-status').textContent = `${name}: sending to the Worker (${(bytes.byteLength / 1024).toFixed(0)} KiB)`;
  const buf = bytes instanceof ArrayBuffer ? bytes : bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
  const t0 = performance.now();
  try {
    const r = await adbRequest('install', { bytes: buf, name }, [buf]);
    const entry = { name, ...r, ms: performance.now() - t0 };
    androidState.installs.push(entry);
    $('apk-status').textContent = `${r.info.package} installed (${(r.installMs / 1000).toFixed(1)} s) and opened (${(r.openMs / 1000).toFixed(1)} s): ${r.component ?? 'no main activity'}`;
    screen.focus();
    return entry;
  } catch (e) {
    androidState.installs.push({ name, error: e.message });
    $('apk-status').textContent = `${name}: ${e.message}`;
    throw e;
  }
}

window.vetroAndroid = {
  state: () => androidState,
  install: installApk,
  shell: (cmd) => adbRequest('shell', { cmd }),
  devices: () => adbRequest('devices'),
};

function renderPhases() {
  const list = $('phases');
  list.textContent = '';
  const seen = new Map(androidState.phases.map((p) => [p.phase, p]));
  const current = androidState.phases.at(-1)?.phase;
  for (const [name, label] of PHASES) {
    const li = document.createElement('li');
    li.textContent = label;
    const p = seen.get(name);
    if (p) {
      li.className = name === current && name !== 'booted' ? 'current' : 'done';
      const t = document.createElement('span');
      t.className = 't';
      t.textContent = `${p.guestSecs.toFixed(0)} s of guest time${p.wallMs !== undefined ? `, ${(p.wallMs / 1000).toFixed(0)} s wall` : ''}`;
      li.append(t);
    }
    list.append(li);
  }
}

function onAndroidMessage(msg) {
  switch (msg.type) {
    case 'progress':
      androidState.phases.push({ phase: msg.phase, label: msg.label, guestSecs: msg.guestSecs, wallMs: msg.wallMs });
      renderPhases();
      if (msg.phase === 'home') {
        androidState.home = { guestSecs: msg.guestSecs, wallMs: msg.wallMs, activity: msg.detail, colors: msg.colors, focusGuestSecs: msg.focusGuestSecs };
        $('boot-info').textContent += ` · home screen at ${msg.guestSecs.toFixed(0)} s of guest time, ${(msg.wallMs / 60000).toFixed(1)} min wall`;
        setStatus('home screen up: the state is saved shortly (the next start resumes from here)');
      } else if (msg.phase !== 'booted') setStatus(`boot: ${msg.label} (${msg.guestSecs.toFixed(0)} s of guest time)`);
      return true;
    case 'booted':
      androidState.booted = { guestSecs: msg.guestSecs, wallMs: msg.wallMs };
      $('boot-info').textContent = `boot finished at ${msg.guestSecs.toFixed(0)} s of guest time, ${(msg.wallMs / 60000).toFixed(1)} min wall`;
      setStatus('boot finished: waiting for the home screen ("Phone is starting" comes first)');
      return true;
    case 'adb-status': {
      androidState.adb = { state: msg.state, devices: msg.devices, error: msg.error };
      const d = msg.devices?.[0];
      $('adb-status').textContent = msg.state === 'ready' ? `connected: ${d?.serial ?? '?'} (${d?.model ?? ''}, ${msg.banner?.props?.['ro.product.name'] ?? ''})`
        : msg.state === 'connecting' ? 'connecting to adbd…' : `waiting for adbd${msg.error ? ` (${msg.error})` : ''}`;
      return true;
    }
    case 'adb-progress':
      $('apk-status').textContent = msg.text;
      return true;
    case 'adb-reply': {
      const p = adbPending.get(msg.id);
      adbPending.delete(msg.id);
      // The time is added only to results that are objects (not to lists).
      if (msg.ok) p?.ok(msg.result && typeof msg.result === 'object' && !Array.isArray(msg.result) ? { ...msg.result, ms: msg.ms } : msg.result);
      else p?.ko(new Error(msg.error));
      return true;
    }
  }
  return false;
}

// APK dropped on the panel or on the screen, or chosen.
async function onApkFiles(files) {
  const f = [...files].find((x) => /\.apk$/i.test(x.name)) ?? files[0];
  if (!f) return;
  await installApk(await f.arrayBuffer(), f.name).catch(() => {});
}
for (const target of [$('apk-drop'), $('screen-wrap')]) {
  target.addEventListener('dragover', (e) => {
    if (osValue() !== 'android' || !e.dataTransfer?.types.includes('Files')) return;
    e.preventDefault();
    target.classList.add('over');
  });
  target.addEventListener('dragleave', () => target.classList.remove('over'));
  target.addEventListener('drop', (e) => {
    target.classList.remove('over');
    if (osValue() !== 'android' || !e.dataTransfer?.files.length) return;
    e.preventDefault();
    onApkFiles(e.dataTransfer.files);
  });
}
$('apk-file').addEventListener('change', (e) => onApkFiles(e.target.files));
$('adb-shell').addEventListener('submit', async (e) => {
  e.preventDefault();
  const cmd = $('adb-cmd').value.trim();
  if (!cmd) return;
  const out = $('adb-out');
  out.textContent += `$ ${cmd}\n`;
  try {
    const r = await adbRequest('shell', { cmd });
    out.textContent += `${r.stdout}${r.stderr}${r.exitCode ? `[exit code ${r.exitCode}]\n` : ''}`;
  } catch (err) {
    out.textContent += `error: ${err.message}\n`;
  }
  out.scrollTop = out.scrollHeight;
});

async function start() {
  const el = form.elements;
  const android = osValue() === 'android';
  const kernel = android ? null : source('kernelUrl', 'kernelFile');
  if (!kernel && !android) return setStatus('manca il kernel');
  const disk = android ? null : source('diskUrl', 'diskFile');
  const config = {
    wasmUrl: new URL('../wasm/vetro_wasm.wasm', location.href).href,
    kernel,
    initrd: source('initrdUrl', 'initrdFile'),
    disks: disk ? [{ ...disk, blockSize: Number(el.blockKiB.value) << 10, readOnly: false }] : [],
    cmdline: el.cmdline.value,
    ramMiB: Number(el.ramMiB.value),
    width: Number(el.width.value),
    height: Number(el.height.value),
    pointer: el.pointer.value,
    net: el.net.checked,
    files: el.files.checked,
    jit: el.jit.checked,
    realtime: el.realtime.checked,
    opfs: el.opfs.checked,
    snapshot: el.snapshot.checked,
    persist: el.persist.checked,
    android: android ? { manifest: new URL(el.manifestUrl.value.trim() || DEFAULT_MANIFEST, location.href).href } : null,
  };
  if (android) {
    config.net = true;
    config.pointer = 'multitouch';
  }
  pointerKind = config.pointer;
  renderer = (el.webgpu.checked && (await WebGpuRenderer.create(screen).catch(() => null))) || new Canvas2DRenderer(screen);
  form.hidden = true;
  $('machine').hidden = false;
  $('files-box').hidden = !config.files;
  $('android-box').hidden = !android;
  $('screen-demo').hidden = android;
  $('screen-android').hidden = !android;
  if (android) renderPhases();
  startedAt = performance.now();
  worker = new Worker(new URL('./worker.mjs', import.meta.url), { type: 'module' });
  worker.onmessage = (e) => {
    const msg = e.data;
    if (panels.onMessage(msg) && msg.type !== 'replay-started' && msg.type !== 'replay-ended') return;
    if (onAndroidMessage(msg)) return;
    switch (msg.type) {
      case 'replay-started':
        // Una riga nel terminale (solo nella pagina, il guest non la vede).
        replaying = true;
        term.feed(new TextEncoder().encode(`\r\n\x1b[7m[vetro: replay dall'istruzione ${msg.from}${msg.target !== null ? `, fermo a ${msg.target}` : ''}]\x1b[0m\r\n`));
        replaying = false;
        renderConsole();
        setStatus(`replay dall'istruzione ${msg.from}`);
        break;
      case 'replay-ended':
        setStatus(msg.status.state === 'Finished' ? `replay identico (${msg.steps} istruzioni): la macchina continua libera` : `replay diverso: ${msg.status.message}`);
        break;
      case 'console':
        term.feed(msg.bytes);
        if (!consoleDirty) {
          consoleDirty = true;
          requestAnimationFrame(renderConsole);
        }
        break;
      case 'frame':
        onFrame(msg);
        break;
      case 'cursor':
        onCursor(msg);
        break;
      case 'stats':
        $('stats').textContent = fmtStats(msg);
        vetroState.disks = msg.disks;
        vetroState.memory = Math.max(vetroState.memory ?? 0, msg.memory ?? 0);
        vetroState.stats = msg;
        break;
      case 'restored': {
        vetroState.boot = { mode: 'snapshot', ms: performance.now() - startedAt, steps: msg.steps, size: msg.size, times: msg.times, memory: msg.memory };
        if (msg.progress) {
          androidState.phases = msg.progress.map((p) => ({ ...p, wallMs: undefined }));
          androidState.booted = { restored: true };
          renderPhases();
          $('boot-info').textContent = 'resumed from the snapshot saved at the home screen';
        }
        replaying = true;
        term.feed(msg.console);
        replaying = false;
        renderConsole();
        const t = msg.times;
        setStatus(`ripristinato dallo snapshot del ${new Date(msg.savedAt).toLocaleString()} (${(msg.size / 2 ** 20).toFixed(1)} MiB, ` +
          `ripristino ${t.restore.toFixed(0)} ms, pronto in ${(vetroState.boot.ms / 1000).toFixed(2)} s)`);
        break;
      }
      case 'cold':
        vetroState.boot = { mode: 'cold', ms: performance.now() - startedAt, times: msg.times, android: msg.android };
        break;
      case 'snapshot':
        vetroState.snapshots.push({ ...msg, at: performance.now() - startedAt });
        $('snapinfo').textContent = `snapshot salvato (${msg.why}): ${(msg.size / 2 ** 20).toFixed(1)} MiB in ${(msg.saveMs + msg.writeMs).toFixed(0)} ms`;
        break;
      case 'status':
        setStatus(msg.text);
        break;
      case 'files-reply': {
        const p = rpcPending.get(msg.id);
        rpcPending.delete(msg.id);
        if (msg.ok) p?.ok(msg.result);
        else p?.ko(Object.assign(new Error(msg.error), { code: msg.code }));
        break;
      }
      case 'files-event':
        filePanel.onEvent(msg.event);
        break;
      case 'files-status': {
        const first = msg.status.state === 'Ready' && filePanel.status.generation === 0;
        filePanel.onStatus(msg.status);
        if (first) filePanel.setRoots(pendingRoots);
        break;
      }
      case 'started':
        if (!msg.restored) setStatus(`in esecuzione (${renderer.name}, ${config.jit ? 'JIT' : 'interprete'}${crossOriginIsolated ? ', isolata' : ''})`);
        consoleEl.focus();
        break;
      case 'stopped':
        vetroState.stopped = msg;
        setStatus(`macchina ferma: ${msg.reason} dopo ${msg.steps} istruzioni`);
        break;
      case 'error':
        setStatus(`errore: ${msg.text.split('\n')[0]}`);
        console.error(msg.text);
        break;
    }
  };
  worker.onerror = (e) => setStatus(`errore nel Worker: ${e.message}`);
  worker.postMessage({ type: 'start', config });
}

$('save').addEventListener('click', () => send({ type: 'save' }));

// Cancella snapshot, overlay e cache dei blocchi (solo a macchina spenta:
// il Worker tiene aperti i file).
$('forget').addEventListener('click', async () => {
  try {
    const root = await navigator.storage.getDirectory();
    for (const name of ['vetro-snapshots', 'vetro-overlays', 'vetro-disks', 'vetro-recordings']) {
      await root.removeEntry(name, { recursive: true }).catch((e) => {
        if (e.name !== 'NotFoundError') throw e;
      });
    }
    setStatus('dati salvati cancellati (snapshot, dischi persistenti, cache dei blocchi, registrazioni)');
  } catch (e) {
    setStatus(`cancellazione non riuscita: ${e.message ?? e}`);
  }
});

form.addEventListener('submit', (e) => {
  e.preventDefault();
  start().catch((err) => setStatus(`errore: ${err.message ?? err}`));
});

// Parametri dell'URL.
const q = new URLSearchParams(location.search);
if (q.get('os') === 'android') {
  form.elements.os.value = 'android';
  showOs();
}
if (q.has('manifest')) form.elements.manifestUrl.value = q.get('manifest');
for (const [param, field] of [['kernel', 'kernelUrl'], ['initrd', 'initrdUrl'], ['disk', 'diskUrl'], ['cmdline', 'cmdline'], ['pointer', 'pointer'], ['ram', 'ramMiB']]) {
  if (q.has(param)) form.elements[field].value = q.get(param);
}
if (q.get('webgpu') === '1') form.elements.webgpu.checked = true;
if (q.get('snapshot') === '0') form.elements.snapshot.checked = false;
if (q.get('persist') === '0') form.elements.persist.checked = false;
if (q.get('nofiles') === '1') form.elements.files.checked = false;
if (q.has('files')) {
  pendingRoots = q.get('files').split(',').map((s) => s.trim()).filter(Boolean);
  rootsFromUrl = true;
}
if (q.get('autostart') === '1') start().catch((err) => setStatus(`errore: ${err.message ?? err}`));
