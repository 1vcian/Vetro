//! Raw Binder: the commands of `ioctl(BINDER_WRITE_READ)` and the
//! transactions they contain, with the Parcel bytes. It is the base of
//! M8's decoder (AIDL mapping): here only the protocol structure
//! (`include/uapi/linux/android/binder.h`), 64-bit.
//!
//! Each command is an `_IOC` code followed by its payload, as long as the
//! code's size field says: the list splits without tables.

/// `_IOWR('b', 1, struct binder_write_read)`.
pub const BINDER_WRITE_READ: u64 = 0xc030_6201;

/// `struct binder_write_read` (48 bytes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WriteRead {
    pub write_size: u64,
    pub write_consumed: u64,
    pub write_buffer: u64,
    pub read_size: u64,
    pub read_consumed: u64,
    pub read_buffer: u64,
}

impl WriteRead {
    pub fn parse(b: &[u8]) -> Option<WriteRead> {
        let f =
            |i: usize| -> Option<u64> { Some(u64::from_le_bytes(b.get(8 * i..8 * i + 8)?.try_into().ok()?)) };
        Some(WriteRead {
            write_size: f(0)?,
            write_consumed: f(1)?,
            write_buffer: f(2)?,
            read_size: f(3)?,
            read_consumed: f(4)?,
            read_buffer: f(5)?,
        })
    }
}

/// A command of the write stream (`BC_*`) or of the read stream (`BR_*`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub code: u32,
    pub payload: Vec<u8>,
}

const BC: [&str; 22] = [
    "BC_TRANSACTION",
    "BC_REPLY",
    "BC_ACQUIRE_RESULT",
    "BC_FREE_BUFFER",
    "BC_INCREFS",
    "BC_ACQUIRE",
    "BC_RELEASE",
    "BC_DECREFS",
    "BC_INCREFS_DONE",
    "BC_ACQUIRE_DONE",
    "BC_ATTEMPT_ACQUIRE",
    "BC_REGISTER_LOOPER",
    "BC_ENTER_LOOPER",
    "BC_EXIT_LOOPER",
    "BC_REQUEST_DEATH_NOTIFICATION",
    "BC_CLEAR_DEATH_NOTIFICATION",
    "BC_DEAD_BINDER_DONE",
    "BC_TRANSACTION_SG",
    "BC_REPLY_SG",
    "BC_REQUEST_FREEZE_NOTIFICATION",
    "BC_CLEAR_FREEZE_NOTIFICATION",
    "BC_FREEZE_NOTIFICATION_DONE",
];

const BR: [&str; 21] = [
    "BR_ERROR",
    "BR_OK",
    "BR_TRANSACTION",
    "BR_REPLY",
    "BR_ACQUIRE_RESULT",
    "BR_DEAD_REPLY",
    "BR_TRANSACTION_COMPLETE",
    "BR_INCREFS",
    "BR_ACQUIRE",
    "BR_RELEASE",
    "BR_DECREFS",
    "BR_ATTEMPT_ACQUIRE",
    "BR_NOOP",
    "BR_SPAWN_LOOPER",
    "BR_FINISHED",
    "BR_DEAD_BINDER",
    "BR_CLEAR_DEATH_NOTIFICATION_DONE",
    "BR_FAILED_REPLY",
    "BR_FROZEN_REPLY",
    "BR_ONEWAY_SPAM_SUSPECT",
    "BR_TRANSACTION_PENDING_FROZEN",
];

impl Command {
    /// `_IOC` type (`'c'` for BC, `'r'` for BR).
    pub fn ioc_type(&self) -> u8 {
        (self.code >> 8) as u8
    }

    pub fn nr(&self) -> u8 {
        self.code as u8
    }

