// Il pannello del gestore dei file (M8, ADR 0020): albero delle radici
// impostate dal chiamante, aggiornato dal vivo con gli eventi di inotify del
// demone `vetro-files` nel guest, e visualizzatori (testo, JSON, XML,
// esadecimale, immagini, SQLite) con modifica e salvataggio immediato nel
// guest (testo, JSON, XML, esadecimale).
//
// Il pannello non parla con la macchina: chiede operazioni con `rpc(op,
// args)` (una Promise: il Worker le passa a `GuestFiles` di vetro.mjs) e
// riceve eventi e stato con `onEvent` e `onStatus`. Le funzioni pure
// (riconoscimento del tipo, esadecimale, modi) sono esportate per i test.

import { formatValue, isSqlite, SqliteDb } from './sqlite.mjs';

/** Byte letti al più per aprire un file. */
export const MAX_OPEN = 16 << 20;
/** Byte al più mostrati (e modificabili) in esadecimale. */
export const MAX_HEX = 256 << 10;
/** Righe al più mostrate per tabella SQLite. */
export const MAX_ROWS = 500;

const IN_MODIFY = 0x2;
const IN_CLOSE_WRITE = 0x8;
const IN_MOVED_TO = 0x80;
const IN_Q_OVERFLOW = 0x4000;

/** `drwxr-x---` di un `st_mode` e del tipo. */
export function modeString(kind, mode) {
  const t = { dir: 'd', symlink: 'l', char: 'c', block: 'b', fifo: 'p', socket: 's' }[kind] ?? '-';
  let s = t;
  for (const shift of [6, 3, 0]) {
    const b = (mode >> shift) & 7;
    s += (b & 4 ? 'r' : '-') + (b & 2 ? 'w' : '-') + (b & 1 ? 'x' : '-');
  }
  return s;
}

/** Dimensione leggibile. */
export function sizeString(n) {
  if (n < 1024) return `${n} B`;
  if (n < 1 << 20) return `${(n / 1024).toFixed(1)} KiB`;
  return `${(n / (1 << 20)).toFixed(1)} MiB`;
}

const utf8 = new TextDecoder('utf-8', { fatal: true });

/** Il testo UTF-8 dei byte, o null se non sono testo (NUL o UTF-8 non valido). */
export function asText(bytes) {
  if (bytes.includes(0)) return null;
  try {
    return utf8.decode(bytes);
  } catch {
    return null;
  }
}

/** Tipo di immagine dai primi byte, o null. */
export function imageType(b) {
  const at = (sig, off = 0) => sig.every((x, i) => b[off + i] === x);
  if (at([0x89, 0x50, 0x4e, 0x47])) return 'image/png';
  if (at([0xff, 0xd8, 0xff])) return 'image/jpeg';
  if (at([0x47, 0x49, 0x46, 0x38])) return 'image/gif';
  if (at([0x52, 0x49, 0x46, 0x46]) && at([0x57, 0x45, 0x42, 0x50], 8)) return 'image/webp';
  if (at([0x42, 0x4d])) return 'image/bmp';
  return null;
}

/**
 * Il visualizzatore adatto: 'sqlite', 'image', 'json', 'xml', 'text' o
 * 'hex', dal contenuto e dall'estensione.
 */
export function detectView(name, bytes) {
  if (isSqlite(bytes)) return 'sqlite';
  if (imageType(bytes)) return 'image';
  const text = asText(bytes);
  if (text === null) return 'hex';
  const ext = name.toLowerCase().split('.').pop();
  const t = text.trimStart();
  if (ext === 'json' || ((t.startsWith('{') || t.startsWith('[')) && isJson(text))) return 'json';
  if (ext === 'xml' || t.startsWith('<?xml') || t.startsWith('<map')) return 'xml';
  return 'text';
}

function isJson(text) {
  try {
    JSON.parse(text);
    return true;
  } catch {
    return false;
  }
}

