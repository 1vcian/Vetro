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
  // there is FallbackHome, "Phone is starting"). Whoever has adb calls `mark`.
  ['home', 'home screen (launcher)', null],
];

/**
 * Bootloader parameters for Vetro's AOSP image (ADR 0028): `nokaslr` like
 * tools/aosp/vetro.sh.
 */
export const ANDROID_PARAMS = 'nokaslr';

/**
 * An app colour as the scanout shows it. Today's image swaps red and blue
 * (the blue app 0x1565c0 arrives as (192, 101, 21): a buffer written as RGBA
 * presented by virtio-gpu as XRGB8888; `display_framebuffer_format=bgra`
 * changes nothing, ADR 0028): tests accept both orders and report which one
 * they saw.
 */
export function colorSeen(px, rgb, tol = 8) {
  if (!px) return null;
  const near = (c) => c.every((v, i) => Math.abs(v - px[i]) <= tol);
  if (near(rgb)) return 'rgb';
  if (near([rgb[2], rgb[1], rgb[0]])) return 'bgr';
  return null;
}

/** adb command for the focused window; the home screen is up if it contains "launcher". */
export const HOME_QUERY = 'dumpsys window | grep -m1 mCurrentFocus';
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
