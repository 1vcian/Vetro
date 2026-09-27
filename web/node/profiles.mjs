// Device profiles (M10, ADR 0035, docs/specs/device-profiles.md): what the
// machine exposes to the guest (screen size and density, RAM, locale, time
// zone, device strings), as a small versioned JSON file. Uses no Node API:
// the page, the Worker and the Node tools share it. The Rust twin is
// `vetro_machine::profile` (`vetro boot --profile`); both produce the same
// boot parameters for the same file (checked by tests/web/unit.mjs and the
// crate's unit tests against the same expected strings).
//
// A profile is applied in three places, and only with what the current image
// supports without an AOSP rebuild:
// - the machine: RAM, and the virtio-gpu scanout size (the ranchu HWC reads
//   the mode from DRM, so Android draws at that size);
// - the bootloader parameters (`androidboot.*`, which Vetro's bootloader puts
//   in the bootconfig, replacing the image's line with the same key):
//   `lcd_density` (-> ro.sf.lcd_density), `serialno` (-> ro.serialno,
//   Build.getSerial()), `hardware.sku` (-> Build.SKU);
// - adb commands after the boot, persisted in the guest's /data: time zone
//   (`cmd alarm set-timezone`), device name (Settings > About), locale
//   (`persist.sys.locale`, read when the framework starts: effective from the
//   next system start).
// Only the fields that differ from the image's own values are emitted, so the
// default profile gives exactly ANDROID_PARAMS and ANDROID_MACHINE, and the
// prebuilt snapshot (ADR 0031, keyed on both) still matches it.

import { ANDROID_MACHINE, ANDROID_PARAMS } from './android.mjs';

/** The only format version this Vetro reads. */
export const PROFILE_VERSION = 1;

/** Profiles shipped with the app (web/app/profiles/<id>.json), in menu order. */
export const STARTER_PROFILES = ['light', 'default', 'phone', 'small-phone', 'tablet'];
/**
 * The profile the app selects (ADR 0037): `light`, the image's layout at
 * 960x600. `default` stays the image's own machine (ANDROID_MACHINE,
 * ANDROID_PARAMS), what `vetro boot` uses without --profile.
 */
export const DEFAULT_PROFILE = 'light';
/** Profiles with a ready-made home-screen snapshot for the default image (ADR 0031, 0037). */
export const PREBUILT_PROFILES = ['light', 'default'];

/**
 * What the image says when the bootloader adds nothing (vendor_boot's
 * bootconfig in guest/aosp/device/vetro/vetro_arm64/BoardConfig.mk, the
 * product's locale).
 */
export const IMAGE_DEFAULTS = { density: 240, serial: 'VETRO00001', locale: 'en-US' };

/** Limits of the format (docs/specs/device-profiles.md). */
export const LIMITS = {
  side: [320, 3840],
  density: [120, 640],
  ramMiB: [1024, 3072],
};

const KEYS = ['vetroProfile', 'id', 'name', 'description', 'screen', 'ramMiB', 'locale', 'timezone', 'device'];
const SCREEN_KEYS = ['width', 'height', 'density'];
const DEVICE_KEYS = ['name', 'serial', 'sku'];
const RE = {
  id: /^[a-z0-9][a-z0-9-]{0,31}$/,
  locale: /^[a-z]{2,3}(-[A-Z][a-z]{3})?(-(?:[A-Z]{2}|[0-9]{3}))?$/,
  timezone: /^(?:UTC|GMT|[A-Z][A-Za-z_-]+(?:\/[A-Za-z0-9_+-]+){1,2})$/,
  // Printable ASCII without the characters a shell or bootconfig would read.
  text: /^[ !#%&(-[\]-_a-z{}~]*$/,
  serial: /^[A-Za-z0-9]{1,20}$/,
  sku: /^[A-Za-z0-9._-]{1,32}$/,
};

/** A profile error: `field` is the JSON path of the bad value. */
export class ProfileError extends Error {
  constructor(field, message) {
    super(`profile: ${field}: ${message}`);
    this.field = field;
  }
}

const isObject = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);

