// Chrome headless pilotato col protocollo DevTools (WebSocket di Node 22,
// nessuna dipendenza npm), comune ai test nel browser (browser.mjs, pages.mjs).

import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
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
      ws.onerror = () => ko(new Fail(`DevTools: connessione a ${url} fallita`));
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
    if (r.exceptionDetails) throw new Fail(`nella pagina: ${r.exceptionDetails.exception?.description ?? r.exceptionDetails.text}`);
    return r.result.value;
  }

  consoleText() {
    return this.eval("document.getElementById('console').textContent");
  }

  async waitFor(what, pred, ms = 120_000) {
    const t0 = Date.now();
    for (;;) {
      const v = await pred();
      if (v) return v;
      if (Date.now() - t0 > ms) {
        const status = await this.eval("document.getElementById('status').textContent");
        const tail = (await this.consoleText()).split('\n').slice(-15).join('\n');
        throw new Fail(`${what}: non arrivato in ${ms / 1000} s (stato: ${status})\n${tail}`);
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

  /** Scrive una riga nella console della pagina, tasto per tasto. */
  async type(line) {
    await this.eval("document.getElementById('console').focus()");
    for (const ch of line) await this.key(ch, '', ch);
    await this.key('Enter', 'Enter');
  }
}

export function findChrome() {
  return CANDIDATES.find((p) => existsSync(p));
}

export async function launch(chrome, profile) {
  const proc = spawn(chrome, [
    '--headless=new', '--remote-debugging-port=0', `--user-data-dir=${profile}`, '--no-first-run',
    '--no-default-browser-check', '--disable-gpu', '--window-size=1400,1000',
  ], { stdio: ['ignore', 'ignore', 'pipe'] });
  const url = await new Promise((ok, ko) => {
    let err = '';
    const t = setTimeout(() => ko(new Fail(`Chrome non risponde:\n${err}`)), 30_000);
    proc.stderr.on('data', (d) => {
      err += d;
      const m = /DevTools listening on (ws:\/\/\S+)/.exec(err);
      if (m) {
        clearTimeout(t);
        ok(m[1]);
      }
    });
    proc.on('exit', (c) => ko(new Fail(`Chrome uscito (${c}):\n${err}`)));
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