    /// Command name (`BR_TRANSACTION_SEC_CTX` is told apart by its
    /// size).
    pub fn name(&self) -> String {
        let nr = usize::from(self.nr());
        match self.ioc_type() {
            b'c' => BC.get(nr).map(|s| s.to_string()),
            b'r' if nr == 2 && self.payload.len() == 72 => Some("BR_TRANSACTION_SEC_CTX".into()),
            b'r' => BR.get(nr).map(|s| s.to_string()),
            _ => None,
        }
        .unwrap_or_else(|| format!("0x{:08x}", self.code))
    }

    /// Carries a transaction (or a reply).
    pub fn transaction(&self) -> Option<Transaction> {
        let reply = match (self.ioc_type(), self.nr()) {
            (b'c', 0 | 17) | (b'r', 2) => false,
            (b'c', 1 | 18) | (b'r', 3) => true,
            _ => return None,
        };
        let p = &self.payload;
        let u64_at =
            |o: usize| -> Option<u64> { Some(u64::from_le_bytes(p.get(o..o + 8)?.try_into().ok()?)) };
        let u32_at =
            |o: usize| -> Option<u32> { Some(u32::from_le_bytes(p.get(o..o + 4)?.try_into().ok()?)) };
        Some(Transaction {
            command: self.name(),
            reply,
            incoming: self.ioc_type() == b'r',
            target: u64_at(0)?,
            cookie: u64_at(8)?,
            code: u32_at(16)?,
            flags: u32_at(20)?,
            sender_pid: u32_at(24)? as i32,
            sender_euid: u32_at(28)?,
            data_size: u64_at(32)?,
            offsets_size: u64_at(40)?,
            buffer: u64_at(48)?,
            offsets: u64_at(56)?,
            data: Vec::new(),
            objects: Vec::new(),
        })
    }
}

/// Splits a stream of binder commands.
pub fn commands(buf: &[u8]) -> Vec<Command> {
    let mut out = Vec::new();
    let mut p = 0usize;
    while p + 4 <= buf.len() && out.len() < 4096 {
        let code = u32::from_le_bytes(buf[p..p + 4].try_into().expect("4 bytes"));
        let size = (code >> 16 & 0x3fff) as usize;
        p += 4;
        let Some(payload) = buf.get(p..p + size) else { break };
        out.push(Command { code, payload: payload.to_vec() });
        p += size;
    }
    out
}

/// `TF_ONE_WAY` flag.
pub const TF_ONE_WAY: u32 = 1;

/// A transaction (`struct binder_transaction_data`) with its data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transaction {
    /// `BC_TRANSACTION`, `BR_REPLY`, ...
    pub command: String,
    pub reply: bool,
    /// Received (BR, in the read stream) or sent (BC).
    pub incoming: bool,
    /// Handle (sent) or node pointer (received).
    pub target: u64,
    pub cookie: u64,
    pub code: u32,
    pub flags: u32,
    pub sender_pid: i32,
    pub sender_euid: u32,
    pub data_size: u64,
    pub offsets_size: u64,
    /// User addresses of the Parcel data and of the object table.
    pub buffer: u64,
    pub offsets: u64,
    /// The Parcel bytes (read from the process memory; may be fewer
    /// than `data_size` if truncated or unreadable).
    pub data: Vec<u8>,
    /// Offsets of the binder objects in the data.
    pub objects: Vec<u64>,
}

impl Transaction {
    pub fn one_way(&self) -> bool {
        self.flags & TF_ONE_WAY != 0
    }

    /// The interface descriptor at the start of the Parcel
    /// (`writeInterfaceToken`), if present: after strict mode, work source and
    /// the `SYST`/`VNDR` header (Android 11+), or with fewer fields in
    /// earlier versions.
    pub fn interface(&self) -> Option<String> {
        if self.reply {
            return None;
        }
        interface_token(&self.data)
    }
}

