// The file manager panel (M8, ADR 0020): tree of the roots set by the
// caller, updated live with the inotify events of the `vetro-files` daemon
// in the guest, and viewers (text, JSON, XML, hexadecimal, images, SQLite)
// with editing and immediate saving into the guest (text, JSON, XML,
// hexadecimal; SharedPreferences in a table that rewrites the XML the way
// Android does; SQLite rows with SQL run in the guest by the real engine,
// ADR 0021).
//
// Paths are surrogateescape strings (ADR 0021): a non-UTF-8 byte of the
// name is a lone surrogate, shown as \xNN.
//
// The panel does not talk to the machine: it requests operations with
// `rpc(op, args)` (a Promise: the Worker hands them to `GuestFiles` of
// vetro.mjs) and receives events and status with `onEvent` and `onStatus`.
// The pure functions (type detection, hexadecimal, modes) are exported for
// the tests.

import { deleteRowSql, formatValue, insertRowSql, isSqlite, SqliteDb, sqlQuote, updateCellSql } from './sqlite.mjs';

/** Maximum bytes read to open a file. */
export const MAX_OPEN = 16 << 20;
/** Maximum bytes shown (and editable) in hexadecimal. */
export const MAX_HEX = 256 << 10;
/** Maximum rows shown per SQLite table. */
export const MAX_ROWS = 500;

const IN_MODIFY = 0x2;
const IN_CLOSE_WRITE = 0x8;
const IN_MOVED_TO = 0x80;
const IN_Q_OVERFLOW = 0x4000;

/** `drwxr-x---` from an `st_mode` and the type. */
export function modeString(kind, mode) {
  const t = { dir: 'd', symlink: 'l', char: 'c', block: 'b', fifo: 'p', socket: 's' }[kind] ?? '-';
  let s = t;
  for (const shift of [6, 3, 0]) {
    const b = (mode >> shift) & 7;
    s += (b & 4 ? 'r' : '-') + (b & 2 ? 'w' : '-') + (b & 1 ? 'x' : '-');
  }
  return s;
}

/** Human-readable size. */
export function sizeString(n) {
  if (n < 1024) return `${n} B`;
  if (n < 1 << 20) return `${(n / 1024).toFixed(1)} KiB`;
  return `${(n / (1 << 20)).toFixed(1)} MiB`;
}

const utf8 = new TextDecoder('utf-8', { fatal: true });

/** The UTF-8 text of the bytes, or null if they are not text (NUL or invalid UTF-8). */
export function asText(bytes) {
  if (bytes.includes(0)) return null;
  try {
    return utf8.decode(bytes);
  } catch {
    return null;
  }
}

/** Image type from the first bytes, or null. */
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
 * The suitable viewer: 'sqlite', 'image', 'json', 'xml', 'text' or
 * 'hex', from the content and the extension.
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

/** Hexadecimal dump: lines `offset  16 bytes  |ascii|`. */
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
 * The bytes of an edited hexadecimal dump: of each line only the digits
 * between the offset and the ASCII column count (bytes can be added or
 * removed). Throws with the line number if a pair is not hexadecimal.
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
      if (!/^[0-9a-f]{2}$/i.test(p)) throw new Error(`line ${i + 1}: "${p}" is not a hexadecimal byte`);
      out.push(parseInt(p, 16));
    }
  });
  return new Uint8Array(out);
}

/** A guest name with the non-UTF-8 bytes (lone surrogates, ADR 0021) shown as \xNN. */
export function displayName(s) {
  return s.replace(/[\udc80-\udcff]/g, (c, i) => {
    const prev = i > 0 ? s.charCodeAt(i - 1) : 0;
    return prev >= 0xd800 && prev <= 0xdbff ? c : `\\x${(c.charCodeAt(0) - 0xdc00).toString(16).padStart(2, '0')}`;
  });
}

// ---- SharedPreferences (ADR 0021) ---------------------------------------------

const ENTITIES = { lt: '<', gt: '>', amp: '&', quot: '"', apos: "'" };

/**
 * Small XML reader (no DOM, also in Node) for SharedPreferences files:
 * elements, attributes, text, entities, CDATA, comments, processing
 * instructions. Returns the root { name, attrs, children, text }; throws
 * with the position if the XML is not well-formed.
 */
