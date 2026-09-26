#!/usr/bin/env node
// Server HTTP statico per l'app web di Vetro (web/app) e per i test
// (tests/web): file con Range (206, 416, suffissi), HEAD, ETag, e le
// intestazioni COOP/COEP che servono a SharedArrayBuffer e ai thread WASM
// (crossOriginIsolated). Nessuna dipendenza, solo node:http.
//
//   node tools/web-serve.mjs [--port 8080] [--host 127.0.0.1]
//
// Monta:
//   /                       -> web/            (l'app è in /app/)
//   /wasm/vetro_wasm.wasm   -> target/wasm32-unknown-unknown/release/vetro_wasm.wasm
//   /guest/                 -> target/guest-kernel/  (Image, initramfs.cpio.gz)
//   /disks/                 -> target/web-disks/     (immagini di disco da provare)
//   /aosp/                  -> target/aosp/out/      (immagine AOSP di Vetro: manifest.json,
//                              immagini, web/disk.json; tools/aosp/fetch.sh e web-disk.mjs)
//
// Come modulo: `serve({ mounts, port, onRequest })` restituisce
// `{ server, url, close() }` (porta 0 = una libera). `isolation: false`
// toglie COOP/COEP, come su GitHub Pages (tests/web/pages.mjs).

import { createServer } from 'node:http';
import { open, stat } from 'node:fs/promises';
import { dirname, extname, join, normalize, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const TYPES = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json',
  '.wasm': 'application/wasm',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
};

/** Intestazioni di isolamento: senza, niente SharedArrayBuffer. */
export const ISOLATION = {
  'Cross-Origin-Opener-Policy': 'same-origin',
  'Cross-Origin-Embedder-Policy': 'require-corp',
  'Cross-Origin-Resource-Policy': 'same-origin',
};

/**
 * Intervallo di un'intestazione Range su un file di `size` byte:
 * { start, end } (inclusivo), null se assente, 'invalid' se non
 * soddisfacibile. Un solo intervallo (niente multipart).
 */
export function parseRange(header, size) {
  if (!header) return null;
  const m = /^bytes=(\d*)-(\d*)$/.exec(header.trim());
  if (!m || (m[1] === '' && m[2] === '')) return 'invalid';
  let start;
  let end;
  if (m[1] === '') {
    const n = Number(m[2]);
    if (n === 0) return 'invalid';
    start = Math.max(0, size - n);
    end = size - 1;
  } else {
    start = Number(m[1]);
    end = m[2] === '' ? size - 1 : Math.min(Number(m[2]), size - 1);
  }
  if (start >= size || start > end) return 'invalid';
  return { start, end };
}

/** Il file di `mounts` ([prefisso URL, directory o file]) per `pathname`. */
function locate(mounts, pathname) {
  for (const [prefix, target] of mounts) {
    if (pathname === prefix || (prefix.endsWith('/') && pathname.startsWith(prefix))) {
      const rest = decodeURIComponent(pathname.slice(prefix.length));
      const base = resolve(target);
      const p = normalize(join(base, rest));
      if (p !== base && !p.startsWith(base + sep)) return null;
      return p;
    }
  }
  return null;
}

export function serve({ mounts, port = 0, host = '127.0.0.1', onRequest = () => {}, isolation = true }) {
  const server = createServer(async (req, res) => {
    const url = new URL(req.url, 'http://x');
    const range = req.headers.range ?? null;
    let file = locate(mounts, url.pathname);
    const head = { ...(isolation ? ISOLATION : {}), 'Accept-Ranges': 'bytes', 'Cache-Control': 'no-cache' };
    const done = (status, extra = {}, body = '') => {
      onRequest({ method: req.method, path: url.pathname, range, status });
      res.writeHead(status, { ...head, ...extra });
      res.end(body);
    };
    if (req.method !== 'GET' && req.method !== 'HEAD') return done(405);
    if (!file) return done(404);
    let st;
    try {
      st = await stat(file);
      if (st.isDirectory()) {
        file = join(file, 'index.html');
        st = await stat(file);
      }
    } catch {
      return done(404, {}, 'non trovato\n');
    }
    const size = st.size;
    const type = TYPES[extname(file)] ?? 'application/octet-stream';
    const etag = `"${size.toString(16)}-${Math.floor(st.mtimeMs).toString(16)}"`;
    const common = { 'Content-Type': type, ETag: etag, 'Last-Modified': st.mtime.toUTCString() };
    const r = parseRange(range, size);
    if (r === 'invalid') return done(416, { 'Content-Range': `bytes */${size}` });
    const [status, start, end] = r ? [206, r.start, r.end] : [200, 0, size - 1];
    const length = size === 0 ? 0 : end - start + 1;
    const extra = { ...common, 'Content-Length': String(length) };
    if (r) extra['Content-Range'] = `bytes ${start}-${end}/${size}`;
    onRequest({ method: req.method, path: url.pathname, range, status, bytes: req.method === 'HEAD' ? 0 : length });
    res.writeHead(status, { ...head, ...extra });
    if (req.method === 'HEAD' || length === 0) return res.end();
    const fh = await open(file, 'r');
    try {
      const buf = Buffer.alloc(length);
      await fh.read(buf, 0, length, start);
      res.end(buf);
    } finally {
      await fh.close();
    }
  });
  return new Promise((ok) => {
    // Connessioni tenute a lungo: un client fermo per secondi (la macchina
    // gira fra una richiesta e l'altra) non trova il socket chiuso.
    server.keepAliveTimeout = 120_000;
    server.listen(port, host, () => {
      const a = server.address();
      ok({ server, url: `http://${host}:${a.port}`, close: () => new Promise((c) => server.close(c)) });
    });
  });
}

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');

/** I montaggi dell'app (vedi l'intestazione). */
export function appMounts() {
  return [
    ['/wasm/vetro_wasm.wasm', join(root, 'target/wasm32-unknown-unknown/release/vetro_wasm.wasm')],
    ['/guest/', join(root, 'target/guest-kernel')],
    ['/disks/', join(root, 'target/web-disks')],
    ['/aosp/', join(root, 'target/aosp/out')],
    ['/', join(root, 'web')],
  ];
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const args = process.argv.slice(2);
  const opt = (n, d) => (args.includes(n) ? args[args.indexOf(n) + 1] : d);
  const { url } = await serve({ mounts: appMounts(), port: Number(opt('--port', '8080')), host: opt('--host', '127.0.0.1') });
  console.log(`Vetro: app su ${url}/app/  (kernel di prova in ${url}/guest/, .wasm in ${url}/wasm/vetro_wasm.wasm)`);
  console.log('Il .wasm si costruisce con: cargo build --release --target wasm32-unknown-unknown -p vetro-wasm');
}
