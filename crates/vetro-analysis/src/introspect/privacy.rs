//! Basic privacy inspector (M8): which Binder calls touch sensitive
//! data. The rules look at interface and method (from the AIDL map)
//! and, for content providers, at the Parcel strings (authorities, keys
//! such as `android_id`), because there the method alone (`query`, `call`)
//! does not say what is being read.

use std::fmt;

/// Category of sensitive data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Category {
    Location,
    Contacts,
    CallLog,
    Sms,
    Calendar,
    Clipboard,
    Identifier,
    Camera,
    Microphone,
    Accounts,
    InstalledApps,
}

impl Category {
    pub fn name(self) -> &'static str {
        match self {
            Category::Location => "location",
            Category::Contacts => "contacts",
            Category::CallLog => "call log",
            Category::Sms => "sms",
            Category::Calendar => "calendar",
            Category::Clipboard => "clipboard",
            Category::Identifier => "identifier",
            Category::Camera => "camera",
            Category::Microphone => "microphone",
            Category::Accounts => "accounts",
            Category::InstalledApps => "installed apps",
        }
    }

    /// Stable name for JSON and exports.
    pub fn id(self) -> &'static str {
        match self {
            Category::Location => "location",
            Category::Contacts => "contacts",
            Category::CallLog => "call_log",
            Category::Sms => "sms",
            Category::Calendar => "calendar",
            Category::Clipboard => "clipboard",
            Category::Identifier => "identifier",
            Category::Camera => "camera",
            Category::Microphone => "microphone",
            Category::Accounts => "accounts",
            Category::InstalledApps => "installed_apps",
        }
    }
}

/// A recognised sensitive access.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sensitive {
    pub category: Category,
    /// What, in short: `ANDROID_ID`, `getPrimaryClip`, `IMEI`, ...
    pub what: String,
}

impl fmt::Display for Sensitive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.category.name(), self.what)
    }
}

fn s(category: Category, what: impl Into<String>) -> Sensitive {
    Sensitive { category, what: what.into() }
}

/// The `Settings` keys that are identifiers.
const ID_SETTINGS: &[(&str, &str)] = &[
    ("android_id", "ANDROID_ID"),
    ("bluetooth_address", "Bluetooth address"),
    ("bluetooth_name", "Bluetooth name"),
];

/// Authorities of sensitive providers.
const AUTHORITIES: &[(&str, Category)] = &[
    ("com.android.contacts", Category::Contacts),
    ("contacts", Category::Contacts),
    ("call_log", Category::CallLog),
    ("sms", Category::Sms),
    ("mms", Category::Sms),
    ("mms-sms", Category::Sms),
    ("com.android.calendar", Category::Calendar),
];

/// Provider methods that read or write data.
const PROVIDER_DATA: &[&str] =
    &["query", "insert", "update", "delete", "bulkInsert", "applyBatch", "openFile", "openAssetFile", "call"];

