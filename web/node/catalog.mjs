// The app catalog (M6, ADR 0033): suggested apps offered in the Android
// mode of the web app, installed on request through the in-page ADB client
// (the same path as a dropped APK). This module is the logic without the DOM,
// shared by the page (web/app/catalog.mjs), the tests and the tool that adds
// entries (tools/catalog): parsing and validation of `catalog/v1.json`, the
// minimum image version, the verified download, the per-app install state and
// the installed packages read from `pm list packages`. Uses no Node API.
//
// catalog/v1.json on R2 (format 1):
//   { "format": 1, "updated": ISO date,
//     "apps": [{ "id", "name", "package", "version", "versionCode",
//                "apk" (URL, relative to the catalog), "size", "sha256",
//                "license" (SPDX), "source" (source code URL, the exact tag
//                for copyleft apps), "icon" (URL, relative), "description",
//                "minImage" (image release, e.g. "android-15.0.0_r36", or
//                null), "advanced" (large or heavy: shown apart),
//                "origin" (where the APK was mirrored from, informative) }] }
// Unknown fields are ignored, so later additions don't break older pages.

/** The catalog the app reads by default (public R2 bucket, like the images). */
export const CATALOG_URL = 'https://pub-06e88fdd7f374fffb06844d60083f2ae.r2.dev/catalog/v1.json';
export const CATALOG_FORMAT = 1;

const ID = /^[a-z0-9][a-z0-9-]{0,39}$/;
const PACKAGE = /^[A-Za-z][A-Za-z0-9_]*(\.[A-Za-z][A-Za-z0-9_]*)+$/;
const SHA256 = /^[0-9a-f]{64}$/;
// An SPDX expression made of identifiers, `AND`/`OR`/`WITH` and brackets.
const SPDX = /^[A-Za-z0-9.+-]+( (AND|OR|WITH) [A-Za-z0-9.+-]+)*$|^\(.+\)$/;

/**
 * Validates one catalog entry and resolves its URLs against `base`: the
 * normalised entry, or throws an Error naming the first problem.
 */
export function parseEntry(e, base) {
  const bad = (what) => {
    throw new Error(`catalog entry ${typeof e?.id === 'string' ? e.id : '?'}: ${what}`);
  };
  if (!e || typeof e !== 'object' || Array.isArray(e)) bad('not an object');
  const str = (k, { optional = false } = {}) => {
    const v = e[k];
    if (v === undefined || v === null) {
      if (optional) return null;
      bad(`${k} missing`);
    }
    if (typeof v !== 'string' || !v.trim()) bad(`${k} is not a string`);
    return v;
  };
  const url = (k, { optional = false, https = false } = {}) => {
    const v = str(k, { optional });
    if (v === null) return null;
    let u;
    try {
      u = new URL(v, base);
    } catch {
      bad(`${k} is not a URL`);
    }
    if (u.protocol !== 'https:' && (https || u.protocol !== 'http:')) bad(`${k} is not an http(s) URL`);
    return u.href;
  };
  const id = str('id');
  if (!ID.test(id)) bad('id: lowercase letters, digits and dashes');
  const pkg = str('package');
  if (!PACKAGE.test(pkg)) bad('package is not a Java package name');
  const size = e.size;
  if (!Number.isSafeInteger(size) || size <= 0) bad('size is not a positive integer');
  const sha256 = str('sha256').toLowerCase();
  if (!SHA256.test(sha256)) bad('sha256 is not 64 hex digits');
  const license = str('license');
  if (!SPDX.test(license)) bad('license is not an SPDX expression');
  if (e.versionCode !== undefined && e.versionCode !== null && !Number.isSafeInteger(e.versionCode)) bad('versionCode is not an integer');
  const minImage = str('minImage', { optional: true });
  if (minImage !== null && !imageRelease(minImage)) bad('minImage is not an image release (android-<x.y.z>_r<n>)');
  if (e.advanced !== undefined && typeof e.advanced !== 'boolean') bad('advanced is not a boolean');
  return {
    id,
    name: str('name'),
    package: pkg,
    version: str('version'),
    versionCode: e.versionCode ?? null,
    apk: url('apk'),
    size,
    sha256,
    license,
    source: url('source', { https: true }),
    icon: url('icon', { optional: true }),
    description: str('description'),
    minImage,
    advanced: e.advanced === true,
    origin: url('origin', { optional: true }),
  };
}

