//! Device profiles (M10, ADR 0035, `docs/specs/device-profiles.md`): what
//! the machine exposes to the guest, as a small versioned JSON file.
//!
//! The same format and rules as `web/node/profiles.mjs` (the browser's copy):
//! for the same file both give the same machine and the same boot
//! parameters. A profile sets
//! - the machine: RAM ([`Profile::ram_mib`]) and the virtio-gpu scanout size
//!   ([`Profile::apply_gpu`]);
//! - the `androidboot.*` parameters ([`Profile::android_params`]) that differ
//!   from the image's own bootconfig: `lcd_density`, `serialno`,
//!   `hardware.sku`;
//! - adb commands for after the boot ([`Profile::adb_commands`]): time zone,
//!   device name, locale.
//!
//! The starter profiles are the app's files (`web/app/profiles/`), built in
//! ([`starter`]).

use vetro_analysis::net::json::{self, Value};
use vetro_platform::virtio::GpuConfig;

/// The only format version this Vetro reads.
pub const PROFILE_VERSION: u64 = 1;

/// The starter profiles, in menu order: (id, JSON).
pub const STARTERS: [(&str, &str); 5] = [
    ("light", include_str!("../../../web/app/profiles/light.json")),
    ("default", include_str!("../../../web/app/profiles/default.json")),
    ("phone", include_str!("../../../web/app/profiles/phone.json")),
    ("small-phone", include_str!("../../../web/app/profiles/small-phone.json")),
    ("tablet", include_str!("../../../web/app/profiles/tablet.json")),
];

/// The image's own values (vendor_boot bootconfig and product locale).
pub const IMAGE_DENSITY: u32 = 240;
pub const IMAGE_SERIAL: &str = "VETRO00001";
pub const IMAGE_LOCALE: &str = "en-US";

/// Screen side limits (pixels), density limits (dpi), RAM limits (MiB).
pub const SIDE: (u32, u32) = (320, 3840);
pub const DENSITY: (u32, u32) = (120, 640);
pub const RAM_MIB: (u32, u32) = (1024, 3072);

/// A validated device profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub description: String,
    pub width: u32,
    pub height: u32,
    pub density: u32,
    pub ram_mib: u32,
    pub locale: String,
    pub timezone: Option<String>,
    pub device_name: Option<String>,
    pub serial: String,
    pub sku: Option<String>,
}

/// A profile error: the JSON path of the bad value and what is wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileError {
    pub field: String,
    pub message: String,
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "profile: {}: {}", self.field, self.message)
    }
}

impl std::error::Error for ProfileError {}

fn err<T>(field: &str, message: impl Into<String>) -> Result<T, ProfileError> {
    Err(ProfileError { field: field.to_string(), message: message.into() })
}

/// The pairs of an object, rejecting unknown and repeated keys.
fn object<'a>(v: &'a Value, field: &str, allowed: &[&str]) -> Result<&'a [(String, Value)], ProfileError> {
    let Value::Object(pairs) = v else {
        return err(field, "not an object");
    };
    for (i, (k, _)) in pairs.iter().enumerate() {
        let path = if field.is_empty() { k.clone() } else { format!("{field}.{k}") };
        if !allowed.contains(&k.as_str()) {
            return err(&path, "unknown field");
        }
        if pairs[..i].iter().any(|(j, _)| j == k) {
            return err(&path, "repeated field");
        }
    }
    Ok(pairs)
}

fn get<'a>(pairs: &'a [(String, Value)], key: &str) -> Option<&'a Value> {
    pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

fn int(v: &Value, field: &str, (lo, hi): (u32, u32)) -> Result<u32, ProfileError> {
    let Value::Number(n) = v else {
        return err(field, format!("{} is not an integer", v.to_compact()));
    };
    // JSON integers only (JavaScript's Number.isInteger also accepts 720.0:
    // written without a fraction it is the same number).
    let digits = n.strip_suffix(".0").unwrap_or(n);
    let Ok(x) = digits.parse::<i64>() else {
        return err(field, format!("{n} is not an integer"));
    };
    if x < i64::from(lo) || x > i64::from(hi) {
        return err(field, format!("{x} is outside {lo}..{hi}"));
    }
    Ok(x as u32)
}

