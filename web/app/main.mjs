// The Vetro page: picks kernel, initramfs and disk, starts the machine
// in the Worker (worker.mjs), shows the virtio-gpu scanout and the cursor,
// the serial console, and sends keyboard, mouse/touch and the power button
// to the guest. No bundler and no dependencies: ES modules served as they
// are (tools/web-serve.mjs).
//
// URL parameters to prefill and optionally start:
//   ?kernel=URL&initrd=URL&disk=URL&cmdline=...&pointer=multitouch&webgpu=1&autostart=1
//   &snapshot=0 (no snapshot cache) &persist=0 (non-persistent disks)
//   &files=/tmp,/root (file manager roots) &nofiles=1 (no file manager)
//
// File manager (M8, ADR 0020): panel next to the screen with the tree of
// the roots (`window.vetroFiles.setRoots([...])`: by hand today; with
// Android they will be set by detecting the foreground app).
//
// Analysis (M7, M10, ADR 0023): below the screen the panels of the network
// inspector, of the input→effects timeline and of recording/replay
// (analysis.mjs); `window.vetroAnalysis` for the tests.
//
// Screen: with the scanout off a message explains that the guest is not
// drawing and a button types `timeout 30 vetro-dev drm-hold` in the console
// (test pattern, see DEMO_COMMAND).
//
// Persistence (M6, ADR 0017): the Worker saves the machine snapshot and the
// disk overlay in OPFS; at the second boot it resumes from the snapshot.
// The state can also be read from `window.vetroState` (for browser tests).
//
// Vetro's AOSP image (M5/M6, ADR 0028): `?os=android` (or the "System"
// selector) and `&manifest=URL` (default: the newest version published on R2,
// the others offered in the field, ANDROID_VERSIONS). The
// panel next to the screen shows the boot phases read from the console and
// the home screen, the adb status and the place to drop an APK (also on the
// screen), which the Worker installs with the ADB client and opens; an
// `adb shell` line. `window.vetroAndroid` for tests. The first start
// downloads the prebuilt snapshot at the home screen (ADR 0031) with a
// progress bar; `&cold=1` (or the "cold boot" box) boots from scratch.
//
// App catalog (M6, ADR 0033): in the Android panel, the suggested apps of
// catalog/v1.json on R2 (`&catalog=URL` to use another one; hidden if it
// can't be loaded) with Install -> download and SHA-256 check -> adb install
// (the same path as a drop, without opening) -> Open. `window.vetroCatalog`
// for tests.
//
// Device profiles (M10, ADR 0035): `&profile=phone` (or the "Device profile"
// menu, or a profile JSON file) fills screen and RAM and gives the Worker the
// profile's bootloader parameters and after-boot adb commands
// (web/node/profiles.mjs, starters in web/app/profiles/).

import { absAxis, BUTTONS, evdevCode } from './keymap.mjs';
import { keyToBytes, Terminal } from './terminal.mjs';
import { Canvas2DRenderer, WebGpuRenderer } from './display.mjs';
import { FilePanel } from './files.mjs';
import { AnalysisPanels } from './analysis.mjs';
import { ANDROID_MACHINE, ANDROID_VERSIONS, DEFAULT_MANIFEST, PHASES } from '../node/android.mjs';
import { CatalogPanel } from './catalog.mjs';
import { CATALOG_URL } from '../node/catalog.mjs';
import { DEFAULT_PROFILE, parseProfile, profileAdbCommands, profileBootParams, profileUrl, STARTER_PROFILES } from '../node/profiles.mjs';

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
/** State visible to tests: how the machine started, saved snapshots, disks. */
const vetroState = (window.vetroState = { boot: null, snapshots: [], disks: [], stopped: null });
let startedAt = 0;

// ---- File manager ------------------------------------------------------------

