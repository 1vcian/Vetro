// APK information read in the browser (M6, ADR 0028): package name, version
// and main activity (if any) from the binary manifest (AXML) inside the ZIP.
// Used to install a dropped APK and open it without a command line. Uses no
// Node API (DecompressionStream exists in browsers and in Node >= 18).

const dec = new TextDecoder();

/** A ZIP's files: Map name -> { method, compressed, size, local }. */
export function zipEntries(bytes) {
  const v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  let eocd = -1;
  for (let i = bytes.length - 22; i >= Math.max(0, bytes.length - 22 - 65535); i--) {
    if (v.getUint32(i, true) === 0x06054b50) {
      eocd = i;
      break;
    }
  }
  if (eocd < 0) throw new Error('APK: not a ZIP (no end of central directory)');
  const count = v.getUint16(eocd + 10, true);
  let at = v.getUint32(eocd + 16, true);
  const out = new Map();
  for (let k = 0; k < count; k++) {
    if (v.getUint32(at, true) !== 0x02014b50) throw new Error('APK: damaged central directory');
    const method = v.getUint16(at + 10, true);
    const compressed = v.getUint32(at + 20, true);
    const size = v.getUint32(at + 24, true);
    const n = v.getUint16(at + 28, true);
    const e = v.getUint16(at + 30, true);
    const c = v.getUint16(at + 32, true);
    const local = v.getUint32(at + 42, true);
    const name = dec.decode(bytes.subarray(at + 46, at + 46 + n));
    out.set(name, { method, compressed, size, local });
    at += 46 + n + e + c;
  }
  return out;
}

/** The bytes of a file in the ZIP (stored or deflate). */
export async function zipRead(bytes, entry) {
  const v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  if (v.getUint32(entry.local, true) !== 0x04034b50) throw new Error('APK: damaged local header');
  const start = entry.local + 30 + v.getUint16(entry.local + 26, true) + v.getUint16(entry.local + 28, true);
  const data = bytes.subarray(start, start + entry.compressed);
  if (entry.method === 0) return data.slice();
  if (entry.method !== 8) throw new Error(`APK: compression ${entry.method} not supported`);
  const stream = new Blob([data]).stream().pipeThrough(new DecompressionStream('deflate-raw'));
  const out = new Uint8Array(await new Response(stream).arrayBuffer());
  if (out.length !== entry.size) throw new Error('APK: wrong decompressed length');
  return out;
}

/**
 * Reads an Android binary XML (AXML): returns the start elements in order,
 * [{ name, depth, attrs: { name: value } }] (string values, or numbers for
 * typed attributes).
 */
export function parseAxml(bytes) {
  const v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  if (v.getUint16(0, true) !== 0x0003) throw new Error('manifest: not a binary XML');
  let strings = [];
  const elements = [];
  let depth = 0;
  let at = v.getUint16(2, true);
  while (at + 8 <= bytes.length) {
    const type = v.getUint16(at, true);
    const hsize = v.getUint16(at + 2, true);
    const size = v.getUint32(at + 4, true);
    if (size < 8 || at + size > bytes.length) throw new Error('manifest: damaged chunk');
    if (type === 0x0001) strings = stringPool(bytes, v, at);
    else if (type === 0x0102) {
      const name = strings[v.getUint32(at + 20, true)];
      const attrStart = v.getUint16(at + 24, true);
      const attrSize = v.getUint16(at + 26, true);
      const attrCount = v.getUint16(at + 28, true);
      const attrs = {};
      for (let i = 0; i < attrCount; i++) {
        const a = at + 16 + attrStart + i * attrSize;
        const key = strings[v.getUint32(a + 4, true)];
        const raw = v.getUint32(a + 8, true);
        const dataType = v.getUint8(a + 15);
        const data = v.getUint32(a + 16, true);
        attrs[key] = raw !== 0xffffffff ? strings[raw] : dataType === 0x03 ? strings[data] : data;
      }
      elements.push({ name, depth, attrs });
      depth++;
    } else if (type === 0x0103) depth--;
    at += size;
    void hsize;
  }
  return elements;
}

