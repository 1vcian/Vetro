#!/usr/bin/env node
// M5: vetro-wasm display and inputs (ABI 3 API), in Node without a browser.
// The same exercise as tests/boot/tests/devices.rs, but through the C API
// that the page uses:
//   - `vetro-dev drm-hold` draws the known pattern: the RGBA framebuffer read
//     with vetro_display_ptr must match pixel for pixel the pattern that
//     the native test checks (`expected_pixel`), the changed rectangle
//     must cover the scanout, the cursor must be the guest's; once the DRM is
//     closed the scanout turns off;
//   - keyboard and tablet via vetro_input_key / vetro_input_abs /
//     vetro_input_button, read by the guest with `vetro-dev input-read`;
//   - LED lit by the guest, read with vetro_input_leds;
//   - equal instructions in two runs.
//
//   node tests/web/devices.mjs [--no-jit]

import { check, compareNative, guestKernel, loadVetro, run, Session, SHELL_PROMPT } from './lib.mjs';

const jit = !process.argv.includes('--no-jit');
const BTN_LEFT = 0x110;
const EV_KEY = 1;
const EV_ABS = 3;
const LED_CAPSL = 1;

/** The pattern of `vetro-dev drm` in RGBA (tests/boot/tests/devices.rs). */
function expectedPixel(x, y) {
  if (x >= 32 && x < 96 && y >= 16 && y < 48) return [255, 255, 255, 255];
  return [x & 0xff, y & 0xff, (x ^ y) & 0xff, 255];
}

function events(text) {
  return text.split('\n').filter((l) => l.startsWith('vetro-dev: evento'));
}

async function session(x, kernel) {
  const s = new Session(x, kernel, { jit });
  const m = s.m;
  let at = await s.until(SHELL_PROMPT);
  check(m.displaySize() === null, 'scanout on before the DRM');

  // ---- virtio-gpu ----------------------------------------------------
  const updates0 = m.displayUpdates();
  m.consoleWrite('vetro-dev drm-hold\n');
  const ready = await s.until('VETRO-DRM-PRONTO', at);
  const size = m.displaySize();
  check(size && size.width === 1280 && size.height === 800, `scanout ${JSON.stringify(size)}, expected 1280x800`);
  check(m.displayUpdates() > updates0, 'update counter stuck');
  const px = m.displayPixels();
  let bad = 0;
  let first = null;
  for (let y = 0; y < size.height; y++) {
    for (let xx = 0; xx < size.width; xx++) {
      const e = expectedPixel(xx, y);
      const o = (y * size.width + xx) * 4;
      if (px[o] !== e[0] || px[o + 1] !== e[1] || px[o + 2] !== e[2] || px[o + 3] !== e[3]) {
        bad++;
        first ??= `(${xx}, ${y}): ${[...px.subarray(o, o + 4)]} instead of ${e}`;
      }
    }
  }
  check(bad === 0, `${bad} pixels differ from the guest's pattern, the first ${first}`);
  const dirty = m.displayTakeDirty();
  check(dirty && dirty.x === 0 && dirty.y === 0 && dirty.width === 1280 && dirty.height === 800,
    `changed rectangle ${JSON.stringify(dirty)}`);
  check(m.displayTakeDirty() === null, 'changed rectangle not cleared by the read');
  const copy = m.displayCopy({ x: 30, y: 16, width: 4, height: 2 });
  check(copy.length === 32 && copy[0] === 30 && copy[1] === 16 && copy[4 * 2] === 255 && copy[4 * 2 + 1] === 255,
    `wrong rectangle copy: ${[...copy.subarray(0, 12)]}`);
  const c = m.cursor();
  check(c && c.resource !== 0 && c.x === 100 && c.y === 50 && c.hotX === 0 && c.hotY === 0 && c.updates > 0,
    `cursor ${JSON.stringify(c)}`);
  const img = m.cursorImage();
  // ARGB8888 0xff000000 | i in the guest: pixel 65 is R=0 G=0 B=65.
  check(img && img.length === 64 * 64 * 4 && img[4 * 65] === 0 && img[4 * 65 + 2] === 65 && img[4 * 65 + 3] === 255,
    `wrong cursor image: ${img && [...img.subarray(4 * 65, 4 * 66)]}`);
  m.consoleWrite('\n');
  at = await s.until(SHELL_PROMPT, ready);
  check(m.displaySize() === null && m.displayPixels() === null, 'scanout still on after closing the DRM');

  // ---- virtio-input ---------------------------------------------------
  // event0 = tablet, event1 = keyboard (as in the native test).
  m.consoleWrite('vetro-dev input-read /dev/input/event1 4\n');
  let r = await s.until('VETRO-INPUT-PRONTO', at);
  check(m.key(30, true) && m.key(30, false), 'keyboard missing');
  at = await s.until(SHELL_PROMPT, r);
  const keys = events(s.text(r, at));
  const want = ['vetro-dev: evento 1 30 1', 'vetro-dev: evento 0 0 0', 'vetro-dev: evento 1 30 0', 'vetro-dev: evento 0 0 0'];
  check(JSON.stringify(keys) === JSON.stringify(want), `keyboard events: ${JSON.stringify(keys)}`);
  m.consoleWrite('vetro-dev input-read /dev/input/event0 5\n');
  r = await s.until('VETRO-INPUT-PRONTO', at);
  check(m.pointerMove(0x1234, 0x7000) && m.pointerButton(BTN_LEFT, true), 'tablet missing');
  at = await s.until(SHELL_PROMPT, r);
  const ptr = events(s.text(r, at));
  const e = (t, cd, v) => `vetro-dev: evento ${t} ${cd} ${v}`;
  const wantPtr = [e(EV_ABS, 0, 0x1234), e(EV_ABS, 1, 0x7000), e(0, 0, 0), e(EV_KEY, BTN_LEFT, 1), e(0, 0, 0)];
  check(JSON.stringify(ptr) === JSON.stringify(wantPtr), `tablet events: ${JSON.stringify(ptr)}`);
  [, at] = await s.command(`vetro-dev led /dev/input/event1 ${LED_CAPSL} 1`, at);
  check(m.leds === 1 << LED_CAPSL, `LED: ${m.leds}`);

  check(!s.text().includes('vetro-dev: ERRORE'), `errors in the guest:\n${s.tail()}`);
  await s.poweroff(at);
  const out = { steps: s.m.steps, log: s.text(), raw: s.log };
  m.free();
  return out;
}

run(async () => {
  const kernel = guestKernel();
  const { exports: x } = await loadVetro();
  const a = await session(x, kernel);
  const b = await session(x, kernel);
  check(a.steps === b.steps && a.log === b.log, `two runs differ: ${a.steps} and ${b.steps} instructions`);
  compareNative('devices', a.steps, a.raw);
  console.log(`display and inputs via the API: ok (${a.steps} instructions, 1280x800 pattern, cursor, keyboard, tablet, LED)`);
});
