//! pcapng (IETF draft `draft-ietf-opsawg-pcapng`): one section, one
//! Ethernet interface (`LINKTYPE_ETHERNET`) with microsecond timestamp
//! resolution, one Enhanced Packet Block per frame with the direction in
//! `epb_flags` (seen from the guest: `outbound` the guest's frames, `inbound`
//! those towards the guest). Everything little-endian.
//!
//! The timestamps are the guest's virtual ones: instant 0 is the machine's
//! power-on (in Wireshark, 1970-01-01 00:00:00 UTC plus the guest time),
//! unless `epoch_us`.
//!
//! [`read`] reads back what [`write`] produces (and generic pcapng in both
//! byte orders, with EPB and SPB): used by the tests and the tools.

use super::capture::{Direction, Frame};

const SHB: u32 = 0x0A0D_0D0A;
const IDB: u32 = 1;
const SPB: u32 = 3;
const EPB: u32 = 6;
const BYTE_ORDER_MAGIC: u32 = 0x1A2B_3C4D;
pub const LINKTYPE_ETHERNET: u16 = 1;

/// Capture options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PcapngOptions {
    /// Unix microseconds to add to the guest time (0: the guest
    /// time is the time since the Unix epoch).
    pub epoch_us: u64,
    /// Interface name (`if_name`).
    pub interface: String,
}

impl Default for PcapngOptions {
    fn default() -> Self {
        PcapngOptions { epoch_us: 0, interface: "eth0".into() }
    }
}

fn option(out: &mut Vec<u8>, code: u16, value: &[u8]) {
    out.extend(code.to_le_bytes());
    out.extend((value.len() as u16).to_le_bytes());
    out.extend(value);
    out.resize(out.len().next_multiple_of(4), 0);
}

fn block(out: &mut Vec<u8>, kind: u32, body: &[u8]) {
    let len = (12 + body.len().next_multiple_of(4)) as u32;
    out.extend(kind.to_le_bytes());
    out.extend(len.to_le_bytes());
    out.extend(body);
    out.resize(out.len().next_multiple_of(4), 0);
    out.extend(len.to_le_bytes());
}

/// Writes the capture as pcapng.
pub fn write(frames: &[Frame], opts: &PcapngOptions) -> Vec<u8> {
    let mut out = Vec::new();
    // Section Header Block.
    let mut b = Vec::new();
    b.extend(BYTE_ORDER_MAGIC.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(0u16.to_le_bytes());
    b.extend((-1i64).to_le_bytes());
    option(&mut b, 4, concat!("Vetro ", env!("CARGO_PKG_VERSION")).as_bytes()); // shb_userappl
    option(&mut b, 0, &[]);
    block(&mut out, SHB, &b);
    // Interface Description Block.
    let mut b = Vec::new();
    b.extend(LINKTYPE_ETHERNET.to_le_bytes());
    b.extend(0u16.to_le_bytes());
    b.extend(0u32.to_le_bytes()); // snaplen: no limit
    option(&mut b, 2, opts.interface.as_bytes()); // if_name
    option(&mut b, 3, b"guest virtio-net (Vetro)"); // if_description
    option(&mut b, 9, &[6]); // if_tsresol: 10^-6
    option(&mut b, 0, &[]);
    block(&mut out, IDB, &b);
    for f in frames {
        let ts = opts.epoch_us.saturating_add(f.at_us);
        let mut b = Vec::with_capacity(32 + f.data.len());
        b.extend(0u32.to_le_bytes()); // interface 0
        b.extend(((ts >> 32) as u32).to_le_bytes());
        b.extend((ts as u32).to_le_bytes());
        b.extend((f.data.len() as u32).to_le_bytes());
        b.extend((f.data.len() as u32).to_le_bytes());
        b.extend(&f.data);
        b.resize(b.len().next_multiple_of(4), 0);
        let flags: u32 = match f.dir {
            Direction::ToGuest => 1,   // inbound
            Direction::FromGuest => 2, // outbound
        };
        option(&mut b, 2, &flags.to_le_bytes()); // epb_flags
        option(&mut b, 0, &[]);
        block(&mut out, EPB, &b);
    }
    out
}

/// Read error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PcapngError(pub String);

impl std::fmt::Display for PcapngError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pcapng: {}", self.0)
    }
}

