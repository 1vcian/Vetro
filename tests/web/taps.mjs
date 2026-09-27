// Taps on the test app (tests/apps/tocco: every touch flips its colour between
// blue and orange) with the time from the press to the screen changing, as
// the page measures it (`vetroState.perf`, web/app/main.mjs). Shared by
// android-chrome.mjs (nightly) and android-responsiveness.mjs.

import { colorSeen } from '../../web/node/android.mjs';

export const BLU = [0x15, 0x65, 0xc0];
export const ARANCIONE = [0xef, 0x6c, 0x00];

/** The pixel at the centre of the screen canvas. */
export const CENTRE = `(() => {
  const c = document.getElementById('screen');
  const d = c.getContext('2d').getImageData(c.width >> 1, c.height >> 1, 1, 1).data;
  return [d[0], d[1], d[2]];
})()`;

export const median = (xs) => {
  const s = xs.filter((x) => x !== null && x !== undefined).sort((a, b) => a - b);
  return s.length ? s[s.length >> 1] : null;
};

const sleep = (ms) => new Promise((ok) => setTimeout(ok, ms));

/**
 * `n` taps at the centre of the canvas, each waiting (up to `limitMs`) for the
 * centre to flip colour. For each: `frame_ms` press to the next frame drawn,
 * `centre_ms` press to the first frame covering the centre (the app's
 * redraw), `polled_ms` press to the flip seen by polling the pixel (a
 * cross-check, 20 ms granularity plus the protocol's round trips),
 * `input_wait_ms` how long the input waited in the Worker, `ripple` whether
 * the page drew its touch feedback. `null` times: not seen.
 */
export async function measureTaps(page, cdp, n, { limitMs = 120_000, pauseMs = 1500, log = console.log } = {}) {
  const r = await page.eval(`(() => {
    const c = document.getElementById('screen');
    c.scrollIntoView({ block: 'center' });
    const b = c.getBoundingClientRect();
    const x = b.left + b.width / 2, y = b.top + b.height / 2;
    const hit = document.elementFromPoint(x, y);
    return { x, y, hit: hit ? (hit.id || hit.className || hit.tagName) : null };
  })()`);
  if (r.hit !== 'screen') throw new Error(`the centre of the screen is covered by ${r.hit} (${r.x}, ${r.y})`);
  const taps = [];
  let colour = await page.eval(CENTRE);
  for (let i = 0; i < n; i++) {
    const want = colorSeen(colour, BLU) ? ARANCIONE : BLU;
    const before = await page.eval('({ n: window.vetroState.perf.taps.length, ripples: window.vetroState.perf.ripples ?? 0 })');
    const tt = Date.now();
    await cdp.send('Input.dispatchMouseEvent', { type: 'mousePressed', x: r.x, y: r.y, button: 'left', clickCount: 1 }, page.s);
    await sleep(80);
    await cdp.send('Input.dispatchMouseEvent', { type: 'mouseReleased', x: r.x, y: r.y, button: 'left', clickCount: 1 }, page.s);
    let polled = null;
    for (;;) {
      colour = await page.eval(CENTRE);
      if (colorSeen(colour, want)) {
        polled = Date.now() - tt;
        break;
      }
      if (Date.now() - tt > limitMs) break;
      await sleep(20);
    }
    const t = await page.eval(`(() => {
      const p = window.vetroState.perf, tap = p.taps[${before.n}], c = document.getElementById('screen');
      const cx = c.width >> 1, cy = c.height >> 1;
      const f = tap && p.frameLog.find((f) => f.t > tap.t && f.x <= cx && cx < f.x + f.w && f.y <= cy && cy < f.y + f.h);
      return { frameMs: tap?.frameMs ?? null, centreMs: f ? f.t - tap.t : null, wait: p.input?.lastMs ?? null, ripples: p.ripples ?? 0 };
    })()`);
    const one = { frame_ms: t.frameMs, centre_ms: t.centreMs, polled_ms: polled, input_wait_ms: t.wait, ripple: t.ripples > before.ripples };
    taps.push(one);
    const ms = (x) => (x === null ? '-' : x.toFixed(0));
    log(`tap ${i + 1}: first frame ${ms(one.frame_ms)} ms, centre changed ${ms(one.centre_ms)} ms (pixel polled ${ms(polled)} ms), ` +
      `waited in the Worker ${one.input_wait_ms === null ? '-' : one.input_wait_ms.toFixed(1)} ms${one.ripple ? ', ripple drawn' : ''}`);
    if (polled === null) break;
    await sleep(pauseMs);
  }
  return taps;
}
