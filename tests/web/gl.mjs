// Accelerated graphics (ADR 0037): the gfxstream decoder's op stream runs on
// WebGL2 in headless Chrome and draws what the guest asked for.
//
//   node tests/web/gl.mjs [--recording=FILE]
//
// 1. The synthetic scene (crates/vetro-gfxstream/src/guest.rs, `scene`),
//    encoded like the guest's gfxstream libraries encode it and recorded by
//    `cargo run -p vetro-gfxstream --example scene`, is replayed with
//    web/app/gl.mjs on Chrome's SwiftShader WebGL2 (the same rasteriser on
//    every host); the window's ColorBuffer, read back into guest memory by
//    the recorded TRANSFER_FROM_HOST, must equal the expected image
//    (quadrant colors, a BGRA CPU buffer sampled as an external texture).
// 2. With --recording=FILE (a `vetro boot --gpu=gfxstream --gl-record=FILE`
//    run), the whole guest session is replayed: no op may fail, the frames
//    are counted and the last one is written to FILE.png.
// Also checks that web/node/android.mjs and vetro-machine agree on the boot
// parameters of the accelerated path.

import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { deflateSync } from 'node:zlib';
import { serve } from '../../tools/web-serve.mjs';
import { closeChrome, findChrome, launch, openPage } from './chrome.mjs';
import { GFXSTREAM_PARAMS } from '../../web/node/android.mjs';
import { check, Fail, root, run } from './lib.mjs';

const arg = (name, def) => {
  const a = process.argv.find((s) => s.startsWith(`--${name}=`));
  return a ? a.slice(name.length + 3) : def;
};

/** PNG of RGBA rows from the top. */
function png(width, height, rgba) {
  const crcTable = Array.from({ length: 256 }, (_, n) => {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    return c >>> 0;
  });
  const crc = (b) => {
    let c = 0xffffffff;
    for (const x of b) c = crcTable[(c ^ x) & 0xff] ^ (c >>> 8);
    return (c ^ 0xffffffff) >>> 0;
  };
  const chunk = (type, data) => {
    const t = Buffer.from(type);
    const len = Buffer.alloc(4);
    len.writeUInt32BE(data.length);
    const c = Buffer.alloc(4);
    c.writeUInt32BE(crc(Buffer.concat([t, data])));
    return Buffer.concat([len, t, data, c]);
  };
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr.set([8, 6, 0, 0, 0], 8);
  const raw = Buffer.alloc((width * 4 + 1) * height);
  for (let y = 0; y < height; y++) raw.set(rgba.subarray(y * width * 4, (y + 1) * width * 4), y * (width * 4 + 1) + 1);
  return Buffer.concat([Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]), chunk('IHDR', ihdr), chunk('IDAT', deflateSync(raw)), chunk('IEND', Buffer.alloc(0))]);
}