const DEFAULT_ROOTS = ['/tmp', '/root', '/etc'];
const ANDROID_ROOTS = ['/data/local/tmp', '/sdcard/Download'];
let rpcId = 0;
const rpcPending = new Map();
/** A file manager operation in the Worker: a Promise of the result. */
function rpc(op, args) {
  return new Promise((ok, ko) => {
    if (!worker) return ko(new Error('machine off'));
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
/** For the tests and for whoever sets the roots (the foreground app). */
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

// While replaying the console tail of a snapshot the terminal does not
// answer (the saved session had already answered ESC[6n: repeating it
// would be an extra input for the guest).
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

// ---- Screen ----------------------------------------------------------------

function placeCursor() {
  if (!cursor || !cursor.resource || !fb.width) {
    cursorCanvas.hidden = true;
    screen.classList.remove('guest-cursor');
    return;
  }
  const k = screen.clientWidth / fb.width;
  cursorCanvas.hidden = false;
  screen.classList.add('guest-cursor');
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
    // A portrait screen (device profiles, ADR 0035) fits the window's height.
    $('screen-wrap').style.maxWidth = fb.height > fb.width ? `calc(85vh * ${fb.width / fb.height})` : '';
    placeCursor();
  }
  renderer.draw(msg.rect, msg.pixels);
}

// Scanout off: with the test kernel the guest does not draw until a program
// uses DRM. The button types in the console a command of the test guest
// that draws the known pattern of `vetro-dev drm-hold`
// (tests/boot/tests/devices.rs) and holds it until a line arrives on stdin
// (Enter in the console) or for 30 s of guest time (BusyBox `timeout`):
// the shell is free again either way, and once DRM is closed the scanout
// turns off again.
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

// ---- Input -----------------------------------------------------------------

const send = (msg) => worker?.postMessage(msg);
const held = new Set();

screen.addEventListener('keydown', (e) => {
  const code = evdevCode(e.code);
  if (code === undefined) return;
  e.preventDefault();
  // Autorepeat is done by the guest (EV_REP): not the browser's repeats.
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

// Touchscreen contacts: pointerId -> slot (0..9).
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

// ---- Boot ------------------------------------------------------------------

function source(urlField, fileField) {
  const f = form.elements[fileField].files[0];
  if (f) return { file: f };
  const u = form.elements[urlField].value.trim();
  return u ? { url: new URL(u, location.href).href } : null;
}

function fmtStats(s) {
  const parts = [
    `${(s.steps / 1e6).toFixed(0)} M instructions`,
    `guest ${s.guestSecs.toFixed(2)} s`,
    `${s.mips.toFixed(1)} MIPS`,
  ];
  if (s.memory) parts.push(`memory ${(s.memory / 2 ** 20).toFixed(0)} MiB`);
  for (const [i, d] of s.disks.entries()) {
    const ov = d.overlay ? ` (persistent, gen. ${d.overlay.generation})` : '';
    parts.push(`vd${String.fromCharCode(97 + i)}: ${d.fills} blocks, ${d.http.requests} reads, cow ${d.dirtyClusters}${ov}`);
  }
  if (s.feeder.served) parts.push(`disk wait ${(s.feeder.waitMs / 1000).toFixed(1)} s`);
  if (s.jit) parts.push(`JIT ${s.jit.modules} modules`);
  return parts.join(' · ');
}

// ---- AOSP: boot phases and adb ---------------------------------------------------

const osValue = () => form.elements.os.value;

// ---- Device profiles (ADR 0035) ----------------------------------------------

/** Starter profiles (web/app/profiles), parsed, by id; a loaded file goes under its own id. */
const profiles = new Map();
const profileSelect = form.elements.profile;

/** The selected profile, or null while the starters are loading. */
const selectedProfile = () => profiles.get(profileSelect.value) ?? null;

/** Fills screen, RAM and the note from the selected profile. */
function applyProfile() {
  const p = selectedProfile();
  if (!p) return;
  const el = form.elements;
  el.width.value = String(p.screen.width);
  el.height.value = String(p.screen.height);
  el.ramMiB.value = String(p.ramMiB);
  // The ready-made snapshot note is about the default machine.
  $('prebuilt-note').hidden = p.id !== DEFAULT_PROFILE;
  $('profile-note').textContent = `${p.description || p.name} ` + (p.id === DEFAULT_PROFILE ? ''
    : 'Ready-made snapshots are published for the default profile: with this one the first start is usually a cold boot ' +
      '(about 45 minutes), then later starts resume in seconds from the snapshot saved in the browser.');
}

function addProfile(p, selected = false) {
  profiles.set(p.id, p);
  let opt = [...profileSelect.options].find((o) => o.value === p.id);
  if (!opt) {
    opt = document.createElement('option');
    opt.value = p.id;
    profileSelect.append(opt);
  }
  opt.textContent = `${p.name} (${p.screen.width} x ${p.screen.height}, ${p.screen.density} dpi, ${p.ramMiB} MiB)`;
  if (selected) profileSelect.value = p.id;
}

/** Loads the starter profiles; `wanted` is the id to select (URL `profile=`). */
async function loadProfiles(wanted) {
  for (const id of STARTER_PROFILES) {
    try {
      const r = await fetch(profileUrl(id, location.href));
      if (!r.ok) throw new Error(`status ${r.status}`);
      addProfile(parseProfile(await r.text()), id === (wanted ?? DEFAULT_PROFILE));
    } catch (e) {
      setStatus(`device profile ${id}: ${e.message ?? e}`);
    }
  }
  if (wanted && !profiles.has(wanted)) setStatus(`unknown device profile ${wanted}: using ${DEFAULT_PROFILE}`);
  if (osValue() === 'android') {
    applyProfile();
    // An explicit `ram=` in the URL wins over the profile's RAM.
    const ram = new URLSearchParams(location.search).get('ram');
    if (ram) form.elements.ramMiB.value = ram;
  }
}

profileSelect.addEventListener('change', applyProfile);
form.elements.profileFile.addEventListener('change', async (e) => {
  const f = e.target.files[0];
  if (!f) return;
  try {
    addProfile(parseProfile(await f.text()), true);
    applyProfile();
    setStatus(`device profile ${profileSelect.value} loaded from ${f.name}`);
  } catch (err) {
    setStatus(`${f.name}: ${err.message ?? err}`);
  }
});

function showOs() {
  const android = osValue() === 'android';
  $('android-fields').hidden = !android;
  $('linux-fields').hidden = android;
  const el = form.elements;
  if (android) {
    // The default machine until the profiles are loaded (loadProfiles applies the selected one).
    el.ramMiB.value = String(ANDROID_MACHINE.ramMiB);
    el.width.value = String(ANDROID_MACHINE.width);
    el.height.value = String(ANDROID_MACHINE.height);
    applyProfile();
    el.pointer.value = 'multitouch';
    el.net.checked = true;
    if (!el.manifestUrl.value) el.manifestUrl.value = DEFAULT_MANIFEST;
    syncVersion();
    if (!rootsFromUrl) pendingRoots = ANDROID_ROOTS;
    showPrebuiltHint();
  } else {
    el.ramMiB.value = '1024';
    el.width.value = '1280';
    el.height.value = '800';
    el.pointer.value = 'tablet';
    if (!rootsFromUrl) pendingRoots = DEFAULT_ROOTS;
  }
}
for (const r of form.elements.os) r.addEventListener('change', showOs);
// The published image versions (the first is the default) in a selector that
// fills the manifest field; the field still takes any URL ("other").
form.elements.androidVersion.prepend(...ANDROID_VERSIONS.map((v) => new Option(v.label, v.manifest)));
const syncVersion = () => {
  const url = form.elements.manifestUrl.value.trim();
  form.elements.androidVersion.value = ANDROID_VERSIONS.some((v) => v.manifest === url) ? url : '';
};
form.elements.androidVersion.addEventListener('change', () => {
  const v = form.elements.androidVersion.value;
  if (!v) return;
  form.elements.manifestUrl.value = v;
  showPrebuiltHint();
});

const mibText = (n) => `${(n / 2 ** 20).toFixed(0)} MiB`;
/** Download time at a given rate (bytes/s), as text. */
const etaText = (bytes, rate) => {
  const s = bytes / rate;
  return s < 90 ? `${Math.max(1, Math.round(s))} s` : `${(s / 60).toFixed(0)} min`;
};

/**
 * The size of the prebuilt snapshot for the default image, from the hint the
 * site build writes next to the app (`android-prebuilt.json`, ADR 0031): the
 * Worker still looks it up by its own key, the hint only sets expectations.
 */
let prebuiltHint;
let prebuiltNote; // the generic text, for versions without a hint
async function showPrebuiltHint() {
  if (prebuiltHint === undefined) {
    prebuiltHint = null;
    try {
      const r = await fetch(new URL('android-prebuilt.json', location.href), { cache: 'no-cache' });
      if (r.ok) prebuiltHint = await r.json();
    } catch {}
  }
  const h = prebuiltHint;
  const manifest = form.elements.manifestUrl.value.trim() || DEFAULT_MANIFEST;
  prebuiltNote ??= $('prebuilt-note').textContent;
  if (!h?.size || new URL(h.manifest).href !== new URL(manifest, location.href).href) {
    $('prebuilt-note').textContent = prebuiltNote;
    return;
  }
  $('prebuilt-note').textContent = `First start: the home screen is downloaded as a ready-made snapshot of the machine (${mibText(h.size)}: ` +
    `about ${etaText(h.size, 25e6)} at 25 MB/s, ${etaText(h.size, 6e6)} at 6 MB/s), kept in the browser's private storage (OPFS), ` +
    'then it resumes in seconds; later starts download nothing again. The disk is read in pieces with HTTP Range as the system needs it.';
}

/** AOSP state for tests: phases, home screen, adb, installs. */
const androidState = { phases: [], booted: null, adb: { state: 'none' }, installs: [], prebuilt: null };
let adbId = 0;
const adbPending = new Map();

/**
 * An ADB request to the Worker: a Promise of the result. `onProgress(msg)`
 * receives the request's `adb-progress` messages ({ text, fraction? }).
 */
function adbRequest(op, args = {}, transfer = [], onProgress = null) {
  return new Promise((ok, ko) => {
    if (!worker) return ko(new Error('machine off'));
    const id = ++adbId;
    adbPending.set(id, { ok, ko, op, onProgress });
    worker.postMessage({ type: 'adb', id, op, ...args }, transfer);
  });
}

// ---- App catalog (ADR 0033) -------------------------------------------------------

const catalog = new CatalogPanel({
  box: $('catalog-box'),
  list: $('catalog-list'),
  advancedBox: $('catalog-advanced'),
  advancedList: $('catalog-advanced-list'),
  note: $('catalog-note'),
}, {
  // The same Worker path as a dropped APK, without opening it (the card offers Open).
  install: async (bytes, name, onProgress) => {
    const buf = bytes.buffer.byteLength === bytes.byteLength ? bytes.buffer : bytes.slice().buffer;
    const t0 = performance.now();
    const r = await adbRequest('install', { bytes: buf, name, open: false }, [buf], onProgress);
    androidState.installs.push({ name, source: 'catalog', ...r, ms: performance.now() - t0 });
    return r;
  },
  open: (pkg, launcher) => adbRequest('open', { package: pkg, launcher }),
  shell: (cmd) => adbRequest('shell', { cmd }),
});
window.vetroCatalog = {
  state: () => catalog.snapshot(),
  install: (id) => catalog.install(id),
  open: (id) => catalog.open(id),
  refresh: () => catalog.refresh(),
};

/** The catalog for the running image (its version from the image manifest). */
async function loadCatalogPanel(manifestUrl, catalogUrl) {
  let version = null;
  try {
    const r = await fetch(manifestUrl);
    if (r.ok) version = (await r.json()).version ?? null;
  } catch {}
  await catalog.load(catalogUrl, version);
  if (androidState.adb.state === 'ready') await catalog.refresh();
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

/** Download of the prebuilt snapshot (ADR 0031): progress bar and text. */
function onPrebuilt(msg) {
  const box = $('prebuilt-box');
  const text = $('prebuilt-text');
  const bar = $('prebuilt-bar');
  const prev = androidState.prebuilt ?? {};
  androidState.prebuilt = { ...prev, ...msg };
  delete androidState.prebuilt.type;
  switch (msg.state) {
    case 'missing':
      $('boot-info').textContent = `No ready-made snapshot for this version of Vetro and of the image (${msg.reason}): cold boot, about 45 minutes before the home screen.`;
      break;
    case 'downloading': {
      box.hidden = false;
      if (msg.loaded === undefined) {
        bar.removeAttribute('value');
        text.textContent = `Downloading ${mibText(msg.size)}${msg.resumedFrom ? ` (resuming at ${mibText(msg.resumedFrom)})` : ''}…`;
        setStatus(text.textContent);
        break;
      }
      bar.value = msg.loaded / msg.total;
      const rate = (msg.loaded - msg.resumedFrom) / Math.max(msg.ms, 1) * 1000;
      text.textContent = `${mibText(msg.loaded)} of ${mibText(msg.total)} · ${(rate / 1e6).toFixed(1)} MB/s` +
        (rate > 0 ? ` · about ${etaText(msg.total - msg.loaded, rate)} left` : '') + (msg.resumedFrom ? ` · resumed at ${mibText(msg.resumedFrom)}` : '');
      break;
    }
    case 'done':
      bar.value = 1;
      text.textContent = `Downloaded and verified: ${mibText(msg.size)} in ${(msg.ms / 1000).toFixed(0)} s${msg.retries ? ` (${msg.retries} retries)` : ''}. Restoring…`;
      setStatus(text.textContent);
      break;
    case 'failed':
      box.hidden = false;
      text.textContent = `Download interrupted: ${msg.error}. Reload the page to resume it.`;
      break;
  }
}

function onAndroidMessage(msg) {
  switch (msg.type) {
    case 'prebuilt':
      onPrebuilt(msg);
      return true;
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
      setStatus('boot finished: waiting for the home screen ("Vetro is starting…" comes first)');
      return true;
    case 'adb-status': {
      const wasReady = androidState.adb.state === 'ready';
      androidState.adb = { state: msg.state, devices: msg.devices, error: msg.error };
      // Installed apps are read again whenever adb (re)connects (also after a restore).
      if (msg.state === 'ready' && !wasReady) catalog.refresh();
      const d = msg.devices?.[0];
      $('adb-status').textContent = msg.state === 'ready' ? `connected: ${d?.serial ?? '?'} (${d?.model ?? ''}, ${msg.banner?.props?.['ro.product.name'] ?? ''})`
        : msg.state === 'connecting' ? 'connecting to adbd…' : `waiting for adbd${msg.error ? ` (${msg.error})` : ''}`;
      return true;
    }
    case 'adb-progress': {
      const p = adbPending.get(msg.id);
      if (p?.onProgress) p.onProgress(msg);
      else $('apk-status').textContent = msg.text;
      return true;
    }
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
  // The profile fills screen and RAM: wait for the starters (autostart).
  if (android) await profilesLoaded;
  const kernel = android ? null : source('kernelUrl', 'kernelFile');
  if (!kernel && !android) return setStatus('kernel missing');
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
    android: android ? { manifest: new URL(el.manifestUrl.value.trim() || DEFAULT_MANIFEST, location.href).href, prebuilt: !el.coldBoot.checked } : null,
  };
  if (android) {
    config.net = true;
    config.pointer = 'multitouch';
    const p = selectedProfile();
    if (p) {
      config.android.profile = p.id;
      config.android.params = profileBootParams(p);
      config.android.setup = profileAdbCommands(p);
      androidState.profile = { id: p.id, params: config.android.params, setup: config.android.setup, width: config.width, height: config.height, ramMiB: config.ramMiB };
    }
  }
  pointerKind = config.pointer;
  renderer = (el.webgpu.checked && (await WebGpuRenderer.create(screen).catch(() => null))) || new Canvas2DRenderer(screen);
  form.hidden = true;
  $('machine').hidden = false;
  $('files-box').hidden = !config.files;
  $('android-box').hidden = !android;
  $('screen-demo').hidden = android;
  $('screen-android').hidden = !android;
  if (android) {
    renderPhases();
    loadCatalogPanel(config.android.manifest, new URL(q.get('catalog') || CATALOG_URL, location.href).href);
  }
  startedAt = performance.now();
  worker = new Worker(new URL('./worker.mjs', import.meta.url), { type: 'module' });
  worker.onmessage = (e) => {
    const msg = e.data;
    if (panels.onMessage(msg) && msg.type !== 'replay-started' && msg.type !== 'replay-ended') return;
    if (onAndroidMessage(msg)) return;
    switch (msg.type) {
      case 'replay-started':
        // A line in the terminal (only in the page, the guest does not see it).
        replaying = true;
        term.feed(new TextEncoder().encode(`\r\n\x1b[7m[vetro: replay from instruction ${msg.from}${msg.target !== null ? `, stopping at ${msg.target}` : ''}]\x1b[0m\r\n`));
        replaying = false;
        renderConsole();
        setStatus(`replay from instruction ${msg.from}`);
        break;
      case 'replay-ended':
        setStatus(msg.status.state === 'Finished' ? `replay identical (${msg.steps} instructions): the machine runs free again` : `replay differs: ${msg.status.message}`);
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
        vetroState.boot = { mode: 'snapshot', ms: performance.now() - startedAt, steps: msg.steps, size: msg.size, times: msg.times, memory: msg.memory, prebuilt: !!msg.prebuilt };
        if (msg.progress) {
          androidState.phases = msg.progress.map((p) => ({ ...p, wallMs: undefined }));
          androidState.booted = { restored: true };
          renderPhases();
          $('boot-info').textContent = msg.prebuilt ? 'resumed from the ready-made snapshot at the home screen (downloaded once, now in OPFS)'
            : 'resumed from the snapshot saved at the home screen';
          if (msg.prebuilt) $('prebuilt-text').textContent += ` Restored in ${(msg.times.restore / 1000).toFixed(1)} s.`;
        }
        replaying = true;
        term.feed(msg.console);
        replaying = false;
        renderConsole();
        const t = msg.times;
        setStatus(`restored from the snapshot of ${new Date(msg.savedAt).toLocaleString()} (${(msg.size / 2 ** 20).toFixed(1)} MiB, ` +
          `restore ${t.restore.toFixed(0)} ms, ready in ${(vetroState.boot.ms / 1000).toFixed(2)} s)`);
        break;
      }
      case 'cold':
        vetroState.boot = { mode: 'cold', ms: performance.now() - startedAt, times: msg.times, android: msg.android };
        break;
      case 'snapshot':
        vetroState.snapshots.push({ ...msg, at: performance.now() - startedAt });
        $('snapinfo').textContent = `snapshot saved (${msg.why}): ${(msg.size / 2 ** 20).toFixed(1)} MiB in ${(msg.saveMs + msg.writeMs).toFixed(0)} ms`;
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
        if (!msg.restored) setStatus(`running (${renderer.name}, ${config.jit ? 'JIT' : 'interpreter'}${crossOriginIsolated ? ', isolated' : ''})`);
        consoleEl.focus();
        break;
      case 'stopped':
        vetroState.stopped = msg;
        setStatus(`machine stopped: ${msg.reason} after ${msg.steps} instructions`);
        break;
      case 'error':
        setStatus(`error: ${msg.text.split('\n')[0]}`);
        console.error(msg.text);
        break;
    }
  };
  worker.onerror = (e) => setStatus(`error in the Worker: ${e.message}`);
  worker.postMessage({ type: 'start', config });
}

$('save').addEventListener('click', () => send({ type: 'save' }));

// Deletes snapshots, overlays and the block cache (only with the machine
// off: the Worker keeps the files open).
$('forget').addEventListener('click', async () => {
  try {
    const root = await navigator.storage.getDirectory();
    for (const name of ['vetro-snapshots', 'vetro-overlays', 'vetro-disks', 'vetro-recordings']) {
      await root.removeEntry(name, { recursive: true }).catch((e) => {
        if (e.name !== 'NotFoundError') throw e;
      });
    }
    setStatus('saved data deleted (snapshots, persistent disks, block cache, recordings)');
  } catch (e) {
    setStatus(`deletion failed: ${e.message ?? e}`);
  }
});

form.addEventListener('submit', (e) => {
  e.preventDefault();
  start().catch((err) => setStatus(`error: ${err.message ?? err}`));
});

// URL parameters.
const q = new URLSearchParams(location.search);
const profilesLoaded = loadProfiles(q.get('profile'));
if (q.get('os') === 'android') {
  form.elements.os.value = 'android';
  showOs();
}
if (q.has('manifest')) {
  form.elements.manifestUrl.value = q.get('manifest');
  syncVersion();
}
if (osValue() === 'android') showPrebuiltHint();
form.elements.manifestUrl.addEventListener('change', () => {
  syncVersion();
  showPrebuiltHint();
});
for (const [param, field] of [['kernel', 'kernelUrl'], ['initrd', 'initrdUrl'], ['disk', 'diskUrl'], ['cmdline', 'cmdline'], ['pointer', 'pointer'], ['ram', 'ramMiB']]) {
  if (q.has(param)) form.elements[field].value = q.get(param);
}
if (q.get('webgpu') === '1') form.elements.webgpu.checked = true;
if (q.get('cold') === '1') form.elements.coldBoot.checked = true;
if (q.get('snapshot') === '0') form.elements.snapshot.checked = false;
if (q.get('persist') === '0') form.elements.persist.checked = false;
if (q.get('nofiles') === '1') form.elements.files.checked = false;
if (q.has('files')) {
  pendingRoots = q.get('files').split(',').map((s) => s.trim()).filter(Boolean);
  rootsFromUrl = true;
}
if (q.get('autostart') === '1') start().catch((err) => setStatus(`error: ${err.message ?? err}`));
