// Headless Chrome driven with the DevTools protocol (Node 22 WebSocket,
// no npm dependencies), shared by the browser tests (browser.mjs, pages.mjs).

import { spawn } from 'node:child_process';
import { existsSync, rmSync } from 'node:fs';
import { Fail } from './lib.mjs';

const CANDIDATES = [
  process.env.VETRO_CHROME,
  '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
  '/usr/bin/google-chrome',
  '/usr/bin/google-chrome-stable',
  '/usr/bin/chromium',
  '/usr/bin/chromium-browser',
].filter(Boolean);

export class Cdp {
  #ws;
  #id = 0;
  #pending = new Map();

  static async connect(url) {
    const ws = new WebSocket(url);
    await new Promise((ok, ko) => {
      ws.onopen = ok;
      ws.onerror = () => ko(new Fail(`DevTools: connection to ${url} failed`));
    });
    return new Cdp(ws);
  }

  constructor(ws) {
    this.#ws = ws;
    ws.onmessage = (e) => {
      const msg = JSON.parse(e.data);
      const p = msg.id !== undefined && this.#pending.get(msg.id);
      if (!p) return;
      this.#pending.delete(msg.id);
      if (msg.error) p.ko(new Fail(`DevTools ${p.method}: ${msg.error.message}`));
      else p.ok(msg.result);
    };
  }

  send(method, params = {}, sessionId = undefined) {
    const id = ++this.#id;
    this.#ws.send(JSON.stringify({ id, method, params, sessionId }));
    return new Promise((ok, ko) => this.#pending.set(id, { ok, ko, method }));
  }

  close() {
    this.#ws.close();
  }
}

export class Page {
  constructor(cdp, sessionId) {
    this.cdp = cdp;
    this.s = sessionId;
  }

  async eval(expr) {
    const r = await this.cdp.send('Runtime.evaluate', { expression: expr, returnByValue: true, awaitPromise: true }, this.s);
    if (r.exceptionDetails) throw new Fail(`in the page: ${r.exceptionDetails.exception?.description ?? r.exceptionDetails.text}`);
    return r.result.value;
  }

  consoleText() {
    // '' while the page is still loading (after a navigation the element may not exist yet).
    return this.eval("document.getElementById('console')?.textContent ?? ''");
  }

  async waitFor(what, pred, ms = 120_000) {
    const t0 = Date.now();
    for (;;) {
      const v = await pred();
      if (v) return v;
      if (Date.now() - t0 > ms) {
        const status = await this.eval("document.getElementById('status')?.textContent ?? '(no status)'");
        const tail = (await this.consoleText()).split('\n').slice(-15).join('\n');
        throw new Fail(`${what}: did not arrive in ${ms / 1000} s (state: ${status})\n${tail}`);
      }
      await new Promise((ok) => setTimeout(ok, 200));
    }
  }

  state() {
    return this.eval('window.vetroState');
  }

  async until(needle, after = 0) {
    return this.waitFor(JSON.stringify(needle), async () => {
      const t = await this.consoleText();
      const i = t.indexOf(needle, after);
      return i >= 0 ? i + needle.length : 0;
    });
  }

  async key(key, code, text) {
    const vk = key === 'Enter' ? 13 : key.length === 1 ? key.toUpperCase().charCodeAt(0) : 0;
    const base = { key, code, windowsVirtualKeyCode: vk, nativeVirtualKeyCode: vk };
    await this.cdp.send('Input.dispatchKeyEvent', { type: 'keyDown', ...base, ...(text ? { text } : {}) }, this.s);
    await this.cdp.send('Input.dispatchKeyEvent', { type: 'keyUp', ...base }, this.s);
  }

  /** Types a line into the page console, key by key. */
  async type(line) {
    await this.eval("document.getElementById('console').focus()");
    for (const ch of line) await this.key(ch, '', ch);
    await this.key('Enter', 'Enter');
  }
}

export function findChrome() {
  return CANDIDATES.find((p) => existsSync(p));
}

/**
 * Starts headless Chrome. `webgl`: WebGL2 on Chrome's own SwiftShader
 * (ANGLE), the same rasteriser on every host, for the rendering tests of the
 * accelerated path (ADR 0037); otherwise the GPU is off.
 */
export async function launch(chrome, profile, { webgl = false } = {}) {
  const gpu = webgl ? ['--use-angle=swiftshader', '--enable-unsafe-swiftshader'] : ['--disable-gpu'];
  const proc = spawn(chrome, [
    '--headless=new', '--remote-debugging-port=0', `--user-data-dir=${profile}`, '--no-first-run',
    '--no-default-browser-check', ...gpu, '--window-size=1400,1000',
  ], { stdio: ['ignore', 'ignore', 'pipe'], detached: true });
  const url = await new Promise((ok, ko) => {
    let err = '';
    const t = setTimeout(() => ko(new Fail(`Chrome does not answer:\n${err}`)), 30_000);
    proc.stderr.on('data', (d) => {
      err += d;
      const m = /DevTools listening on (ws:\/\/\S+)/.exec(err);
      if (m) {
        clearTimeout(t);
        ok(m[1]);
      }
    });
    proc.on('exit', (c) => ko(new Fail(`Chrome exited (${c}):\n${err}`)));
  });
  return { proc, cdp: await Cdp.connect(url) };
}

export async function openPage(cdp, url) {
  const { targetId } = await cdp.send('Target.createTarget', { url: 'about:blank' });
  const { sessionId } = await cdp.send('Target.attachToTarget', { targetId, flatten: true });
  const page = new Page(cdp, sessionId);
  await cdp.send('Page.enable', {}, sessionId);
  await cdp.send('Page.navigate', { url }, sessionId);
  return { page, targetId };
}

/**
 * Closes Chrome and deletes the profile. First Browser.close from the protocol,
 * then SIGKILL to the whole process group (Chrome is launched `detached`:
 * the child processes, which may still write to the profile, are in its
 * group). Deleting the profile is cleanup: if it fails we say so,
 * but the test doesn't turn red because of it.
 */
export async function closeChrome(proc, cdp, profile) {
  const exited = proc.exitCode !== null ? Promise.resolve() : new Promise((ok) => proc.once('exit', ok));
  await Promise.race([cdp.send('Browser.close').catch(() => {}), new Promise((ok) => setTimeout(ok, 3000))]);
  cdp.close();
  await Promise.race([exited, new Promise((ok) => setTimeout(ok, 5000))]);
  try {
    process.kill(-proc.pid, 'SIGKILL');
  } catch {
    // group already finished
  }
  await exited;
  await new Promise((ok) => setTimeout(ok, 300));
  try {
    rmSync(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
  } catch (e) {
    console.log(`warning: Chrome profile not deleted (${profile}): ${e.message}`);
  }
}
