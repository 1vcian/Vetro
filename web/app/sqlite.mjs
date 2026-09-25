// Lettore del formato file di SQLite 3 (https://www.sqlite.org/fileformat.html),
// senza dipendenze, per il gestore dei file (M8): tabelle, colonne e righe
// di un database letto dal guest. Solo lettura: le modifiche passano dal
// motore SQLite vero nel guest (richiesta SQL del demone, ADR 0021);
// `sqlQuote` e i costruttori di SQL qui sotto le preparano.
//
// Copre: intestazione (pagine da 512 a 65536 byte, byte riservati,
// codifica UTF-8 / UTF-16), b-tree delle tabelle (pagine interne e foglie),
// payload con pagine di overflow, formato dei record (NULL, interi da 1 a 8
// byte, reali, 0/1, BLOB, testo), la tabella sqlite_schema, gli alias del
// rowid (INTEGER PRIMARY KEY), le tabelle WITHOUT ROWID (b-tree di indice)
// e il WAL (file -wal): come al recupero di SQLite, frame con salt e checksum
// cumulativi validi fino all'ultimo commit, l'ultima versione di ogni pagina
// (ADR 0021; il -shm non serve). Non copre: le pagine libere (non servono
// per leggere).

const MAGIC = 'SQLite format 3\0';

export class SqliteError extends Error {}

/** È un file SQLite (dai primi 16 byte)? */
export function isSqlite(bytes) {
  if (bytes.length < 100) return false;
  for (let i = 0; i < 16; i++) if (bytes[i] !== MAGIC.charCodeAt(i)) return false;
  return true;
}

/** Legge un varint di SQLite da `b` in `at`: [valore (BigInt), byte usati]. */
export function varint(b, at) {
  let v = 0n;
  for (let i = 0; i < 8; i++) {
    const x = b[at + i];
    if (x === undefined) throw new SqliteError('varint oltre la fine della pagina');
    v = (v << 7n) | BigInt(x & 0x7f);
    if (x < 0x80) return [v, i + 1];
  }
  const x = b[at + 8];
  if (x === undefined) throw new SqliteError('varint oltre la fine della pagina');
  return [(v << 8n) | BigInt(x), 9];
}

const num = (v) => (v >= BigInt(Number.MIN_SAFE_INTEGER) && v <= BigInt(Number.MAX_SAFE_INTEGER) ? Number(v) : v);

const WAL_MAGIC = 0x377f0682;

/**
 * Le pagine confermate di un WAL (https://www.sqlite.org/walformat.html):
 * { pageSize, pages: Map(numero → offset nel WAL), dbPages (dimensione del
 * database all'ultimo commit), frames (frame validi) } o null se il WAL è
 * vuoto, di un'altra dimensione di pagina o senza commit validi.
 */
export function walPages(wal, pageSize = 0) {
  if (!wal || wal.length < 32) return null;
  const dv = new DataView(wal.buffer, wal.byteOffset, wal.byteLength);
  const magic = dv.getUint32(0);
  if ((magic & ~1) >>> 0 !== WAL_MAGIC || dv.getUint32(4) !== 3007000) return null;
  const ps = dv.getUint32(8) || 65536;
  if (ps < 512 || ps > 65536 || (ps & (ps - 1)) !== 0 || (pageSize && ps !== pageSize)) return null;
  // Checksum di SQLite su parole a 32 bit, nell'ordine dei byte indicato dal bit 0 della magia.
  const big = (magic & 1) === 1;
  let s0 = 0;
  let s1 = 0;
  const sum = (off, len) => {
    for (let i = off; i < off + len; i += 8) {
      s0 = (s0 + dv.getUint32(i, !big) + s1) >>> 0;
      s1 = (s1 + dv.getUint32(i + 4, !big) + s0) >>> 0;
    }
  };
  sum(0, 24);
  if (s0 !== dv.getUint32(24) || s1 !== dv.getUint32(28)) return null;
  const salt1 = dv.getUint32(16);
  const salt2 = dv.getUint32(20);
  const pages = new Map();
  let pending = [];
  let dbPages = 0;
  let frames = 0;
  for (let off = 32; off + 24 + ps <= wal.length; off += 24 + ps) {
    if (dv.getUint32(off + 8) !== salt1 || dv.getUint32(off + 12) !== salt2) break;
    sum(off, 8);
    sum(off + 24, ps);
    if (s0 !== dv.getUint32(off + 16) || s1 !== dv.getUint32(off + 20)) break;
    const pgno = dv.getUint32(off);
    if (pgno === 0) break;
    pending.push([pgno, off + 24]);
    const commit = dv.getUint32(off + 4);
    if (commit !== 0) {
      // Frame di commit: la transazione (e le precedenti) valgono.
      for (const [n, at] of pending) pages.set(n, at);
      pending = [];
      dbPages = commit;
      frames = (off - 32) / (24 + ps) + 1;
    }
  }
  return frames ? { pageSize: ps, pages, dbPages, frames } : null;
}

