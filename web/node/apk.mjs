// Informazioni di un APK lette nel browser (M6, ADR 0028): nome del
// pacchetto, versione ed eventuale attività principale dal manifesto binario
// (AXML) dentro lo ZIP. Serve a installare un APK trascinato e ad aprirlo
// senza riga di comando. Non usa API di Node (DecompressionStream c'è nei
// browser e in Node >= 18).

const dec = new TextDecoder();

/** I file di uno ZIP: Map nome -> { method, compressed, size, offset }. */
export function zipEntries(bytes) {
  const v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  let eocd = -1;
  for (let i = bytes.length - 22; i >= Math.max(0, bytes.length - 22 - 65535); i--) {
    if (v.getUint32(i, true) === 0x06054b50) {
      eocd = i;
      break;
    }
  }
  if (eocd < 0) throw new Error('APK: non è uno ZIP (manca la fine della directory centrale)');
  const count = v.getUint16(eocd + 10, true);
  let at = v.getUint32(eocd + 16, true);
  const out = new Map();
  for (let k = 0; k < count; k++) {
    if (v.getUint32(at, true) !== 0x02014b50) throw new Error('APK: directory centrale rovinata');
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

/** I byte di un file dello ZIP (stored o deflate). */
export async function zipRead(bytes, entry) {
  const v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  if (v.getUint32(entry.local, true) !== 0x04034b50) throw new Error('APK: intestazione locale rovinata');
  const start = entry.local + 30 + v.getUint16(entry.local + 26, true) + v.getUint16(entry.local + 28, true);
  const data = bytes.subarray(start, start + entry.compressed);
  if (entry.method === 0) return data.slice();
  if (entry.method !== 8) throw new Error(`APK: compressione ${entry.method} non gestita`);
  const stream = new Blob([data]).stream().pipeThrough(new DecompressionStream('deflate-raw'));
  const out = new Uint8Array(await new Response(stream).arrayBuffer());
  if (out.length !== entry.size) throw new Error('APK: lunghezza decompressa sbagliata');
  return out;
}

/**
 * Legge un XML binario di Android (AXML): restituisce gli elementi di
 * apertura in ordine, [{ name, depth, attrs: { nome: valore } }] (valori
 * stringa, o numeri per gli attributi tipizzati).
 */
export function parseAxml(bytes) {
  const v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  if (v.getUint16(0, true) !== 0x0003) throw new Error('manifesto: non è un XML binario');
  let strings = [];
  const elements = [];
  let depth = 0;
  let at = v.getUint16(2, true);
  while (at + 8 <= bytes.length) {
    const type = v.getUint16(at, true);
    const hsize = v.getUint16(at + 2, true);
    const size = v.getUint32(at + 4, true);
    if (size < 8 || at + size > bytes.length) throw new Error('manifesto: pezzo rovinato');
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
      // Lunghezza in UTF-16 e poi in byte, ognuna su 1 o 2 byte.
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
 * { package, versionName, versionCode, label, launcher } di un APK
 * (Uint8Array): `launcher` è il nome completo dell'attività con MAIN e
 * LAUNCHER, o null.
 */
export async function apkInfo(bytes) {
  const entries = zipEntries(bytes);
  const m = entries.get('AndroidManifest.xml');
  if (!m) throw new Error('APK: manca AndroidManifest.xml');
  const els = parseAxml(await zipRead(bytes, m));
  const manifest = els.find((e) => e.name === 'manifest');
  if (!manifest?.attrs.package) throw new Error('APK: manifesto senza pacchetto');
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
  return {
    package: pkg,
    versionName: manifest.attrs.versionName ?? null,
    versionCode: manifest.attrs.versionCode ?? null,
    label: typeof app?.attrs.label === 'string' ? app.attrs.label : null,
    launcher,
  };
}
