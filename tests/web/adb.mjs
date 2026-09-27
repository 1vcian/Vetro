// The ADB client in web/node/adb.mjs (M5/M6, ADR 0028) against the in-memory
// fake adbd of tests/web/fake-adbd.mjs, which speaks AOSP's protocol and
// checks what a real adbd would reject. The test against a real adbd
// (Android in the guest) is in tests/web/android.mjs (VETRO_ANDROID=1) and in
// the long Chrome test.

import { ADB, AdbClient, AdbKey, adbVerify, decodeMessages, encodeMessage, parseBanner } from '../../web/node/adb.mjs';
import { Fail } from './lib.mjs';
import { drive, FakeAdbd, Pipe } from './fake-adbd.mjs';

const enc = new TextEncoder();
const dec = new TextDecoder();

function check(cond, what) {
  if (!cond) throw new Fail(what);
  console.log(`ok: ${what}`);
}

async function main() {
  // Banner.
  const b = parseBanner('device::ro.product.name=x;ro.product.model=Vetro arm64;features=shell_v2,cmd\0');
  check(b.kind === 'device' && b.props['ro.product.model'] === 'Vetro arm64' && b.features.join() === 'shell_v2,cmd', 'device banner');

  // Device without authentication (ro.adb.secure=0, userdebug).
  const adbd = new FakeAdbd();
  const c = new AdbClient(new Pipe(adbd));
  const banner = await drive(c, c.connect());
  check(banner.props['ro.product.name'] === 'vetro_arm64' && c.maxData === 4096, 'CNXN: banner and maximum data size');
  const devs = await drive(c, c.devices());
  check(devs.length === 1 && devs[0].serial === 'VETRO00001' && devs[0].model === 'Vetro arm64' && devs[0].state === 'device', 'devices: serial, model, state');
  const r = await drive(c, c.shell('echo ciao vetro'));
  check(r.stdout === 'ciao vetro\n' && r.exitCode === 0, 'shell v2: output and exit code');
  const e = await drive(c, c.shell('nonesiste'));
  check(e.exitCode === 127 && e.stderr.includes('not found') && e.stdout === '', 'shell v2: separate stderr, exit code 127');
  // A fake 300 KB APK: many WRTEs, one at a time, 64 KiB DATA pieces.
  const apk = new Uint8Array(300_000).map((_, i) => (i * 7) & 0xff);
  apk.set(enc.encode('PK\x03\x04'));
  const out = await drive(c, c.install(apk, { name: 'prova.apk' }));
  check(out === 'Success' && adbd.installed?.length === apk.length && adbd.installed.every((x, i) => x === apk[i]), 'install: 300 KB push and pm install');
  check(adbd.opened.includes('sync:') && adbd.quit && adbd.files.size === 0, 'install: sync with QUIT, temporary file removed');
  check(adbd.commands.some((x) => x.startsWith('pm install -r /data/local/tmp/prova.apk')), 'install: pm install -r');
  // Push progress (the app catalog's bar): monotonic, in 1 MiB batches, ending at the total.
  const big = new Uint8Array(2_500_000).map((_, i) => (i * 13) & 0xff);
  big.set(enc.encode('PK\x03\x04'));
  const seen = [];
  await drive(c, c.install(big, { name: 'grande.apk', onProgress: (sent, total) => seen.push([sent, total]) }));
  check(adbd.installed.length === big.length && adbd.installed.every((x, i) => x === big[i]), 'install with progress: same bytes on the device');
  check(seen.length === 3 && seen.every(([s, t], i) => t === big.length && (i === 0 || s > seen[i - 1][0])) && seen.at(-1)[0] === big.length &&
    seen[0][0] === 1 << 20, `install with progress: ${JSON.stringify(seen)}`);
  const bad = await drive(c, c.install(enc.encode('not an apk')).then(() => null, (err) => err));
  check(bad instanceof Error && bad.message.includes('INSTALL_FAILED_INVALID_APK'), 'install: pm error reported');
  const fail = await drive(c, c.push('/proc/x', enc.encode('x')).then(() => null, (err) => err));
  check(fail instanceof Error && fail.message.includes('read-only file system'), 'push: device FAIL reported');
  await drive(c, c.push('/sdcard/a.txt', enc.encode('ciao'), { mode: 0o600, mtime: 1234 }));
  const f = adbd.files.get('/sdcard/a.txt');
  check(f && dec.decode(f.bytes) === 'ciao' && f.mode === 0o600 && f.mtime === 1234, 'push: content, mode and mtime');

  // Device without shell_v2: raw shell:.
  const old = new FakeAdbd({ features: 'cmd' });
  const c2 = new AdbClient(new Pipe(old, { chunk: 7 }));
  await drive(c2, c2.connect());
  const r2 = await drive(c2, c2.shell('echo vecchio'));
  check(r2.stdout === 'vecchio\n' && r2.exitCode === null && old.opened[0] === 'shell:echo vecchio', 'shell without v2 (bytes in pieces of 7)');

  // AUTH: signature accepted.
  const key = await AdbKey.generate();
  const sec = new FakeAdbd({ secure: true });
  sec.key = key;
  const c3 = new AdbClient(new Pipe(sec), { key });
  await drive(c3, c3.connect());
  check(sec.authorized && adbVerify(key.n, key.e, sec.token, sec.signature), 'AUTH: RSA signature of the token verified');
  // AUTH: signature rejected, then the public key (adb_keys format).
  const sec2 = new FakeAdbd({ secure: true, acceptSignature: false });
  const c4 = new AdbClient(new Pipe(sec2), { key: AdbKey.fromJSON(JSON.parse(JSON.stringify(key.toJSON()))) });
  await drive(c4, c4.connect());
  const [b64key, name] = sec2.publicKey.split(' ');
  const raw = Uint8Array.from(atob(b64key), (ch) => ch.charCodeAt(0));
  const v = new DataView(raw.buffer);
  let n = 0n;
  for (let i = 63; i >= 0; i--) n = (n << 32n) | BigInt(v.getUint32(8 + 4 * i, true));
  const n0inv = BigInt(v.getUint32(4, true));
  check(name === 'vetro@browser' && raw.length === 524 && v.getUint32(0, true) === 64 && n === key.n && v.getUint32(520, true) === 65537 &&
    ((n % (1n << 32n)) * n0inv + 1n) % (1n << 32n) === 0n, 'AUTH: public key in adb format (n, n0inv, e)');
  // Without a key, AUTH is a clear error.
  const c5 = new AdbClient(new Pipe(new FakeAdbd({ secure: true })));
  const noKey = await drive(c5, c5.connect().then(() => null, (err) => err));
  check(noKey instanceof Error && noKey.message.includes('AUTH'), 'AUTH without a key: error');
  // Wrong magic: error.
  const broken = encodeMessage(ADB.A_OKAY, 1, 2);
  broken[20] ^= 1;
  let threw = false;
  try {
    decodeMessages(broken);
  } catch {
    threw = true;
  }
  check(threw, 'message with the wrong magic rejected');
  console.log('adb: all good');
}

main().catch((e) => {
  console.error(e instanceof Fail ? `FAILED: ${e.message}` : e);
  process.exit(1);
});
