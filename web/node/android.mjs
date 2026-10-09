// Vetro's AOSP image in the browser and in Node (M5, ADR 0028): the boot
// phases read from the guest console, the home screen, the boot parameters.
// Uses no Node API.
//
// The GKI kernel writes to ttyAMA0 and init (with `printk.devkmsg=on`) writes
// to kmsg: the lines below are stable from one boot to the next and are
// enough to tell where the machine is. The order is the boot order; a phase
// seen implies the earlier ones.

/** Boot phases: [name, label, line matcher]. */
export const PHASES = [
  ['kernel', 'kernel', /Booting Linux on physical CPU/],
  ['init', 'init, first stage', /Run \/init as init process/],
  ['init2', 'init, second stage', /init: init second stage started/],
  ['zygote', 'zygote', /init: starting service 'zygote'/],
  ['surfaceflinger', 'graphics (surfaceflinger)', /init: starting service 'surfaceflinger'/],
  ['system_server', 'system_server', /\(system_server\)/],
  // `sys.boot_completed=1`: init queues the sys-boot-completed-set event (an
  // action of init.cutf_cvm.rc in Vetro's image).
  ['booted', 'boot finished', /\(sys\.boot_completed=1\)|sys-boot-completed-set/],
  // Not from the console: the focused window becomes the launcher (before it
  // there is FallbackHome, "Vetro is starting…" since image 64fcd35). Whoever has adb calls `mark`.
  ['home', 'home screen (launcher)', null],
];

const R2_AOSP = 'https://pub-06e88fdd7f374fffb06844d60083f2ae.r2.dev/aosp';

/**
 * The versions of Vetro's AOSP image the app offers, on R2 (ADR 0022, 0028,
 * 0030), newest first: the first is the default. `…-f08b79e` (ADR 0043) is
 * the slim image (no telephony, printing, backup, cameras, biometrics, demo
 * apps or unused HALs) on top of `…-8b519e5`'s idle guest (ADR 0040). The
 * older images (`…-8b519e5`, `…-64fcd35`, `…-bd09e2f`, `…-9d91633`) were
 * removed from R2 on 2026-10-09 to stay within the free tier.
 */
export const ANDROID_VERSIONS = [
  { version: 'android-15.0.0_r36-BP1A.250505.005.D1-f08b79e', label: 'AOSP 15, image f08b79e (default: slim)' },
].map((v) => ({ ...v, manifest: `${R2_AOSP}/${v.version}/manifest.json` }));

/** The version of Vetro's AOSP image the app uses by default. */
export const DEFAULT_MANIFEST = ANDROID_VERSIONS[0].manifest;

/**
 * Bootloader parameters for Vetro's AOSP image (ADR 0028): `nokaslr` like
 * tools/aosp/vetro.sh.
 */
export const ANDROID_PARAMS = 'nokaslr';

/**
 * Boot parameters that switch the image from SwiftShader to gfxstream GLES
 * over virtio-gpu 3D (ADR 0037): the same as
 * `vetro_machine::android::GFXSTREAM_PARAMS` (checked by tests/web/gl.mjs).
 */
export const GFXSTREAM_PARAMS = 'androidboot.hardware.egl=emulation androidboot.hardware.gltransport=virtio-gpu-pipe androidboot.hardware.hwcomposer.mode=client androidboot.hardware.hwcomposer.display_framebuffer_format=rgba androidboot.opengles.version=196608';

/**
 * The machine the app builds for Vetro's AOSP image (ADR 0028): the
 * prebuilt snapshot (ADR 0031) is made with exactly this machine, and its key
 * contains it. `files`: virtio-vsock for the file manager (on by default in
 * the app).
 */
export const ANDROID_MACHINE = { ramMiB: 2048, width: 1280, height: 800, pointer: 'multitouch', net: true, files: true };

/**
 * The disk of the AOSP image in the app (the `web/disk.json` map): 1 MiB
 * blocks, 64 of them in the module's memory (the rest in the OPFS cache),
 * writable (copy-on-write in memory). The same for the prebuilt snapshot.
 */