/** Dump esadecimale: righe `offset  16 byte  |ascii|`. */
export function hexDump(bytes) {
  const lines = [];
  for (let off = 0; off < bytes.length; off += 16) {
    const row = bytes.subarray(off, off + 16);
    const hex = Array.from(row, (x) => x.toString(16).padStart(2, '0')).join(' ');
    const ascii = Array.from(row, (x) => (x >= 0x20 && x < 0x7f ? String.fromCharCode(x) : '.')).join('');
    lines.push(`${off.toString(16).padStart(8, '0')}  ${hex.padEnd(47)}  |${ascii}|`);
  }
  return lines.join('\n');
}

/**
 * I byte di un dump esadecimale modificato: di ogni riga contano solo le
 * cifre fra l'offset e la colonna ASCII (si possono aggiungere o togliere
 * byte). Lancia con il numero di riga se una coppia non è esadecimale.
 */
export function parseHex(text) {
  const out = [];
  text.split('\n').forEach((line, i) => {
    let body = line;
    const bar = body.indexOf('|');
    if (bar >= 0) body = body.slice(0, bar);
    body = body.trim();
    if (!body) return;
    const parts = body.split(/\s+/);
    if (/^[0-9a-f]{8}$/i.test(parts[0]) && parts.length > 1) parts.shift();
    else if (parts.length === 1 && /^[0-9a-f]{8}$/i.test(parts[0])) return;
    for (const p of parts) {
      if (!/^[0-9a-f]{2}$/i.test(p)) throw new Error(`riga ${i + 1}: "${p}" non è un byte esadecimale`);
      out.push(parseInt(p, 16));
    }
  });
  return new Uint8Array(out);
}

/** Voci di un file XML delle SharedPreferences: [{ type, name, value }], o null. */
export function sharedPrefs(xml) {
  if (typeof DOMParser === 'undefined') return null;
  const doc = new DOMParser().parseFromString(xml, 'application/xml');
  if (doc.querySelector('parsererror') || doc.documentElement.nodeName !== 'map') return null;
  return Array.from(doc.documentElement.children, (e) => ({
    type: e.nodeName,
    name: e.getAttribute('name'),
    value: e.hasAttribute('value') ? e.getAttribute('value') : e.nodeName === 'set' ? Array.from(e.children, (c) => c.textContent).join(', ') : e.textContent,
  }));
}

/** Errore di sintassi XML (testo) o null. */
function xmlError(text) {
  if (typeof DOMParser === 'undefined') return null;
  const doc = new DOMParser().parseFromString(text, 'application/xml');
  const err = doc.querySelector('parsererror');
  return err ? err.textContent.split('\n')[0] : null;
}

const join = (dir, name) => (dir.endsWith('/') ? dir + name : `${dir}/${name}`);
const el = (tag, props = {}, ...children) => {
  const e = document.createElement(tag);
  Object.assign(e, props);
  for (const c of children) e.append(c);
  return e;
};

/**
 * Il pannello. `els`: { box, status, roots, tree, path, info, mode, save,
 * reload, content, message }; `rpc(op, args)`: Promise del Worker.
 */
