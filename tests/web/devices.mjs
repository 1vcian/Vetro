#!/usr/bin/env node
// M5: display e ingressi di vetro-wasm (API ABI 3), in Node senza browser.
// Lo stesso esercizio di tests/boot/tests/devices.rs, ma attraverso l'API C
// che usa la pagina:
//   - `vetro-dev drm-hold` disegna il motivo noto: il framebuffer RGBA letto
//     con vetro_display_ptr deve coincidere pixel per pixel col motivo che
//     il test nativo controlla (`expected_pixel`), il rettangolo cambiato
//     deve coprire lo scanout, il cursore dev'essere quello del guest; chiuso
//     il DRM lo scanout si spegne;
//   - tastiera e tablet via vetro_input_key / vetro_input_abs /
//     vetro_input_button, letti dal guest con `vetro-dev input-read`;
//   - LED acceso dal guest, letto con vetro_input_leds;
//   - istruzioni uguali in due esecuzioni.
//
//   node tests/web/devices.mjs [--no-jit]

import { check, compareNative, guestKernel, loadVetro, run, Session, SHELL_PROMPT } from './lib.mjs';

const jit = !process.argv.includes('--no-jit');
const BTN_LEFT = 0x110;
const EV_KEY = 1;
const EV_ABS = 3;
const LED_CAPSL = 1;

/** Il motivo di `vetro-dev drm` in RGBA (tests/boot/tests/devices.rs). */
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
  check(m.displaySize() === null, 'scanout acceso prima del DRM');

  // ---- virtio-gpu ----------------------------------------------------
  const updates0 = m.displayUpdates();
  m.consoleWrite('vetro-dev drm-hold\n');
  const ready = await s.until('VETRO-DRM-PRONTO', at);
  const size = m.displaySize();
  check(size && size.width === 1280 && size.height === 800, `scanout ${JSON.stringify(size)}, atteso 1280x800`);
  check(m.displayUpdates() > updates0, 'contatore degli aggiornamenti fermo');
  const px = m.displayPixels();
  let bad = 0;
  let first = null;
  for (let y = 0; y < size.height; y++) {
    for (let xx = 0; xx < size.width; xx++) {
      const e = expectedPixel(xx, y);
      const o = (y * size.width + xx) * 4;
      if (px[o] !== e[0] || px[o + 1] !== e[1] || px[o + 2] !== e[2] || px[o + 3] !== e[3]) {
        bad++;
        first ??= `(${xx}, ${y}): ${[...px.subarray(o, o + 4)]} invece di ${e}`;
      }
    }
  }
  check(bad === 0, `${bad} pixel diversi dal motivo del guest, il primo ${first}`);
  const dirty = m.displayTakeDirty();
  check(dirty && dirty.x === 0 && dirty.y === 0 && dirty.width === 1280 && dirty.height === 800,
    `rettangolo cambiato ${JSON.stringify(dirty)}`);
  check(m.displayTakeDirty() === null, 'rettangolo cambiato non azzerato dalla lettura');
  const copy = m.displayCopy({ x: 30, y: 16, width: 4, height: 2 });
  check(copy.length === 32 && copy[0] === 30 && copy[1] === 16 && copy[4 * 2] === 255 && copy[4 * 2 + 1] === 255,
    `copia di un rettangolo sbagliata: ${[...copy.subarray(0, 12)]}`);
  const c = m.cursor();
  check(c && c.resource !== 0 && c.x === 100 && c.y === 50 && c.hotX === 0 && c.hotY === 0 && c.updates > 0,
    `cursore ${JSON.stringify(c)}`);
  const img = m.cursorImage();
  // ARGB8888 0xff000000 | i nel guest: il pixel 65 è R=0 G=0 B=65.
  check(img && img.length === 64 * 64 * 4 && img[4 * 65] === 0 && img[4 * 65 + 2] === 65 && img[4 * 65 + 3] === 255,
    `immagine del cursore sbagliata: ${img && [...img.subarray(4 * 65, 4 * 66)]}`);
  m.consoleWrite('\n');
  at = await s.until(SHELL_PROMPT, ready);
  check(m.displaySize() === null && m.displayPixels() === null, 'scanout ancora acceso dopo la chiusura del DRM');

  // ---- virtio-input ---------------------------------------------------
  // event0 = tablet, event1 = tastiera (come nel test nativo).
  m.consoleWrite('vetro-dev input-read /dev/input/event1 4\n');
  let r = await s.until('VETRO-INPUT-PRONTO', at);
  check(m.key(30, true) && m.key(30, false), 'tastiera assente');
  at = await s.until(SHELL_PROMPT, r);
  const keys = events(s.text(r, at));
  const want = ['vetro-dev: evento 1 30 1', 'vetro-dev: evento 0 0 0', 'vetro-dev: evento 1 30 0', 'vetro-dev: evento 0 0 0'];
  check(JSON.stringify(keys) === JSON.stringify(want), `eventi della tastiera: ${JSON.stringify(keys)}`);
  m.consoleWrite('vetro-dev input-read /dev/input/event0 5\n');
  r = await s.until('VETRO-INPUT-PRONTO', at);
  check(m.pointerMove(0x1234, 0x7000) && m.pointerButton(BTN_LEFT, true), 'tablet assente');
  at = await s.until(SHELL_PROMPT, r);
  const ptr = events(s.text(r, at));
  const e = (t, cd, v) => `vetro-dev: evento ${t} ${cd} ${v}`;
  const wantPtr = [e(EV_ABS, 0, 0x1234), e(EV_ABS, 1, 0x7000), e(0, 0, 0), e(EV_KEY, BTN_LEFT, 1), e(0, 0, 0)];
  check(JSON.stringify(ptr) === JSON.stringify(wantPtr), `eventi del tablet: ${JSON.stringify(ptr)}`);
  [, at] = await s.command(`vetro-dev led /dev/input/event1 ${LED_CAPSL} 1`, at);
  check(m.leds === 1 << LED_CAPSL, `LED: ${m.leds}`);

  check(!s.text().includes('vetro-dev: ERRORE'), `errori nel guest:\n${s.tail()}`);
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
  check(a.steps === b.steps && a.log === b.log, `due esecuzioni diverse: ${a.steps} e ${b.steps} istruzioni`);
  compareNative('devices', a.steps, a.raw);
  console.log(`display e ingressi via API: ok (${a.steps} istruzioni, motivo 1280x800, cursore, tastiera, tablet, LED)`);
});