fn string<'a>(
    v: &'a Value,
    field: &str,
    max: usize,
    ok: impl Fn(&str) -> bool,
    what: &str,
) -> Result<&'a str, ProfileError> {
    let Value::String(s) = v else {
        return err(field, format!("{} is not a string", v.to_compact()));
    };
    if s.is_empty() || s.chars().count() > max || !ok(s) {
        return err(field, format!("{} is not {what}", v.to_compact()));
    }
    Ok(s)
}

fn is_id(s: &str) -> bool {
    let b = s.as_bytes();
    b[0] != b'-' && b.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

/// Printable ASCII without what a shell or bootconfig would read: `"`, `$`,
/// `'`, `\`, `` ` ``, `|`.
fn is_text(s: &str) -> bool {
    s.bytes().all(|c| (0x20..0x7f).contains(&c) && !b"\"$'\\`|".contains(&c))
}

fn is_locale(s: &str) -> bool {
    let mut parts = s.split('-');
    let lang = parts.next().unwrap_or_default();
    if !(2..=3).contains(&lang.len()) || !lang.bytes().all(|c| c.is_ascii_lowercase()) {
        return false;
    }
    let rest: Vec<&str> = parts.collect();
    let script = |p: &str| {
        p.len() == 4
            && p.as_bytes()[0].is_ascii_uppercase()
            && p.bytes().skip(1).all(|c| c.is_ascii_lowercase())
    };
    let region = |p: &str| {
        (p.len() == 2 && p.bytes().all(|c| c.is_ascii_uppercase()))
            || (p.len() == 3 && p.bytes().all(|c| c.is_ascii_digit()))
    };
    match rest.as_slice() {
        [] => true,
        [a] => script(a) || region(a),
        [a, b] => script(a) && region(b),
        _ => false,
    }
}

fn is_timezone(s: &str) -> bool {
    if s == "UTC" || s == "GMT" {
        return true;
    }
    let parts: Vec<&str> = s.split('/').collect();
    if !(2..=3).contains(&parts.len()) {
        return false;
    }
    let area = parts[0];
    let ok_area = area.len() >= 2
        && area.as_bytes()[0].is_ascii_uppercase()
        && area.bytes().all(|c| c.is_ascii_alphabetic() || c == b'_' || c == b'-');
    ok_area
        && parts[1..]
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_alphanumeric() || b"_+-".contains(&c)))
}

