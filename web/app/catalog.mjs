// The app catalog panel in the Android mode (M6, ADR 0033): the suggested
// apps of catalog/v1.json (web/node/catalog.mjs) as cards with icon, name,
// description, size, licence and source link, and a button that follows the
// app's state: Install (or Update) -> downloading (progress, then SHA-256
// check) -> installing (push progress, then pm) -> Open; Retry after a
// failure. The APK goes to the Worker's ADB client exactly like a dropped
// one (`install` with `open: false`); the Worker then saves the snapshot, so
// the app persists with the user's local snapshot (OPFS). Nothing is
// preinstalled. If the catalog can't be loaded the panel stays hidden.
//
// Dependencies are passed in (the page's adb requests), so the panel has no
// knowledge of the Worker protocol:
//   install(bytes, name, onProgress) -> { info, component }
//   open(pkg, launcher) -> { component }
//   shell(cmd) -> { stdout }
//
// A future Free-plan limit would hook into `install` (see ADR 0033): nothing
// is enforced today.

import { apkInfo } from '../node/apk.mjs';
import { downloadApk, imageSatisfies, initialState, loadCatalog, nextState, PACKAGES_QUERY, parsePackages, sizeText } from '../node/catalog.mjs';

/** Licences whose terms ask for the source to be offered with the binary. */
const COPYLEFT = /\b(A?GPL|LGPL|MPL|EUPL|CDDL)/;

export class CatalogPanel {
  #el;
  #deps;
  /** id -> entry */
  entries = new Map();
  /** id -> state (nextState) */
  states = new Map();
  /** id -> launcher component known from an install in this session */
  #launchers = new Map();
  #cards = new Map();
  #dirty = new Set();
  #frame = false;
  loaded = false;
  error = null;
  hidden = [];
  problems = [];

  /** `el`: { box, list, advancedBox, advancedList, note }. */
  constructor(el, deps) {
    this.#el = el;
    this.#deps = deps;
  }