impl std::error::Error for PcapngError {}

fn err<T>(msg: impl Into<String>) -> Result<T, PcapngError> {
    Err(PcapngError(msg.into()))
}

/// An interface read back: link type and timestamp unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interface {
    pub linktype: u16,
    /// Units per second (10^6 if `if_tsresol` is missing).
    pub units_per_sec: u64,
    pub name: Option<String>,
}

/// The content of a file: interfaces and frames (with the time converted to
/// microseconds, without removing the epoch; direction from `epb_flags`, `ToGuest`
/// if missing).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PcapngFile {
    pub interfaces: Vec<Interface>,
    pub frames: Vec<Frame>,
}

/// Reads back a pcapng file.
pub fn read(data: &[u8]) -> Result<PcapngFile, PcapngError> {
    let mut out = PcapngFile::default();
    let mut pos = 0usize;
    let mut le = true;
    let u32_at = |le: bool, b: &[u8], at: usize| -> Option<u32> {
        let w: [u8; 4] = b.get(at..at + 4)?.try_into().ok()?;
        Some(if le { u32::from_le_bytes(w) } else { u32::from_be_bytes(w) })
    };
    let u16_at = |le: bool, b: &[u8], at: usize| -> Option<u16> {
        let w: [u8; 2] = b.get(at..at + 2)?.try_into().ok()?;
        Some(if le { u16::from_le_bytes(w) } else { u16::from_be_bytes(w) })
    };
    let mut first = true;
    while pos < data.len() {
        let Some(kind) = u32_at(true, data, pos) else { return err("truncated block") };
        if kind == SHB {
            match data.get(pos + 8..pos + 12) {
                Some([0x4D, 0x3C, 0x2B, 0x1A]) => le = true,
                Some([0x1A, 0x2B, 0x3C, 0x4D]) => le = false,
                _ => return err("invalid byte-order magic"),
            }
            out.interfaces.clear();
        } else if first {
            return err("the file does not start with a Section Header Block");
        }
        first = false;
        let kind = u32_at(le, data, pos).unwrap_or(kind);
        let Some(len) = u32_at(le, data, pos + 4).map(|l| l as usize) else { return err("truncated block") };
        if len < 12 || len % 4 != 0 || pos + len > data.len() {
            return err(format!("invalid block length ({len}) at {pos}"));
        }
        if u32_at(le, data, pos + len - 4) != Some(len as u32) {
            return err(format!("trailing length differs at {pos}"));
        }
        let body = &data[pos + 8..pos + len - 4];
        match kind {
            IDB => {
                let linktype = u16_at(le, body, 0).ok_or_else(|| PcapngError("short IDB".into()))?;
                let mut iface = Interface { linktype, units_per_sec: 1_000_000, name: None };
                for (code, v) in options(le, body.get(8..).unwrap_or_default())? {
                    match (code, v) {
                        (9, [r, ..]) => {
                            let exp = u32::from(r & 0x7f);
                            iface.units_per_sec = if r & 0x80 != 0 {
                                2u64.checked_pow(exp).unwrap_or(u64::MAX)
                            } else {
                                10u64.checked_pow(exp).unwrap_or(u64::MAX)
                            };
                        }
                        (2, v) => iface.name = Some(String::from_utf8_lossy(v).into_owned()),
                        _ => {}
                    }
                }
                out.interfaces.push(iface);
            }
            EPB => {
                let field = |i: usize| u32_at(le, body, i).ok_or_else(|| PcapngError("short EPB".into()));
                let iface = field(0)? as usize;
                let ts = (u64::from(field(4)?) << 32) | u64::from(field(8)?);
                let caplen = field(12)? as usize;
                let Some(bytes) = body.get(20..20 + caplen) else { return err("EPB: truncated data") };
                let Some(i) = out.interfaces.get(iface) else { return err("EPB: unknown interface") };
                let at_us = (u128::from(ts) * 1_000_000 / u128::from(i.units_per_sec.max(1))) as u64;
                let mut dir = Direction::ToGuest;
                for (code, v) in options(le, &body[(20 + caplen).next_multiple_of(4).min(body.len())..])? {
                    if code == 2 && v.len() == 4 {
                        let flags = u32_at(le, v, 0).unwrap_or(0);
                        if flags & 3 == 2 {
                            dir = Direction::FromGuest;
                        }
                    }
                }
                out.frames.push(Frame { at_us, dir, data: bytes.to_vec() });
            }
            SPB => {
                let orig = u32_at(le, body, 0).ok_or_else(|| PcapngError("short SPB".into()))? as usize;
                let bytes = &body[4..(4 + orig).min(body.len())];
                out.frames.push(Frame { at_us: 0, dir: Direction::ToGuest, data: bytes.to_vec() });
            }
            _ => {}
        }
        pos += len;
    }
    Ok(out)
}