impl Profile {
    /// Parses and validates a profile. Strict like the browser's
    /// `parseProfile`: unknown or repeated fields and out-of-range values are
    /// errors.
    pub fn parse(text: &[u8]) -> Result<Profile, ProfileError> {
        let v = json::parse(text)
            .map_err(|e| ProfileError { field: "(file)".into(), message: format!("not JSON: {e}") })?;
        let Value::Object(top) = &v else {
            return err("(file)", "not a JSON object");
        };
        match get(top, "vetroProfile") {
            None => return err("vetroProfile", "missing: not a Vetro device profile"),
            Some(Value::Number(n)) if n == "1" => {}
            Some(Value::Number(n)) if n.parse::<u64>().is_ok_and(|x| x > PROFILE_VERSION) => {
                return err(
                    "vetroProfile",
                    format!("version {n} needs a newer Vetro (this one reads version {PROFILE_VERSION})"),
                );
            }
            Some(x) => return err("vetroProfile", format!("unknown version {}", x.to_compact())),
        }
        let top = object(
            &v,
            "",
            &[
                "vetroProfile",
                "id",
                "name",
                "description",
                "screen",
                "ramMiB",
                "locale",
                "timezone",
                "device",
            ],
        )?;
        for k in ["id", "name", "screen", "ramMiB"] {
            if get(top, k).is_none() {
                return err(k, "missing");
            }
        }
        let text_what = "printable text without quotes, backslashes, $ or ` (at most 40)";
        let id = string(
            get(top, "id").unwrap(),
            "id",
            32,
            is_id,
            "lowercase letters, digits and dashes (at most 32)",
        )?;
        let name = string(get(top, "name").unwrap(), "name", 40, is_text, text_what)?;
        let description = match get(top, "description") {
            None => "",
            Some(d) => string(
                d,
                "description",
                300,
                |s| !s.chars().any(|c| c < ' '),
                "one line of text (at most 300)",
            )?,
        };
        let screen = object(get(top, "screen").unwrap(), "screen", &["width", "height", "density"])?;
        let side = |k: &str| -> Result<u32, ProfileError> {
            let field = format!("screen.{k}");
            let v = int(
                get(screen, k).ok_or(ProfileError { field: field.clone(), message: "missing".into() })?,
                &field,
                SIDE,
            )?;
            if v % 2 != 0 {
                return err(&field, format!("{v} is odd"));
            }
            Ok(v)
        };
        let (width, height) = (side("width")?, side("height")?);
        let density = int(
            get(screen, "density")
                .ok_or(ProfileError { field: "screen.density".into(), message: "missing".into() })?,
            "screen.density",
            DENSITY,
        )?;
        let ram_mib = int(get(top, "ramMiB").unwrap(), "ramMiB", RAM_MIB)?;
        if ram_mib % 64 != 0 {
            return err("ramMiB", format!("{ram_mib} is not a multiple of 64"));
        }
        let locale = match get(top, "locale") {
            None => IMAGE_LOCALE,
            Some(l) => string(l, "locale", 16, is_locale, "a language tag like en-US")?,
        };
        let timezone = match get(top, "timezone") {
            None => None,
            Some(t) => Some(string(t, "timezone", 64, is_timezone, "an IANA time zone like Europe/Rome")?),
        };
        let (mut device_name, mut serial, mut sku) = (None, IMAGE_SERIAL, None);
        if let Some(d) = get(top, "device") {
            let d = object(d, "device", &["name", "serial", "sku"])?;
            if let Some(n) = get(d, "name") {
                device_name = Some(string(n, "device.name", 40, is_text, text_what)?);
            }
            if let Some(s) = get(d, "serial") {
                serial = string(
                    s,
                    "device.serial",
                    20,
                    |s| s.bytes().all(|c| c.is_ascii_alphanumeric()),
                    "letters and digits (at most 20)",
                )?;
            }
            if let Some(s) = get(d, "sku") {
                sku = Some(string(
                    s,
                    "device.sku",
                    32,
                    |s| s.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)),
                    "letters, digits, dots, dashes and underscores (at most 32)",
                )?);
            }
        }
        Ok(Profile {
            id: id.into(),
            name: name.into(),
            description: description.into(),
            width,
            height,
            density,
            ram_mib,
            locale: locale.into(),
            timezone: timezone.map(Into::into),
            device_name: device_name.map(Into::into),
            serial: serial.into(),
            sku: sku.map(Into::into),
        })
    }

    /// The `androidboot.*` parameters that differ from the image, space
    /// separated (empty for the default profile). The same string as
    /// `profileAndroidParams` in the browser.
    pub fn android_params(&self) -> String {
        let mut out = Vec::new();
        if self.density != IMAGE_DENSITY {
            out.push(format!("androidboot.lcd_density={}", self.density));
        }
        if self.serial != IMAGE_SERIAL {
            out.push(format!("androidboot.serialno={}", self.serial));
        }
        if let Some(s) = &self.sku {
            out.push(format!("androidboot.hardware.sku={s}"));
        }
        out.join(" ")
    }

    /// adb shell commands that apply the rest of the profile after the boot
    /// (idempotent). The same list as `profileAdbCommands` in the browser.
    pub fn adb_commands(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(tz) = &self.timezone {
            out.push(format!("cmd alarm set-timezone {tz}"));
        }
        if let Some(n) = &self.device_name {
            out.push(format!("settings put global device_name '{n}'"));
        }
        if self.locale != IMAGE_LOCALE {
            out.push(format!("su 0 setprop persist.sys.locale {}", self.locale));
        }
        out
    }

    /// Sets the scanout size of a virtio-gpu configuration.
    pub fn apply_gpu(&self, gpu: &mut GpuConfig) {
        gpu.width = self.width;
        gpu.height = self.height;
    }
}

/// A starter profile by id.
pub fn starter(id: &str) -> Option<Profile> {
    STARTERS
        .iter()
        .find(|(k, _)| *k == id)
        .map(|(_, text)| Profile::parse(text.as_bytes()).expect("starter profiles are valid"))
}