export const ANDROID_DISK = { blockSize: 1 << 20, maxBlocks: 64, readOnly: false, readahead: 1 };

/**
 * vetro-wasm device bits for a machine configuration (`DEV` of vetro.mjs):
 * the same function for the app's Worker and the prebuilt snapshot tool.
 */
export function machineDevices(DEV, c) {
  let devices = DEV.GPU | DEV.KEYBOARD;
  devices |= c.pointer === 'multitouch' ? DEV.MULTITOUCH : DEV.TABLET;
  if (c.net) devices |= DEV.NET;
  if (c.files) devices |= DEV.VSOCK;
  if (c.gpu === 'webgl') devices |= DEV.GPU_3D;
  return devices;
}

/** adb command that keeps the screen on and wakes it (after connecting). */
export const ANDROID_WAKE = 'svc power stayon true; settings put system screen_off_timeout 2147483647; input keyevent KEYCODE_WAKEUP; wm dismiss-keyguard';

/**
 * Graphics settings (ADR 0039), adb commands after connecting, kept in the
 * guest's /data and idempotent (they run again after every connection):
 * `light` (the app's default and the prebuilt snapshot's): no window and
 * transition animations (full-screen frames the emulated phone draws slowly;
 * they also lengthen opening an app), in-app animations at half their length
 * (motion cues stay), no window blurs; `full`: Android's own values.
 */
export const ANDROID_GRAPHICS = {
  light: 'settings put global window_animation_scale 0; settings put global transition_animation_scale 0; '
    + 'settings put global animator_duration_scale 0.5; settings put global disable_window_blurs 1',
  full: 'settings put global window_animation_scale 1; settings put global transition_animation_scale 1; '
    + 'settings put global animator_duration_scale 1; settings put global disable_window_blurs 0',
};

/**
 * The user's first steps on the home screen, run once with adb before the
 * prebuilt snapshot (`tools/aosp/prebuilt-snapshot.mjs --warm`): the app
 * drawer opened with a swipe and closed, Settings opened and closed, back to
 * the home screen. `sleep`s are guest time, long enough on one emulated CPU.
 */
export const ANDROID_WARMUP = (width, height) => {
  const x = Math.round(width / 2);
  return [
    `input swipe ${x} ${Math.round(height * 0.85)} ${x} ${Math.round(height * 0.25)} 300; sleep 90`,
    'input keyevent KEYCODE_HOME; sleep 30',
    'am start -W -n com.android.settings/.Settings | tail -n 3; sleep 60',
    'input keyevent KEYCODE_HOME; sleep 30',
    `input swipe ${x} ${Math.round(height * 0.85)} ${x} ${Math.round(height * 0.25)} 300; sleep 45`,
    'input keyevent KEYCODE_HOME; sleep 30; dumpsys window | grep mCurrentFocus',
  ];
};

/** Guest time after the home screen is drawn before the Android snapshot. */
export const ANDROID_HOME_NS = 5_000_000_000n;
/** At most this much guest time from the launcher being focused to the home screen drawn. */
export const HOME_DRAW_NS = 300_000_000_000n;
/** How often (guest time) adb is asked whether the home screen is up. */
export const HOME_POLL_NS = 5_000_000_000n;

/**
 * In-guest compaction before a snapshot (ADR 0031): the clean page cache is
 * dropped and free memory is filled with zeros (a file in /dev, which is
 * tmpfs, then deleted), so those pages are zero in the snapshot. Keeps 96 MiB
 * free for the file itself and lmkd. Prints MemFree and Cached after.
 */
export const ANDROID_COMPACT = "su 0 sh -c 'sync; echo 3 > /proc/sys/vm/drop_caches; free=$(awk \"/MemFree/ {print int(\\$2/1024) - 96}\" /proc/meminfo); dd if=/dev/zero of=/dev/vetro-zeros bs=1M count=$free 2>&1 | tail -1; rm -f /dev/vetro-zeros; grep -E \"MemFree|^Cached\" /proc/meminfo'";

