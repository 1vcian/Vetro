//! Ispettore privacy di base (M8): quali chiamate Binder toccano dati
//! sensibili. Le regole guardano interfaccia e metodo (dalla mappa AIDL)
//! e, per i content provider, le stringhe del Parcel (autorità, chiavi
//! come `android_id`), perché lì il metodo da solo (`query`, `call`) non
//! dice che cosa si legge.

use std::fmt;

/// Categoria di dato sensibile.
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
            Category::Location => "posizione",
            Category::Contacts => "contatti",
            Category::CallLog => "registro chiamate",
            Category::Sms => "sms",
            Category::Calendar => "calendario",
            Category::Clipboard => "appunti",
            Category::Identifier => "identificativo",
            Category::Camera => "fotocamera",
            Category::Microphone => "microfono",
            Category::Accounts => "account",
            Category::InstalledApps => "app installate",
        }
    }

    /// Nome stabile per JSON ed esportazioni.
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

/// Un accesso sensibile riconosciuto.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sensitive {
    pub category: Category,
    /// Che cosa, in breve: `ANDROID_ID`, `getPrimaryClip`, `IMEI`, ...
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

/// Le chiavi di `Settings` che sono identificativi.
const ID_SETTINGS: &[(&str, &str)] = &[
    ("android_id", "ANDROID_ID"),
    ("bluetooth_address", "indirizzo Bluetooth"),
    ("bluetooth_name", "nome Bluetooth"),
];

/// Autorità dei provider sensibili.
const AUTHORITIES: &[(&str, Category)] = &[
    ("com.android.contacts", Category::Contacts),
    ("contacts", Category::Contacts),
    ("call_log", Category::CallLog),
    ("sms", Category::Sms),
    ("mms", Category::Sms),
    ("mms-sms", Category::Sms),
    ("com.android.calendar", Category::Calendar),
];

/// Metodi di provider che leggono o scrivono dati.
const PROVIDER_DATA: &[&str] =
    &["query", "insert", "update", "delete", "bulkInsert", "applyBatch", "openFile", "openAssetFile", "call"];

/// Classifica una chiamata. `method` è il nome dalla mappa AIDL, se c'è;
/// `strings` le stringhe del Parcel.
pub fn classify(descriptor: &str, method: Option<&str>, strings: &[String]) -> Vec<Sensitive> {
    let m = method.unwrap_or("");
    let mut out = Vec::new();
    let has = |needle: &str| strings.iter().any(|x| x == needle);
    let starts = |p: &[&str]| p.iter().any(|x| m.starts_with(x));
    match descriptor {
        "android.content.IClipboard" => {
            if starts(&["getPrimaryClip", "hasPrimaryClip", "hasClipboardText"]) {
                out.push(s(Category::Clipboard, format!("lettura ({m})")));
            } else if starts(&["setPrimaryClip"]) {
                out.push(s(Category::Clipboard, format!("scrittura ({m})")));
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
                Some("ID del dispositivo (IMEI/MEID)")
            } else if m.contains("SubscriberId") {
                Some("IMSI")
            } else if m.contains("IccSerial") {
                Some("seriale della SIM (ICCID)")
            } else if m.contains("Line1Number") || m.contains("PhoneNumber") {
                Some("numero di telefono")
            } else if m.starts_with("getCellLocation") || m.starts_with("getAllCellInfo") {
                out.push(s(Category::Location, format!("celle radio ({m})")));
                None
            } else {
                None
            };
            if let Some(w) = what {
                out.push(s(Category::Identifier, format!("{w} ({m})")));
            }
        }
        "android.os.IDeviceIdentifiersPolicyService" if m.starts_with("getSerial") => {
            out.push(s(Category::Identifier, format!("numero di serie ({m})")));
        }
        "android.hardware.ICameraService" if m.starts_with("connect") => {
            out.push(s(Category::Camera, m));
        }
        "android.media.IAudioRecord" => out.push(s(Category::Microphone, format!("registrazione ({m})"))),
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
