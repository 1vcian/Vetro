#!/usr/bin/env node
// Adds or updates an entry of the app catalog (M6, ADR 0033): fetches the APK
// from its official release, verifies it against the digest published by the
// origin, reads package, version and icon with web/node/apk.mjs, uploads APK
// and icon to R2 and rewrites catalog/v1.json there (validated with
// web/node/catalog.mjs before the upload).
//
//   node tools/catalog/add.mjs <origin> --id ID [options]
//
// Origins, each verified in its own way:
//   github:OWNER/REPO[@TAG]  a GitHub release (latest, or TAG): the .apk asset
//                            (--asset REGEX to choose among several), checked
//                            against the asset's SHA-256 digest from the API;
//                            source link: the tag's tree.
//   fdroid:PACKAGE[@CODE]    F-Droid's main repository (suggested version, or
//                            version code CODE): entry.json -> index-v2.json
//                            (its SHA-256 from entry.json) -> the APK's SHA-256;
//                            name, licence, summary and source from the index;
//                            for copyleft licences the source link is F-Droid's
//                            source tarball of that exact version.
//   chromium:stable          official Chromium snapshot builds for Android
//                            arm64 (chromium-browser-snapshots/Android_Arm64):
//                            the newest snapshot at or below the current
//                            stable release's branch position (chromiumdash),
//                            chrome-android.zip checked against the MD5 that
//                            Google Cloud Storage publishes, then
//                            apks/ChromePublic.apk extracted; source link: the
//                            snapshot's commit.
//   url:URL --sha256 HEX     any HTTPS URL with a digest known from elsewhere.
//
// Options: --name, --description, --license (SPDX), --source (URL), --package
// (expected package: error if the APK says otherwise), --min-image
// (android-<x.y.z>_r<n>), --advanced, --icon FILE (when the APK's icon is a
// vector: an adaptive icon with a raster foreground becomes an SVG), --dry-run (no upload: target/catalog/v1.json only),
// --replace (overwrite an object already on R2 with different bytes).
//
// R2 credentials: ~/.config/vetro/r2.env (VETRO_R2_ENV to point elsewhere),
// never printed. Needs the aws CLI (as tools/aosp/upload-snapshot.sh).
// Downloads are cached in target/catalog/downloads.
//
// Layout on R2 (relative URLs in the catalog, so it can move to another host):
//   catalog/v1.json                              no-cache
//   catalog/apks/<id>/<package>-<code>.apk       immutable
//   catalog/icons/<id>-<code>.<png|webp|jpg|svg> immutable

