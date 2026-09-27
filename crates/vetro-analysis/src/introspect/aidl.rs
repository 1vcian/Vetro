//! AIDL method names (M8): interface descriptor + transaction code ->
//! method name.
//!
//! The `aidl_aosp15.tsv` table comes from the compiled stubs of Vetro's
//! AOSP 15 image (`tools/aosp/aidl-map.sh`: `TRANSACTION_*` constants of
//! the `$Stub`s and `*_TRANSACTION` of `IContentProvider` in the jars of
//! `/system/framework`), so the codes are those of the running image.
//! Native-only interfaces (C++/NDK, HAL) and those of the jars in the APEXes
//! are not there: for them the numeric code remains.

use std::collections::BTreeMap;
use std::sync::OnceLock;

const TABLE: &str = include_str!("aidl_aosp15.tsv");

/// Reserved `IBinder` codes (`B_PACK_CHARS`).
pub const FIRST_CALL_TRANSACTION: u32 = 1;
pub const LAST_CALL_TRANSACTION: u32 = 0x00ff_ffff;

const fn pack(s: &[u8; 4]) -> u32 {
    (s[0] as u32) << 24 | (s[1] as u32) << 16 | (s[2] as u32) << 8 | s[3] as u32
}

/// Name of a reserved `IBinder` code, if it is one.
pub fn reserved_code(code: u32) -> Option<&'static str> {
    Some(match code {
        c if c == pack(b"_PNG") => "PING_TRANSACTION",
        c if c == pack(b"_DMP") => "DUMP_TRANSACTION",
        c if c == pack(b"_CMD") => "SHELL_COMMAND_TRANSACTION",
        c if c == pack(b"_NTF") => "INTERFACE_TRANSACTION",
        c if c == pack(b"_TWT") => "TWEET_TRANSACTION",
        c if c == pack(b"_LIK") => "LIKE_TRANSACTION",
        c if c == pack(b"_SPR") => "SYSPROPS_TRANSACTION",
        c if c == pack(b"_EXT") => "EXTENSION_TRANSACTION",
        c if c == pack(b"_PID") => "DEBUG_PID_TRANSACTION",
        c if c == pack(b"_RPC") => "SET_RPC_CLIENT_TRANSACTION",
        _ => return None,
    })
}

fn table() -> &'static BTreeMap<&'static str, BTreeMap<u32, &'static str>> {
    static T: OnceLock<BTreeMap<&'static str, BTreeMap<u32, &'static str>>> = OnceLock::new();
    T.get_or_init(|| {
        let mut m: BTreeMap<&str, BTreeMap<u32, &str>> = BTreeMap::new();
        for line in TABLE.lines() {
            let mut f = line.split('\t');
            if let (Some(d), Some(c), Some(n)) = (f.next(), f.next(), f.next())
                && let Ok(c) = c.parse::<u32>()
            {
                m.entry(d).or_default().insert(c, n);
            }
        }
        m
    })
}

/// Method `code` of interface `descriptor`, if it is in the table (or
/// is a reserved `IBinder` code).
pub fn method(descriptor: &str, code: u32) -> Option<&'static str> {
    if let Some(r) = reserved_code(code) {
        return Some(r);
    }
    table().get(descriptor)?.get(&code).copied()
}

/// The interface is in the table.
pub fn known_interface(descriptor: &str) -> bool {
    table().contains_key(descriptor)
}

/// Number of interfaces and of methods in the table.
pub fn table_size() -> (usize, usize) {
    let t = table();
    (t.len(), t.values().map(BTreeMap::len).sum())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interfacce_di_sistema_principali() {
        assert_eq!(method("android.content.IClipboard", 4), Some("getPrimaryClip"));
        assert_eq!(method("android.content.IContentProvider", 1), Some("query"));
        assert_eq!(method("android.content.IContentProvider", 21), Some("call"));
        assert_eq!(method("android.content.IClipboard", pack(b"_NTF")), Some("INTERFACE_TRANSACTION"));
        for d in [
            "android.app.IActivityManager",
            "android.app.IActivityTaskManager",
            "android.content.pm.IPackageManager",
            "android.location.ILocationManager",
            "com.android.internal.telephony.ITelephony",
            "com.android.internal.telephony.IPhoneSubInfo",
            "android.hardware.ICameraService",
            "android.os.IDeviceIdentifiersPolicyService",
        ] {
            assert!(known_interface(d), "{d}");
        }
        assert!(method("android.location.ILocationManager", 1).is_some());
        assert_eq!(method("vetro.INessuno", 1), None);
        let (i, m) = table_size();
        assert!(i > 1000 && m > 10000, "{i} {m}");
    }
}