/**
 * Parses catalog/v1.json (the object, or its text) fetched from `base`:
 * { format, updated, apps, problems }. A file of another format or without an
 * app list throws (the page hides the panel); a bad entry is left out and
 * named in `problems`; duplicate ids or packages keep the first.
 */
export function parseCatalog(json, base) {
  const c = typeof json === 'string' ? JSON.parse(json) : json;
  if (!c || typeof c !== 'object') throw new Error('catalog: not an object');
  if (c.format !== CATALOG_FORMAT) throw new Error(`catalog: format ${c.format}, this page reads ${CATALOG_FORMAT}`);
  if (!Array.isArray(c.apps)) throw new Error('catalog: no app list');
  const apps = [];
  const problems = [];
  const ids = new Set();
  const packages = new Set();
  for (const raw of c.apps) {
    try {
      const e = parseEntry(raw, base);
      if (ids.has(e.id) || packages.has(e.package)) throw new Error(`catalog entry ${e.id}: duplicate id or package`);
      ids.add(e.id);
      packages.add(e.package);
      apps.push(e);
    } catch (err) {
      problems.push(err.message);
    }
  }
  return { format: c.format, updated: typeof c.updated === 'string' ? c.updated : null, apps, problems };
}

/** Fetches and parses the catalog; throws if it can't be loaded or read. */
export async function loadCatalog(url = CATALOG_URL, { fetch: get = globalThis.fetch } = {}) {
  const res = await get(url, { cache: 'no-cache' });
  if (!res.ok) throw new Error(`catalog ${url}: status ${res.status}`);
  return parseCatalog(await res.text(), res.url || url);
}

/**
 * The AOSP release of an image version (`android-15.0.0_r36-BP1A...-bd09e2f`
 * or just `android-15.0.0_r36`): [major, minor, patch, revision], or null.
 * The build id and the image commit that follow are not ordered, so the
 * minimum image version compares releases only (ADR 0033).
 */
export function imageRelease(version) {
  const m = /^android-(\d+)\.(\d+)\.(\d+)_r(\d+)(?:-|$)/.exec(version ?? '');
  return m ? m.slice(1, 5).map(Number) : null;
}

/**
 * Whether an entry fits the running image: no minimum, or an image release at
 * least `minImage`. An image version that isn't a release (a local build
 * without the usual name) accepts every entry: pm has the last word.
 */
export function imageSatisfies(minImage, version) {
  if (!minImage) return true;
  const want = imageRelease(minImage);
  const have = imageRelease(version);
  if (!want || !have) return true;
  for (let i = 0; i < 4; i++) if (have[i] !== want[i]) return have[i] > want[i];
  return true;
}

/** SHA-256 as hex (WebCrypto, in the browser and in Node). */
export async function sha256Hex(bytes) {
  const d = await crypto.subtle.digest('SHA-256', bytes);
  return [...new Uint8Array(d)].map((b) => b.toString(16).padStart(2, '0')).join('');
}

/** Throws unless `bytes` has the entry's size and SHA-256. */
export async function verifyApk(bytes, entry) {
  if (bytes.length !== entry.size) throw new Error(`${entry.id}: ${bytes.length} bytes instead of ${entry.size}`);
  const h = await sha256Hex(bytes);
  if (h !== entry.sha256) throw new Error(`${entry.id}: SHA-256 ${h} instead of ${entry.sha256}: the APK is not the one in the catalog`);
}

/**
 * Downloads an entry's APK and verifies it: a Uint8Array of exactly `size`
 * bytes with the catalog's SHA-256, or throws. `onProgress({ loaded, total })`
 * as bytes arrive. A body longer than `size` stops early.
 */
