// The ADB client in web/node/adb.mjs against a real adbd reached over TCP
// (for example Vetro's AOSP image under QEMU with hostfwd, or `vetro boot
// --hostfwd`): a manual test, not in CI (it needs a running Android).
//
//   node tests/web/adb-tcp.mjs HOST:PORT [APK]
//
// Runs devices, shell (output, stderr, exit code), push and, with an APK,
// install and opening of the main activity (am start).

import { connect } from 'node:net';
import { readFileSync } from 'node:fs';
import { AdbClient } from '../../web/node/adb.mjs';
import { apkInfo } from '../../web/node/apk.mjs';

const [hostPort, apkPath] = process.argv.slice(2);
const [host, port] = hostPort.split(':');

/** A Node socket with GuestSocket's interface (send/recv/state). */
class TcpTransport {
  #chunks = [];
  #closed = false;
  constructor(sock) {
    this.sock = sock;
    sock.on('data', (d) => this.#chunks.push(new Uint8Array(d)));
    sock.on('close', () => (this.#closed = true));
  }
  send(bytes) {
    this.sock.write(bytes);
    return bytes.length;
  }
  recv() {
    if (!this.#chunks.length) return new Uint8Array();
    const n = this.#chunks.reduce((s, c) => s + c.length, 0);
    const out = new Uint8Array(n);
    let at = 0;
    for (const c of this.#chunks.splice(0)) {
      out.set(c, at);
      at += c.length;
    }
    return out;
  }
  state() {
    return { state: this.#closed ? 'Closed' : 'Open', guestEof: this.#closed };
  }
}

const sock = connect({ host, port: Number(port) });
await new Promise((ok, ko) => sock.once('connect', ok).once('error', ko));
const adb = new AdbClient(new TcpTransport(sock));
const pump = setInterval(() => adb.pump(), 2);
const t = (p) => Promise.race([p, new Promise((_, ko) => setTimeout(() => ko(new Error('timed out')), 120_000))]);
try {
  const banner = await t(adb.connect());
  console.log('banner:', JSON.stringify(banner));
  console.log('devices:', JSON.stringify(await t(adb.devices())));
  console.log('shell:', JSON.stringify(await t(adb.shell('echo ciao; echo errore >&2; getprop sys.boot_completed; exit 3'))));
  await t(adb.push('/data/local/tmp/vetro-prova.txt', new TextEncoder().encode('ciao vetro\n')));
  console.log('push + cat:', JSON.stringify(await t(adb.shell('cat /data/local/tmp/vetro-prova.txt; rm /data/local/tmp/vetro-prova.txt'))));
  if (apkPath) {
    const apk = new Uint8Array(readFileSync(apkPath));
    const info = await apkInfo(apk);
    console.log('apk:', JSON.stringify(info));
    const t0 = Date.now();
    console.log('install:', await t(adb.install(apk, { name: `${info.package}.apk` })), `${Date.now() - t0} ms`);
    const act = (await t(adb.shell(`cmd package resolve-activity --brief -a android.intent.action.MAIN -c android.intent.category.LAUNCHER ${info.package} | tail -n 1`))).stdout.trim();
    console.log('activity:', act);
    console.log('am start:', JSON.stringify(await t(adb.shell(`am start -W -n ${act}`))));
  }
} finally {
  clearInterval(pump);
  sock.destroy();
}