function onlyKeys(obj, allowed, where) {
  for (const k of Object.keys(obj)) {
    if (!allowed.includes(k)) throw new ProfileError(where ? `${where}.${k}` : k, 'unknown field');
  }
}

function int(v, field, [lo, hi], extra) {
  if (!Number.isInteger(v)) throw new ProfileError(field, `${JSON.stringify(v)} is not an integer`);
  if (v < lo || v > hi) throw new ProfileError(field, `${v} is outside ${lo}..${hi}`);
  if (extra) extra(v);
  return v;
}

function str(v, field, re, max, what) {
  if (typeof v !== 'string') throw new ProfileError(field, `${JSON.stringify(v)} is not a string`);
  if (v.length === 0 || v.length > max || !re.test(v)) throw new ProfileError(field, `${JSON.stringify(v)} is not ${what}`);
  return v;
}

/**
 * Parses and validates a profile (JSON text or an already parsed object).
 * Strict: unknown fields and out-of-range values are errors, so a typo never
 * silently changes the machine. Returns a normalised object:
 * { version, id, name, description, screen: { width, height, density },
 *   ramMiB, locale, timezone (or null), device: { name, serial, sku } (null
 *   when absent) }.
 */
export function parseProfile(input) {
  let p = input;
  if (typeof input === 'string') {
    try {
      p = JSON.parse(input);
    } catch (e) {
      throw new ProfileError('(file)', `not JSON: ${e.message}`);
    }
  }
  if (!isObject(p)) throw new ProfileError('(file)', 'not a JSON object');
  if (!('vetroProfile' in p)) throw new ProfileError('vetroProfile', 'missing: not a Vetro device profile');
  if (p.vetroProfile !== PROFILE_VERSION) {
    throw new ProfileError('vetroProfile', Number.isInteger(p.vetroProfile) && p.vetroProfile > PROFILE_VERSION
      ? `version ${p.vetroProfile} needs a newer Vetro (this one reads version ${PROFILE_VERSION})` : `unknown version ${JSON.stringify(p.vetroProfile)}`);
  }
  onlyKeys(p, KEYS, '');
  for (const k of ['id', 'name', 'screen', 'ramMiB']) if (!(k in p)) throw new ProfileError(k, 'missing');
  const id = str(p.id, 'id', RE.id, 32, 'lowercase letters, digits and dashes (at most 32)');
  const name = str(p.name, 'name', RE.text, 40, 'printable text without quotes, backslashes, $ or ` (at most 40)');
  const description = p.description === undefined ? '' : str(p.description, 'description', /^[^\0-\x1f]*$/, 300, 'one line of text (at most 300)');
  if (!isObject(p.screen)) throw new ProfileError('screen', 'not an object');
  onlyKeys(p.screen, SCREEN_KEYS, 'screen');
  for (const k of SCREEN_KEYS) if (!(k in p.screen)) throw new ProfileError(`screen.${k}`, 'missing');
  const even = (field) => (v) => {
    if (v % 2) throw new ProfileError(field, `${v} is odd`);
  };
  const screen = {
    width: int(p.screen.width, 'screen.width', LIMITS.side, even('screen.width')),
    height: int(p.screen.height, 'screen.height', LIMITS.side, even('screen.height')),
    density: int(p.screen.density, 'screen.density', LIMITS.density),
  };
  const ramMiB = int(p.ramMiB, 'ramMiB', LIMITS.ramMiB, (v) => {
    if (v % 64) throw new ProfileError('ramMiB', `${v} is not a multiple of 64`);
  });
  const locale = p.locale === undefined ? IMAGE_DEFAULTS.locale : str(p.locale, 'locale', RE.locale, 16, 'a language tag like en-US');
  const timezone = p.timezone === undefined ? null : str(p.timezone, 'timezone', RE.timezone, 64, 'an IANA time zone like Europe/Rome');
  let device = { name: null, serial: IMAGE_DEFAULTS.serial, sku: null };
  if (p.device !== undefined) {
    if (!isObject(p.device)) throw new ProfileError('device', 'not an object');
    onlyKeys(p.device, DEVICE_KEYS, 'device');
    const d = p.device;
    device = {
      name: d.name === undefined ? null : str(d.name, 'device.name', RE.text, 40, 'printable text without quotes, backslashes, $ or ` (at most 40)'),
      serial: d.serial === undefined ? IMAGE_DEFAULTS.serial : str(d.serial, 'device.serial', RE.serial, 20, 'letters and digits (at most 20)'),
      sku: d.sku === undefined ? null : str(d.sku, 'device.sku', RE.sku, 32, 'letters, digits, dots, dashes and underscores (at most 32)'),
    };
  }
  return { version: PROFILE_VERSION, id, name, description, screen, ramMiB, locale, timezone, device };
}