run(async () => {
  // Boot parameters: the JS copy must match vetro-machine's.
  const rs = readFileSync(join(root, 'crates/vetro-machine/src/android/mod.rs'), 'utf8');
  const m = /pub const GFXSTREAM_PARAMS: &str = "([^"]*)";/s.exec(rs);
  check(m && m[1].replace(/\\\n/g, '').replace(/\s+/g, ' ') === GFXSTREAM_PARAMS, `GFXSTREAM_PARAMS differ:\n  rust: ${m?.[1]}\n  js:   ${GFXSTREAM_PARAMS}`);

  const out = join(root, 'target/gl');
  execFileSync('cargo', ['run', '-q', '-p', 'vetro-gfxstream', '--example', 'scene', '--', out], { cwd: root, stdio: 'inherit' });

  const chrome = findChrome();
  if (!chrome) {
    const msg = 'SKIP: Chrome not found (VETRO_CHROME): accelerated rendering not tried';
    if (process.env.VETRO_REQUIRE_BROWSER === '1') throw new Fail(msg);
    console.log(msg);
    return;
  }
  const recording = arg('recording', null);
  const mounts = [['/app/', join(root, 'web/app')], ['/data/', out]];
  if (recording) mounts.push(['/rec/', join(recording, '..')]);
  const srv = await serve({ mounts, isolation: false });
  const profile = mkdtempSync(join(tmpdir(), 'vetro-chrome-'));
  const { proc, cdp } = await launch(chrome, profile, { webgl: true });
  try {
    const { page } = await openPage(cdp, `${srv.url}/app/index.html?advanced=1`);
    await page.waitFor('page', () => page.eval("document.readyState === 'complete'"), 30_000);
    const r = await page.eval(`(async () => {
      const { WebGlExecutor, replayRecording } = await import('/app/gl.mjs');
      const canvas = new OffscreenCanvas(64, 64);
      let frames = 0;
      const exec = new WebGlExecutor(canvas, (b) => { frames++; b.close(); });
      const renderer = exec.gl.getParameter(exec.gl.RENDERER);
      const bytes = new Uint8Array(await (await fetch('/data/scene.bin')).arrayBuffer());
      const expected = new Uint8Array(await (await fetch('/data/scene-expected.rgba')).arrayBuffer());
      let last = null;
      const batches = replayRecording(exec, bytes, { onBatch: (o) => { if (o.length) last = o.slice(); } });
      let bad = 0, first = null;
      for (let i = 0; i < expected.length; i++) {
        if (Math.abs(expected[i] - (last?.[i] ?? -99)) > 2) {
          bad++;
          if (first === null) first = { i, want: expected[i], got: last?.[i] };
        }
      }
      return { renderer, batches, frames, bad, first, log: exec.log, stats: exec.stats, got: last ? Array.from(last) : null };
    })()`);
    console.log(`WebGL2 renderer: ${r.renderer}; ${r.batches} batches, ${r.stats.ops} ops, ${r.frames} frame(s)`);
    if (r.got) writeFileSync(join(out, 'scene.png'), png(64, 64, Uint8Array.from(r.got)));
    check(r.log.length === 0, `executor errors: ${r.log.join('\n')}`);
    check(r.frames === 1, `expected one presented frame, got ${r.frames}`);
    check(r.bad === 0, `scene differs from the expected image in ${r.bad} bytes (first: ${JSON.stringify(r.first)}); see target/gl/scene.png`);
    console.log('synthetic scene: identical to the expected image (clears, scissor, BGRA buffer as external texture, client-side arrays)');

    // Snapshot with GPU resources: phase A in WebGL gives the read-back
    // contents, phase B (Rust) saves with them and restores into a new
    // renderer, which the guest then uses to redraw; replayed on a fresh
    // WebGL2 context, the window must be the expected one.
    execFileSync('cargo', ['run', '-q', '-p', 'vetro-gfxstream', '--example', 'snapshot', '--', 'save', out], { cwd: root, stdio: 'inherit' });
    const outs = await page.eval(`(async () => {
      const { WebGlExecutor, replayRecording } = await import('/app/gl.mjs');
      const exec = new WebGlExecutor(new OffscreenCanvas(64, 64), (b) => b.close());
      const bytes = new Uint8Array(await (await fetch('/data/snap-a.bin')).arrayBuffer());
      let last = null;
      replayRecording(exec, bytes, { onBatch: (o) => { last = o.slice(); } });
      exec.gl.getExtension('WEBGL_lose_context')?.loseContext();
      return { bytes: Array.from(last ?? []), log: exec.log };
    })()`);
    check(outs.log.length === 0, `executor errors in phase A: ${outs.log.join('\n')}`);
    writeFileSync(join(out, 'snap-outs.bin'), Uint8Array.from(outs.bytes));
    execFileSync('cargo', ['run', '-q', '-p', 'vetro-gfxstream', '--example', 'snapshot', '--', 'restore', out, join(out, 'snap-outs.bin')], { cwd: root, stdio: 'inherit' });
    const s = await page.eval(`(async () => {
      const { WebGlExecutor, replayRecording } = await import('/app/gl.mjs');
      let frames = 0;
      const exec = new WebGlExecutor(new OffscreenCanvas(64, 64), (b) => { frames++; b.close(); });
      const bytes = new Uint8Array(await (await fetch('/data/snap-b.bin')).arrayBuffer());
      const expected = new Uint8Array(await (await fetch('/data/snap-expected.rgba')).arrayBuffer());
      let last = null;
      replayRecording(exec, bytes, { onBatch: (o) => { if (o.length) last = o.slice(); } });
      let bad = 0, first = null;
      for (let i = 0; i < expected.length; i++) {
        if (Math.abs(expected[i] - (last?.[i] ?? -99)) > 2) { bad++; if (first === null) first = { i, want: expected[i], got: last?.[i] }; }
      }
      return { bad, first, frames, log: exec.log, got: last ? Array.from(last) : null };
    })()`);
    if (s.got) writeFileSync(join(out, 'snap.png'), png(64, 64, Uint8Array.from(s.got)));
    check(s.log.length === 0, `executor errors after the restore: ${s.log.join('\n')}`);
    check(s.bad === 0, `after snapshot and restore the redraw differs in ${s.bad} bytes (first: ${JSON.stringify(s.first)}); see target/gl/snap.png`);
    console.log(`snapshot with GPU resources: ${outs.bytes.length} bytes read back, restored on a fresh WebGL2 context, the guest redraws with its program and textures: identical`);

    if (recording) {
      const name = recording.split('/').pop();
      const rr = await page.eval(`(async () => {
        const { WebGlExecutor, replayRecording } = await import('/app/gl.mjs');
        const canvas = new OffscreenCanvas(1280, 800);
        let frames = 0, lastFrame = null;
        const exec = new WebGlExecutor(canvas, (b, w, h) => { frames++; lastFrame?.close(); lastFrame = b; });
        const bytes = new Uint8Array(await (await fetch('/rec/${name}')).arrayBuffer());
        const t0 = performance.now();
        const batches = replayRecording(exec, bytes);
        const ms = performance.now() - t0;
        let png = null;
        if (lastFrame) {
          const c = new OffscreenCanvas(lastFrame.width, lastFrame.height);
          c.getContext('2d').drawImage(lastFrame, 0, 0);
          const blob = await c.convertToBlob({ type: 'image/png' });
          png = Array.from(new Uint8Array(await blob.arrayBuffer()));
        }
        return { batches, frames, ms, log: exec.log, stats: exec.stats, png };
      })()`);
      console.log(`recording ${name}: ${rr.batches} batches, ${rr.stats.ops} ops, ${rr.frames} frames in ${(rr.ms / 1000).toFixed(1)} s`);
      if (rr.png) writeFileSync(`${recording}.png`, Buffer.from(rr.png));
      for (const l of rr.log.slice(0, 40)) console.log(`  executor: ${l}`);
      check(rr.frames > 0, 'no frame presented in the recording');
    }
  } finally {
    await closeChrome(proc, cdp, profile);
    srv.close();
  }
});