import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { createReadStream, createWriteStream, existsSync, mkdirSync, readFileSync, renameSync, statSync, writeFileSync } from 'node:fs';
import { homedir, tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { Readable } from 'node:stream';
import { pipeline } from 'node:stream/promises';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import { apkIcon, apkInfo, zipEntries, zipRead } from '../../web/node/apk.mjs';
import { CATALOG_FORMAT, imageRelease, parseCatalog, parseEntry } from '../../web/node/catalog.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const work = join(root, 'target/catalog');
const downloads = join(work, 'downloads');
/** API level of Vetro's image (Android 15). */
const IMAGE_SDK = 35;
/** Android 15 refuses to install apps targeting an API level below this. */
const MIN_TARGET_SDK = 24;

const { values: opt, positionals } = parseArgs({
  allowPositionals: true,
  options: {
    id: { type: 'string' },
    name: { type: 'string' },
    description: { type: 'string' },
    license: { type: 'string' },
    source: { type: 'string' },
    package: { type: 'string' },
    'min-image': { type: 'string' },
    advanced: { type: 'boolean', default: false },
    icon: { type: 'string' },
    asset: { type: 'string' },
    sha256: { type: 'string' },
    'dry-run': { type: 'boolean', default: false },
    replace: { type: 'boolean', default: false },
  },
});

const die = (msg) => {
  console.error(`ERROR: ${msg}`);
  process.exit(1);
};
const log = (msg) => console.log(msg);
const hex = (algo, bytes) => createHash(algo).update(bytes).digest('hex');

async function getJson(url) {
  const r = await fetch(url, { headers: { 'User-Agent': 'vetro-catalog', Accept: 'application/json' } });
  if (!r.ok) throw new Error(`${url}: status ${r.status}`);
  return r.json();
}

/** Downloads `url` to the cache (resumes nothing: a partial file is discarded). */
async function download(url, name) {
  mkdirSync(downloads, { recursive: true });
  const file = join(downloads, name);
  if (existsSync(file)) {
    log(`cached: ${file}`);
    return file;
  }
  log(`downloading ${url}`);
  const r = await fetch(url, { headers: { 'User-Agent': 'vetro-catalog' }, redirect: 'follow' });
  if (!r.ok) throw new Error(`${url}: status ${r.status}`);
  const part = `${file}.part`;
  await pipeline(Readable.fromWeb(r.body), createWriteStream(part));
  renameSync(part, file);
  log(`downloaded: ${(statSync(file).size / 2 ** 20).toFixed(1)} MiB`);
  return file;
}

async function fileHash(algo, file) {
  const h = createHash(algo);
  await pipeline(createReadStream(file), h);
  return h.digest();
}

// ---- Origins ---------------------------------------------------------------------

/** GitHub release: { bytes, origin, source, meta }. */
async function fromGithub(spec) {
  const [repo, tag] = spec.split('@');
  if (!/^[\w.-]+\/[\w.-]+$/.test(repo)) die(`github:${spec}: OWNER/REPO[@TAG]`);
  const rel = await getJson(`https://api.github.com/repos/${repo}/releases/${tag ? `tags/${encodeURIComponent(tag)}` : 'latest'}`);
  let apks = rel.assets.filter((a) => a.name.toLowerCase().endsWith('.apk'));
  if (opt.asset) apks = apks.filter((a) => new RegExp(opt.asset).test(a.name));
  else if (apks.length > 1) {
    const pick = apks.filter((a) => /arm64|universal/i.test(a.name));
    if (pick.length) apks = pick;
  }
  if (apks.length !== 1) die(`${repo} ${rel.tag_name}: ${apks.length} APK assets (${rel.assets.map((a) => a.name).join(', ')}): choose one with --asset`);
  const a = apks[0];
  const digest = /^sha256:([0-9a-f]{64})$/.exec(a.digest ?? '')?.[1];
  if (!digest) die(`${a.name}: the release API gives no SHA-256 digest for the asset; use url: with a digest from the project`);
  const file = await download(a.browser_download_url, `github-${repo.replace('/', '_')}-${rel.tag_name}-${a.name}`);
  const bytes = new Uint8Array(readFileSync(file));
  if (hex('sha256', bytes) !== digest) die(`${a.name}: SHA-256 differs from the release's digest ${digest}`);
  log(`verified: SHA-256 equal to the digest of the GitHub release ${rel.tag_name}`);
  return { bytes, origin: a.browser_download_url, source: `https://github.com/${repo}/tree/${rel.tag_name}`, meta: {} };
}

/** F-Droid main repository: { bytes, origin, source, meta }. */
async function fromFdroid(spec) {
  const [pkg, code] = spec.split('@');
  const repo = 'https://f-droid.org/repo';
  const entry = await getJson(`${repo}/entry.json`);
  mkdirSync(work, { recursive: true });
  const indexFile = join(work, `fdroid-index-v2-${entry.index.sha256}.json`);
  if (!existsSync(indexFile)) {
    log(`downloading the F-Droid index (${(entry.index.size / 2 ** 20).toFixed(0)} MiB)`);
    const r = await fetch(`${repo}${entry.index.name}`);
    if (!r.ok) die(`F-Droid index: status ${r.status}`);
    const b = new Uint8Array(await r.arrayBuffer());
    if (hex('sha256', b) !== entry.index.sha256) die('F-Droid index: SHA-256 differs from entry.json');
    writeFileSync(indexFile, b);
  }
  const index = JSON.parse(readFileSync(indexFile, 'utf8'));
  const app = index.packages[pkg];
  if (!app) die(`F-Droid has no package ${pkg}`);
  let want = code ? Number(code) : null;
  if (!want) want = (await getJson(`https://f-droid.org/api/v1/packages/${pkg}`)).suggestedVersionCode;
  const v = Object.values(app.versions).find((x) => x.manifest.versionCode === want);
  if (!v) die(`${pkg}: no version code ${want} in the index`);
  const origin = `${repo}${v.file.name}`;
  const file = await download(origin, `fdroid-${pkg}-${want}.apk`);
  const bytes = new Uint8Array(readFileSync(file));
  if (bytes.length !== v.file.size || hex('sha256', bytes) !== v.file.sha256) die(`${v.file.name}: size or SHA-256 differs from the F-Droid index`);
  log(`verified: SHA-256 equal to the F-Droid index entry (index-v2.json checked against entry.json, ${entry.index.sha256.slice(0, 12)}…)`);
  const md = app.metadata;
  const en = (o) => o?.['en-US'] ?? (o ? Object.values(o)[0] : undefined);
  const copyleft = /\b(A?GPL|LGPL|MPL|EUPL)/.test(md.license ?? '');
  const source = copyleft && v.src?.name ? `${repo}${v.src.name}` : md.sourceCode;
  return { bytes, origin, source, meta: { name: en(md.name), description: en(md.summary), license: md.license } };
}

/** Chromium snapshot for Android arm64 at the stable branch point: { bytes, origin, source, meta }. */
async function fromChromium(spec) {
  if (spec !== 'stable') die('chromium:stable is the only channel');
  const [rel] = await getJson('https://chromiumdash.appspot.com/fetch_releases?channel=Stable&platform=Android&num=1');
  const pos = rel.chromium_main_branch_position;
  log(`Chromium stable for Android: ${rel.version}, branch position ${pos}`);
  const bucket = 'https://www.googleapis.com/storage/v1/b/chromium-browser-snapshots/o';
  // The newest snapshot at or below the branch position: list by shrinking prefixes.
  let found = null;
  for (let digits = String(pos).length - 1; digits >= String(pos).length - 4 && !found; digits--) {
    const prefix = String(pos).slice(0, digits);
    const list = await getJson(`${bucket}?prefix=${encodeURIComponent(`Android_Arm64/${prefix}`)}&delimiter=/`);
    const revs = (list.prefixes ?? []).map((p) => Number(p.split('/')[1])).filter((n) => n <= pos).sort((a, b) => b - a);
    for (const r of revs) {
      const o = await getJson(`${bucket}/${encodeURIComponent(`Android_Arm64/${r}/chrome-android.zip`)}`).catch(() => null);
      if (o) {
        found = { rev: r, obj: o };
        break;
      }
    }
  }
  if (!found) die(`no Android_Arm64 snapshot near branch position ${pos}`);
  const { rev, obj } = found;
  log(`snapshot ${rev} (${pos - rev} commits before the branch point), chrome-android.zip ${(Number(obj.size) / 2 ** 20).toFixed(0)} MiB`);
  const revisions = await (await fetch(`https://commondatastorage.googleapis.com/chromium-browser-snapshots/Android_Arm64/${rev}/REVISIONS`)).json();
  const origin = `https://commondatastorage.googleapis.com/chromium-browser-snapshots/Android_Arm64/${rev}/chrome-android.zip`;
  const file = await download(origin, `chromium-android-arm64-${rev}.zip`);
  const md5 = (await fileHash('md5', file)).toString('base64');
  if (statSync(file).size !== Number(obj.size) || md5 !== obj.md5Hash) die(`${file}: size or MD5 differs from Google Cloud Storage's metadata`);
  log('verified: MD5 and size equal to the metadata Google Cloud Storage publishes for the object');
  const zip = new Uint8Array(readFileSync(file));
  const e = zipEntries(zip).get('chrome-android/apks/ChromePublic.apk');
  if (!e) die('chrome-android.zip without apks/ChromePublic.apk');
  const bytes = await zipRead(zip, e);
  return {
    bytes,
    origin: `${origin}#chrome-android/apks/ChromePublic.apk`,
    source: `https://chromium.googlesource.com/chromium/src/+/${revisions.got_revision}`,
    meta: { name: 'Chromium', license: 'BSD-3-Clause', description: `The open-source browser behind Chrome (snapshot ${rev}, at the ${rel.milestone} stable branch point). Large, and slow in the emulator.` },
  };
}

async function fromUrl(url) {
  const want = opt.sha256?.toLowerCase();
  if (!/^https:\/\//.test(url) || !/^[0-9a-f]{64}$/.test(want ?? '')) die('url:https://... needs --sha256 with the digest published by the project');
  const file = await download(url, `url-${hex('sha256', url).slice(0, 16)}.apk`);
  const bytes = new Uint8Array(readFileSync(file));
  if (hex('sha256', bytes) !== want) die(`${url}: SHA-256 differs from --sha256`);
  return { bytes, origin: url, source: null, meta: {} };
}

// ---- R2 ----------------------------------------------------------------------------

/** R2 settings from the env file (values never printed). */
function r2() {
  const file = process.env.VETRO_R2_ENV ?? join(homedir(), '.config/vetro/r2.env');
  if (!existsSync(file)) die(`${file} missing (R2 credentials)`);
  const env = {};
  for (const line of readFileSync(file, 'utf8').split('\n')) {
    const m = /^\s*(?:export\s+)?([A-Z0-9_]+)=(.*)$/.exec(line);
    if (m) env[m[1]] = m[2].trim().replace(/^(['"])(.*)\1$/, '$2');
  }
  for (const k of ['R2_ENDPOINT', 'R2_ACCESS_KEY_ID', 'R2_SECRET_ACCESS_KEY', 'R2_BUCKET', 'R2_PUBLIC_URL']) if (!env[k]) die(`${file}: ${k} missing`);
  const awsEnv = { ...process.env, AWS_ACCESS_KEY_ID: env.R2_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY: env.R2_SECRET_ACCESS_KEY, AWS_DEFAULT_REGION: 'auto' };
  const aws = (args, { input } = {}) => spawnSync('aws', ['--endpoint-url', env.R2_ENDPOINT, ...args], { env: awsEnv, input, encoding: 'buffer', maxBuffer: 64 << 20 });
  return { bucket: env.R2_BUCKET, publicUrl: env.R2_PUBLIC_URL.replace(/\/$/, ''), aws };
}

/**
 * Uploads a local file unless the same bytes are there (sha256 metadata);
 * other bytes under the same key are an error unless --replace or `mutable`.
 */
function put(R, file, key, type, cache, { mutable = false } = {}) {
  const sha = hex('sha256', readFileSync(file));
  const head = R.aws(['s3api', 'head-object', '--bucket', R.bucket, '--key', key, '--query', 'Metadata.sha256', '--output', 'text']);
  const have = head.status === 0 ? head.stdout.toString().trim() : '';
  if (have === sha) {
    log(`already on R2: ${key}`);
    return;
  }
  if (have && have !== 'None' && !opt.replace && !mutable) die(`${key} exists on R2 with another SHA-256 (${have}); --replace to overwrite`);
  const r = R.aws(['s3', 'cp', '--only-show-errors', '--metadata', `sha256=${sha}`, '--content-type', type, '--cache-control', cache, file, `s3://${R.bucket}/${key}`]);
  if (r.status !== 0) die(`upload of ${key} failed: ${r.stderr.toString().trim()}`);
  log(`uploaded: ${key}`);
}

/** The catalog currently on R2, or a new one. */
function currentCatalog(R) {
  const tmp = join(tmpdir(), `vetro-catalog-${process.pid}.json`);
  const r = R.aws(['s3', 'cp', '--only-show-errors', `s3://${R.bucket}/catalog/v1.json`, tmp]);
  if (r.status !== 0) {
    const err = r.stderr.toString();
    if (/404|Not Found|NoSuchKey|does not exist/i.test(err)) {
      log('no catalog on R2 yet: starting a new one');
      return { format: CATALOG_FORMAT, apps: [] };
    }
    die(`reading catalog/v1.json from R2 failed: ${err.trim()}`);
  }
  return JSON.parse(readFileSync(tmp, 'utf8'));
}

// ---- Main --------------------------------------------------------------------------

const [originSpec] = positionals;
if (!originSpec || !opt.id) die('usage: node tools/catalog/add.mjs <github:OWNER/REPO[@TAG] | fdroid:PACKAGE[@CODE] | chromium:stable | url:URL --sha256 HEX> --id ID [options]');
if (opt['min-image'] && !imageRelease(opt['min-image'])) die('--min-image: android-<x.y.z>_r<n>');
const colon = originSpec.indexOf(':');
const kind = originSpec.slice(0, colon);
const spec = originSpec.slice(colon + 1);
const fetched = await ({ github: fromGithub, fdroid: fromFdroid, chromium: fromChromium, url: fromUrl }[kind] ?? (() => die(`unknown origin ${kind}`)))(spec);
const { bytes } = fetched;
const info = await apkInfo(bytes);
log(`APK: ${info.package} ${info.versionName} (code ${info.versionCode}), minSdk ${info.minSdk}, targetSdk ${info.targetSdk}, native code ${info.abis.join(', ') || 'none'}, launcher ${info.launcher}`);
if (opt.package && opt.package !== info.package) die(`the APK is ${info.package}, expected ${opt.package}`);
if (info.abis.length && !info.abis.includes('arm64-v8a')) die(`native code only for ${info.abis.join(', ')}: Vetro's image is arm64-only`);
if (info.minSdk !== null && info.minSdk > IMAGE_SDK) die(`minSdk ${info.minSdk} > ${IMAGE_SDK} (the image's API level)`);
if (info.targetSdk !== null && info.targetSdk < MIN_TARGET_SDK) die(`targetSdk ${info.targetSdk} < ${MIN_TARGET_SDK}: Android 15 refuses to install it`);
if (!info.launcher) log('warning: no launcher activity (Open will ask the package manager)');

let icon = null;
if (opt.icon) {
  const b = new Uint8Array(readFileSync(opt.icon));
  const type = /\.svg$/i.test(opt.icon) ? 'image/svg+xml' : /\.webp$/i.test(opt.icon) ? 'image/webp' : /\.jpe?g$/i.test(opt.icon) ? 'image/jpeg' : 'image/png';
  icon = { bytes: b, type, path: opt.icon };
} else icon = await apkIcon(bytes, info);
if (icon) log(`icon: ${icon.path} (${icon.type}, ${icon.bytes.length} bytes${icon.density ? `, density ${icon.density}` : ''})`);
else log('warning: the APK icon is a vector drawable: the card shows none; --icon FILE to give one');

const id = opt.id;
const code = typeof info.versionCode === 'number' ? info.versionCode : 0;
const apkKey = `catalog/apks/${id}/${info.package}-${code}.apk`;
const ext = { 'image/png': 'png', 'image/webp': 'webp', 'image/jpeg': 'jpg', 'image/svg+xml': 'svg' }[icon?.type];
const iconKey = icon ? `catalog/icons/${id}-${code}.${ext}` : null;
const raw = {
  id,
  name: opt.name ?? fetched.meta.name ?? info.label ?? id,
  package: info.package,
  version: String(info.versionName ?? code),
  versionCode: typeof info.versionCode === 'number' ? info.versionCode : null,
  apk: apkKey.replace(/^catalog\//, ''),
  size: bytes.length,
  sha256: hex('sha256', bytes),
  license: opt.license ?? fetched.meta.license,
  source: opt.source ?? fetched.source,
  icon: iconKey ? iconKey.replace(/^catalog\//, '') : null,
  description: opt.description ?? fetched.meta.description,
  minImage: opt['min-image'] ?? null,
  advanced: opt.advanced,
  origin: fetched.origin,
  added: new Date().toISOString().slice(0, 10),
};
parseEntry(raw, 'https://example.invalid/catalog/v1.json');

mkdirSync(work, { recursive: true });
const apkFile = join(work, `${id}.apk`);
writeFileSync(apkFile, bytes);
let iconFile = null;
if (icon) {
  iconFile = join(work, `${id}.${ext}`);
  writeFileSync(iconFile, icon.bytes);
}

const R = opt['dry-run'] ? null : r2();
const catalog = R ? currentCatalog(R) : existsSync(join(work, 'v1.json')) ? JSON.parse(readFileSync(join(work, 'v1.json'), 'utf8')) : { format: CATALOG_FORMAT, apps: [] };
const at = catalog.apps.findIndex((a) => a.id === id);
if (at >= 0) catalog.apps[at] = raw;
else catalog.apps.push(raw);
catalog.format = CATALOG_FORMAT;
catalog.updated = new Date().toISOString();
const checked = parseCatalog(catalog, `${R?.publicUrl ?? 'https://example.invalid'}/catalog/v1.json`);
if (checked.problems.length) die(`the new catalog has problems: ${checked.problems.join('; ')}`);
const json = `${JSON.stringify(catalog, null, 2)}\n`;
writeFileSync(join(work, 'v1.json'), json);
log(`catalog: ${checked.apps.length} app(s), written to target/catalog/v1.json`);
if (!R) {
  log('dry run: nothing uploaded');
  process.exit(0);
}
put(R, apkFile, apkKey, 'application/vnd.android.package-archive', 'public, max-age=31536000, immutable');
if (iconFile) put(R, iconFile, iconKey, icon.type, 'public, max-age=31536000, immutable');
// The catalog last, so it never names an object that isn't there yet.
put(R, join(work, 'v1.json'), 'catalog/v1.json', 'application/json', 'no-cache', { mutable: true });
log(`${R.publicUrl}/catalog/v1.json`);
