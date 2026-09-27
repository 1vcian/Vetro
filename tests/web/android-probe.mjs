#!/usr/bin/env node
// Asks the guest what it is doing, in headless Chrome, from a Chrome profile
// with an Android snapshot (M5): restore, wait for adb, then run the given
// adb shell commands and print their output. A diagnostic, not a test.
//
//   VETRO_ANDROID=1 VETRO_ANDROID_PROFILE=DIR node tests/web/android-probe.mjs 'top -b -n 1 -m 12' ...

import { appMounts, serve } from '../../tools/web-serve.mjs';
import { Fail, run } from './lib.mjs';
import { findChrome, launch, openPage } from './chrome.mjs';
import { DEFAULT_MANIFEST } from '../../web/node/android.mjs';

if (process.env.VETRO_ANDROID !== '1' || !process.env.VETRO_ANDROID_PROFILE) {
  console.log('SKIP: Android probe (VETRO_ANDROID=1 and VETRO_ANDROID_PROFILE=DIR)');
  process.exit(0);
}

run(async () => {
  const chrome = findChrome();
  if (!chrome) throw new Fail('Chrome not found (VETRO_CHROME)');
  const srv = await serve({ mounts: appMounts(), port: Number(process.env.VETRO_WEB_PORT ?? 8080) });
  const url = `${srv.url}/app/?os=android&autostart=1&manifest=${encodeURIComponent(process.env.VETRO_ANDROID_MANIFEST ?? DEFAULT_MANIFEST)}${process.env.VETRO_APP_QUERY ?? ''}`;
  const { proc, cdp } = await launch(chrome, process.env.VETRO_ANDROID_PROFILE);
  try {
    const { page } = await openPage(cdp, url);
    await page.waitFor('restore', async () => (await page.state())?.boot, 10 * 60_000);
    const t0 = Date.now();
    await page.waitFor('adb', async () => (await page.eval('window.vetroAndroid.state()')).adb.state === 'ready', 20 * 60_000);
    console.log(`adb ready ${((Date.now() - t0) / 1000).toFixed(0)} s after the restore`);
    for (const cmd of process.argv.slice(2)) {
      const t = Date.now();
      const r = await page.eval(`window.vetroAndroid.shell(${JSON.stringify(cmd)})`);
      const s = (await page.state()).stats;
      console.log(`$ ${cmd}   (${((Date.now() - t) / 1000).toFixed(1)} s, ${s.mips.toFixed(0)} MIPS, guest ${s.guestSecs.toFixed(0)} s)\n${r.stdout}${r.stderr}`);
    }
  } finally {
    await Promise.race([cdp.send('Browser.close').catch(() => {}), new Promise((ok) => setTimeout(ok, 3000))]);
    cdp.close();
    try {
      process.kill(-proc.pid, 'SIGKILL');
    } catch {}
    await srv.close();
  }
});