  /**
   * Loads the catalog from `url` for the image `imageVersion` (the version
   * field of the image manifest, or null) and shows the panel; on failure the
   * panel stays hidden and `error` says why.
   */
  async load(url, imageVersion, { fetch: get = globalThis.fetch } = {}) {
    let catalog;
    try {
      catalog = await loadCatalog(url, { fetch: get });
    } catch (e) {
      this.error = String(e.message ?? e);
      this.#el.box.hidden = true;
      console.warn(`app catalog not shown: ${this.error}`);
      return false;
    }
    this.problems = catalog.problems;
    for (const p of catalog.problems) console.warn(`app catalog: ${p}`);
    const fits = catalog.apps.filter((e) => imageSatisfies(e.minImage, imageVersion));
    this.hidden = catalog.apps.filter((e) => !fits.includes(e)).map((e) => e.id);
    if (!fits.length) {
      this.error = 'no app in the catalog fits this image';
      this.#el.box.hidden = true;
      return false;
    }
    this.#el.list.textContent = '';
    this.#el.advancedList.textContent = '';
    for (const e of fits) {
      this.entries.set(e.id, e);
      this.states.set(e.id, initialState());
      const card = this.#card(e);
      this.#cards.set(e.id, card);
      (e.advanced ? this.#el.advancedList : this.#el.list).append(card.li);
      this.#render(e.id);
    }
    this.#el.advancedBox.hidden = !fits.some((e) => e.advanced);
    this.#el.note.textContent = `Downloaded from Vetro's mirror and checked (SHA-256) before installing; installed apps are kept in this browser's saved state.` +
      (this.hidden.length ? ` ${this.hidden.length} app(s) need a newer image.` : '');
    this.#el.box.hidden = false;
    this.loaded = true;
    return true;
  }

  /** Reads the installed packages from the device (after adb connects, after installs). */
  async refresh() {
    if (!this.loaded) return;
    try {
      const r = await this.#deps.shell(PACKAGES_QUERY);
      const packages = parsePackages(r.stdout);
      for (const id of this.entries.keys()) this.#event(id, { type: 'packages', packages });
    } catch (e) {
      console.warn(`app catalog: installed packages not read (${e.message ?? e})`);
    }
  }

  /** Downloads, verifies and installs app `id`; resolves when done (the state says how it went). */
  async install(id) {
    const entry = this.entries.get(id);
    if (!entry) throw new Error(`no app ${id} in the catalog`);
    const before = this.states.get(id);
    if (this.#event(id, { type: 'download', total: entry.size }) === before) return this.states.get(id);
    try {
      const bytes = await downloadApk(entry, { onProgress: ({ loaded }) => this.#event(id, { type: 'progress', loaded }) });
      const info = await apkInfo(bytes);
      if (info.package !== entry.package) throw new Error(`the APK is ${info.package}, the catalog says ${entry.package}`);
      this.#event(id, { type: 'downloaded' });
      const r = await this.#deps.install(bytes, `${entry.package}.apk`, (p) => {
        if (typeof p.fraction === 'number') this.#event(id, { type: 'pushing', fraction: p.fraction });
      });
      if (r?.component) this.#launchers.set(id, r.component.split('/')[1]);
      this.#event(id, { type: 'installed', versionCode: typeof info.versionCode === 'number' ? info.versionCode : null });
    } catch (e) {
      this.#event(id, { type: 'failed', error: e.message ?? e });
    }
    return this.states.get(id);
  }

  /** Opens installed app `id`. */
  async open(id) {
    const entry = this.entries.get(id);
    if (!entry) throw new Error(`no app ${id} in the catalog`);
    const card = this.#cards.get(id);
    card.status.textContent = 'opening…';
    try {
      const r = await this.#deps.open(entry.package, this.#launchers.get(id) ?? null);
      card.status.textContent = `opened (${r.component})`;
      return r;
    } catch (e) {
      card.status.textContent = `not opened: ${e.message ?? e}`;
      throw e;
    }
  }

  /** For tests: what the panel shows. */
  snapshot() {
    return {
      loaded: this.loaded,
      error: this.error,
      hidden: this.hidden,
      problems: this.problems,
      apps: [...this.entries.values()].map((e) => ({ id: e.id, package: e.package, advanced: e.advanced, ...this.states.get(e.id), button: this.#cards.get(e.id)?.button.textContent ?? null })),
    };
  }

  #event(id, event) {
    const entry = this.entries.get(id);
    const prev = this.states.get(id);
    const next = nextState(prev, event, entry);
    if (next !== prev) {
      this.states.set(id, next);
      // Progress events come fast: one render per frame.
      if (event.type === 'progress' || event.type === 'pushing') this.#later(id);
      else this.#render(id);
    }
    return next;
  }

  #later(id) {
    this.#dirty.add(id);
    if (this.#frame) return;
    this.#frame = true;
    const flush = () => {
      this.#frame = false;
      for (const d of this.#dirty) this.#render(d);
      this.#dirty.clear();
    };
    if (typeof requestAnimationFrame === 'function') requestAnimationFrame(flush);
    else setTimeout(flush, 16);
  }

  #card(e) {
    const mk = (tag, cls, text) => {
      const n = document.createElement(tag);
      if (cls) n.className = cls;
      if (text !== undefined) n.textContent = text;
      return n;
    };
    const li = mk('li', 'app-card');
    li.dataset.app = e.id;
    const icon = mk('img', 'app-icon');
    icon.alt = '';
    icon.width = 48;
    icon.height = 48;
    icon.loading = 'lazy';
    // The page is cross-origin isolated (COEP): images from R2 need CORS.
    icon.crossOrigin = 'anonymous';
    icon.addEventListener('error', () => icon.classList.add('missing'));
    if (e.icon) icon.src = e.icon;
    else icon.classList.add('missing');
    const body = mk('div', 'app-body');
    const title = mk('div', 'app-title');
    title.append(mk('strong', '', e.name), mk('span', 'dim', ` ${e.version}`));
    if (e.advanced) title.append(mk('span', 'app-badge', 'advanced'));
    const desc = mk('p', 'app-desc', e.description);
    const meta = mk('p', 'app-meta dim');
    const src = mk('a', '', COPYLEFT.test(e.license) ? 'source code (this version)' : 'source code');
    src.href = e.source;
    src.target = '_blank';
    src.rel = 'noopener';
    meta.append(`${sizeText(e.size)} · ${e.license} · `, src);
    const row = mk('div', 'app-actions');
    const button = mk('button', 'app-button');
    button.type = 'button';
    button.addEventListener('click', () => {
      const s = this.states.get(e.id);
      if (s.phase === 'installed') this.open(e.id).catch(() => {});
      else this.install(e.id);
    });
    const bar = mk('progress', 'app-progress');
    bar.max = 1;
    const status = mk('span', 'app-status dim');
    row.append(button, bar, status);
    body.append(title, desc, meta, row);
    li.append(icon, body);
    return { li, button, bar, status };
  }

  #render(id) {
    const s = this.states.get(id);
    const c = this.#cards.get(id);
    if (!c) return;
    c.li.dataset.state = s.phase;
    c.bar.hidden = s.phase !== 'downloading' && s.phase !== 'installing';
    if (s.fraction === null) c.bar.removeAttribute('value');
    else c.bar.value = s.fraction;
    c.button.disabled = s.phase === 'downloading' || s.phase === 'installing';
    switch (s.phase) {
      case 'absent':
        c.button.textContent = s.outdated ? 'Update' : 'Install';
        c.status.textContent = s.outdated ? `version code ${s.installedCode} installed` : '';
        break;
      case 'downloading':
        c.button.textContent = 'Downloading…';
        c.status.textContent = `${sizeText(s.loaded)} of ${sizeText(s.total)}`;
        break;
      case 'installing':
        c.button.textContent = 'Installing…';
        c.status.textContent = s.fraction === null ? 'verified, sending to the device' : s.fraction < 1 ? `sending to the device (${Math.round(s.fraction * 100)}%)` : 'installing (pm)';
        break;
      case 'installed':
        c.button.textContent = 'Open';
        c.status.textContent = 'installed';
        break;
      case 'failed':
        c.button.textContent = 'Retry';
        c.status.textContent = `failed: ${s.error}`;
        break;
    }
  }
}