/** Il database aperto da un Uint8Array (e dal suo WAL, se c'è). */
export class SqliteDb {
  constructor(bytes, wal = null) {
    // Un database in WAL può avere ancora tutto nel WAL (file principale vuoto).
    const w0 = !isSqlite(bytes) && bytes.length < 100 ? walPages(wal) : null;
    let head = bytes;
    if (w0?.pages.has(1)) head = wal.subarray(w0.pages.get(1), w0.pages.get(1) + w0.pageSize);
    if (!isSqlite(head)) throw new SqliteError('non è un database SQLite 3');
    this.b = bytes;
    const dv = new DataView(head.buffer, head.byteOffset, head.byteLength);
    let ps = dv.getUint16(16);
    if (ps === 1) ps = 65536;
    if (ps < 512 || ps > 65536 || (ps & (ps - 1)) !== 0) throw new SqliteError(`dimensione di pagina ${ps}`);
    this.pageSize = ps;
    this.pageCount = Math.floor(bytes.length / ps);
    this.wal = null;
    const w = walPages(wal, ps);
    if (w) {
      this.wal = { bytes: wal, ...w };
      this.pageCount = w.dbPages;
      if (w.pages.has(1)) head = this.page(1);
    }
    this.usable = ps - head[20];
    const enc = new DataView(head.buffer, head.byteOffset, head.byteLength).getUint32(56);
    this.encoding = enc === 2 ? 'utf-16le' : enc === 3 ? 'utf-16be' : 'utf-8';
    this.text = new TextDecoder(this.encoding);
    this._schema = null;
  }

  /** Frame del WAL applicati (0 senza WAL). */
  get walFrames() {
    return this.wal?.frames ?? 0;
  }

  page(n) {
    if (n < 1 || n > this.pageCount) throw new SqliteError(`pagina ${n} fuori dal file (${this.pageCount} pagine)`);
    const at = this.wal?.pages.get(n);
    if (at !== undefined) return this.wal.bytes.subarray(at, at + this.pageSize);
    const p = this.b.subarray((n - 1) * this.pageSize, n * this.pageSize);
    if (p.length < this.pageSize) throw new SqliteError(`pagina ${n} oltre la fine del file`);
    return p;
  }