/**
 * What makes an Android snapshot applicable (ADR 0031), the same for the
 * app's own snapshots in OPFS and for the prebuilt one on R2: snapshot format
 * and machine configuration hash of vetro-wasm, the machine (RAM, screen,
 * devices), the image version and the sha256 of its boot images, the
 * bootloader parameters, and the disk map (sha256 of its text and size, not
 * its URL: the same image from R2 or from a local server gives the same key).
 * `snapshotKey` (persist.mjs) of this object is the key.
 */
export function androidKeyParts({ format, configHash, ramMiB, width, height, devices, version, images, params, disk }) {
  return {
    kind: 'android',
    format,
    config: configHash === null || configHash === undefined ? null : configHash.toString(16).padStart(16, '0'),
    ramMiB,
    width,
    height,
    devices,
    android: version,
    images,
    params,
    disk,
  };
}

/**
 * Whether pixel `px` shows app colour `rgb` (within `tol` per channel), in
 * the right order: the images since `…-64fcd35` (ADR 0032) convert their
 * frame to the scanout's BGRX format. The previous image (`…-bd09e2f`)
 * swapped red and blue, and tests accepted both orders until it was the
 * default.
 */
export function colorSeen(px, rgb, tol = 8) {
  return !!px && rgb.every((v, i) => Math.abs(v - px[i]) <= tol);
}

/**
 * adb command for the focused window; the home screen is up if it contains
 * "launcher". The dump is read to the end: `grep -m1` closed the pipe at the
 * first match, dumpsys died with its dump transaction still open in
 * system_server, and every poll left binder errors on the console ("release
 * ... still active", "reply target not found") and a dead process. (The
 * `windows` section alone does not print `mCurrentFocus` on Android 15.)
 */
export const HOME_QUERY = 'dumpsys window | grep mCurrentFocus';
export const isHome = (out) => /launcher/i.test(out);

/**
 * Distinct colours on a 16-pixel grid of an RGBA image: with the launcher
 * focused, the scanout can still show FallbackHome for tens of seconds of
 * guest time (text on black, about ten colours); the drawn home screen has
 * many more (icons, wallpaper).
 */
export function gridColors(px, width, height) {
  const seen = new Set();
  for (let y = 0; y < height; y += 16) {
    for (let x = 0; x < width; x += 16) {
      const o = (y * width + x) * 4;
      seen.add((px[o] << 16) | (px[o + 1] << 8) | px[o + 2]);
    }
  }
  return seen.size;
}

/** Grid colours above which the home screen counts as drawn. */
export const HOME_MIN_COLORS = 40;

/** Follows the console and tells when a new phase starts. */
export class BootProgress {
  /** Index of the last phase seen (-1 = none). */
  index = -1;
  /** [{ phase, label, guestSecs }] in order. */
  events = [];
  #line = '';

  get phase() {
    return this.index < 0 ? null : PHASES[this.index][0];
  }

  get label() {
    return this.index < 0 ? 'waiting for the kernel' : PHASES[this.index][1];
  }

  /** New console text (string); returns the new phases. */
  feed(text, guestSecs) {
    const out = [];
    const lines = (this.#line + text).split('\n');
    this.#line = lines.pop();
    // A very long line without a newline must not grow forever.
    if (this.#line.length > 4096) this.#line = this.#line.slice(-4096);
    for (const l of lines) {
      for (let k = this.index + 1; k < PHASES.length; k++) {
        if (PHASES[k][2]?.test(l)) {
          // Skipped phases (lost lines) count as seen now.
          for (let j = this.index + 1; j <= k; j++) {
            const ev = { phase: PHASES[j][0], label: PHASES[j][1], guestSecs };
            this.events.push(ev);
            out.push(ev);
          }
          this.index = k;
          break;
        }
      }
    }
    return out;
  }

  /** Marks a phase seen outside the console (the home screen); returns the new phases. */
  mark(phase, guestSecs) {
    const k = PHASES.findIndex((p) => p[0] === phase);
    if (k <= this.index) return [];
    const out = [];
    for (let j = this.index + 1; j <= k; j++) {
      const ev = { phase: PHASES[j][0], label: PHASES[j][1], guestSecs };
      this.events.push(ev);
      out.push(ev);
    }
    this.index = k;
    return out;
  }
}
