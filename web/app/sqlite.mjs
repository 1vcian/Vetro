// Reader of the SQLite 3 file format (https://www.sqlite.org/fileformat.html),
// with no dependencies, for the file manager (M8): tables, columns and rows
// of a database read from the guest. Read-only: changes go through the
// real SQLite engine in the guest (SQL request of the daemon, ADR 0021);
// `sqlQuote` and the SQL builders below prepare them.
//
// Covers: header (pages from 512 to 65536 bytes, reserved bytes,
// UTF-8 / UTF-16 encoding), table b-trees (interior and leaf pages),
// payload with overflow pages, record format (NULL, integers from 1 to 8
// bytes, reals, 0/1, BLOB, text), the sqlite_schema table, rowid aliases
// (INTEGER PRIMARY KEY), WITHOUT ROWID tables (index b-trees) and the WAL
// (-wal file): as in SQLite's recovery, frames with valid cumulative salt
// and checksum up to the last commit, the latest version of each page
// (ADR 0021; the -shm is not needed). Does not cover: free pages (not
// needed for reading).

const MAGIC = 'SQLite format 3\0';

export class SqliteError extends Error {}

/** Is it an SQLite file (from the first 16 bytes)? */
export function isSqlite(bytes) {
  if (bytes.length < 100) return false;
  for (let i = 0; i < 16; i++) if (bytes[i] !== MAGIC.charCodeAt(i)) return false;
  return true;
}

/** Reads an SQLite varint from `b` at `at`: [value (BigInt), bytes used]. */
export function varint(b, at) {
  let v = 0n;
  for (let i = 0; i < 8; i++) {
    const x = b[at + i];
    if (x === undefined) throw new SqliteError('varint past the end of the page');
    v = (v << 7n) | BigInt(x & 0x7f);
    if (x < 0x80) return [v, i + 1];
  }
  const x = b[at + 8];
  if (x === undefined) throw new SqliteError('varint past the end of the page');
  return [(v << 8n) | BigInt(x), 9];
}

const num = (v) => (v >= BigInt(Number.MIN_SAFE_INTEGER) && v <= BigInt(Number.MAX_SAFE_INTEGER) ? Number(v) : v);

const WAL_MAGIC = 0x377f0682;

/**
 * The committed pages of a WAL (https://www.sqlite.org/walformat.html):
 * { pageSize, pages: Map(number → offset in the WAL), dbPages (size of the
 * database at the last commit), frames (valid frames) } or null if the WAL
 * is empty, of another page size or without valid commits.
 */
export function walPages(wal, pageSize = 0) {
  if (!wal || wal.length < 32) return null;
  const dv = new DataView(wal.buffer, wal.byteOffset, wal.byteLength);
  const magic = dv.getUint32(0);
  if ((magic & ~1) >>> 0 !== WAL_MAGIC || dv.getUint32(4) !== 3007000) return null;
  const ps = dv.getUint32(8) || 65536;
  if (ps < 512 || ps > 65536 || (ps & (ps - 1)) !== 0 || (pageSize && ps !== pageSize)) return null;
  // SQLite checksum over 32-bit words, in the byte order given by bit 0 of the magic.
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
      // Commit frame: the transaction (and the previous ones) count.
      for (const [n, at] of pending) pages.set(n, at);
      pending = [];
      dbPages = commit;
      frames = (off - 32) / (24 + ps) + 1;
    }
  }
  return frames ? { pageSize: ps, pages, dbPages, frames } : null;
}