/// A starter profile by id, or else a profile file.
pub fn load(name_or_path: &str) -> Result<Profile, String> {
    if let Some(p) = starter(name_or_path) {
        return Ok(p);
    }
    let text = std::fs::read(name_or_path).map_err(|e| {
        let ids: Vec<&str> = STARTERS.iter().map(|(k, _)| *k).collect();
        format!("{name_or_path}: not a starter profile ({}) and not a readable file: {e}", ids.join(", "))
    })?;
    Profile::parse(&text).map_err(|e| format!("{name_or_path}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The boot parameters each starter profile gives. The same strings are
    /// checked for the browser in tests/web/unit.mjs ("device profiles").
    const EXPECTED: [(&str, &str); 5] = [
        ("light", "androidboot.lcd_density=180"),
        ("default", ""),
        (
            "phone",
            "androidboot.lcd_density=320 androidboot.serialno=VETROPHONE01 androidboot.hardware.sku=phone",
        ),
        ("small-phone", "androidboot.serialno=VETROSMALL01 androidboot.hardware.sku=small-phone"),
        (
            "tablet",
            "androidboot.lcd_density=213 androidboot.serialno=VETROTABLET1 androidboot.hardware.sku=tablet",
        ),
    ];

    fn parse(s: &str) -> Result<Profile, ProfileError> {
        Profile::parse(s.as_bytes())
    }

    fn field_of(s: &str) -> String {
        parse(s).unwrap_err().field
    }

    const MINIMAL: &str = r#"{"vetroProfile":1,"id":"x","name":"X","screen":{"width":720,"height":1280,"density":320},"ramMiB":2048}"#;

    #[test]
    fn starters_parse_and_give_the_expected_params() {
        for (id, want) in EXPECTED {
            let p = starter(id).unwrap();
            assert_eq!(p.id, id);
            assert_eq!(p.android_params(), want, "{id}");
        }
        assert!(starter("nope").is_none());
    }

    #[test]
    fn default_profile_is_the_image_and_the_app_machine() {
        // The prebuilt snapshot (ADR 0031) is keyed on the machine and the
        // parameters: the default profile must change neither.
        let p = starter("default").unwrap();
        assert_eq!((p.width, p.height, p.density, p.ram_mib), (1280, 800, 240, 2048));
        let gpu = GpuConfig::default();
        assert_eq!((gpu.width, gpu.height), (p.width, p.height));
        assert_eq!(p.android_params(), "");
        assert!(p.adb_commands().is_empty());
    }

    #[test]
    fn light_profile_is_the_default_layout_with_fewer_pixels() {
        // The web app's default (ADR 0036): same dp size as the image's machine.
        let p = starter("light").unwrap();
        let d = starter("default").unwrap();
        assert_eq!((p.width, p.height, p.density, p.ram_mib), (960, 600, 180, 2048));
        assert_eq!(p.width * 160 / p.density, d.width * 160 / d.density);
        assert_eq!(p.height * 160 / p.density, d.height * 160 / d.density);
        assert!(p.adb_commands().is_empty());
    }

    #[test]
    fn phone_machine_and_adb_commands() {
        let p = starter("phone").unwrap();
        let mut gpu = GpuConfig::default();
        p.apply_gpu(&mut gpu);
        assert_eq!((gpu.width, gpu.height), (720, 1280));
        assert_eq!(p.ram_mib, 2048);
        assert_eq!(
            p.adb_commands(),
            vec![
                "cmd alarm set-timezone UTC".to_string(),
                "settings put global device_name 'Vetro Phone'".to_string()
            ]
        );
        let small = starter("small-phone").unwrap();
        assert_eq!((small.width, small.height, small.density, small.ram_mib), (480, 800, 240, 1536));
    }

    #[test]
    fn minimal_profile_takes_the_image_defaults() {
        let p = parse(MINIMAL).unwrap();
        assert_eq!(p.locale, "en-US");
        assert_eq!(p.serial, IMAGE_SERIAL);
        assert_eq!((p.timezone.as_deref(), p.device_name.as_deref(), p.sku.as_deref()), (None, None, None));
        assert_eq!(p.android_params(), "androidboot.lcd_density=320");
        assert!(p.adb_commands().is_empty());
        let it = parse(
            &MINIMAL.replace("\"ramMiB\"", "\"locale\":\"it-IT\",\"timezone\":\"Europe/Rome\",\"ramMiB\""),
        )
        .unwrap();
        assert_eq!(
            it.adb_commands(),
            vec![
                "cmd alarm set-timezone Europe/Rome".to_string(),
                "su 0 setprop persist.sys.locale it-IT".to_string()
            ]
        );
    }

    #[test]
    fn versions() {
        let v2 = MINIMAL.replace("\"vetroProfile\":1", "\"vetroProfile\":2");
        let e = parse(&v2).unwrap_err();
        assert_eq!(e.field, "vetroProfile");
        assert!(e.message.contains("newer Vetro"), "{e}");
        assert_eq!(field_of(&MINIMAL.replace("\"vetroProfile\":1,", "")), "vetroProfile");
        assert_eq!(
            field_of(&MINIMAL.replace("\"vetroProfile\":1", "\"vetroProfile\":\"1\"")),
            "vetroProfile"
        );
        assert_eq!(field_of(&MINIMAL.replace("\"vetroProfile\":1", "\"vetroProfile\":0")), "vetroProfile");
    }

    #[test]
    fn rejections() {
        assert_eq!(field_of("[1]"), "(file)");
        assert_eq!(field_of("{"), "(file)");
        assert_eq!(field_of(&MINIMAL.replace("\"ramMiB\"", "\"colour\":1,\"ramMiB\"")), "colour");
        assert_eq!(field_of(&MINIMAL.replace("\"density\":320", "\"density\":320,\"dpi\":1")), "screen.dpi");
        assert_eq!(field_of(&MINIMAL.replace("\"id\":\"x\"", "\"id\":\"x\",\"id\":\"y\"")), "id");
        assert_eq!(field_of(&MINIMAL.replace("\"id\":\"x\"", "\"id\":\"Phone\"")), "id");
        assert_eq!(field_of(&MINIMAL.replace("\"id\":\"x\",", "")), "id");
        assert_eq!(field_of(&MINIMAL.replace("\"name\":\"X\"", "\"name\":\"it's\"")), "name");
        assert_eq!(field_of(&MINIMAL.replace("\"width\":720", "\"width\":721")), "screen.width");
        assert_eq!(field_of(&MINIMAL.replace("\"width\":720", "\"width\":100")), "screen.width");
        assert_eq!(field_of(&MINIMAL.replace("\"height\":1280", "\"height\":1280.5")), "screen.height");
        assert_eq!(field_of(&MINIMAL.replace("\"height\":1280,", "")), "screen.height");
        assert_eq!(field_of(&MINIMAL.replace("\"density\":320", "\"density\":1000")), "screen.density");
        assert_eq!(field_of(&MINIMAL.replace("\"ramMiB\":2048", "\"ramMiB\":2000")), "ramMiB");
        assert_eq!(field_of(&MINIMAL.replace("\"ramMiB\":2048", "\"ramMiB\":8192")), "ramMiB");
        assert_eq!(field_of(&MINIMAL.replace("\"ramMiB\":2048", "\"ramMiB\":\"2048\"")), "ramMiB");
        let with = |k: &str, v: &str| MINIMAL.replace("\"ramMiB\"", &format!("\"{k}\":{v},\"ramMiB\""));
        assert_eq!(field_of(&with("locale", "\"english\"")), "locale");
        assert_eq!(field_of(&with("timezone", "\"Rome\"")), "timezone");
        assert_eq!(field_of(&with("timezone", "\"Europe/Rome; reboot\"")), "timezone");
        assert_eq!(field_of(&with("device", "{\"serial\":\"VETRO 1\"}")), "device.serial");
        assert_eq!(field_of(&with("device", "{\"sku\":\"a b\"}")), "device.sku");
        assert_eq!(field_of(&with("device", "{\"name\":\"$(reboot)\"}")), "device.name");
        assert_eq!(field_of(&with("device", "{\"model\":\"x\"}")), "device.model");
        for ok in ["en", "en-US", "zh-Hant-TW", "es-419", "sr-Latn"] {
            assert!(parse(&with("locale", &format!("\"{ok}\""))).is_ok(), "{ok}");
        }
        for ok in ["UTC", "Europe/Rome", "America/Argentina/Buenos_Aires", "Etc/GMT+3"] {
            assert!(parse(&with("timezone", &format!("\"{ok}\""))).is_ok(), "{ok}");
        }
    }
}