export async function downloadApk(entry, { fetch: get = globalThis.fetch, onProgress = () => {}, signal } = {}) {
  const res = await get(entry.apk, { signal });
  if (!res.ok) throw new Error(`${entry.id}: download status ${res.status}`);
  const out = new Uint8Array(entry.size);
  let loaded = 0;
  onProgress({ loaded, total: entry.size });
  if (res.body?.getReader) {
    const reader = res.body.getReader();
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      if (loaded + value.length > entry.size) {
        await reader.cancel().catch(() => {});
        throw new Error(`${entry.id}: the download is longer than the catalog's ${entry.size} bytes`);
      }
      out.set(value, loaded);
      loaded += value.length;
      onProgress({ loaded, total: entry.size });
    }
  } else {
    const all = new Uint8Array(await res.arrayBuffer());
    if (all.length > entry.size) throw new Error(`${entry.id}: the download is longer than the catalog's ${entry.size} bytes`);
    out.set(all);
    loaded = all.length;
    onProgress({ loaded, total: entry.size });
  }
  await verifyApk(loaded === entry.size ? out : out.subarray(0, loaded), entry);
  return out;
}

/**
 * Installed packages from `pm list packages --show-versioncode` (lines
 * `package:<name> versionCode:<n>`; without the option, no versionCode):
 * Map package -> versionCode (a number, or null).
 */
export function parsePackages(text) {
  const out = new Map();
  for (const line of text.split('\n')) {
    const m = /^package:(\S+)(?:\s+versionCode:(\d+))?/.exec(line.trim());
    if (m) out.set(m[1], m[2] === undefined ? null : Number(m[2]));
  }
  return out;
}

/** adb command for the installed packages with their version codes. */
export const PACKAGES_QUERY = 'pm list packages --show-versioncode';

// ---- Per-app install state -------------------------------------------------------

/**
 * The states of an app card: `absent` (not installed), `downloading`,
 * `installing`, `installed`, `failed`. `outdated` is `absent` with an older
 * version on the device (the button says Update).
 */
export const STATES = ['absent', 'downloading', 'installing', 'installed', 'failed'];

/** The initial state of an app. */
export const initialState = () => ({ phase: 'absent', loaded: 0, total: 0, fraction: null, error: null, installedCode: null, outdated: false });

/**
 * The next state of an app after `event` ({ type, ... }), for `entry`:
 * - `packages` { packages: Map } (from parsePackages): installed or not; ignored while busy;
 * - `download` { total }: starts a download (from absent or failed);
 * - `progress` { loaded }: bytes downloaded;
 * - `downloaded`: verified, the install starts;
 * - `pushing` { fraction }: part of the APK sent to the device;
 * - `installed` { versionCode? }: pm said Success;
 * - `failed` { error }: from downloading or installing.
 * Events that don't apply to the current phase leave it unchanged (the same object).
 */
export function nextState(state, event, entry) {
  const busy = state.phase === 'downloading' || state.phase === 'installing';
  switch (event.type) {
    case 'packages': {
      if (busy) return state;
      if (!event.packages.has(entry.package)) {
        return state.phase === 'failed' ? { ...state, installedCode: null, outdated: false } : { ...initialState() };
      }
      const code = event.packages.get(entry.package);
      const outdated = code !== null && entry.versionCode !== null && code < entry.versionCode;
      if (outdated) return { ...initialState(), installedCode: code, outdated: true };
      return { ...initialState(), phase: 'installed', installedCode: code };
    }
    case 'download':
      if (state.phase !== 'absent' && state.phase !== 'failed') return state;
      return { ...state, phase: 'downloading', loaded: 0, total: event.total ?? entry.size, fraction: 0, error: null };
    case 'progress':
      if (state.phase !== 'downloading') return state;
      return { ...state, loaded: event.loaded, fraction: state.total ? event.loaded / state.total : null };
    case 'downloaded':
      if (state.phase !== 'downloading') return state;
      return { ...state, phase: 'installing', fraction: null };
    case 'pushing':
      if (state.phase !== 'installing') return state;
      return { ...state, fraction: event.fraction };
    case 'installed':
      if (state.phase !== 'installing') return state;
      return { ...initialState(), phase: 'installed', installedCode: event.versionCode ?? entry.versionCode };
    case 'failed':
      if (!busy) return state;
      return { ...state, phase: 'failed', fraction: null, error: String(event.error) };
    default:
      return state;
  }
}

/** Human size: 1.4 MiB, 75 MiB, 364 MiB. */
export function sizeText(n) {
  if (n < 1 << 20) return `${Math.max(1, Math.round(n / 1024))} KiB`;
  const m = n / 2 ** 20;
  return `${m < 10 ? m.toFixed(1) : Math.round(m)} MiB`;
}