export function parseXml(xml) {
  let i = 0;
  const fail = (m) => {
    throw new Error(`invalid XML: ${m} (character ${i})`);
  };
  const decode = (s) =>
    s.replace(/&([^;&\s]*);|&/g, (m, e) => {
      if (e === undefined) fail('& without an entity');
      if (e in ENTITIES) return ENTITIES[e];
      const n = /^#x([0-9a-fA-F]+)$/.exec(e) ? parseInt(e.slice(2), 16) : /^#([0-9]+)$/.exec(e) ? parseInt(e.slice(1), 10) : NaN;
      if (!(n >= 0 && n <= 0x10ffff)) fail(`entity &${e};`);
      return String.fromCodePoint(n);
    });
  const skip = () => {
    for (;;) {
      while (i < xml.length && /\s/.test(xml[i])) i++;
      if (xml.startsWith('<!--', i)) {
        const e = xml.indexOf('-->', i + 4);
        if (e < 0) fail('unclosed comment');
        i = e + 3;
      } else if (xml.startsWith('<?', i)) {
        const e = xml.indexOf('?>', i + 2);
        if (e < 0) fail('unclosed instruction');
        i = e + 2;
      } else if (xml.startsWith('<!DOCTYPE', i)) {
        const e = xml.indexOf('>', i);
        if (e < 0) fail('unclosed DOCTYPE');
        i = e + 1;
      } else return;
    }
  };
  const NAME = /[A-Za-z_:][\w.:-]*/y;
  const name = () => {
    NAME.lastIndex = i;
    const m = NAME.exec(xml);
    if (!m) fail('name expected');
    i += m[0].length;
    return m[0];
  };
  const element = () => {
    if (xml[i] !== '<') fail('< expected');
    i++;
    const node = { name: name(), attrs: {}, children: [], text: '' };
    for (;;) {
      const before = i;
      while (/\s/.test(xml[i] ?? '')) i++;
      if (xml.startsWith('/>', i)) {
        i += 2;
        return node;
      }
      if (xml[i] === '>') {
        i++;
        break;
      }
      if (i === before) fail('space expected between attributes');
      const a = name();
      while (/\s/.test(xml[i] ?? '')) i++;
      if (xml[i] !== '=') fail('= expected');
      i++;
      while (/\s/.test(xml[i] ?? '')) i++;
      const q = xml[i];
      if (q !== '"' && q !== "'") fail('quotes expected');
      const e = xml.indexOf(q, i + 1);
      if (e < 0) fail('unclosed attribute');
      if (a in node.attrs) fail(`repeated attribute ${a}`);
      const raw = xml.slice(i + 1, e);
      if (raw.includes('<')) fail('< in an attribute');
      node.attrs[a] = decode(raw);
      i = e + 1;
    }
    for (;;) {
      if (i >= xml.length) fail(`unclosed <${node.name}>`);
      if (xml.startsWith('</', i)) {
        i += 2;
        if (name() !== node.name) fail(`closing tag differs from <${node.name}>`);
        while (/\s/.test(xml[i] ?? '')) i++;
        if (xml[i] !== '>') fail('> expected');
        i++;
        return node;
      }
      if (xml.startsWith('<!--', i)) {
        const e = xml.indexOf('-->', i + 4);
        if (e < 0) fail('unclosed comment');
        i = e + 3;
      } else if (xml.startsWith('<![CDATA[', i)) {
        const e = xml.indexOf(']]>', i + 9);
        if (e < 0) fail('unclosed CDATA');
        node.text += xml.slice(i + 9, e);
        i = e + 3;
      } else if (xml.startsWith('<?', i)) {
        const e = xml.indexOf('?>', i + 2);
        if (e < 0) fail('unclosed instruction');
        i = e + 2;
      } else if (xml[i] === '<') node.children.push(element());
      else {
        const e = xml.indexOf('<', i);
        const end = e < 0 ? xml.length : e;
        node.text += decode(xml.slice(i, end));
        i = end;
      }
    }
  };
  skip();
  const root = element();
  skip();
  if (i < xml.length) fail('text after the root');
  return root;
}

/** SharedPreferences types. */
export const PREF_TYPES = ['string', 'int', 'long', 'float', 'boolean', 'set', 'null'];

/**
 * Entries of a SharedPreferences XML file: [{ type, name, value }] with
 * `value` a string (for `set` an array of strings, for `null` null), or null
 * if the root is not <map> or there is a type the table does not handle.
 * Throws if the XML is not well-formed.
 */
export function parsePrefs(xml) {
  const root = parseXml(xml);
  if (root.name !== 'map') return null;
  const out = [];
  for (const e of root.children) {
    const name = e.attrs.name ?? null;
    if (name === null) return null;
    if (e.name === 'string') out.push({ type: 'string', name, value: e.text });
    else if (['int', 'long', 'float', 'boolean'].includes(e.name)) {
      if (!('value' in e.attrs)) return null;
      out.push({ type: e.name, name, value: e.attrs.value });
    } else if (e.name === 'set') {
      if (e.children.some((c) => c.name !== 'string')) return null;
      out.push({ type: 'set', name, value: e.children.map((c) => c.text) });
    } else if (e.name === 'null') out.push({ type: 'null', name, value: null });
    else return null;
  }
  return out;
}