function stringPool(bytes, v, at) {
  const count = v.getUint32(at + 8, true);
  const utf8 = (v.getUint32(at + 16, true) & 0x100) !== 0;
  const base = at + v.getUint32(at + 20, true);
  const offsets = at + v.getUint16(at + 2, true);
  const out = [];
  for (let i = 0; i < count; i++) {
    let p = base + v.getUint32(offsets + 4 * i, true);
    if (utf8) {
      // Length in UTF-16 units, then in bytes, each on 1 or 2 bytes.
      p += bytes[p] & 0x80 ? 2 : 1;
      let n = bytes[p];
      if (n & 0x80) {
        n = ((n & 0x7f) << 8) | bytes[p + 1];
        p += 2;
      } else p += 1;
      out.push(dec.decode(bytes.subarray(p, p + n)));
    } else {
      let n = v.getUint16(p, true);
      if (n & 0x8000) {
        n = ((n & 0x7fff) << 16) | v.getUint16(p + 2, true);
        p += 4;
      } else p += 2;
      let s = '';
      for (let k = 0; k < n; k++) s += String.fromCharCode(v.getUint16(p + 2 * k, true));
      out.push(s);
    }
  }
  return out;
}

/**
 * { package, versionName, versionCode, label, launcher } of an APK
 * (Uint8Array): `launcher` is the full name of the activity with MAIN and
 * LAUNCHER, or null.
 */
export async function apkInfo(bytes) {
  const entries = zipEntries(bytes);
  const m = entries.get('AndroidManifest.xml');
  if (!m) throw new Error('APK: AndroidManifest.xml missing');
  const els = parseAxml(await zipRead(bytes, m));
  const manifest = els.find((e) => e.name === 'manifest');
  if (!manifest?.attrs.package) throw new Error('APK: manifest without a package');
  const pkg = manifest.attrs.package;
  const app = els.find((e) => e.name === 'application');
  let launcher = null;
  let current = null;
  let filter = null;
  for (const e of els) {
    if ((e.name === 'activity' || e.name === 'activity-alias') && e.depth === 2) current = e.attrs.name ?? null;
    else if (e.depth <= 2) current = null;
    if (e.name === 'intent-filter') filter = { main: false, launcher: false, depth: e.depth };
    else if (filter && e.depth <= filter.depth) filter = null;
    if (filter && current) {
      if (e.name === 'action' && e.attrs.name === 'android.intent.action.MAIN') filter.main = true;
      if (e.name === 'category' && e.attrs.name === 'android.intent.category.LAUNCHER') filter.launcher = true;
      if (filter.main && filter.launcher && !launcher) launcher = current.startsWith('.') ? pkg + current : current.includes('.') ? current : `${pkg}.${current}`;
    }
  }
  const sdk = els.find((e) => e.name === 'uses-sdk');
  const abis = new Set();
  for (const name of entries.keys()) {
    const m = /^lib\/([^/]+)\//.exec(name);
    if (m) abis.add(m[1]);
  }
  return {
    package: pkg,
    versionName: manifest.attrs.versionName ?? null,
    versionCode: manifest.attrs.versionCode ?? null,
    label: typeof app?.attrs.label === 'string' ? app.attrs.label : null,
    launcher,
    minSdk: typeof sdk?.attrs.minSdkVersion === 'number' ? sdk.attrs.minSdkVersion : null,
    targetSdk: typeof sdk?.attrs.targetSdkVersion === 'number' ? sdk.attrs.targetSdkVersion : null,
    /** Resource id (number) or path (string) of the application icon, or null. */
    icon: app?.attrs.icon ?? null,
    /** ABIs with native code (`lib/<abi>/`), sorted; empty for pure Java/Kotlin apps. */
    abis: [...abis].sort(),
  };
}

