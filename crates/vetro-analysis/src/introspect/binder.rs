//! Binder grezzo: i comandi di `ioctl(BINDER_WRITE_READ)` e le
//! transazioni che contengono, con i byte del Parcel. È la base del
//! decoder di M8 (mappatura AIDL): qui solo la struttura del protocollo
//! (`include/uapi/linux/android/binder.h`), a 64 bit.
//!
//! Ogni comando è un codice `_IOC` seguito dal suo carico, lungo quanto
//! dice il campo dimensione del codice: la lista si divide senza tabelle.

/// `_IOWR('b', 1, struct binder_write_read)`.
pub const BINDER_WRITE_READ: u64 = 0xc030_6201;

/// `struct binder_write_read` (48 byte).
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

/// Un comando del flusso di scrittura (`BC_*`) o di lettura (`BR_*`).
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
    /// Tipo `_IOC` (`'c'` per BC, `'r'` per BR).
    pub fn ioc_type(&self) -> u8 {
        (self.code >> 8) as u8
    }

    pub fn nr(&self) -> u8 {
        self.code as u8
    }

    /// Nome del comando (`BR_TRANSACTION_SEC_CTX` si distingue dalla
    /// dimensione).
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

    /// Porta una transazione (o una risposta).
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

/// Divide un flusso di comandi binder.
pub fn commands(buf: &[u8]) -> Vec<Command> {
    let mut out = Vec::new();
    let mut p = 0usize;
    while p + 4 <= buf.len() && out.len() < 4096 {
        let code = u32::from_le_bytes(buf[p..p + 4].try_into().expect("4 byte"));
        let size = (code >> 16 & 0x3fff) as usize;
        p += 4;
        let Some(payload) = buf.get(p..p + size) else { break };
        out.push(Command { code, payload: payload.to_vec() });
        p += size;
    }
    out
}

/// Flag `TF_ONE_WAY`.
pub const TF_ONE_WAY: u32 = 1;

/// Una transazione (`struct binder_transaction_data`) con i suoi dati.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transaction {
    /// `BC_TRANSACTION`, `BR_REPLY`, ...
    pub command: String,
    pub reply: bool,
    /// Ricevuta (BR, nel flusso di lettura) o inviata (BC).
    pub incoming: bool,
    /// Handle (inviata) o puntatore del nodo (ricevuta).
    pub target: u64,
    pub cookie: u64,
    pub code: u32,
    pub flags: u32,
    pub sender_pid: i32,
    pub sender_euid: u32,
    pub data_size: u64,
    pub offsets_size: u64,
    /// Indirizzi utente dei dati del Parcel e della tabella degli oggetti.
    pub buffer: u64,
    pub offsets: u64,
    /// I byte del Parcel (letti dalla memoria del processo; possono essere
    /// meno di `data_size` se troncati o non leggibili).
    pub data: Vec<u8>,
    /// Offset degli oggetti binder nei dati.
    pub objects: Vec<u64>,
}

impl Transaction {
    pub fn one_way(&self) -> bool {
        self.flags & TF_ONE_WAY != 0
    }

    /// Il descrittore d'interfaccia all'inizio del Parcel
    /// (`writeInterfaceToken`), se c'è: dopo strict mode, work source e
    /// intestazione `SYST`/`VNDR` (Android 11+), o con meno campi nelle
    /// versioni precedenti.
    pub fn interface(&self) -> Option<String> {
        if self.reply {
            return None;
        }
        interface_token(&self.data)
    }
}

/// Descrittore d'interfaccia (String16) in testa a un Parcel.
pub fn interface_token(d: &[u8]) -> Option<String> {
    for skip in [12usize, 8, 4] {
        let Some(len) = d.get(skip..skip + 4).map(|b| i32::from_le_bytes(b.try_into().expect("4 byte")))
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
        // BC_INCREFS (4 byte), BC_TRANSACTION, BC_ENTER_LOOPER (0 byte).
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
        // Flusso di lettura: BR_NOOP, BR_TRANSACTION_SEC_CTX, BR_REPLY.
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
        // Troncato: niente panic, comandi completi soltanto.
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
