// Il pannello del gestore dei file (M8, ADR 0020): albero delle radici
// impostate dal chiamante, aggiornato dal vivo con gli eventi di inotify del
// demone `vetro-files` nel guest, e visualizzatori (testo, JSON, XML,
// esadecimale, immagini, SQLite) con modifica e salvataggio immediato nel
// guest (testo, JSON, XML, esadecimale; SharedPreferences in una tabella
// che riscrive l'XML come Android; righe SQLite con SQL eseguito nel guest
// dal motore vero, ADR 0021).
//
// I percorsi sono stringhe in surrogateescape (ADR 0021): un byte non UTF-8
// del nome è un surrogato solitario, mostrato come \xNN.
//
// Il pannello non parla con la macchina: chiede operazioni con `rpc(op,
// args)` (una Promise: il Worker le passa a `GuestFiles` di vetro.mjs) e
// riceve eventi e stato con `onEvent` e `onStatus`. Le funzioni pure
// (riconoscimento del tipo, esadecimale, modi) sono esportate per i test.

import { deleteRowSql, formatValue, insertRowSql, isSqlite, SqliteDb, sqlQuote, updateCellSql } from './sqlite.mjs';

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

/** Un nome del guest con i byte non UTF-8 (surrogati solitari, ADR 0021) mostrati come \xNN. */
export function displayName(s) {
  return s.replace(/[\udc80-\udcff]/g, (c, i) => {
    const prev = i > 0 ? s.charCodeAt(i - 1) : 0;
    return prev >= 0xd800 && prev <= 0xdbff ? c : `\\x${(c.charCodeAt(0) - 0xdc00).toString(16).padStart(2, '0')}`;
  });
}

// ---- SharedPreferences (ADR 0021) ---------------------------------------------

const ENTITIES = { lt: '<', gt: '>', amp: '&', quot: '"', apos: "'" };

/**
 * Lettore XML piccolo (senza DOM, anche in Node) per i file delle
 * SharedPreferences: elementi, attributi, testo, entità, CDATA, commenti,
 * istruzioni di elaborazione. Restituisce la radice { name, attrs, children,
 * text }; lancia con la posizione se l'XML non è ben formato.
 */