// ---- resources.arsc (M6, app catalog, ADR 0033) ------------------------------------

const RES_STRING_POOL = 0x0001;
const RES_TABLE = 0x0002;
const RES_TABLE_PACKAGE = 0x0200;
const RES_TABLE_TYPE = 0x0201;
const TYPE_REFERENCE = 0x01;
const TYPE_STRING = 0x03;

/**
 * The compiled resource table (`resources.arsc`): { strings, packages }, with
 * `packages` a Map id -> Map typeId -> [{ density, chunk offset, flags,
 * entryCount, entriesStart, headerSize }]. Enough to resolve an id to its
 * values per configuration (the icon, the label).
 */
export function parseArsc(bytes) {
  const v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  if (v.getUint16(0, true) !== RES_TABLE) throw new Error('resources.arsc: not a resource table');
  let strings = [];
  const packages = new Map();
  let at = v.getUint16(2, true);
  while (at + 8 <= bytes.length) {
    const type = v.getUint16(at, true);
    const size = v.getUint32(at + 4, true);
    if (size < 8 || at + size > bytes.length) throw new Error('resources.arsc: damaged chunk');
    if (type === RES_STRING_POOL) strings = stringPool(bytes, v, at);
    else if (type === RES_TABLE_PACKAGE) packages.set(v.getUint32(at + 8, true), arscPackage(bytes, v, at, size));
    at += size;
  }
  return { strings, packages, bytes, v };
}

function arscPackage(bytes, v, start, size) {
  const types = new Map();
  let at = start + v.getUint16(start + 2, true);
  while (at + 8 <= start + size) {
    const type = v.getUint16(at, true);
    const csize = v.getUint32(at + 4, true);
    if (csize < 8 || at + csize > start + size) throw new Error('resources.arsc: damaged package chunk');
    if (type === RES_TABLE_TYPE) {
      const id = v.getUint8(at + 8);
      const config = at + 20;
      const configSize = v.getUint32(config, true);
      // ResTable_config: size, imsi (4), locale (4), screenType (orientation,
      // touchscreen, density u16).
      const density = configSize >= 16 ? v.getUint16(config + 14, true) : 0;
      if (!types.has(id)) types.set(id, []);
      types.get(id).push({ at, flags: v.getUint8(at + 9), entryCount: v.getUint32(at + 12, true), entriesStart: v.getUint32(at + 16, true), headerSize: v.getUint16(at + 2, true), density });
    }
    at += csize;
  }
  return types;
}

/** The entry offset of index `e` in a type chunk (dense, sparse or 16-bit offsets), or -1. */
function entryOffset(v, t, e) {
  const table = t.at + t.headerSize;
  if (t.flags & 0x01) {
    // FLAG_SPARSE: sorted (index u16, offset / 4 u16) pairs.
    for (let i = 0; i < t.entryCount; i++) {
      const idx = v.getUint16(table + 4 * i, true);
      if (idx === e) return v.getUint16(table + 4 * i + 2, true) * 4;
      if (idx > e) break;
    }
    return -1;
  }
  if (e >= t.entryCount) return -1;
  if (t.flags & 0x02) {
    // FLAG_OFFSET16: offset / 4 as u16, 0xffff = none.
    const o = v.getUint16(table + 2 * e, true);
    return o === 0xffff ? -1 : o * 4;
  }
  const o = v.getUint32(table + 4 * e, true);
  return o === 0xffffffff ? -1 : o;
}

/**
 * The values of resource `id` in every configuration: [{ density, dataType,
 * data, string }], `string` for string values (a file path for drawables).
 * References are followed (at most 8 levels).
 */
