#!/usr/bin/env node
// Background JIT compilation (ADR 0038) on the Android workload in headless
// Chrome: the app resumes the prebuilt home-screen snapshot (ADR 0031) from
// OPFS, with and without `jitbg=1`, and the guest instructions executed in
// the first minutes of wall time are compared. Both variants run at the same
// time (one Chrome each, same machine load), from copies of one profile that
// already holds the snapshot, so both start from the same state.
//
//   node tools/aosp/chrome-jit-bg.mjs [--secs=240] [--rounds=2]
//
// The first run downloads the prebuilt snapshot from R2 into a template
// profile (target/mc/chrome-template, kept). Needs Chrome (VETRO_CHROME) and
// port 8080 free (the bucket's CORS allows localhost:8080). Measurements in
// target/mc/chrome-jit-bg.json.

import { cpSync, existsSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { appMounts, serve } from '../web-serve.mjs';
import { Fail, root, run } from '../../tests/web/lib.mjs';
import { closeChrome, findChrome, launch, openPage } from '../../tests/web/chrome.mjs';
import { DEFAULT_MANIFEST } from '../../web/node/android.mjs';

const arg = (name, def) => {
  const a = process.argv.find((s) => s.startsWith(`--${name}=`));
  return a ? a.slice(name.length + 3) : def;
};
const SECS = Number(arg('secs', 240));
const ROUNDS = Number(arg('rounds', 2));
const dir = join(root, 'target/mc');
const template = join(dir, 'chrome-template');
const sleep = (ms) => new Promise((ok) => setTimeout(ok, ms));

/** Opens the app on `profile` and waits until the machine resumed from a snapshot. */
async function open(chrome, profile, url) {
  const { proc, cdp } = await launch(chrome, profile);
  const { page } = await openPage(cdp, url);
  const boot = await page.waitFor('restore', async () => {
    const st = await page.state();
    const pb = await page.eval('window.vetroAndroid ? window.vetroAndroid.state().prebuilt : null');
    if (pb?.state === 'failed' || pb?.state === 'missing') throw new Fail(`prebuilt: ${JSON.stringify(pb)}`);
    if (st?.stopped) throw new Fail(`machine stopped: ${JSON.stringify(st.stopped)}`);
    return st?.boot;
  }, 60 * 60_000);
  return { proc, cdp, page, boot };
}

/** Guest instructions over SECS seconds of wall time after the restore. */
async function measure(chrome, name, url) {
  const profile = join(dir, `chrome-run-${name}`);
  rmSync(profile, { recursive: true, force: true });
  cpSync(template, profile, { recursive: true });
  const { proc, cdp, page, boot } = await open(chrome, profile, url);
  try {
    const first = await page.waitFor('first stats', async () => (await page.state())?.stats, 60_000);
    const t0 = Date.now();
    const s0 = first.steps;
    const samples = [];
    while (Date.now() - t0 < SECS * 1000) {
      await sleep(10_000);
      const s = (await page.state()).stats;
      samples.push({ t: (Date.now() - t0) / 1000, steps: s.steps, modules: s.jit?.modules });
    }
    const last = samples.at(-1);
    const r = {
      name,
      restoreMs: boot.ms,
      wallSecs: last.t,
      guestSecs: (last.steps - s0) / 1e8,
      mips: (last.steps - s0) / last.t / 1e6,
      modules: last.modules,
      samples,
    };
    console.log(`${name}: ${r.guestSecs.toFixed(1)} s of guest time in ${r.wallSecs.toFixed(0)} s wall (${r.mips.toFixed(1)} MIPS), ${r.modules} modules, restore ${(boot.ms / 1000).toFixed(1)} s`);
    return r;
  } finally {
    await closeChrome(proc, cdp, profile);
  }
}

run(async () => {
  const chrome = findChrome();
  if (!chrome) throw new Fail('Chrome not found (VETRO_CHROME)');
  mkdirSync(dir, { recursive: true });
  const srv = await serve({ mounts: appMounts(), port: 8080 });
  const manifest = process.env.VETRO_ANDROID_MANIFEST ?? DEFAULT_MANIFEST;
  const url = `${srv.url}/app/?os=android&autostart=1&manifest=${encodeURIComponent(manifest)}`;
  const res = { manifest, secs: SECS, rounds: [] };
  try {
    if (!existsSync(template)) {
      console.log('template profile: downloading the prebuilt snapshot');
      const { proc, cdp, boot } = await open(chrome, template, url);
      console.log(`prebuilt restored in ${(boot.ms / 1000).toFixed(1)} s`);
      // Let the app finish writing the snapshot into OPFS.
      await sleep(20_000);
      await closeChrome(proc, cdp, null);
    }
    for (let i = 0; i < ROUNDS; i++) {
      const [sync, bg] = await Promise.all([measure(chrome, 'sync', url), measure(chrome, 'bg', `${url}&jitbg=1`)]);
      res.rounds.push({ sync, bg });
      console.log(`round ${i + 1}: background/sync guest time ${(bg.guestSecs / sync.guestSecs).toFixed(3)}`);
    }
  } finally {
    writeFileSync(join(dir, 'chrome-jit-bg.json'), JSON.stringify(res, null, 1));
    await srv.close();
  }
});