  /** Payload di una cella: i byte locali più le pagine di overflow. */
  #payload(page, at, size, isTable) {
    const u = this.usable;
    const x = isTable ? u - 35 : Math.floor(((u - 12) * 64) / 255) - 23;
    if (size <= x) return page.subarray(at, at + size);
    const m = Math.floor(((u - 12) * 32) / 255) - 23;
    const k = m + ((size - m) % (u - 4));
    const local = k <= x ? k : m;
    const out = new Uint8Array(size);
    out.set(page.subarray(at, at + local), 0);
    let filled = local;
    let next = new DataView(page.buffer, page.byteOffset + at + local, 4).getUint32(0);
    let guard = 0;
    while (filled < size) {
      if (next === 0 || guard++ > this.pageCount) throw new SqliteError('catena di overflow interrotta');
      const p = this.page(next);
      next = new DataView(p.buffer, p.byteOffset, 4).getUint32(0);
      const n = Math.min(u - 4, size - filled);
      out.set(p.subarray(4, 4 + n), filled);
      filled += n;
    }
    return out;
  }

  /**
   * Decodifica un record: array di valori (null, Number/BigInt, String,
   * Uint8Array); con `types` vi aggiunge i tipi ('null', 'integer', 'real',
   * 'text', 'blob').
   */
  record(p, types = null) {
    const [hs, n0] = varint(p, 0);
    const headerSize = Number(hs);
    let h = n0;
    let body = headerSize;
    const out = [];
    const dv = new DataView(p.buffer, p.byteOffset, p.byteLength);
    while (h < headerSize) {
      const [t, n] = varint(p, h);
      h += n;
      const st = Number(t);
      types?.push(st === 0 ? 'null' : st <= 6 || st === 8 || st === 9 ? 'integer' : st === 7 ? 'real' : st % 2 === 0 ? 'blob' : 'text');
      if (st === 0) out.push(null);
      else if (st >= 1 && st <= 6) {
        const len = [0, 1, 2, 3, 4, 6, 8][st];
        let v = 0n;
        for (let i = 0; i < len; i++) v = (v << 8n) | BigInt(p[body + i]);
        v = BigInt.asIntN(len * 8, v);
        out.push(num(v));
        body += len;
      } else if (st === 7) {
        out.push(dv.getFloat64(body));
        body += 8;
      } else if (st === 8 || st === 9) out.push(st - 8);
      else if (st >= 12) {
        const len = st % 2 === 0 ? (st - 12) / 2 : (st - 13) / 2;
        const v = p.subarray(body, body + len);
        out.push(st % 2 === 0 ? v.slice() : this.text.decode(v));
        body += len;
      } else throw new SqliteError(`tipo seriale ${st} riservato`);
    }
    return out;
  }

  /** Righe di un b-tree di tabella: [{ rowid, values, types }] in ordine di rowid. */
  tableRows(root, limit = Infinity) {
    const rows = [];
    const walk = (n, depth) => {
      if (depth > 64) throw new SqliteError('b-tree troppo profondo (ciclo?)');
      const page = this.page(n);
      const base = n === 1 ? 100 : 0;
      const type = page[base];
      const dv = new DataView(page.buffer, page.byteOffset, page.byteLength);
      const cells = dv.getUint16(base + 3);
      const hdr = type === 0x05 || type === 0x02 ? 12 : 8;
      if (type === 0x05) {
        for (let i = 0; i < cells && rows.length < limit; i++) {
          walk(dv.getUint32(dv.getUint16(base + hdr + 2 * i)), depth + 1);
        }
        if (rows.length < limit) walk(dv.getUint32(base + 8), depth + 1);
      } else if (type === 0x0d) {
        for (let i = 0; i < cells && rows.length < limit; i++) {
          let at = dv.getUint16(base + hdr + 2 * i);
          const [size, a] = varint(page, at);
          at += a;
          const [rowid, b] = varint(page, at);
          at += b;
          const types = [];
          rows.push({ rowid: num(rowid), values: this.record(this.#payload(page, at, Number(size), true), types), types });
        }
      } else throw new SqliteError(`pagina ${n} di tipo ${type}, attesa una pagina di tabella`);
    };
    walk(root, 0);
    return rows;
  }

  /** Record di un b-tree di indice (tabelle WITHOUT ROWID), in ordine; i tipi in `types`. */
  indexRecords(root, limit = Infinity, types = null) {
    const out = [];
    const walk = (n, depth) => {
      if (depth > 64) throw new SqliteError('b-tree troppo profondo (ciclo?)');
      const page = this.page(n);
      const base = n === 1 ? 100 : 0;
      const type = page[base];
      const dv = new DataView(page.buffer, page.byteOffset, page.byteLength);
      const cells = dv.getUint16(base + 3);
      const interior = type === 0x02;
      if (type !== 0x02 && type !== 0x0a) throw new SqliteError(`pagina ${n} di tipo ${type}, attesa una pagina di indice`);
      const hdr = interior ? 12 : 8;
      for (let i = 0; i < cells && out.length < limit; i++) {
        let at = dv.getUint16(base + hdr + 2 * i);
        if (interior) {
          walk(dv.getUint32(at), depth + 1);
          at += 4;
        }
        if (out.length >= limit) break;
        const [size, a] = varint(page, at);
        const t = [];
        out.push(this.record(this.#payload(page, at + a, Number(size), false), t));
        types?.push(t);
      }
      if (interior && out.length < limit) walk(dv.getUint32(base + 8), depth + 1);
    };
    walk(root, 0);
    return out;
  }

  /** Voci di sqlite_schema: [{ type, name, table, root, sql }]. */
  schema() {
    this._schema ??= this.tableRows(1).map(({ values: [type, name, table, root, sql] }) => ({ type, name, table, root, sql }));
    return this._schema;
  }

  /** Le tabelle: [{ name, columns, withoutRowid, root, sql }]. */
  tables() {
    return this.schema()
      .filter((e) => e.type === 'table')
      .map((e) => ({ name: e.name, root: e.root, sql: e.sql, ...parseCreateTable(e.sql ?? '') }));
  }

  /**
   * Righe della tabella `name` (al più `limit`): { columns, rows, rowids,
   * types, table } con `rows` array di array nell'ordine delle colonne
   * (rowid in testa se la tabella ne ha uno senza alias), `rowids` il rowid
   * di ogni riga (null per WITHOUT ROWID), `types` i tipi dei valori (come
   * `record`) e `table` la voce di `tables()`.
   */
  rows(name, limit = 1000) {
    const t = this.tables().find((x) => x.name === name);
    if (!t) throw new SqliteError(`tabella ${name} non trovata`);
    if (t.withoutRowid) {
      const types = [];
      const recs = this.indexRecords(t.root, limit, types);
      return {
        columns: t.columns,
        rows: recs.map((r) => orderWithoutRowid(t, r, null)),
        rowids: recs.map(() => null),
        types: types.map((ty) => orderWithoutRowid(t, ty, 'null')),
        table: t,
      };
    }
    const alias = t.rowidAlias;
    const all = this.tableRows(t.root, limit);
    const rows = all.map(({ rowid, values }) => {
      const v = t.columns.map((_, i) => (i < values.length ? values[i] : null));
      if (alias >= 0 && v[alias] === null) v[alias] = rowid;
      return alias >= 0 ? v : [rowid, ...v];
    });
    const types = all.map(({ types: ty, values }) => {
      const v = t.columns.map((_, i) => (i < values.length ? ty[i] : 'null'));
      if (alias >= 0 && v[alias] === 'null') v[alias] = 'integer';
      return alias >= 0 ? v : ['integer', ...v];
    });
    return { columns: alias >= 0 ? t.columns : ['rowid', ...t.columns], rows, rowids: all.map((r) => r.rowid), types, table: t };
  }
}

/** Un identificatore SQL fra virgolette doppie. */
export function sqlQuote(name) {
  return `"${String(name).replace(/"/g, '""')}"`;
}

/**
 * Il WHERE che identifica una riga: `rowid = ?N` se la tabella ha un rowid,
 * altrimenti le colonne della chiave primaria (tutte le colonne se non la
 * trova) con `IS ?N`. `row` sono i valori di `rows()` per quella riga.
 * Restituisce { where, params } a partire dal parametro `first`.
 */
function whereRow(t, row, rowid, first) {
  if (!t.withoutRowid) return { where: `rowid = ?${first}`, params: [rowid] };
  const cols = t.pk.length ? t.pk : t.columns.map((_, i) => i);
  return {
    where: cols.map((c, k) => `${sqlQuote(t.columns[c])} IS ?${first + k}`).join(' AND '),
    params: cols.map((c) => row[c]),
  };
}

/** Colonna vera della tabella per l'indice `i` di `rows().columns` (-1 = rowid senza alias). */
function tableColumn(t, i) {
  return t.withoutRowid || t.rowidAlias >= 0 ? i : i - 1;
}

/**
 * SQL per cambiare una cella: { sql, params } con il nuovo valore in ?1
 * (`value`: parametro di GuestFiles.sql, anche { type, value }).
 * `view` è il risultato di `rows()`, `r` e `i` riga e colonna della vista.
 */
export function updateCellSql(view, r, i, value) {
  const t = view.table;
  const col = tableColumn(t, i);
  if (col < 0) throw new SqliteError('il rowid di una tabella senza alias non si cambia da qui');
  const w = whereRow(t, t.withoutRowid ? view.rows[r] : null, view.rowids[r], 2);
  return { sql: `UPDATE ${sqlQuote(t.name)} SET ${sqlQuote(t.columns[col])} = ?1 WHERE ${w.where}`, params: [value, ...w.params] };
}

/** SQL per togliere la riga `r` della vista. */
export function deleteRowSql(view, r) {
  const t = view.table;
  const w = whereRow(t, t.withoutRowid ? view.rows[r] : null, view.rowids[r], 1);
  return { sql: `DELETE FROM ${sqlQuote(t.name)} WHERE ${w.where}`, params: w.params };
}

/**
 * SQL per inserire una riga: `values` mappa il nome di colonna al valore
 * (le colonne che mancano prendono il loro DEFAULT).
 */
export function insertRowSql(table, values) {
  const cols = Object.keys(values);
  if (!cols.length) return { sql: `INSERT INTO ${sqlQuote(table.name)} DEFAULT VALUES`, params: [] };
  return {
    sql: `INSERT INTO ${sqlQuote(table.name)} (${cols.map(sqlQuote).join(', ')}) VALUES (${cols.map((_, k) => `?${k + 1}`).join(', ')})`,
    params: cols.map((c) => values[c]),
  };
}

/** Le colonne di un record di una tabella WITHOUT ROWID: prima la chiave, poi le altre. */
function orderWithoutRowid(t, rec, empty = null) {
  const order = [...t.pk, ...t.columns.map((_, i) => i).filter((i) => !t.pk.includes(i))];
  const v = new Array(t.columns.length).fill(empty);
  order.forEach((col, k) => {
    if (k < rec.length) v[col] = rec[k];
  });
  return v;
}

function unquote(s) {
  s = s.trim();
  if (/^(["`[]).*(["`\]])$/s.test(s)) return s.slice(1, -1).replace(/""/g, '"');
  if (/^'.*'$/s.test(s)) return s.slice(1, -1).replace(/''/g, "'");
  return s;
}

/** Divide `s` sulle virgole di primo livello (fuori da parentesi e virgolette). */
function splitTop(s) {
  const out = [];
  let depth = 0;
  let quote = null;
  let cur = '';
  for (const ch of s) {
    if (quote) {
      cur += ch;
      if (ch === quote || (quote === '[' && ch === ']')) quote = null;
      continue;
    }
    if (ch === '"' || ch === "'" || ch === '`' || ch === '[') quote = ch;
    else if (ch === '(') depth++;
    else if (ch === ')') depth--;
    else if (ch === ',' && depth === 0) {
      out.push(cur);
      cur = '';
      continue;
    }
    cur += ch;
  }
  if (cur.trim()) out.push(cur);
  return out;
}

/** Prima parola (nome) di una definizione di colonna, virgolette comprese. */
function firstToken(s) {
  s = s.trim();
  const m = /^("(?:[^"]|"")*"|`[^`]*`|\[[^\]]*\]|'(?:[^']|'')*'|\S+)/s.exec(s);
  return m ? m[1] : '';
}

/**
 * Colonne di un CREATE TABLE: { columns, rowidAlias (indice o -1),
 * withoutRowid, pk (indici della chiave primaria) }.
 */
export function parseCreateTable(sql) {
  const open = sql.indexOf('(');
  const close = sql.lastIndexOf(')');
  const res = { columns: [], rowidAlias: -1, withoutRowid: false, pk: [] };
  if (open < 0 || close < open) return res;
  res.withoutRowid = /\bWITHOUT\s+ROWID\b/i.test(sql.slice(close));
  const constraints = [];
  for (const def of splitTop(sql.slice(open + 1, close))) {
    const d = def.trim();
    if (/^(CONSTRAINT|PRIMARY\s+KEY|UNIQUE|CHECK|FOREIGN\s+KEY)\b/i.test(d)) {
      constraints.push(d);
      continue;
    }
    const tok = firstToken(d);
    const name = unquote(tok);
    const rest = d.slice(tok.length);
    const i = res.columns.length;
    res.columns.push(name);
    if (/\bPRIMARY\s+KEY\b/i.test(rest)) {
      res.pk = [i];
      // Solo "INTEGER" esatto rende la colonna un alias del rowid.
      if (/^\s*INTEGER\s+PRIMARY\s+KEY\b/i.test(rest) && !/\bDESC\b/i.test(rest)) res.rowidAlias = i;
    }
  }
  for (const c of constraints) {
    const m = /PRIMARY\s+KEY\s*\(([^)]*)\)/i.exec(c);
    if (!m) continue;
    const cols = splitTop(m[1]).map((x) => unquote(firstToken(x)));
    res.pk = cols.map((n) => res.columns.findIndex((c2) => c2.toLowerCase() === n.toLowerCase())).filter((i) => i >= 0);
    if (res.pk.length === 1) {
      const def = splitTop(sql.slice(open + 1, close)).map((x) => x.trim()).find((x) => unquote(firstToken(x)).toLowerCase() === cols[0].toLowerCase());
      if (def && /^\S+\s+INTEGER\b/i.test(def)) res.rowidAlias = res.pk[0];
    }
  }
  if (res.withoutRowid) res.rowidAlias = -1;
  return res;
}

/** Un valore per la tabella della pagina. */
export function formatValue(v) {
  if (v === null) return 'NULL';
  if (v instanceof Uint8Array) {
    const hex = Array.from(v.subarray(0, 32), (x) => x.toString(16).padStart(2, '0')).join('');
    return `x'${hex}${v.length > 32 ? '…' : ''}' (${v.length} byte)`;
  }
  return String(v);
}