/**
 * The `androidboot.*` parameters of a profile: only those that differ from
 * the image (IMAGE_DEFAULTS), space-separated ('' for the default profile).
 * `Profile::android_params` in Rust gives the same string.
 */
export function profileAndroidParams(p) {
  const out = [];
  if (p.screen.density !== IMAGE_DEFAULTS.density) out.push(`androidboot.lcd_density=${p.screen.density}`);
  if (p.device.serial !== IMAGE_DEFAULTS.serial) out.push(`androidboot.serialno=${p.device.serial}`);
  if (p.device.sku) out.push(`androidboot.hardware.sku=${p.device.sku}`);
  return out.join(' ');
}

/** The app's bootloader parameters for a profile: ANDROID_PARAMS, then profileAndroidParams. */
export function profileBootParams(p) {
  return [ANDROID_PARAMS, profileAndroidParams(p)].filter(Boolean).join(' ');
}

/** The machine of a profile, as ANDROID_MACHINE (same devices, the profile's RAM and screen). */
export function profileMachine(p) {
  return { ...ANDROID_MACHINE, ramMiB: p.ramMiB, width: p.screen.width, height: p.screen.height };
}

/**
 * adb shell commands that apply the rest of a profile after the boot (as
 * root where needed; idempotent, so they run again after every connection).
 */
export function profileAdbCommands(p) {
  const out = [];
  if (p.timezone) out.push(`cmd alarm set-timezone ${p.timezone}`);
  if (p.device.name) out.push(`settings put global device_name '${p.device.name}'`);
  // Read by ActivityTaskManager when the framework starts: from the next start.
  if (p.locale !== IMAGE_DEFAULTS.locale) out.push(`su 0 setprop persist.sys.locale ${p.locale}`);
  return out;
}

/** What the running guest should report for a profile (for tests and tools). */
export function profileExpect(p) {
  return {
    size: `${p.screen.width}x${p.screen.height}`,
    density: String(p.screen.density),
    serial: p.device.serial,
    sku: p.device.sku ?? '',
    timezone: p.timezone,
    deviceName: p.device.name,
  };
}

/**
 * adb command that reads back what a profile set, one `key=value` per line
 * (parsed by `parseProfileReport`).
 */
export const PROFILE_REPORT = 'echo size=$(wm size | tail -n 1 | sed "s/.*: //"); echo density=$(getprop ro.sf.lcd_density); '
  + 'echo serial=$(getprop ro.serialno); echo sku=$(getprop ro.boot.hardware.sku); echo timezone=$(getprop persist.sys.timezone); '
  + 'echo deviceName=$(settings get global device_name)';

/** `key=value` lines of PROFILE_REPORT -> object. */
export function parseProfileReport(text) {
  const out = {};
  for (const line of text.split('\n')) {
    const k = line.indexOf('=');
    if (k > 0) out[line.slice(0, k).trim()] = line.slice(k + 1).trim();
  }
  return out;
}

/** Differences between a PROFILE_REPORT result and profileExpect: a list of text, empty if all match. */
export function profileMismatches(p, report) {
  const want = profileExpect(p);
  const bad = [];
  for (const [k, v] of Object.entries(want)) {
    if (v === null) continue;
    if (report[k] !== v) bad.push(`${k}: ${JSON.stringify(report[k])} instead of ${JSON.stringify(v)}`);
  }
  return bad;
}

/** URL of a starter profile next to the app (web/app/profiles). */
export const profileUrl = (id, base) => new URL(`profiles/${id}.json`, base).href;