/** The database opened from a Uint8Array (and from its WAL, if there is one). */
export class SqliteDb {
  constructor(bytes, wal = null) {
    // A database in WAL mode can still have everything in the WAL (empty main file).
    const w0 = !isSqlite(bytes) && bytes.length < 100 ? walPages(wal) : null;
    let head = bytes;
    if (w0?.pages.has(1)) head = wal.subarray(w0.pages.get(1), w0.pages.get(1) + w0.pageSize);
    if (!isSqlite(head)) throw new SqliteError('not an SQLite 3 database');
    this.b = bytes;
    const dv = new DataView(head.buffer, head.byteOffset, head.byteLength);
    let ps = dv.getUint16(16);
    if (ps === 1) ps = 65536;
    if (ps < 512 || ps > 65536 || (ps & (ps - 1)) !== 0) throw new SqliteError(`page size ${ps}`);
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

  /** WAL frames applied (0 without a WAL). */
  get walFrames() {
    return this.wal?.frames ?? 0;
  }

  page(n) {
    if (n < 1 || n > this.pageCount) throw new SqliteError(`page ${n} outside the file (${this.pageCount} pages)`);
    const at = this.wal?.pages.get(n);
    if (at !== undefined) return this.wal.bytes.subarray(at, at + this.pageSize);
    const p = this.b.subarray((n - 1) * this.pageSize, n * this.pageSize);
    if (p.length < this.pageSize) throw new SqliteError(`page ${n} past the end of the file`);
    return p;
  }

  /** Payload of a cell: the local bytes plus the overflow pages. */
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
      if (next === 0 || guard++ > this.pageCount) throw new SqliteError('broken overflow chain');
      const p = this.page(next);
      next = new DataView(p.buffer, p.byteOffset, 4).getUint32(0);
      const n = Math.min(u - 4, size - filled);
      out.set(p.subarray(4, 4 + n), filled);
      filled += n;
    }
    return out;
  }

  /**
   * Decodes a record: array of values (null, Number/BigInt, String,
   * Uint8Array); with `types` it adds the types to it ('null', 'integer',
   * 'real', 'text', 'blob').
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
      } else throw new SqliteError(`reserved serial type ${st}`);
    }
    return out;
  }

  /** Rows of a table b-tree: [{ rowid, values, types }] in rowid order. */
  tableRows(root, limit = Infinity) {
    const rows = [];
    const walk = (n, depth) => {
      if (depth > 64) throw new SqliteError('b-tree too deep (cycle?)');
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
      } else throw new SqliteError(`page ${n} of type ${type}, expected a table page`);
    };
    walk(root, 0);
    return rows;
  }

  /** Records of an index b-tree (WITHOUT ROWID tables), in order; the types in `types`. */
  indexRecords(root, limit = Infinity, types = null) {
    const out = [];
    const walk = (n, depth) => {
      if (depth > 64) throw new SqliteError('b-tree too deep (cycle?)');
      const page = this.page(n);
      const base = n === 1 ? 100 : 0;
      const type = page[base];
      const dv = new DataView(page.buffer, page.byteOffset, page.byteLength);
      const cells = dv.getUint16(base + 3);
      const interior = type === 0x02;
      if (type !== 0x02 && type !== 0x0a) throw new SqliteError(`page ${n} of type ${type}, expected an index page`);
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

  /** Entries of sqlite_schema: [{ type, name, table, root, sql }]. */
  schema() {
    this._schema ??= this.tableRows(1).map(({ values: [type, name, table, root, sql] }) => ({ type, name, table, root, sql }));
    return this._schema;
  }

  /** The tables: [{ name, columns, withoutRowid, root, sql }]. */
  tables() {
    return this.schema()
      .filter((e) => e.type === 'table')
      .map((e) => ({ name: e.name, root: e.root, sql: e.sql, ...parseCreateTable(e.sql ?? '') }));
  }

  /**
   * Rows of the table `name` (at most `limit`): { columns, rows, rowids,
   * types, table } with `rows` an array of arrays in column order (rowid
   * first if the table has one without an alias), `rowids` the rowid of
   * each row (null for WITHOUT ROWID), `types` the types of the values (as
   * in `record`) and `table` the entry of `tables()`.
   */
  rows(name, limit = 1000) {
    const t = this.tables().find((x) => x.name === name);
    if (!t) throw new SqliteError(`table ${name} not found`);
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

/** An SQL identifier in double quotes. */
export function sqlQuote(name) {
  return `"${String(name).replace(/"/g, '""')}"`;
}

/**
 * The WHERE that identifies a row: `rowid = ?N` if the table has a rowid,
 * otherwise the primary key columns (all columns if it does not find it)
 * with `IS ?N`. `row` are the values of `rows()` for that row.
 * Returns { where, params } starting from parameter `first`.
 */
function whereRow(t, row, rowid, first) {
  if (!t.withoutRowid) return { where: `rowid = ?${first}`, params: [rowid] };
  const cols = t.pk.length ? t.pk : t.columns.map((_, i) => i);
  return {
    where: cols.map((c, k) => `${sqlQuote(t.columns[c])} IS ?${first + k}`).join(' AND '),
    params: cols.map((c) => row[c]),
  };
}

/** Real column of the table for index `i` of `rows().columns` (-1 = rowid without an alias). */
function tableColumn(t, i) {
  return t.withoutRowid || t.rowidAlias >= 0 ? i : i - 1;
}

/**
 * SQL to change a cell: { sql, params } with the new value in ?1
 * (`value`: a GuestFiles.sql parameter, also { type, value }).
 * `view` is the result of `rows()`, `r` and `i` row and column of the view.
 */
export function updateCellSql(view, r, i, value) {
  const t = view.table;
  const col = tableColumn(t, i);
  if (col < 0) throw new SqliteError('the rowid of a table without an alias cannot be changed from here');
  const w = whereRow(t, t.withoutRowid ? view.rows[r] : null, view.rowids[r], 2);
  return { sql: `UPDATE ${sqlQuote(t.name)} SET ${sqlQuote(t.columns[col])} = ?1 WHERE ${w.where}`, params: [value, ...w.params] };
}

/** SQL to remove row `r` of the view. */
export function deleteRowSql(view, r) {
  const t = view.table;
  const w = whereRow(t, t.withoutRowid ? view.rows[r] : null, view.rowids[r], 1);
  return { sql: `DELETE FROM ${sqlQuote(t.name)} WHERE ${w.where}`, params: w.params };
}

/**
 * SQL to insert a row: `values` maps the column name to the value
 * (missing columns take their DEFAULT).
 */
export function insertRowSql(table, values) {
  const cols = Object.keys(values);
  if (!cols.length) return { sql: `INSERT INTO ${sqlQuote(table.name)} DEFAULT VALUES`, params: [] };
  return {
    sql: `INSERT INTO ${sqlQuote(table.name)} (${cols.map(sqlQuote).join(', ')}) VALUES (${cols.map((_, k) => `?${k + 1}`).join(', ')})`,
    params: cols.map((c) => values[c]),
  };
}

/** The columns of a record of a WITHOUT ROWID table: the key first, then the others. */
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

/** Splits `s` on the top-level commas (outside parentheses and quotes). */
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

/** First word (name) of a column definition, quotes included. */
function firstToken(s) {
  s = s.trim();
  const m = /^("(?:[^"]|"")*"|`[^`]*`|\[[^\]]*\]|'(?:[^']|'')*'|\S+)/s.exec(s);
  return m ? m[1] : '';
}

/**
 * Columns of a CREATE TABLE: { columns, rowidAlias (index or -1),
 * withoutRowid, pk (indices of the primary key) }.
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
      // Only exactly "INTEGER" makes the column an alias of the rowid.
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

/** A value for the page's table. */
export function formatValue(v) {
  if (v === null) return 'NULL';
  if (v instanceof Uint8Array) {
    const hex = Array.from(v.subarray(0, 32), (x) => x.toString(16).padStart(2, '0')).join('');
    return `x'${hex}${v.length > 32 ? '…' : ''}' (${v.length} bytes)`;
  }
  return String(v);
}