/// Interface descriptor (String16) at the head of a Parcel.
pub fn interface_token(d: &[u8]) -> Option<String> {
    for skip in [12usize, 8, 4] {
        let Some(len) = d.get(skip..skip + 4).map(|b| i32::from_le_bytes(b.try_into().expect("4 bytes")))
        else {
            continue;
        };
        if !(1..=256).contains(&len) {
            continue;
        }
        let start = skip + 4;
        let Some(chars) = d.get(start..start + 2 * len as usize) else { continue };
        let units: Vec<u16> = chars.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        if units.iter().all(|&u| (0x20..0x7f).contains(&u)) && d.get(start + 2 * len as usize..).is_some() {
            return Some(String::from_utf16_lossy(&units));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tr(code_cmd: u32, target: u64, code: u32, flags: u32, data_size: u64) -> Vec<u8> {
        let mut v = code_cmd.to_le_bytes().to_vec();
        let mut t = vec![0u8; 64];
        t[0..8].copy_from_slice(&target.to_le_bytes());
        t[16..20].copy_from_slice(&code.to_le_bytes());
        t[20..24].copy_from_slice(&flags.to_le_bytes());
        t[32..40].copy_from_slice(&data_size.to_le_bytes());
        t[48..56].copy_from_slice(&0x7000_1000u64.to_le_bytes());
        v.extend_from_slice(&t);
        v
    }

    #[test]
    fn comandi_e_transazioni() {
        // BC_INCREFS (4 bytes), BC_TRANSACTION, BC_ENTER_LOOPER (0 bytes).
        let mut w = 0x4004_6304u32.to_le_bytes().to_vec();
        w.extend_from_slice(&7u32.to_le_bytes());
        w.extend_from_slice(&tr(0x4040_6300, 3, 0x5f4e_5446, TF_ONE_WAY, 96));
        w.extend_from_slice(&0x0000_630cu32.to_le_bytes());
        let cmds = commands(&w);
        assert_eq!(
            cmds.iter().map(|c| c.name()).collect::<Vec<_>>(),
            ["BC_INCREFS", "BC_TRANSACTION", "BC_ENTER_LOOPER"]
        );
        let t = cmds[1].transaction().unwrap();
        assert_eq!((t.target, t.code, t.data_size, t.buffer), (3, 0x5f4e_5446, 96, 0x7000_1000));
        assert!(t.one_way() && !t.reply && !t.incoming);
        assert!(cmds[0].transaction().is_none());
        // Read stream: BR_NOOP, BR_TRANSACTION_SEC_CTX, BR_REPLY.
        let mut r = 0x0000_720cu32.to_le_bytes().to_vec();
        let mut sec = tr(0x8048_7202, 0xdead, 1, 0, 8);
        sec.extend_from_slice(&[0; 8]);
        r.extend_from_slice(&sec);
        r.extend_from_slice(&tr(0x8040_7203, 0, 0, 0, 4));
        let cmds = commands(&r);
        assert_eq!(
            cmds.iter().map(|c| c.name()).collect::<Vec<_>>(),
            ["BR_NOOP", "BR_TRANSACTION_SEC_CTX", "BR_REPLY"]
        );
        assert!(cmds[1].transaction().unwrap().incoming);
        assert!(cmds[2].transaction().unwrap().reply);
        // Truncated: no panic, complete commands only.
        for cut in 0..r.len() {
            let _ = commands(&r[..cut]);
        }
        let bwr = WriteRead::parse(&[1u8; 48]).unwrap();
        assert_eq!(bwr.read_buffer, 0x0101_0101_0101_0101);
    }

    #[test]
    fn descrittore_dell_interfaccia() {
        let name = "android.os.IServiceManager";
        let mut d = Vec::new();
        d.extend_from_slice(&0x4200_0004u32.to_le_bytes());
        d.extend_from_slice(&(-1i32).to_le_bytes());
        d.extend_from_slice(b"TSYS");
        d.extend_from_slice(&(name.len() as i32).to_le_bytes());
        for u in name.encode_utf16() {
            d.extend_from_slice(&u.to_le_bytes());
        }
        d.extend_from_slice(&[0, 0, 0, 0]);
        assert_eq!(interface_token(&d).as_deref(), Some(name));
        assert_eq!(interface_token(&d[..20]), None);
    }
}
