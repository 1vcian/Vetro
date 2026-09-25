// La pagina di Vetro: sceglie kernel, initramfs e disco, avvia la macchina
// nel Worker (worker.mjs), mostra lo scanout di virtio-gpu e il cursore,
// la console seriale, e manda al guest tastiera, mouse/tocco e il tasto di
// accensione. Nessun bundler né dipendenza: moduli ES serviti così come
// sono (tools/web-serve.mjs).
//
// Parametri dell'URL per precompilare ed eventualmente avviare:
//   ?kernel=URL&initrd=URL&disk=URL&cmdline=...&pointer=multitouch&webgpu=1&autostart=1
//   &snapshot=0 (niente cache degli snapshot) &persist=0 (dischi non persistenti)
//
// Persistenza (M6, ADR 0017): il Worker salva in OPFS lo snapshot della
// macchina e l'overlay dei dischi; al secondo avvio riparte dallo snapshot.
// Lo stato si legge anche da `window.vetroState` (per i test nel browser).

import { absAxis, BUTTONS, evdevCode } from './keymap.mjs';
import { keyToBytes, Terminal } from './terminal.mjs';
import { Canvas2DRenderer, WebGpuRenderer } from './display.mjs';

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

const setStatus = (t) => {
  statusEl.textContent = t;
  statusEl.title = t;
};

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
  $('screen-off').hidden = true;
  if (msg.width !== fb.width || msg.height !== fb.height) {
    fb = { width: msg.width, height: msg.height };
    renderer.resize(fb.width, fb.height);
    placeCursor();
  }
  renderer.draw(msg.rect, msg.pixels);
}

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
  for (const [i, d] of s.disks.entries()) {
    const ov = d.overlay ? ` (persistente, gen. ${d.overlay.generation})` : '';
    parts.push(`vd${String.fromCharCode(97 + i)}: ${d.fills} blocchi, ${d.http.requests} letture, cow ${d.dirtyClusters}${ov}`);
  }
  if (s.feeder.served) parts.push(`attesa disco ${(s.feeder.waitMs / 1000).toFixed(1)} s`);
  if (s.jit) parts.push(`JIT ${s.jit.modules} moduli`);
  return parts.join(' · ');
}

async function start() {
  const el = form.elements;
  const kernel = source('kernelUrl', 'kernelFile');
  if (!kernel) return setStatus('manca il kernel');
  const disk = source('diskUrl', 'diskFile');
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
    jit: el.jit.checked,
    realtime: el.realtime.checked,
    opfs: el.opfs.checked,
    snapshot: el.snapshot.checked,
    persist: el.persist.checked,
  };
  pointerKind = config.pointer;
  renderer = (el.webgpu.checked && (await WebGpuRenderer.create(screen).catch(() => null))) || new Canvas2DRenderer(screen);
  form.hidden = true;
  $('machine').hidden = false;
  startedAt = performance.now();
  worker = new Worker(new URL('./worker.mjs', import.meta.url), { type: 'module' });
  worker.onmessage = (e) => {
    const msg = e.data;
    switch (msg.type) {
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
        break;
      case 'restored': {
        vetroState.boot = { mode: 'snapshot', ms: performance.now() - startedAt, steps: msg.steps, size: msg.size, times: msg.times };
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
        vetroState.boot = { mode: 'cold', ms: performance.now() - startedAt, times: msg.times };
        break;
      case 'snapshot':
        vetroState.snapshots.push({ ...msg, at: performance.now() - startedAt });
        $('snapinfo').textContent = `snapshot salvato (${msg.why}): ${(msg.size / 2 ** 20).toFixed(1)} MiB in ${(msg.saveMs + msg.writeMs).toFixed(0)} ms`;
        break;
      case 'status':
        setStatus(msg.text);
        break;
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
    for (const name of ['vetro-snapshots', 'vetro-overlays', 'vetro-disks']) {
      await root.removeEntry(name, { recursive: true }).catch((e) => {
        if (e.name !== 'NotFoundError') throw e;
      });
    }
    setStatus('dati salvati cancellati (snapshot, dischi persistenti, cache dei blocchi)');
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
for (const [param, field] of [['kernel', 'kernelUrl'], ['initrd', 'initrdUrl'], ['disk', 'diskUrl'], ['cmdline', 'cmdline'], ['pointer', 'pointer']]) {
  if (q.has(param)) form.elements[field].value = q.get(param);
}
if (q.get('webgpu') === '1') form.elements.webgpu.checked = true;
if (q.get('snapshot') === '0') form.elements.snapshot.checked = false;
if (q.get('persist') === '0') form.elements.persist.checked = false;
if (q.get('autostart') === '1') start().catch((err) => setStatus(`errore: ${err.message ?? err}`));