/** Like FastXmlSerializer's ESCAPE_TABLE: control characters as &#N;, then " & < >. */
function prefEscape(s) {
  return s.replace(/[\u0000-\u001f"&<>]/g, (c) =>
    c === '"' ? '&quot;' : c === '&' ? '&amp;' : c === '<' ? '&lt;' : c === '>' ? '&gt;' : `&#${c.charCodeAt(0)};`,
  );
}

/**
 * The SharedPreferences XML as Android writes it (XmlUtils.writeMapXml
 * with FastXmlSerializer and indentation): same header, 4-space indent,
 * empty tags as ` />`, same escaped characters, `\n` after every closing
 * tag. Like FastXmlSerializer, a text that ends with `\n` indents the
 * closing tag.
 */
export function prefsToXml(entries) {
  let out = "<?xml version='1.0' encoding='utf-8' standalone='yes' ?>\n";
  if (!entries.length) return `${out}<map />\n`;
  const text = (indent, tag, attrs, value) => {
    const end = value.endsWith('\n') ? indent : '';
    return `${indent}<${tag}${attrs}>${prefEscape(value)}${end}</${tag}>\n`;
  };
  out += '<map>\n';
  for (const e of entries) {
    const attrs = ` name="${prefEscape(e.name)}"`;
    if (e.type === 'string') out += text('    ', 'string', attrs, e.value);
    else if (e.type === 'null') out += `    <null${attrs} />\n`;
    else if (e.type === 'set') {
      if (!e.value.length) out += `    <set${attrs} />\n`;
      else out += `    <set${attrs}>\n${e.value.map((v) => text('        ', 'string', '', v)).join('')}    </set>\n`;
    } else out += `    <${e.type}${attrs} value="${prefEscape(String(e.value))}" />\n`;
  }
  return `${out}</map>\n`;
}

/** Java's `Float.toString` for a 32-bit float. */
export function javaFloatString(x) {
  const f = Math.fround(x);
  if (Number.isNaN(f)) return 'NaN';
  if (f === Infinity) return 'Infinity';
  if (f === -Infinity) return '-Infinity';
  if (f === 0) return Object.is(f, -0) ? '-0.0' : '0.0';
  // The shortest digits that give back the same float.
  let d = f;
  for (let p = 1; p <= 9; p++) {
    const v = Number(f.toPrecision(p));
    if (Math.fround(v) === f) {
      d = v;
      break;
    }
  }
  const a = Math.abs(f);
  if (a >= 1e-3 && a < 1e7) {
    const s = String(d);
    return s.includes('.') ? s : `${s}.0`;
  }
  let [m, e] = d.toExponential().split('e');
  if (!m.includes('.')) m += '.0';
  return `${m}E${Number(e)}`;
}

/**
 * Checks a value the way Android reads it back (Integer.parseInt,
 * Long.parseLong, Float.parseFloat, true/false) and returns it in the
 * form Android would write; throws if it is not valid.
 */
export function checkPrefValue(type, text) {
  const s = String(text);
  if (type === 'int' || type === 'long') {
    if (!/^[+-]?\d+$/.test(s)) throw new Error(`"${s}" is not an integer`);
    const v = BigInt(s);
    const bits = type === 'int' ? 32 : 64;
    if (BigInt.asIntN(bits, v) !== v) throw new Error(`${s} out of the range of a ${type}`);
    return v.toString();
  }
  if (type === 'float') {
    const t = s.trim().replace(/[fFdD]$/, '');
    if (!/^[+-]?(NaN|Infinity|(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?)$/.test(t)) throw new Error(`"${s}" is not a float`);
    return javaFloatString(Number(t));
  }
  if (type === 'boolean') {
    if (s !== 'true' && s !== 'false') throw new Error(`"${s}" is not true or false`);
    return s;
  }
  return s;
}

/** XML syntax error (text) or null. */
function xmlError(text) {
  if (typeof DOMParser === 'undefined') return null;
  const doc = new DOMParser().parseFromString(text, 'application/xml');
  const err = doc.querySelector('parsererror');
  return err ? err.textContent.split('\n')[0] : null;
}

const join = (dir, name) => (dir.endsWith('/') ? dir + name : `${dir}/${name}`);
const base = (path) => path.split('/').pop();

/** An SQL parameter as text for the preview. */
function paramString(p) {
  const v = p !== null && typeof p === 'object' && !(p instanceof Uint8Array) ? p.value : p;
  if (v === null || v === undefined) return 'NULL';
  if (v instanceof Uint8Array) return formatValue(v);
  if (typeof v === 'string') return `'${v.replace(/'/g, "''")}'`;
  return String(v);
}

/** Table of a query result. */
function resultTable(r) {
  const table = el('table', { className: 'ftable fsql-result' });
  table.append(el('tr', {}, ...r.columns.map((c) => el('th', { textContent: c }))));
  for (const row of r.rows) table.append(el('tr', {}, ...row.map((v) => el('td', { textContent: formatValue(v), className: v === null ? 'fnull' : '' }))));
  return el('div', {}, el('div', { className: 'fnote', textContent: `${r.rows.length} rows${r.truncated ? ' (truncated)' : ''}` }), table);
}
const el = (tag, props = {}, ...children) => {
  const e = document.createElement(tag);
  Object.assign(e, props);
  for (const c of children) e.append(c);
  return e;
};

/**
 * The panel. `els`: { box, status, roots, tree, path, info, mode, save,
 * reload, content, message }; `rpc(op, args)`: a Promise from the Worker.
 */
export class FilePanel {
  constructor(els, rpc) {
    this.els = els;
    this.rpc = rpc;
    this.roots = [];
    /** Open folders: path → { wd, entries }. */
    this.open = new Map();
    /** wd → path. */
    this.watches = new Map();
    /** Folders whose first listing is awaited (in the tree: "…"). */
    this.loading = new Set();
    this.refreshTimers = new Map();
    this.status = { state: 'None', generation: 0 };
    /** File in the viewer: { path, stat, bytes, view, dirty }. */
    this.current = null;
    this.imageUrl = null;
    els.mode.addEventListener('change', () => this.current && this.#render(els.mode.value));
    els.save.addEventListener('click', () => this.save().catch((e) => this.#message(`not saved: ${e.message}`, true)));
    els.reload.addEventListener('click', () => this.current && this.openFile(this.current.path));
    els.roots.addEventListener('change', () => this.setRoots(els.roots.value.split(',').map((s) => s.trim()).filter(Boolean)));
  }

  /** State visible to tests (window.vetroState.files). */
  snapshot() {
    return {
      ...this.status,
      roots: this.roots,
      shown: [...this.els.tree.querySelectorAll('[data-path]')].map((e) => e.dataset.path),
      current: this.current ? { path: this.current.path, view: this.current.view, dirty: this.current.dirty } : null,
      message: this.els.message.textContent,
    };
  }

  /** The roots to show (the foreground app, or by hand). */
  async setRoots(roots) {
    this.roots = roots;
    this.els.roots.value = roots.join(', ');
    for (const [path, d] of this.open) if (d.wd !== null) this.rpc('unwatch', { wd: d.wd }).catch(() => {});
    this.open.clear();
    this.watches.clear();
    this.loading.clear();
    this.#renderTree();
    if (this.status.state === 'Ready') for (const r of roots) await this.expand(r).catch(() => {});
  }

  /** Connection status from the Worker. */
  onStatus(st) {
    const reconnected = st.state === 'Ready' && st.generation !== this.status.generation;
    this.status = st;
    this.els.status.textContent = st.state === 'Ready' ? `connected${st.selinux ? ' (SELinux)' : ''}` : st.state === 'None' ? 'off' : 'connecting…';
    if (reconnected) {
      // New connection: the daemon's watches are lost.
      const paths = [...new Set([...this.roots, ...this.open.keys()])];
      this.open.clear();
      this.watches.clear();
      for (const p of paths) this.expand(p).catch(() => {});
    }
  }

  /** inotify event from the guest. */
  onEvent(ev) {
    if (ev.mask & IN_Q_OVERFLOW) {
      for (const p of this.open.keys()) this.#scheduleRefresh(p);
      return;
    }
    const dir = this.watches.get(ev.wd);
    if (!dir) return;
    this.#scheduleRefresh(dir);
    const cur = this.current;
    // A database: the file or its -wal changes (SQLite writes without closing).
    if (cur?.view === 'sqlite' && ev.name && (join(dir, ev.name) === cur.path || join(dir, ev.name) === `${cur.path}-wal`)) {
      if (!this.sqlBusy && !this.els.content.querySelector('.fsql-edit')) {
        clearTimeout(this.sqlTimer);
        this.sqlTimer = setTimeout(() => this.openFile(cur.path, { quiet: true }).catch(() => {}), 150);
      }
      return;
    }
    if (cur && ev.name && join(dir, ev.name) === cur.path && ev.mask & (IN_CLOSE_WRITE | IN_MOVED_TO | IN_MODIFY) && !this.saving) {
      if (cur.dirty) this.#message('the file changed in the guest: "Reload" to read it again (the changes here would be lost)', true);
      else if (ev.mask & (IN_CLOSE_WRITE | IN_MOVED_TO)) this.openFile(cur.path, { quiet: true }).catch(() => {});
    }
  }

  #scheduleRefresh(dir) {
    clearTimeout(this.refreshTimers.get(dir));
    this.refreshTimers.set(dir, setTimeout(() => this.refresh(dir).catch(() => {}), 100));
  }

  /** Opens (list + watch) a folder. */
  async expand(path) {
    this.loading.add(path);
    this.#renderTree();
    let entries;
    try {
      entries = await this.rpc('list', { path });
    } finally {
      this.loading.delete(path);
    }
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

  /** Reads an open folder again (after an event). */
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
    // stat null: a root (a folder, whose metadata is not shown).
    const node = (path, name, stat, depth) => {
      const isDir = !stat || stat.kind === 'dir';
      const isOpen = this.open.has(path);
      const isLoading = !isOpen && this.loading.has(path);
      const row = el('div', { className: `fnode${this.current?.path === path ? ' sel' : ''}` });
      row.dataset.path = path;
      row.style.paddingLeft = `${depth * 14 + 4}px`;
      row.append(el('span', { className: 'twisty', textContent: isDir ? (isOpen || isLoading ? '▾' : '▸') : ' ' }));
      row.append(el('span', { className: `fname ${stat?.kind ?? 'dir'}`, textContent: displayName(name) }));
      if (stat) {
        row.append(el('span', { className: 'fmeta', textContent: `${modeString(stat.kind, stat.mode)} ${stat.uid}:${stat.gid}${isDir ? '' : ` ${sizeString(stat.size)}`}` }));
        row.title = `${displayName(path)}\n${modeString(stat.kind, stat.mode)} uid ${stat.uid} gid ${stat.gid}, ${stat.size} bytes, ` +
          `modified ${new Date(stat.mtime * 1000).toISOString()}${stat.link ? `\n→ ${stat.link}` : ''}${stat.selinux ? `\nSELinux: ${stat.selinux}` : ''}`;
      }
      row.addEventListener('click', () => {
        if (isDir) (isOpen ? this.collapse(path) : this.expand(path)).catch((e) => this.#message(`${path}: ${e.message}`, true));
        else this.openFile(path).catch((e) => this.#message(`${path}: ${e.message}`, true));
      });
      tree.append(row);
      // Rows without data-path: they are not entries (vetroFiles.state().shown).
      const note = (text) => {
        const n = el('div', { className: 'fempty', textContent: text });
        n.style.paddingLeft = `${(depth + 1) * 14 + 4}px`;
        tree.append(n);
      };
      if (isLoading) note('…');
      if (isOpen) {
        const d = this.open.get(path);
        if (d.error) tree.append(el('div', { className: 'ferr', textContent: d.error }));
        // Folders first, then files, in name order.
        const dirFirst = (e) => (e.stat.kind === 'dir' ? 0 : 1);
        const sorted = [...d.entries].sort((a, b) => dirFirst(a) - dirFirst(b) || (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
        for (const e of sorted) node(join(path, e.name), e.name, e.stat, depth + 1);
        if (!sorted.length && !d.error) note('(empty)');
      }
    };
    for (const r of this.roots) node(r, r, null, 0);
  }

  #message(text, error = false) {
    this.els.message.textContent = text;
    this.els.message.className = error ? 'err' : '';
  }

  /**
   * Opens a file in the viewer. `quiet`: false = empty message, true
   * = "reloaded", null = the message stays. An SQLite database is read
   * with its -wal, if there is one (ADR 0021).
   */
  async openFile(path, { quiet = false } = {}) {
    const stat = await this.rpc('stat', { path });
    const { data } = await this.rpc('read', { path, offset: 0, length: MAX_OPEN });
    const view = detectView(base(path), data);
    let wal = null;
    if (view === 'sqlite') {
      wal = await this.rpc('read', { path: `${path}-wal`, offset: 0, length: MAX_OPEN }).then((r) => r.data, () => null);
    }
    this.current = { path, stat, bytes: data, wal, view, dirty: false, truncated: stat.size > data.length };
    this.els.mode.value = view;
    this.#render(view);
    this.#renderTree();
    if (quiet === false) this.#message('');
    else if (quiet) this.#message('reloaded: the file changed in the guest');
  }

  #info() {
    const c = this.current;
    const s = c.stat;
    this.els.path.textContent = displayName(c.path);
    this.els.info.textContent = `${modeString(s.kind, s.mode)} ${s.uid}:${s.gid} ${sizeString(s.size)}${s.selinux ? ` · ${s.selinux}` : ''}${c.truncated ? ' · showing the first 16 MiB' : ''}`;
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
        box.append(el('div', { className: 'fnote', textContent: `showing the first ${sizeString(MAX_HEX)}: editing disabled` }));
      }
    } else {
      text = asText(c.bytes) ?? new TextDecoder().decode(c.bytes);
    }
    const area = el('textarea', { className: 'fedit', spellcheck: false, value: text, readOnly: !editable || (view === 'hex' && c.bytes.length > MAX_HEX) });
    area.addEventListener('input', () => {
      c.dirty = true;
      this.els.save.disabled = !!c.invalid;
      this.#validate(area.value);
    });
    box.append(area);
    c.invalid = false;
    if (view === 'xml' && editable) this.#renderPrefs(box, text, area);
    if (view === 'json') {
      const fmt = el('button', { type: 'button', textContent: 'Format', className: 'fsmall' });
      fmt.addEventListener('click', () => {
        try {
          area.value = JSON.stringify(JSON.parse(area.value), null, 2);
          area.dispatchEvent(new Event('input'));
        } catch (e) {
          this.#message(`invalid JSON: ${e.message}`, true);
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
        this.#message('valid JSON');
      } catch (e) {
        this.#message(`invalid JSON: ${e.message}`, true);
      }
    } else if (v === 'xml') {
      const err = xmlError(text);
      this.#message(err ? `invalid XML: ${err}` : 'valid XML', !!err);
    } else if (v === 'hex') {
      try {
        const n = parseHex(text).length;
        this.#message(`${n} bytes`);
      } catch (e) {
        this.#message(e.message, true);
      }
    }
  }

  #renderPrefs(box, text, area) {
    let prefs;
    try {
      prefs = parsePrefs(text);
    } catch {
      prefs = null;
    }
    if (!prefs) return;
    const c = this.current;
    const wrap = el('div', { className: 'fprefs' });
    const note = el('div', { className: 'fnote', textContent: 'SharedPreferences: edit in the table (rewrites the XML the way Android does) or in the text above; then "Save"' });
    const table = el('table', { className: 'ftable' });
    const errors = new Set();
    // The table rewrites the text; the text, when it changes, redraws the table.
    const sync = () => {
      c.invalid = errors.size > 0;
      if (c.invalid) {
        this.els.save.disabled = true;
        return;
      }
      area.value = prefsToXml(prefs);
      area.dispatchEvent(new Event('input'));
    };
    const draw = () => {
      table.textContent = '';
      table.append(el('tr', {}, el('th', { textContent: 'type' }), el('th', { textContent: 'name' }), el('th', { textContent: 'value' }), el('th')));
      prefs.forEach((p, k) => {
        const type = el('select', { className: 'fpref-type' });
        for (const t of PREF_TYPES) type.append(el('option', { value: t, textContent: t, selected: t === p.type }));
        const name = el('input', { className: 'fpref-name', value: p.name, spellcheck: false });
        const value = p.type === 'set'
          ? el('textarea', { className: 'fpref-value', value: p.value.join('\n'), rows: Math.max(2, p.value.length), spellcheck: false, title: 'one element per line' })
          : el('input', { className: 'fpref-value', value: p.value ?? '', disabled: p.type === 'null', spellcheck: false });
        const check = () => {
          try {
            if (p.type === 'set') p.value = value.value === '' ? [] : value.value.split('\n');
            else if (p.type === 'null') p.value = null;
            else p.value = checkPrefValue(p.type, value.value);
            errors.delete(k);
            value.classList.remove('bad');
            this.#message('');
          } catch (e) {
            errors.add(k);
            value.classList.add('bad');
            this.#message(`${p.name}: ${e.message}`, true);
          }
        };
        value.addEventListener('change', () => {
          check();
          if (!errors.has(k) && p.type !== 'set' && p.type !== 'null') value.value = p.value;
          sync();
        });
        name.addEventListener('change', () => {
          p.name = name.value;
          sync();
        });
        type.addEventListener('change', () => {
          const old = p.type;
          p.type = type.value;
          if (p.type === 'set') p.value = old === 'null' || p.value === null || p.value === '' ? [] : [String(p.value)];
          else if (p.type === 'null') p.value = null;
          else if (old === 'set') p.value = p.value.join('');
          else if (p.value === null) p.value = p.type === 'boolean' ? 'false' : p.type === 'string' ? '' : '0';
          draw();
          const v = table.querySelectorAll('.fpref-value')[k];
          if (p.type !== 'set' && p.type !== 'null') {
            try {
              p.value = checkPrefValue(p.type, p.value);
              v.value = p.value;
              errors.delete(k);
            } catch (e) {
              errors.add(k);
              v.classList.add('bad');
              this.#message(`${p.name}: ${e.message}`, true);
            }
          } else errors.delete(k);
          sync();
        });
        const del = el('button', { type: 'button', className: 'fsmall fpref-del', textContent: '✕', title: 'Remove the entry' });
        del.addEventListener('click', () => {
          prefs.splice(k, 1);
          errors.clear();
          draw();
          sync();
        });
        table.append(el('tr', {}, el('td', {}, type), el('td', {}, name), el('td', {}, value), el('td', {}, del)));
      });
    };
    const add = el('button', { type: 'button', className: 'fsmall fpref-add', textContent: 'Add entry' });
    add.addEventListener('click', () => {
      let n = 1;
      while (prefs.some((p) => p.name === `new${n}`)) n++;
      prefs.push({ type: 'string', name: `new${n}`, value: '' });
      draw();
      sync();
    });
    area.addEventListener('change', () => {
      try {
        const again = parsePrefs(area.value);
        if (!again) return;
        prefs = again;
        errors.clear();
        draw();
      } catch {
        // Invalid XML: the text validation already gives the message.
      }
    });
    draw();
    wrap.append(note, table, add);
    box.append(wrap);
  }

  #renderSqlite(box) {
    const c = this.current;
    let db;
    try {
      db = new SqliteDb(c.bytes, c.wal);
    } catch (e) {
      box.append(el('div', { className: 'ferr', textContent: `SQLite: ${e.message}` }));
      return;
    }
    const tables = db.tables();
    const pick = el('select', { className: 'fsmall fsql-table' });
    for (const t of tables) pick.append(el('option', { value: t.name, textContent: t.name, selected: t.name === this.sqliteTable }));
    const insert = el('button', { type: 'button', className: 'fsmall fsql-insert', textContent: 'Insert row' });
    const free = el('button', { type: 'button', className: 'fsmall fsql-free', textContent: 'SQL…' });
    // Editor (value of a cell or new row) and query preview.
    const editor = el('div', { className: 'fsql-editor' });
    const out = el('div', { className: 'fsql' });
    let view = null;
    const show = () => {
      out.textContent = '';
      editor.textContent = '';
      const name = pick.value;
      this.sqliteTable = name;
      if (!name) return;
      try {
        view = db.rows(name, MAX_ROWS);
        const table = el('table', { className: 'ftable' });
        table.append(el('tr', {}, ...view.columns.map((col) => el('th', { textContent: col })), el('th')));
        view.rows.forEach((r, ri) => {
          const tr = el('tr');
          r.forEach((v, ci) => {
            const td = el('td', { textContent: formatValue(v), className: v === null ? 'fnull' : '', title: `${view.types[ri][ci]} · click to edit` });
            td.dataset.r = ri;
            td.dataset.c = ci;
            td.addEventListener('click', () => this.#editCell(editor, view, ri, ci));
            tr.append(td);
          });
          const del = el('button', { type: 'button', className: 'fsmall fsql-delete', textContent: '✕', title: 'Remove the row' });
          del.addEventListener('click', () => this.#sqlPreview(editor, deleteRowSql(view, ri), 1));
          tr.append(el('td', {}, del));
          table.append(tr);
        });
        const wal = db.walFrames ? ` · WAL: ${db.walFrames} frames applied` : '';
        out.append(el('div', { className: 'fnote', textContent: `${view.rows.length} rows${view.rows.length === MAX_ROWS ? ' (the first ones)' : ''} · ${db.pageSize}-byte pages${wal} · click a cell to edit it (SQL in the guest)` }), table);
      } catch (e) {
        view = null;
        out.append(el('div', { className: 'ferr', textContent: e.message }));
      }
    };
    pick.addEventListener('change', show);
    insert.addEventListener('click', () => view && this.#insertRow(editor, view.table));
    free.addEventListener('click', () => this.#sqlPreview(editor, { sql: view ? `SELECT * FROM ${sqlQuote(view.table.name)} LIMIT 10` : '', params: [] }, null));
    box.append(el('div', {}, `${tables.length} tables: `, pick, ' ', insert, ' ', free), editor, out);
    show();
  }

  /** Editor of a value: type and text. Returns { box, value() } (value throws if not valid). */
  #valueEditor(v, type, allowDefault = false) {
    const types = [...(allowDefault ? ['default'] : []), 'text', 'integer', 'real', 'null', 'blob'];
    const sel = el('select', { className: 'fsmall fsql-type' });
    for (const t of types) sel.append(el('option', { value: t, textContent: t === 'default' ? 'DEFAULT' : t.toUpperCase(), selected: t === type }));
    const text = v === null ? '' : v instanceof Uint8Array ? Array.from(v, (x) => x.toString(16).padStart(2, '0')).join('') : String(v);
    const input = el('input', { className: 'fsql-value', value: text, spellcheck: false });
    const sync = () => (input.disabled = sel.value === 'null' || sel.value === 'default');
    sel.addEventListener('change', sync);
    sync();
    const value = () => {
      const s = input.value;
      switch (sel.value) {
        case 'null':
          return null;
        case 'integer':
          if (!/^\s*[+-]?\d+\s*$/.test(s)) throw new Error(`"${s}" is not an integer`);
          if (BigInt.asIntN(64, BigInt(s.trim())) !== BigInt(s.trim())) throw new Error(`${s} does not fit in 64 bits`);
          return { type: 'integer', value: BigInt(s.trim()) };
        case 'real':
          if (s.trim() === '' || Number.isNaN(Number(s))) throw new Error(`"${s}" is not a number`);
          return { type: 'real', value: Number(s) };
        case 'blob': {
          const h = s.replace(/\s+/g, '');
          if (!/^([0-9a-fA-F]{2})*$/.test(h)) throw new Error('BLOB: hexadecimal digits in pairs');
          return { type: 'blob', value: new Uint8Array(h.match(/../g)?.map((x) => parseInt(x, 16)) ?? []) };
        }
        case 'default':
          return undefined;
        default:
          return { type: 'text', value: s };
      }
    };
    return { box: el('span', {}, sel, ' ', input), value, input };
  }

  #editCell(editor, view, r, i) {
    editor.textContent = '';
    const t = view.table;
    const col = view.columns[i];
    if (!t.withoutRowid && t.rowidAlias < 0 && i === 0) {
      this.#message('the rowid of a table without INTEGER PRIMARY KEY cannot be changed from here', true);
      return;
    }
    const ed = this.#valueEditor(view.rows[r][i], view.types[r][i]);
    const go = el('button', { type: 'button', className: 'fsmall fsql-preview', textContent: 'Preview' });
    const cancel = el('button', { type: 'button', className: 'fsmall', textContent: 'Cancel' });
    const who = t.withoutRowid ? `row ${r + 1}` : `rowid ${view.rowids[r]}`;
    go.addEventListener('click', () => {
      try {
        this.#sqlPreview(editor, updateCellSql(view, r, i, ed.value()), 1);
      } catch (e) {
        this.#message(e.message, true);
      }
    });
    cancel.addEventListener('click', () => (editor.textContent = ''));
    editor.append(el('div', { className: 'fsql-edit' }, `${t.name} · ${who} · ${col}: `, ed.box, ' ', go, ' ', cancel));
    ed.input.focus();
  }

  #insertRow(editor, table) {
    editor.textContent = '';
    const eds = table.columns.map((col, k) => {
      const alias = k === table.rowidAlias;
      return [col, this.#valueEditor(null, alias ? 'default' : 'null', true)];
    });
    const go = el('button', { type: 'button', className: 'fsmall fsql-preview', textContent: 'Preview' });
    const cancel = el('button', { type: 'button', className: 'fsmall', textContent: 'Cancel' });
    go.addEventListener('click', () => {
      try {
        const values = {};
        for (const [col, ed] of eds) {
          const v = ed.value();
          if (v !== undefined) values[col] = v;
        }
        this.#sqlPreview(editor, insertRowSql(table, values), 1);
      } catch (e) {
        this.#message(e.message, true);
      }
    });
    cancel.addEventListener('click', () => (editor.textContent = ''));
    const rows = eds.map(([col, ed]) => el('div', { className: 'fsql-col' }, `${col}: `, ed.box));
    editor.append(el('div', { className: 'fsql-edit' }, `New row in ${table.name}`, ...rows, go, ' ', cancel));
  }

  /**
   * Preview of the query (editable) with the parameters, then "Run": an SQL
   * request to the daemon, run in the guest by SQLite as the owner of the
   * database (ADR 0021). `expect`: rows that must change (null = any; if
   * the text changes it no longer applies).
   */
  #sqlPreview(editor, { sql, params }, expect) {
    editor.textContent = '';
    const area = el('textarea', { className: 'fsql-query', value: sql, spellcheck: false, rows: 3 });
    const shown = params.map((p, k) => `?${k + 1} = ${paramString(p)}`).join('   ');
    const run = el('button', { type: 'button', className: 'fsmall fsql-run', textContent: 'Run in the guest' });
    const cancel = el('button', { type: 'button', className: 'fsmall', textContent: 'Cancel' });
    cancel.addEventListener('click', () => (editor.textContent = ''));
    run.addEventListener('click', () => {
      const text = area.value;
      const changed = text !== sql;
      this.#runSql(text, params, changed ? null : expect)
        .then((r) => {
          editor.textContent = '';
          if (r.columns.length) editor.append(resultTable(r));
        })
        .catch((e) => this.#message(`not run: ${e.message}`, true));
    });
    editor.append(el('div', { className: 'fsql-edit' }, el('div', { className: 'fnote', textContent: `Preview${expect !== null ? ` (must change ${expect} row, otherwise it is rolled back)` : ''}:` }), area, shown ? el('div', { className: 'fsql-params', textContent: shown }) : '', run, ' ', cancel));
  }

  async #runSql(sql, params, expect) {
    const c = this.current;
    this.sqlBusy = true;
    try {
      const r = await this.rpc('sql', { path: c.path, sql, params, expect });
      this.#message(`run in the guest: ${r.changes} rows changed${r.columns.length ? `, ${r.rows.length} rows read` : ''}`);
      await this.openFile(c.path, { quiet: null });
      return r;
    } finally {
      setTimeout(() => (this.sqlBusy = false), 500);
    }
  }

  /** Saves the content of the viewer into the guest (atomic write). */
  async save() {
    const c = this.current;
    const area = this.els.content.querySelector('textarea.fedit');
    if (!c || !area) return;
    if (c.invalid) throw new Error('there are invalid values in the table');
    let bytes;
    if (c.view === 'hex') bytes = parseHex(area.value);
    else {
      if (c.view === 'json') JSON.parse(area.value);
      if (c.view === 'xml') {
        const err = xmlError(area.value);
        if (err) throw new Error(`invalid XML: ${err}`);
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
      this.#message(`saved in the guest: ${bytes.length} bytes, ${modeString(stat.kind, stat.mode)} ${stat.uid}:${stat.gid}`);
    } finally {
      // The events of our own write arrive shortly after.
      setTimeout(() => (this.saving = false), 500);
    }
  }
}