/// Classifies a call. `method` is the name from the AIDL map, if any;
/// `strings` the Parcel strings.
pub fn classify(descriptor: &str, method: Option<&str>, strings: &[String]) -> Vec<Sensitive> {
    let m = method.unwrap_or("");
    let mut out = Vec::new();
    let has = |needle: &str| strings.iter().any(|x| x == needle);
    let starts = |p: &[&str]| p.iter().any(|x| m.starts_with(x));
    match descriptor {
        "android.content.IClipboard" => {
            if starts(&["getPrimaryClip", "hasPrimaryClip", "hasClipboardText"]) {
                out.push(s(Category::Clipboard, format!("read ({m})")));
            } else if starts(&["setPrimaryClip"]) {
                out.push(s(Category::Clipboard, format!("write ({m})")));
            }
        }
        "android.location.ILocationManager" => {
            if starts(&[
                "getLastLocation",
                "getCurrentLocation",
                "registerLocationListener",
                "registerLocationPendingIntent",
                "requestLocationUpdates",
                "registerGnss",
                "addGnss",
                "startGnss",
            ]) {
                out.push(s(Category::Location, m));
            }
        }
        "com.android.internal.telephony.ITelephony" | "com.android.internal.telephony.IPhoneSubInfo" => {
            let what = if m.contains("Imei") {
                Some("IMEI")
            } else if m.contains("Meid") {
                Some("MEID")
            } else if m.starts_with("getDeviceId") {
                Some("device ID (IMEI/MEID)")
            } else if m.contains("SubscriberId") {
                Some("IMSI")
            } else if m.contains("IccSerial") {
                Some("SIM serial (ICCID)")
            } else if m.contains("Line1Number") || m.contains("PhoneNumber") {
                Some("phone number")
            } else if m.starts_with("getCellLocation") || m.starts_with("getAllCellInfo") {
                out.push(s(Category::Location, format!("cell towers ({m})")));
                None
            } else {
                None
            };
            if let Some(w) = what {
                out.push(s(Category::Identifier, format!("{w} ({m})")));
            }
        }
        "android.os.IDeviceIdentifiersPolicyService" if m.starts_with("getSerial") => {
            out.push(s(Category::Identifier, format!("serial number ({m})")));
        }
        "android.hardware.ICameraService" if m.starts_with("connect") => {
            out.push(s(Category::Camera, m));
        }
        "android.media.IAudioRecord" => out.push(s(Category::Microphone, format!("recording ({m})"))),
        "android.accounts.IAccountManager" if m.starts_with("getAccounts") => {
            out.push(s(Category::Accounts, m));
        }
        "android.content.pm.IPackageManager"
            if starts(&["getInstalledPackages", "getInstalledApplications"]) =>
        {
            out.push(s(Category::InstalledApps, m));
        }
        "android.content.IContentProvider" if method.is_none_or(|m| PROVIDER_DATA.contains(&m)) => {
            // `Settings.Secure.getString(cr, "android_id")`: call("GET_secure", "android_id").
            for (key, what) in ID_SETTINGS {
                if has(key) {
                    out.push(s(
                        Category::Identifier,
                        format!("{what} (Settings, {})", method.unwrap_or("?")),
                    ));
                }
            }
            let mut seen = Vec::new();
            for st in strings {
                let auth =
                    st.strip_prefix("content://").map_or(st.as_str(), |r| r.split('/').next().unwrap_or(""));
                if let Some((a, c)) = AUTHORITIES.iter().find(|(a, _)| *a == auth)
                    && !seen.contains(c)
                {
                    seen.push(*c);
                    out.push(s(*c, format!("provider {a} ({})", method.unwrap_or("?"))));
                }
            }
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(x: &[&str]) -> Vec<String> {
        x.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn regole() {
        let c = classify("android.content.IClipboard", Some("getPrimaryClip"), &[]);
        assert_eq!(c[0].category, Category::Clipboard);
        let a = classify(
            "android.content.IContentProvider",
            Some("call"),
            &v(&["com.vetro.probe", "settings", "GET_secure", "android_id"]),
        );
        assert_eq!(a.len(), 1);
        assert_eq!(
            (a[0].category, a[0].what.as_str()),
            (Category::Identifier, "ANDROID_ID (Settings, call)")
        );
        let q = classify(
            "android.content.IContentProvider",
            Some("query"),
            &v(&["com.vetro.probe", "content://com.android.contacts/contacts"]),
        );
        assert_eq!(q[0].category, Category::Contacts);
        let i = classify("com.android.internal.telephony.ITelephony", Some("getImeiForSlot"), &[]);
        assert!(i[0].what.starts_with("IMEI"));
        assert!(
            classify("android.hardware.ICameraService", Some("connectDevice"), &[])[0].category
                == Category::Camera
        );
        assert!(
            classify("android.location.ILocationManager", Some("isLocationEnabledForUser"), &[]).is_empty()
        );
        assert!(
            classify("android.content.IContentProvider", Some("getType"), &v(&["android_id"])).is_empty()
        );
        assert!(classify("android.app.IActivityManager", Some("getPrimaryClip"), &[]).is_empty());
    }
}