export function resolveResource(table, id, depth = 0) {
  const { v } = table;
  const types = table.packages.get(id >>> 24);
  const out = [];
  for (const t of types?.get((id >>> 16) & 0xff) ?? []) {
    const off = entryOffset(v, t, id & 0xffff);
    if (off < 0) continue;
    const e = t.at + t.entriesStart + off;
    const flags = v.getUint16(e + 2, true);
    let dataType;
    let data;
    if (flags & 0x0008) {
      // FLAG_COMPACT (Android 14): the type in the flags' high byte, the data after the key.
      dataType = flags >>> 8;
      data = v.getUint32(e + 4, true);
    } else if (flags & 0x0001) continue; // a map (style, array): not a single value
    else {
      const value = e + v.getUint16(e, true);
      dataType = v.getUint8(value + 3);
      data = v.getUint32(value + 4, true);
    }
    if (dataType === TYPE_REFERENCE && depth < 8 && data !== id) {
      for (const r of resolveResource(table, data, depth + 1)) out.push({ ...r, density: r.density || t.density });
      continue;
    }
    out.push({ density: t.density, dataType, data, string: dataType === TYPE_STRING ? table.strings[data] ?? null : null });
  }
  return out;
}

/** The image type of `b` from its first bytes (PNG, WebP, JPEG), or null. */
export function rasterType(b) {
  if (b.length >= 8 && b[0] === 0x89 && b[1] === 0x50 && b[2] === 0x4e && b[3] === 0x47) return 'image/png';
  if (b.length >= 12 && String.fromCharCode(...b.subarray(0, 4)) === 'RIFF' && String.fromCharCode(...b.subarray(8, 12)) === 'WEBP') return 'image/webp';
  if (b.length >= 3 && b[0] === 0xff && b[1] === 0xd8 && b[2] === 0xff) return 'image/jpeg';
  return null;
}

/** Whether `b` is a binary XML (compiled drawable, adaptive icon). */
const isAxml = (b) => b.length >= 8 && b[0] === 0x03 && b[1] === 0x00 && b[2] === 0x08 && b[3] === 0x00;

/**
 * The application icon as an image: { path, bytes, type, density }. The
 * highest-density PNG/WebP/JPEG among the icon's configurations (recognised
 * by content: shrunk APKs drop the file extensions); else an adaptive icon
 * whose foreground is a raster (with a colour or raster background) becomes
 * an SVG with the layers inside, cropped to the visible 72 of 108 dp and
 * rounded like a launcher mask; null for vector icons, which would need a
 * renderer. For the app catalog's tool (tools/catalog).
 */
export async function apkIcon(bytes, info = null) {
  info ??= await apkInfo(bytes);
  const entries = zipEntries(bytes);
  let table = null;
  let paths = [];
  if (typeof info.icon === 'string') paths = [{ path: info.icon, density: 0 }];
  else if (typeof info.icon === 'number') {
    const arsc = entries.get('resources.arsc');
    if (!arsc) return null;
    table = parseArsc(await zipRead(bytes, arsc));
    paths = resolveResource(table, info.icon).filter((r) => r.string).map((r) => ({ path: r.string, density: r.density === 0xffff ? 0 : r.density }));
  }
  const best = await densestRaster(bytes, entries, paths);
  if (best || !table) return best;
  for (const p of paths) {
    if (!entries.has(p.path)) continue;
    const xml = await zipRead(bytes, entries.get(p.path));
    if (!isAxml(xml)) continue;
    const svg = await adaptiveSvg(bytes, entries, table, parseAxml(xml));
    if (svg) return { path: p.path, bytes: new TextEncoder().encode(svg), type: 'image/svg+xml', density: 0 };
  }
  return null;
}

/** The densest PNG/WebP/JPEG among [{ path, density }] present in the ZIP, or null. */
async function densestRaster(bytes, entries, paths) {
  // density 0 = default (mdpi-like); 0xfffe = anydpi (vectors: XML, skipped by content).
  const sorted = paths.filter((p) => entries.has(p.path) && !p.path.toLowerCase().endsWith('.xml')).sort((a, b) => b.density - a.density);
  for (const p of sorted) {
    const b = await zipRead(bytes, entries.get(p.path));
    const type = rasterType(b);
    if (type) return { path: p.path, bytes: b, type, density: p.density };
  }
  return null;
}