export function parseXml(xml) {
  let i = 0;
  const fail = (m) => {
    throw new Error(`XML non valido: ${m} (carattere ${i})`);
  };
  const decode = (s) =>
    s.replace(/&([^;&\s]*);|&/g, (m, e) => {
      if (e === undefined) fail('& senza entità');
      if (e in ENTITIES) return ENTITIES[e];
      const n = /^#x([0-9a-fA-F]+)$/.exec(e) ? parseInt(e.slice(2), 16) : /^#([0-9]+)$/.exec(e) ? parseInt(e.slice(1), 10) : NaN;
      if (!(n >= 0 && n <= 0x10ffff)) fail(`entità &${e};`);
      return String.fromCodePoint(n);
    });
  const skip = () => {
    for (;;) {
      while (i < xml.length && /\s/.test(xml[i])) i++;
      if (xml.startsWith('<!--', i)) {
        const e = xml.indexOf('-->', i + 4);
        if (e < 0) fail('commento non chiuso');
        i = e + 3;
      } else if (xml.startsWith('<?', i)) {
        const e = xml.indexOf('?>', i + 2);
        if (e < 0) fail('istruzione non chiusa');
        i = e + 2;
      } else if (xml.startsWith('<!DOCTYPE', i)) {
        const e = xml.indexOf('>', i);
        if (e < 0) fail('DOCTYPE non chiuso');
        i = e + 1;
      } else return;
    }
  };
  const NAME = /[A-Za-z_:][\w.:-]*/y;
  const name = () => {
    NAME.lastIndex = i;
    const m = NAME.exec(xml);
    if (!m) fail('nome atteso');
    i += m[0].length;
    return m[0];
  };
  const element = () => {
    if (xml[i] !== '<') fail('< atteso');
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
      if (i === before) fail('spazio atteso fra gli attributi');
      const a = name();
      while (/\s/.test(xml[i] ?? '')) i++;
      if (xml[i] !== '=') fail('= atteso');
      i++;
      while (/\s/.test(xml[i] ?? '')) i++;
      const q = xml[i];
      if (q !== '"' && q !== "'") fail('virgolette attese');
      const e = xml.indexOf(q, i + 1);
      if (e < 0) fail('attributo non chiuso');
      if (a in node.attrs) fail(`attributo ${a} ripetuto`);
      const raw = xml.slice(i + 1, e);
      if (raw.includes('<')) fail('< in un attributo');
      node.attrs[a] = decode(raw);
      i = e + 1;
    }
    for (;;) {
      if (i >= xml.length) fail(`<${node.name}> non chiuso`);
      if (xml.startsWith('</', i)) {
        i += 2;
        if (name() !== node.name) fail(`chiusura diversa da <${node.name}>`);
        while (/\s/.test(xml[i] ?? '')) i++;
        if (xml[i] !== '>') fail('> atteso');
        i++;
        return node;
      }
      if (xml.startsWith('<!--', i)) {
        const e = xml.indexOf('-->', i + 4);
        if (e < 0) fail('commento non chiuso');
        i = e + 3;
      } else if (xml.startsWith('<![CDATA[', i)) {
        const e = xml.indexOf(']]>', i + 9);
        if (e < 0) fail('CDATA non chiuso');
        node.text += xml.slice(i + 9, e);
        i = e + 3;
      } else if (xml.startsWith('<?', i)) {
        const e = xml.indexOf('?>', i + 2);
        if (e < 0) fail('istruzione non chiusa');
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
  if (i < xml.length) fail('testo dopo la radice');
  return root;
}

/** Tipi delle SharedPreferences. */
export const PREF_TYPES = ['string', 'int', 'long', 'float', 'boolean', 'set', 'null'];

/**
 * Voci di un file XML delle SharedPreferences: [{ type, name, value }] con
 * `value` stringa (per `set` array di stringhe, per `null` null), o null se
 * la radice non è <map> o c'è un tipo che la tabella non gestisce. Lancia
 * se l'XML non è ben formato.
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

/** Come ESCAPE_TABLE di FastXmlSerializer: caratteri di controllo come &#N;, poi " & < >. */
function prefEscape(s) {
  return s.replace(/[\u0000-\u001f"&<>]/g, (c) =>
    c === '"' ? '&quot;' : c === '&' ? '&amp;' : c === '<' ? '&lt;' : c === '>' ? '&gt;' : `&#${c.charCodeAt(0)};`,
  );
}

/**
 * L'XML delle SharedPreferences come lo scrive Android (XmlUtils.writeMapXml
 * con FastXmlSerializer e rientro): stessa intestazione, rientro di 4 spazi,
 * tag vuoti come ` />`, stessi caratteri protetti, `\n` dopo ogni tag di
 * chiusura. Come FastXmlSerializer, un testo che finisce con `\n` fa
 * rientrare il tag di chiusura.
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

/** `Float.toString` di Java per un float a 32 bit. */
export function javaFloatString(x) {
  const f = Math.fround(x);
  if (Number.isNaN(f)) return 'NaN';
  if (f === Infinity) return 'Infinity';
  if (f === -Infinity) return '-Infinity';
  if (f === 0) return Object.is(f, -0) ? '-0.0' : '0.0';
  // Le cifre più corte che ridanno lo stesso float.
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
 * Controlla un valore come lo rilegge Android (Integer.parseInt,
 * Long.parseLong, Float.parseFloat, true/false) e lo restituisce nella
 * forma che Android scriverebbe; lancia se non è valido.
 */
export function checkPrefValue(type, text) {
  const s = String(text);
  if (type === 'int' || type === 'long') {
    if (!/^[+-]?\d+$/.test(s)) throw new Error(`"${s}" non è un intero`);
    const v = BigInt(s);
    const bits = type === 'int' ? 32 : 64;
    if (BigInt.asIntN(bits, v) !== v) throw new Error(`${s} fuori dall'intervallo di un ${type}`);
    return v.toString();
  }
  if (type === 'float') {
    const t = s.trim().replace(/[fFdD]$/, '');
    if (!/^[+-]?(NaN|Infinity|(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?)$/.test(t)) throw new Error(`"${s}" non è un float`);
    return javaFloatString(Number(t));
  }
  if (type === 'boolean') {
    if (s !== 'true' && s !== 'false') throw new Error(`"${s}" non è true o false`);
    return s;
  }
  return s;
}

/** Errore di sintassi XML (testo) o null. */
function xmlError(text) {
  if (typeof DOMParser === 'undefined') return null;
  const doc = new DOMParser().parseFromString(text, 'application/xml');
  const err = doc.querySelector('parsererror');
  return err ? err.textContent.split('\n')[0] : null;
}

const join = (dir, name) => (dir.endsWith('/') ? dir + name : `${dir}/${name}`);
const base = (path) => path.split('/').pop();

/** Un parametro SQL come testo per l'anteprima. */
function paramString(p) {
  const v = p !== null && typeof p === 'object' && !(p instanceof Uint8Array) ? p.value : p;
  if (v === null || v === undefined) return 'NULL';
  if (v instanceof Uint8Array) return formatValue(v);
  if (typeof v === 'string') return `'${v.replace(/'/g, "''")}'`;
  return String(v);
}

/** Tabella del risultato di una query. */
function resultTable(r) {
  const table = el('table', { className: 'ftable fsql-result' });
  table.append(el('tr', {}, ...r.columns.map((c) => el('th', { textContent: c }))));
  for (const row of r.rows) table.append(el('tr', {}, ...row.map((v) => el('td', { textContent: formatValue(v), className: v === null ? 'fnull' : '' }))));
  return el('div', {}, el('div', { className: 'fnote', textContent: `${r.rows.length} righe${r.truncated ? ' (troncate)' : ''}` }), table);
}
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
    /** Cartelle di cui si aspetta la prima lista (nell'albero: "…"). */
    this.loading = new Set();
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
    this.loading.clear();
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
    // Un database: cambia il file o il suo -wal (SQLite scrive senza chiudere).
    if (cur?.view === 'sqlite' && ev.name && (join(dir, ev.name) === cur.path || join(dir, ev.name) === `${cur.path}-wal`)) {
      if (!this.sqlBusy && !this.els.content.querySelector('.fsql-edit')) {
        clearTimeout(this.sqlTimer);
        this.sqlTimer = setTimeout(() => this.openFile(cur.path, { quiet: true }).catch(() => {}), 150);
      }
      return;
    }
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
      const isLoading = !isOpen && this.loading.has(path);
      const row = el('div', { className: `fnode${this.current?.path === path ? ' sel' : ''}` });
      row.dataset.path = path;
      row.style.paddingLeft = `${depth * 14 + 4}px`;
      row.append(el('span', { className: 'twisty', textContent: isDir ? (isOpen || isLoading ? '▾' : '▸') : ' ' }));
      row.append(el('span', { className: `fname ${stat?.kind ?? 'dir'}`, textContent: displayName(name) }));
      if (stat) {
        row.append(el('span', { className: 'fmeta', textContent: `${modeString(stat.kind, stat.mode)} ${stat.uid}:${stat.gid}${isDir ? '' : ` ${sizeString(stat.size)}`}` }));
        row.title = `${displayName(path)}\n${modeString(stat.kind, stat.mode)} uid ${stat.uid} gid ${stat.gid}, ${stat.size} byte, ` +
          `modificato ${new Date(stat.mtime * 1000).toISOString()}${stat.link ? `\n→ ${stat.link}` : ''}${stat.selinux ? `\nSELinux: ${stat.selinux}` : ''}`;
      }
      row.addEventListener('click', () => {
        if (isDir) (isOpen ? this.collapse(path) : this.expand(path)).catch((e) => this.#message(`${path}: ${e.message}`, true));
        else this.openFile(path).catch((e) => this.#message(`${path}: ${e.message}`, true));
      });
      tree.append(row);
      // Righe senza data-path: non sono voci (vetroFiles.state().shown).
      const note = (text) => {
        const n = el('div', { className: 'fempty', textContent: text });
        n.style.paddingLeft = `${(depth + 1) * 14 + 4}px`;
        tree.append(n);
      };
      if (isLoading) note('…');
      if (isOpen) {
        const d = this.open.get(path);
        if (d.error) tree.append(el('div', { className: 'ferr', textContent: d.error }));
        // Prima le cartelle, poi i file, in ordine di nome.
        const dirFirst = (e) => (e.stat.kind === 'dir' ? 0 : 1);
        const sorted = [...d.entries].sort((a, b) => dirFirst(a) - dirFirst(b) || (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
        for (const e of sorted) node(join(path, e.name), e.name, e.stat, depth + 1);
        if (!sorted.length && !d.error) note('(vuota)');
      }
    };
    for (const r of this.roots) node(r, r, null, 0);
  }

  #message(text, error = false) {
    this.els.message.textContent = text;
    this.els.message.className = error ? 'err' : '';
  }

  /**
   * Apre un file nel visualizzatore. `quiet`: false = messaggio vuoto, true
   * = "ricaricato", null = il messaggio resta. Un database SQLite si legge
   * con il suo -wal, se c'è (ADR 0021).
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
    else if (quiet) this.#message('ricaricato: il file è cambiato nel guest');
  }

  #info() {
    const c = this.current;
    const s = c.stat;
    this.els.path.textContent = displayName(c.path);
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
      this.els.save.disabled = !!c.invalid;
      this.#validate(area.value);
    });
    box.append(area);
    c.invalid = false;
    if (view === 'xml' && editable) this.#renderPrefs(box, text, area);
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
    const note = el('div', { className: 'fnote', textContent: 'SharedPreferences: modifica nella tabella (riscrive l\'XML come Android) o nel testo qui sopra; poi "Salva"' });
    const table = el('table', { className: 'ftable' });
    const errors = new Set();
    // La tabella riscrive il testo; il testo, quando cambia, ridisegna la tabella.
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
      table.append(el('tr', {}, el('th', { textContent: 'tipo' }), el('th', { textContent: 'nome' }), el('th', { textContent: 'valore' }), el('th')));
      prefs.forEach((p, k) => {
        const type = el('select', { className: 'fpref-type' });
        for (const t of PREF_TYPES) type.append(el('option', { value: t, textContent: t, selected: t === p.type }));
        const name = el('input', { className: 'fpref-name', value: p.name, spellcheck: false });
        const value = p.type === 'set'
          ? el('textarea', { className: 'fpref-value', value: p.value.join('\n'), rows: Math.max(2, p.value.length), spellcheck: false, title: 'un elemento per riga' })
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
        const del = el('button', { type: 'button', className: 'fsmall fpref-del', textContent: '✕', title: 'Togli la voce' });
        del.addEventListener('click', () => {
          prefs.splice(k, 1);
          errors.clear();
          draw();
          sync();
        });
        table.append(el('tr', {}, el('td', {}, type), el('td', {}, name), el('td', {}, value), el('td', {}, del)));
      });
    };
    const add = el('button', { type: 'button', className: 'fsmall fpref-add', textContent: 'Aggiungi voce' });
    add.addEventListener('click', () => {
      let n = 1;
      while (prefs.some((p) => p.name === `nuova${n}`)) n++;
      prefs.push({ type: 'string', name: `nuova${n}`, value: '' });
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
        // XML non valido: il messaggio lo dà già la validazione del testo.
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
    const insert = el('button', { type: 'button', className: 'fsmall fsql-insert', textContent: 'Inserisci riga' });
    const free = el('button', { type: 'button', className: 'fsmall fsql-free', textContent: 'SQL…' });
    // Editor (valore di una cella o riga nuova) e anteprima della query.
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
            const td = el('td', { textContent: formatValue(v), className: v === null ? 'fnull' : '', title: `${view.types[ri][ci]} · clic per modificare` });
            td.dataset.r = ri;
            td.dataset.c = ci;
            td.addEventListener('click', () => this.#editCell(editor, view, ri, ci));
            tr.append(td);
          });
          const del = el('button', { type: 'button', className: 'fsmall fsql-delete', textContent: '✕', title: 'Togli la riga' });
          del.addEventListener('click', () => this.#sqlPreview(editor, deleteRowSql(view, ri), 1));
          tr.append(el('td', {}, del));
          table.append(tr);
        });
        const wal = db.walFrames ? ` · WAL: ${db.walFrames} frame applicati` : '';
        out.append(el('div', { className: 'fnote', textContent: `${view.rows.length} righe${view.rows.length === MAX_ROWS ? ' (le prime)' : ''} · pagine da ${db.pageSize} byte${wal} · clic su una cella per modificarla (SQL nel guest)` }), table);
      } catch (e) {
        view = null;
        out.append(el('div', { className: 'ferr', textContent: e.message }));
      }
    };
    pick.addEventListener('change', show);
    insert.addEventListener('click', () => view && this.#insertRow(editor, view.table));
    free.addEventListener('click', () => this.#sqlPreview(editor, { sql: view ? `SELECT * FROM ${sqlQuote(view.table.name)} LIMIT 10` : '', params: [] }, null));
    box.append(el('div', {}, `${tables.length} tabelle: `, pick, ' ', insert, ' ', free), editor, out);
    show();
  }

  /** Editor di un valore: tipo e testo. Restituisce { box, value() } (value lancia se non valido). */
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
          if (!/^\s*[+-]?\d+\s*$/.test(s)) throw new Error(`"${s}" non è un intero`);
          if (BigInt.asIntN(64, BigInt(s.trim())) !== BigInt(s.trim())) throw new Error(`${s} non sta in 64 bit`);
          return { type: 'integer', value: BigInt(s.trim()) };
        case 'real':
          if (s.trim() === '' || Number.isNaN(Number(s))) throw new Error(`"${s}" non è un numero`);
          return { type: 'real', value: Number(s) };
        case 'blob': {
          const h = s.replace(/\s+/g, '');
          if (!/^([0-9a-fA-F]{2})*$/.test(h)) throw new Error('BLOB: cifre esadecimali a coppie');
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
      this.#message('il rowid di una tabella senza INTEGER PRIMARY KEY non si cambia da qui', true);
      return;
    }
    const ed = this.#valueEditor(view.rows[r][i], view.types[r][i]);
    const go = el('button', { type: 'button', className: 'fsmall fsql-preview', textContent: 'Anteprima' });
    const cancel = el('button', { type: 'button', className: 'fsmall', textContent: 'Annulla' });
    const who = t.withoutRowid ? `riga ${r + 1}` : `rowid ${view.rowids[r]}`;
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
    const go = el('button', { type: 'button', className: 'fsmall fsql-preview', textContent: 'Anteprima' });
    const cancel = el('button', { type: 'button', className: 'fsmall', textContent: 'Annulla' });
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
    editor.append(el('div', { className: 'fsql-edit' }, `Riga nuova in ${table.name}`, ...rows, go, ' ', cancel));
  }

  /**
   * Anteprima della query (modificabile) con i parametri, poi "Esegui":
   * richiesta SQL al demone, eseguita nel guest da SQLite come il
   * proprietario del database (ADR 0021). `expect`: righe che devono
   * cambiare (null = qualsiasi; se il testo cambia non vale più).
   */
  #sqlPreview(editor, { sql, params }, expect) {
    editor.textContent = '';
    const area = el('textarea', { className: 'fsql-query', value: sql, spellcheck: false, rows: 3 });
    const shown = params.map((p, k) => `?${k + 1} = ${paramString(p)}`).join('   ');
    const run = el('button', { type: 'button', className: 'fsmall fsql-run', textContent: 'Esegui nel guest' });
    const cancel = el('button', { type: 'button', className: 'fsmall', textContent: 'Annulla' });
    cancel.addEventListener('click', () => (editor.textContent = ''));
    run.addEventListener('click', () => {
      const text = area.value;
      const changed = text !== sql;
      this.#runSql(text, params, changed ? null : expect)
        .then((r) => {
          editor.textContent = '';
          if (r.columns.length) editor.append(resultTable(r));
        })
        .catch((e) => this.#message(`non eseguito: ${e.message}`, true));
    });
    editor.append(el('div', { className: 'fsql-edit' }, el('div', { className: 'fnote', textContent: `Anteprima${expect !== null ? ` (deve cambiare ${expect} riga, altrimenti si annulla)` : ''}:` }), area, shown ? el('div', { className: 'fsql-params', textContent: shown }) : '', run, ' ', cancel));
  }

  async #runSql(sql, params, expect) {
    const c = this.current;
    this.sqlBusy = true;
    try {
      const r = await this.rpc('sql', { path: c.path, sql, params, expect });
      this.#message(`eseguito nel guest: ${r.changes} righe cambiate${r.columns.length ? `, ${r.rows.length} righe lette` : ''}`);
      await this.openFile(c.path, { quiet: null });
      return r;
    } finally {
      setTimeout(() => (this.sqlBusy = false), 500);
    }
  }

  /** Salva il contenuto del visualizzatore nel guest (scrittura atomica). */
  async save() {
    const c = this.current;
    const area = this.els.content.querySelector('textarea.fedit');
    if (!c || !area) return;
    if (c.invalid) throw new Error('ci sono valori non validi nella tabella');
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