export class FilePanel {
  constructor(els, rpc) {
    this.els = els;
    this.rpc = rpc;
    this.roots = [];
    /** Cartelle aperte: percorso → { wd, entries }. */
    this.open = new Map();
    /** wd → percorso. */
    this.watches = new Map();
    this.refreshTimers = new Map();
    this.status = { state: 'None', generation: 0 };
    /** File nel visualizzatore: { path, stat, bytes, view, dirty }. */
    this.current = null;
    this.imageUrl = null;
    els.mode.addEventListener('change', () => this.current && this.#render(els.mode.value));
    els.save.addEventListener('click', () => this.save().catch((e) => this.#message(`non salvato: ${e.message}`, true)));
    els.reload.addEventListener('click', () => this.current && this.openFile(this.current.path));
    els.roots.addEventListener('change', () => this.setRoots(els.roots.value.split(',').map((s) => s.trim()).filter(Boolean)));
  }

  /** Stato visibile ai test (window.vetroState.files). */
  snapshot() {
    return {
      ...this.status,
      roots: this.roots,
      shown: [...this.els.tree.querySelectorAll('[data-path]')].map((e) => e.dataset.path),
      current: this.current ? { path: this.current.path, view: this.current.view, dirty: this.current.dirty } : null,
      message: this.els.message.textContent,
    };
  }

  /** Le radici da mostrare (l'app in primo piano, o a mano). */
  async setRoots(roots) {
    this.roots = roots;
    this.els.roots.value = roots.join(', ');
    for (const [path, d] of this.open) if (d.wd !== null) this.rpc('unwatch', { wd: d.wd }).catch(() => {});
    this.open.clear();
    this.watches.clear();
    this.#renderTree();
    if (this.status.state === 'Ready') for (const r of roots) await this.expand(r).catch(() => {});
  }

  /** Stato del collegamento dal Worker. */
  onStatus(st) {
    const reconnected = st.state === 'Ready' && st.generation !== this.status.generation;
    this.status = st;
    this.els.status.textContent = st.state === 'Ready' ? `collegato${st.selinux ? ' (SELinux)' : ''}` : st.state === 'None' ? 'spento' : 'in collegamento…';
    if (reconnected) {
      // Connessione nuova: le osservazioni del demone sono perse.
      const paths = [...new Set([...this.roots, ...this.open.keys()])];
      this.open.clear();
      this.watches.clear();
      for (const p of paths) this.expand(p).catch(() => {});
    }
  }

  /** Evento di inotify dal guest. */
  onEvent(ev) {
    if (ev.mask & IN_Q_OVERFLOW) {
      for (const p of this.open.keys()) this.#scheduleRefresh(p);
      return;
    }
    const dir = this.watches.get(ev.wd);
    if (!dir) return;
    this.#scheduleRefresh(dir);
    const cur = this.current;
    if (cur && ev.name && join(dir, ev.name) === cur.path && ev.mask & (IN_CLOSE_WRITE | IN_MOVED_TO | IN_MODIFY) && !this.saving) {
      if (cur.dirty) this.#message('il file è cambiato nel guest: "Ricarica" per rileggerlo (le modifiche qui andrebbero perse)', true);
      else if (ev.mask & (IN_CLOSE_WRITE | IN_MOVED_TO)) this.openFile(cur.path, { quiet: true }).catch(() => {});
    }
  }

  #scheduleRefresh(dir) {
    clearTimeout(this.refreshTimers.get(dir));
    this.refreshTimers.set(dir, setTimeout(() => this.refresh(dir).catch(() => {}), 100));
  }

  /** Apre (lista + osservazione) una cartella. */
  async expand(path) {
    const entries = await this.rpc('list', { path });
    const known = this.open.get(path);
    let wd = known?.wd ?? null;
    if (wd === null) {
      wd = await this.rpc('watch', { path }).catch(() => null);
      if (wd !== null) this.watches.set(wd, path);
    }
    this.open.set(path, { wd, entries, error: null });
    this.#renderTree();
  }

  async collapse(path) {
    for (const p of [...this.open.keys()]) {
      if (p !== path && !p.startsWith(join(path, ''))) continue;
      const d = this.open.get(p);
      this.open.delete(p);
      if (d.wd !== null) {
        this.watches.delete(d.wd);
        this.rpc('unwatch', { wd: d.wd }).catch(() => {});
      }
    }
    this.#renderTree();
  }

  /** Rilegge una cartella aperta (dopo un evento). */
  async refresh(path) {
    const d = this.open.get(path);
    if (!d) return;
    try {
      d.entries = await this.rpc('list', { path });
      d.error = null;
    } catch (e) {
      d.error = e.message;
      d.entries = [];
    }
    this.#renderTree();
  }

  #renderTree() {
    const tree = this.els.tree;
    tree.textContent = '';
    // stat null: una radice (una cartella, di cui non si mostrano i metadati).
    const node = (path, name, stat, depth) => {
      const isDir = !stat || stat.kind === 'dir';
      const isOpen = this.open.has(path);
      const row = el('div', { className: `fnode${this.current?.path === path ? ' sel' : ''}` });
      row.dataset.path = path;
      row.style.paddingLeft = `${depth * 14 + 4}px`;
      row.append(el('span', { className: 'twisty', textContent: isDir ? (isOpen ? '▾' : '▸') : ' ' }));
      row.append(el('span', { className: `fname ${stat?.kind ?? 'dir'}`, textContent: name }));
      if (stat) {
        row.append(el('span', { className: 'fmeta', textContent: `${modeString(stat.kind, stat.mode)} ${stat.uid}:${stat.gid}${isDir ? '' : ` ${sizeString(stat.size)}`}` }));
        row.title = `${path}\n${modeString(stat.kind, stat.mode)} uid ${stat.uid} gid ${stat.gid}, ${stat.size} byte, ` +
          `modificato ${new Date(stat.mtime * 1000).toISOString()}${stat.link ? `\n→ ${stat.link}` : ''}${stat.selinux ? `\nSELinux: ${stat.selinux}` : ''}`;
      }
      row.addEventListener('click', () => {
        if (isDir) (isOpen ? this.collapse(path) : this.expand(path)).catch((e) => this.#message(`${path}: ${e.message}`, true));
        else this.openFile(path).catch((e) => this.#message(`${path}: ${e.message}`, true));
      });
      tree.append(row);
      if (isOpen) {
        const d = this.open.get(path);
        if (d.error) tree.append(el('div', { className: 'ferr', textContent: d.error }));
        // Prima le cartelle, poi i file, in ordine di nome.
        const dirFirst = (e) => (e.stat.kind === 'dir' ? 0 : 1);
        const sorted = [...d.entries].sort((a, b) => dirFirst(a) - dirFirst(b) || (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
        for (const e of sorted) node(join(path, e.name), e.name, e.stat, depth + 1);
      }
    };
    for (const r of this.roots) node(r, r, null, 0);
  }

  #message(text, error = false) {
    this.els.message.textContent = text;
    this.els.message.className = error ? 'err' : '';
  }

  /** Apre un file nel visualizzatore. */
  async openFile(path, { quiet = false } = {}) {
    const stat = await this.rpc('stat', { path });
    const { data } = await this.rpc('read', { path, offset: 0, length: MAX_OPEN });
    const name = path.split('/').pop();
    const view = detectView(name, data);
    this.current = { path, stat, bytes: data, view, dirty: false, truncated: stat.size > data.length };
    this.els.mode.value = view;
    this.#render(view);
    this.#renderTree();
    if (!quiet) this.#message('');
    else this.#message('ricaricato: il file è cambiato nel guest');
  }

  #info() {
    const c = this.current;
    const s = c.stat;
    this.els.path.textContent = c.path;
    this.els.info.textContent = `${modeString(s.kind, s.mode)} ${s.uid}:${s.gid} ${sizeString(s.size)}${s.selinux ? ` · ${s.selinux}` : ''}${c.truncated ? ' · mostrati i primi 16 MiB' : ''}`;
  }

  #render(view) {
    const c = this.current;
    c.view = view;
    this.#info();
    const box = this.els.content;
    box.textContent = '';
    if (this.imageUrl) URL.revokeObjectURL(this.imageUrl);
    this.imageUrl = null;
    const editable = ['text', 'json', 'xml', 'hex'].includes(view) && !c.truncated;
    this.els.save.disabled = true;
    if (view === 'image') {
      const type = imageType(c.bytes) ?? 'application/octet-stream';
      this.imageUrl = URL.createObjectURL(new Blob([c.bytes], { type }));
      box.append(el('img', { src: this.imageUrl, alt: c.path, className: 'fimg' }));
      return;
    }
    if (view === 'sqlite') {
      this.#renderSqlite(box);
      return;
    }
    let text;
    if (view === 'hex') {
      const part = c.bytes.subarray(0, MAX_HEX);
      text = hexDump(part);
      if (c.bytes.length > MAX_HEX) {
        box.append(el('div', { className: 'fnote', textContent: `mostrati i primi ${sizeString(MAX_HEX)}: modifica disattivata` }));
      }
    } else {
      text = asText(c.bytes) ?? new TextDecoder().decode(c.bytes);
    }
    const area = el('textarea', { className: 'fedit', spellcheck: false, value: text, readOnly: !editable || (view === 'hex' && c.bytes.length > MAX_HEX) });
    area.addEventListener('input', () => {
      c.dirty = true;
      this.els.save.disabled = false;
      this.#validate(area.value);
    });
    box.append(area);
    if (view === 'xml') this.#renderPrefs(box, text);
    if (view === 'json') {
      const fmt = el('button', { type: 'button', textContent: 'Formatta', className: 'fsmall' });
      fmt.addEventListener('click', () => {
        try {
          area.value = JSON.stringify(JSON.parse(area.value), null, 2);
          area.dispatchEvent(new Event('input'));
        } catch (e) {
          this.#message(`JSON non valido: ${e.message}`, true);
        }
      });
      box.prepend(fmt);
    }
    this.#validate(text);
  }

  #validate(text) {
    const v = this.current.view;
    if (v === 'json') {
      try {
        JSON.parse(text);
        this.#message('JSON valido');
      } catch (e) {
        this.#message(`JSON non valido: ${e.message}`, true);
      }
    } else if (v === 'xml') {
      const err = xmlError(text);
      this.#message(err ? `XML non valido: ${err}` : 'XML valido', !!err);
    } else if (v === 'hex') {
      try {
        const n = parseHex(text).length;
        this.#message(`${n} byte`);
      } catch (e) {
        this.#message(e.message, true);
      }
    }
  }

  #renderPrefs(box, text) {
    const prefs = sharedPrefs(text);
    if (!prefs) return;
    const table = el('table', { className: 'ftable' });
    table.append(el('tr', {}, el('th', { textContent: 'tipo' }), el('th', { textContent: 'nome' }), el('th', { textContent: 'valore' })));
    for (const p of prefs) table.append(el('tr', {}, el('td', { textContent: p.type }), el('td', { textContent: p.name ?? '' }), el('td', { textContent: p.value ?? '' })));
    box.append(el('div', { className: 'fnote', textContent: 'SharedPreferences (si modifica dal testo qui sopra)' }), table);
  }