/** Framework colours an adaptive icon background may name (android.R.color). */
const FRAMEWORK_COLORS = { 0x01060000: '#aaaaaa', 0x0106000b: '#ffffff', 0x0106000c: '#000000', 0x0106000d: '#00000000' };

const TYPE_COLOR_FIRST = 0x1c;
const TYPE_COLOR_LAST = 0x1f;
const colorCss = (argb) => `#${((argb >>> 0) & 0xffffff).toString(16).padStart(6, '0')}${(argb >>> 24) === 0xff ? '' : (argb >>> 24).toString(16).padStart(2, '0')}`;

/**
 * An adaptive icon layer (`drawable` attribute value): { raster } or
 * { color } or null. Follows references, `inset` wrappers and `shape`/`solid`
 * colours; a number that is no resource of the APK is a colour literal.
 */
async function iconLayer(bytes, entries, table, value, depth = 0) {
  if (typeof value !== 'number' || depth > 4) return null;
  if (value >>> 24 === 0x01) return { color: FRAMEWORK_COLORS[value] ?? '#ffffff' };
  const values = value >>> 24 === 0x7f ? resolveResource(table, value) : [];
  if (!values.length) return { color: colorCss(value) };
  const colors = values.filter((r) => r.dataType >= TYPE_COLOR_FIRST && r.dataType <= TYPE_COLOR_LAST);
  if (colors.length) return { color: colorCss(colors[0].data) };
  const files = values.filter((r) => r.string).map((r) => ({ path: r.string, density: r.density === 0xffff ? 0 : r.density }));
  const raster = await densestRaster(bytes, entries, files);
  if (raster) return { raster };
  for (const f of files) {
    if (!entries.has(f.path)) continue;
    const xml = await zipRead(bytes, entries.get(f.path));
    if (!isAxml(xml)) continue;
    const els = parseAxml(xml);
    const root = els[0];
    if (root?.name === 'inset' || root?.name === 'bitmap') {
      const inner = await iconLayer(bytes, entries, table, root.attrs.drawable ?? root.attrs.src, depth + 1);
      if (inner) return inner;
    }
    if (root?.name === 'shape') {
      const solid = els.find((e) => e.name === 'solid');
      if (solid && solid.attrs.color !== undefined) return iconLayer(bytes, entries, table, solid.attrs.color, depth + 1);
    }
    if (root?.name === 'color' && root.attrs.color !== undefined) return iconLayer(bytes, entries, table, root.attrs.color, depth + 1);
  }
  return null;
}

async function adaptiveSvg(bytes, entries, table, els) {
  if (els[0]?.name !== 'adaptive-icon') return null;
  const layer = async (name) => {
    const e = els.find((x) => x.name === name && x.depth === 1);
    return e ? iconLayer(bytes, entries, table, e.attrs.drawable) : null;
  };
  const fg = await layer('foreground');
  if (!fg?.raster) return null;
  const bg = await layer('background');
  const b64 = (u8) => {
    let s = '';
    for (let i = 0; i < u8.length; i += 0x8000) s += String.fromCharCode(...u8.subarray(i, i + 0x8000));
    return btoa(s);
  };
  const img = (r) => `<image href="data:${r.type};base64,${b64(r.bytes)}" width="108" height="108" preserveAspectRatio="none"/>`;
  const back = bg?.raster ? img(bg.raster) : `<rect width="108" height="108" fill="${bg?.color ?? '#ffffff'}"/>`;
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="18 18 72 72" width="192" height="192">` +
    `<clipPath id="m"><rect x="18" y="18" width="72" height="72" rx="16"/></clipPath>` +
    `<g clip-path="url(#m)">${back}${img(fg.raster)}</g></svg>`;
}