fn options(le: bool, mut b: &[u8]) -> Result<Vec<(u16, &[u8])>, PcapngError> {
    let mut v = Vec::new();
    while b.len() >= 4 {
        let (code, len) = if le {
            (u16::from_le_bytes([b[0], b[1]]), usize::from(u16::from_le_bytes([b[2], b[3]])))
        } else {
            (u16::from_be_bytes([b[0], b[1]]), usize::from(u16::from_be_bytes([b[2], b[3]])))
        };
        if code == 0 {
            break;
        }
        let Some(val) = b.get(4..4 + len) else { return err("truncated option") };
        v.push((code, val));
        b = b.get(4 + len.next_multiple_of(4)..).unwrap_or_default();
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames() -> Vec<Frame> {
        vec![
            Frame { at_us: 0, dir: Direction::FromGuest, data: vec![1; 60] },
            Frame { at_us: 1_500_001, dir: Direction::ToGuest, data: vec![2; 61] },
            Frame { at_us: 5_000_000_000_000, dir: Direction::FromGuest, data: (0..=255).collect() },
        ]
    }

    #[test]
    fn andata_e_ritorno() {
        let bytes = write(&frames(), &PcapngOptions::default());
        assert_eq!(bytes.len() % 4, 0);
        let f = read(&bytes).unwrap();
        assert_eq!(f.frames, frames());
        assert_eq!(
            f.interfaces,
            [Interface { linktype: LINKTYPE_ETHERNET, units_per_sec: 1_000_000, name: Some("eth0".into()) }]
        );
    }

    #[test]
    fn struttura_dei_blocchi() {
        let bytes = write(&frames()[..1], &PcapngOptions { epoch_us: 1 << 33, ..Default::default() });
        // SHB: type, length, magic, version 1.0, section length -1.
        assert_eq!(&bytes[0..4], &[0x0A, 0x0D, 0x0D, 0x0A]);
        assert_eq!(&bytes[8..12], &[0x4D, 0x3C, 0x2B, 0x1A]);
        assert_eq!(&bytes[12..16], &[1, 0, 0, 0]);
        assert_eq!(&bytes[16..24], &[0xff; 8]);
        let shb_len = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        let idb = &bytes[shb_len..];
        assert_eq!(&idb[0..4], &[1, 0, 0, 0]);
        assert_eq!(&idb[8..10], &[1, 0], "LINKTYPE_ETHERNET");
        let idb_len = u32::from_le_bytes(idb[4..8].try_into().unwrap()) as usize;
        let epb = &idb[idb_len..];
        assert_eq!(&epb[0..4], &[6, 0, 0, 0]);
        assert_eq!(&epb[12..16], &2u32.to_le_bytes(), "high timestamp");
        assert_eq!(&epb[16..20], &0u32.to_le_bytes(), "low timestamp");
        let f = read(&bytes).unwrap();
        assert_eq!(f.frames[0].at_us, 1 << 33);
    }

    #[test]
    fn errori() {
        assert!(read(b"").unwrap().frames.is_empty());
        assert!(read(&[1, 0, 0, 0, 12, 0, 0, 0, 12, 0, 0, 0]).is_err(), "without SHB");
        let mut b = write(&frames(), &PcapngOptions::default());
        b.truncate(b.len() - 3);
        assert!(read(&b).is_err());
    }
}