  #renderSqlite(box) {
    let db;
    try {
      db = new SqliteDb(this.current.bytes);
    } catch (e) {
      box.append(el('div', { className: 'ferr', textContent: `SQLite: ${e.message}` }));
      return;
    }
    const tables = db.tables();
    const pick = el('select', { className: 'fsmall' });
    for (const t of tables) pick.append(el('option', { value: t.name, textContent: t.name }));
    const out = el('div', { className: 'fsql' });
    const show = () => {
      out.textContent = '';
      const name = pick.value;
      if (!name) return;
      try {
        const { columns, rows } = db.rows(name, MAX_ROWS);
        const table = el('table', { className: 'ftable' });
        table.append(el('tr', {}, ...columns.map((c) => el('th', { textContent: c }))));
        for (const r of rows) table.append(el('tr', {}, ...r.map((v) => el('td', { textContent: formatValue(v), className: v === null ? 'fnull' : '' }))));
        out.append(el('div', { className: 'fnote', textContent: `${rows.length} righe${rows.length === MAX_ROWS ? ' (le prime)' : ''} · pagine da ${db.pageSize} byte · solo lettura` }), table);
      } catch (e) {
        out.append(el('div', { className: 'ferr', textContent: e.message }));
      }
    };
    pick.addEventListener('change', show);
    box.append(el('div', {}, `${tables.length} tabelle: `, pick), out);
    show();
  }

  /** Salva il contenuto del visualizzatore nel guest (scrittura atomica). */
  async save() {
    const c = this.current;
    const area = this.els.content.querySelector('textarea');
    if (!c || !area) return;
    let bytes;
    if (c.view === 'hex') bytes = parseHex(area.value);
    else {
      if (c.view === 'json') JSON.parse(area.value);
      if (c.view === 'xml') {
        const err = xmlError(area.value);
        if (err) throw new Error(`XML non valido: ${err}`);
      }
      bytes = new TextEncoder().encode(area.value);
    }
    this.saving = true;
    try {
      const stat = await this.rpc('write', { path: c.path, bytes, mode: c.stat.mode & 0o7777 });
      c.stat = stat;
      c.bytes = bytes;
      c.dirty = false;
      this.els.save.disabled = true;
      this.#info();
      this.#message(`salvato nel guest: ${bytes.length} byte, ${modeString(stat.kind, stat.mode)} ${stat.uid}:${stat.gid}`);
    } finally {
      // Gli eventi della propria scrittura arrivano poco dopo.
      setTimeout(() => (this.saving = false), 500);
    }
  }
}
