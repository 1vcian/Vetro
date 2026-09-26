//! Parcel di Binder (M8): lettura dei tipi di base (`libbinder`,
//! `Parcel.cpp`) e dell'intestazione di una chiamata AIDL.
//!
//! Formato: tutto allineato a 4 byte, little-endian. `String16` = int32
//! lunghezza in unità UTF-16 (-1 = null), le unità, un terminatore a 0,
//! riempimento fino a 4. Intestazione di `writeInterfaceToken` (Android
//! 11+): int32 politica di strict mode, int32 uid della work source, int32
//! `'SYST'` (o `'VNDR'`), poi il descrittore come `String16`.

/// `'SYST'` e `'VNDR'` come li scrive `Parcel::writeInterfaceToken`.
pub const HEADER_SYSTEM: u32 = 0x5359_5354;
pub const HEADER_VENDOR: u32 = 0x564e_4452;

/// Un lettore sequenziale di un Parcel.
#[derive(Clone, Debug)]
pub struct Parcel<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Parcel<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Parcel { data, pos: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    pub fn i32(&mut self) -> Option<i32> {
        let b = self.data.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        Some(i32::from_le_bytes(b.try_into().ok()?))
    }

    pub fn i64(&mut self) -> Option<i64> {
        let b = self.data.get(self.pos..self.pos + 8)?;
        self.pos += 8;
        Some(i64::from_le_bytes(b.try_into().ok()?))
    }

    /// `String16`: `Some(None)` per il null.
    pub fn string16(&mut self) -> Option<Option<String>> {
        let save = self.pos;
        let len = self.i32()?;
        if len == -1 {
            return Some(None);
        }
        let n = usize::try_from(len).ok().filter(|&n| n <= 1 << 20)?;
        let bytes = (n + 1) * 2;
        let Some(raw) = self.data.get(self.pos..self.pos + n * 2) else {
            self.pos = save;
            return None;
        };
        let units: Vec<u16> = raw.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        self.pos += bytes.next_multiple_of(4);
        if self.pos > self.data.len() {
            self.pos = save;
            return None;
        }
        Some(Some(String::from_utf16_lossy(&units)))
    }

    /// Intestazione di una chiamata AIDL: descrittore dell'interfaccia.
    pub fn interface_header(&mut self) -> Option<InterfaceHeader> {
        let save = self.pos;
        let strict_mode = self.i32()?;
        let work_source = self.i32()?;
        let header = self.i32()? as u32;
        if (header == HEADER_SYSTEM || header == HEADER_VENDOR)
            && let Some(Some(d)) = self.string16()
        {
            return Some(InterfaceHeader { strict_mode, work_source_uid: work_source, descriptor: d });
        }
        // Formati più vecchi (senza intestazione o senza work source).
        for skip in [8usize, 4] {
            self.pos = save + skip;
            if let Some(Some(d)) = self.string16()
                && plausible_descriptor(&d)
            {
                return Some(InterfaceHeader { strict_mode, work_source_uid: -1, descriptor: d });
            }
        }
        self.pos = save;
        None
    }
}

/// Descrittore credibile: nome Java/AIDL (lettere, cifre, `.`, `_`, `$`, `/`).
pub fn plausible_descriptor(d: &str) -> bool {
    !d.is_empty()
        && d.len() <= 256
        && d.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '$' | '/' | '@' | ':'))
}

/// L'intestazione di `writeInterfaceToken`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterfaceHeader {
    pub strict_mode: i32,
    pub work_source_uid: i32,
    pub descriptor: String,
}

/// Tutte le stringhe `String16` leggibili nel Parcel (a ogni offset
/// allineato a 4): per l'ispettore privacy, che cerca chiavi note
/// (`android_id`, autorità dei provider) senza conoscere la firma del
/// metodo.
pub fn strings16(data: &[u8]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut o = 0usize;
    while o + 4 <= data.len() {
        let len = i32::from_le_bytes(data[o..o + 4].try_into().expect("4 byte"));
        if (2..=512).contains(&len) {
            let n = len as usize;
            if let Some(raw) = data.get(o + 4..o + 4 + n * 2)
                && data.get(o + 4 + n * 2..o + 6 + n * 2) == Some(&[0, 0])
            {
                let units: Vec<u16> = raw.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
                if units.iter().all(|&u| (0x20..0x7f).contains(&u)) {
                    out.push((o, String::from_utf16_lossy(&units)));
                    o += (4 + (n + 1) * 2).next_multiple_of(4);
                    continue;
                }
            }
        }
        o += 4;
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Costruisce un Parcel come `Parcel.java`.
    #[derive(Default)]
    pub(crate) struct Builder(pub Vec<u8>);

    impl Builder {
        pub fn i32(mut self, v: i32) -> Self {
            self.0.extend_from_slice(&v.to_le_bytes());
            self
        }
        pub fn s16(mut self, s: &str) -> Self {
            let u: Vec<u16> = s.encode_utf16().collect();
            self.0.extend_from_slice(&(u.len() as i32).to_le_bytes());
            for c in &u {
                self.0.extend_from_slice(&c.to_le_bytes());
            }
            self.0.extend_from_slice(&[0, 0]);
            while !self.0.len().is_multiple_of(4) {
                self.0.push(0);
            }
            self
        }
        pub fn token(self, d: &str) -> Self {
            self.i32(0x4200_0004).i32(-1).i32(HEADER_SYSTEM as i32).s16(d)
        }
    }

    #[test]
    fn intestazione_e_stringhe() {
        let p = Builder::default().token("android.content.IClipboard").s16("com.vetro.probe").i32(0).i32(0).0;
        let mut r = Parcel::new(&p);
        let h = r.interface_header().unwrap();
        assert_eq!(h.descriptor, "android.content.IClipboard");
        assert_eq!(h.work_source_uid, -1);
        assert_eq!(r.string16(), Some(Some("com.vetro.probe".into())));
        assert_eq!(r.i32(), Some(0));
        let s: Vec<String> = strings16(&p).into_iter().map(|x| x.1).collect();
        assert_eq!(s, ["android.content.IClipboard", "com.vetro.probe"]);
        // Null e troncati: niente panic.
        let mut n = Parcel::new(&[0xff, 0xff, 0xff, 0xff]);
        assert_eq!(n.string16(), Some(None));
        for cut in 0..p.len() {
            let _ = Parcel::new(&p[..cut]).interface_header();
            let _ = strings16(&p[..cut]);
        }
        // Senza intestazione SYST (formato vecchio, dopo strict mode).
        let old = Builder::default().i32(0).s16("android.os.IFoo").0;
        assert_eq!(Parcel::new(&old).interface_header().unwrap().descriptor, "android.os.IFoo");
    }
}
