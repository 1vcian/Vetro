// How the page starts (M10, zero-choice app): what to boot from the URL, and
// whether this browser can run Vetro. No DOM: unit-tested in
// tests/web/unit.mjs.
//
// The default page (`app/`) asks nothing: it starts the phone at once with
// the defaults (the default image, the light profile, the prebuilt snapshot
// or the user's saved one). `?os=linux` starts the small Linux test system
// instead. `?advanced=1` shows the full setup form (every option, for
// developers and tests) and starts only with `&autostart=1`. The other URL
// parameters (profile=, cpus=, gpu=, cold=, manifest=, kernel=, ...) work in
// both modes.

/** The Linux demo's kernel command line: no autotest, straight to the shell. */
export const LINUX_DEMO_CMDLINE = 'console=ttyAMA0 vetro.noautotest';

/**
 * The launch plan from the page's query string: `{ advanced, os, autostart,
 * cmdline }`. `os` is 'android' unless `os=linux`; `cmdline` is the Linux
 * demo's command line when the page picks it (the plain `?os=linux`), null
 * when the form's or the URL's applies.
 */
export function launchPlan(search) {
  const q = search instanceof URLSearchParams ? search : new URLSearchParams(search);
  const advanced = q.get('advanced') === '1';
  const os = q.get('os') === 'linux' ? 'linux' : 'android';
  return {
    advanced,
    os,
    autostart: !advanced || q.get('autostart') === '1',
    cmdline: !advanced && os === 'linux' && !q.has('cmdline') ? LINUX_DEMO_CMDLINE : null,
  };
}

/**
 * The smallest module with a SIMD instruction (wasm-feature-detect's probe):
 * `WebAssembly.validate` says whether the engine has fixed-width SIMD, which
 * the JIT's code uses.
 */
export const SIMD_PROBE = Uint8Array.of(
  0, 97, 115, 109, 1, 0, 0, 0, 1, 5, 1, 96, 0, 1, 123, 3, 2, 1, 0, 10, 10, 1, 8, 0, 65, 0, 253, 15, 253, 98, 11,
);

/** Browsers Vetro is tested on: desktop Chromium (Chrome, Edge, Chromium itself). */
const CHROMIUM_BRANDS = ['Google Chrome', 'Microsoft Edge', 'Chromium', 'HeadlessChrome'];

/** Memory the computer should have (GB, as `navigator.deviceMemory` reports it) for each system. */
export const MEMORY_GB = { android: 8, linux: 2 };

/** What the page needs to know about the browser, read from `g` (globalThis). */
export function probeBrowser(g = globalThis) {
  const nav = g.navigator ?? {};
  let simd = false;
  try {
    simd = typeof g.WebAssembly?.validate === 'function' && g.WebAssembly.validate(SIMD_PROBE);
  } catch {}
  return {
    wasm: typeof g.WebAssembly?.instantiate === 'function',
    simd,
    worker: typeof g.Worker === 'function',
    bigint: typeof g.BigInt64Array === 'function',
    opfs: typeof nav.storage?.getDirectory === 'function',
    // Chromium only, rounded down to a power of two; undefined elsewhere.
    memoryGB: typeof nav.deviceMemory === 'number' ? nav.deviceMemory : null,
    // `userAgentData` exists only in secure contexts: elsewhere the UA string says Chromium or not.
    brands: nav.userAgentData?.brands?.map((b) => b.brand) ?? (/\b(HeadlessChrome|Chrome|Chromium|Edg)\/\d/.test(nav.userAgent ?? '') ? ['Chromium'] : null),
    mobile: nav.userAgentData?.mobile ?? /Android|iPhone|iPad|Mobile/.test(nav.userAgent ?? ''),
  };
}

/**
 * The problems of a browser (`probeBrowser`) for the system `os`, as
 * `{ id, fatal, text }`: a fatal one means the machine cannot run here (the
 * page does not start it); the others are warnings shown above the screen
 * while the machine starts anyway.
 */
export function browserIssues(b, os = 'android') {
  const issues = [];
  const fatal = (id, text) => issues.push({ id, fatal: true, text });
  const warn = (id, text) => issues.push({ id, fatal: false, text });
  if (!b.wasm) fatal('wasm', 'This browser has no WebAssembly: Vetro cannot run here.');
  else if (!b.simd) fatal('simd', 'This browser has no WebAssembly SIMD: Vetro cannot run here. Update the browser, or use a recent desktop Chrome or Edge.');
  if (!b.worker) fatal('worker', 'This browser has no Web Workers: Vetro cannot run here.');
  if (!b.bigint) fatal('bigint', 'This browser is too old (no BigInt64Array): use a recent desktop Chrome or Edge.');
  if (b.mobile) warn('mobile', 'Phones and tablets are not supported: Vetro needs a desktop computer.');
  else if (!b.brands || !b.brands.some((x) => CHROMIUM_BRANDS.includes(x))) {
    warn('browser', 'Vetro is made for desktop Chrome or Edge: in this browser it may not start or may be very slow.');
  }
  if (!b.opfs) warn('opfs', "This browser has no private storage (OPFS): nothing is saved, and every visit starts over.");
  const need = MEMORY_GB[os] ?? MEMORY_GB.android;
  if (b.memoryGB !== null && b.memoryGB < need) {
    warn('memory', `This computer reports about ${b.memoryGB} GB of memory; ${os === 'linux' ? 'the Linux demo' : 'the phone'} needs at least ${need} GB and may not start.`);
  }
  return issues;
}
