//! Virtqueue split (virtio v1.2, §2.7).
//!
//! Aree in RAM del guest, con `n` = Queue Size (potenza di 2):
//! - tabella dei descrittori: `n` voci da 16 byte (addr, len, flags, next);
//! - area del driver (available ring): flags, idx, `ring[n]`, used_event;
//! - area del dispositivo (used ring): flags, idx, `ring[n]` di (id, len),
//!   avail_event.
//!
//! Regole di validazione (una violazione è un errore della coda: il
//! trasporto porta il dispositivo in DEVICE_NEEDS_RESET, come fa QEMU con
//! `virtio_error`):
//! - testa e `next` dentro la tabella; una catena non visita più
//!   descrittori di quanti ne ha la tabella (niente cicli);
//! - i buffer scrivibili dal dispositivo seguono tutti quelli leggibili;
//! - INDIRECT solo se negoziato e solo sul descrittore di testa, con
//!   lunghezza multipla di 16 e non nulla; il flag NEXT sul descrittore
//!   indiretto si ignora (la catena finisce con la tabella, come QEMU) e
//!   dentro una tabella indiretta INDIRECT è vietato;
//! - `avail.idx` non può avanzare di più di `n` oltre l'ultimo consumato.

use core::fmt;

use super::{GuestRam, GuestRamExt, RamError};

pub const DESC_F_NEXT: u16 = 1;
pub const DESC_F_WRITE: u16 = 2;
pub const DESC_F_INDIRECT: u16 = 4;
/// `avail.flags`: il driver non vuole interrupt (senza EVENT_IDX).
pub const AVAIL_F_NO_INTERRUPT: u16 = 1;

/// Dimensione massima di una coda split (§2.7).
pub const MAX_QUEUE_SIZE: u16 = 32768;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueError {
    Ram(RamError),
    /// `avail.idx` è avanzato di più della dimensione della coda.
    AvailIdx {
        last: u16,
        avail: u16,
    },
    HeadOutOfRange(u16),
    NextOutOfRange(u16),
    /// Catena più lunga della tabella: c'è un ciclo.
    ChainLoop,
    /// Buffer leggibile dopo uno scrivibile.
    ReadableAfterWritable,
    IndirectNotNegotiated,
    /// Tabella indiretta vuota o di lunghezza non multipla di 16.
    IndirectLen(u32),
    /// INDIRECT fuori dal descrittore di testa o dentro una tabella.
    IndirectMisplaced,
    /// Richiesta malformata per il dispositivo (es. intestazione mancante).
    Malformed(&'static str),
}

impl From<RamError> for QueueError {
    fn from(e: RamError) -> Self {
        QueueError::Ram(e)
    }
}

impl fmt::Display for QueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueueError::Ram(e) => write!(f, "{e}"),
            QueueError::AvailIdx { last, avail } => {
                write!(f, "avail.idx salta da {last} a {avail}")
            }
            QueueError::HeadOutOfRange(h) => write!(f, "testa {h} fuori dalla tabella"),
            QueueError::NextOutOfRange(n) => write!(f, "next {n} fuori dalla tabella"),
            QueueError::ChainLoop => write!(f, "ciclo nella catena di descrittori"),
            QueueError::ReadableAfterWritable => write!(f, "buffer leggibile dopo uno scrivibile"),
            QueueError::IndirectNotNegotiated => write!(f, "INDIRECT senza VIRTIO_F_INDIRECT_DESC"),
            QueueError::IndirectLen(l) => write!(f, "tabella indiretta di {l} byte"),
            QueueError::IndirectMisplaced => write!(f, "INDIRECT fuori dal descrittore di testa"),
            QueueError::Malformed(m) => write!(f, "richiesta malformata: {m}"),
        }
    }
}

/// Un buffer della catena: indirizzo fisico e lunghezza.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Buf {
    pub addr: u64,
    pub len: u32,
}

/// Catena di descrittori estratta dall'available ring. `head` è l'id da
/// restituire nello used ring.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DescChain {
    pub head: u16,
    /// Buffer leggibili dal dispositivo (dal driver al dispositivo).
    pub readable: Vec<Buf>,
    /// Buffer scrivibili dal dispositivo (dal dispositivo al driver).
    pub writable: Vec<Buf>,
}

fn total(bufs: &[Buf]) -> u64 {
    bufs.iter().map(|b| u64::from(b.len)).sum()
}

/// Copia tra una sequenza di buffer vista come spazio contiguo e `len`
/// byte a partire da `offset`; `f(addr, inizio, fine)` agisce su ogni pezzo.
fn for_each_piece(
    bufs: &[Buf],
    mut offset: u64,
    len: usize,
    mut f: impl FnMut(u64, usize, usize) -> Result<(), RamError>,
) -> Result<usize, RamError> {
    let mut done = 0usize;
    for b in bufs {
        if done == len {
            break;
        }
        let blen = u64::from(b.len);
        if offset >= blen {
            offset -= blen;
            continue;
        }
        let n = ((blen - offset) as usize).min(len - done);
        f(b.addr + offset, done, done + n)?;
        done += n;
        offset = 0;
    }
    Ok(done)
}

impl DescChain {
    pub fn readable_len(&self) -> u64 {
        total(&self.readable)
    }

    pub fn writable_len(&self) -> u64 {
        total(&self.writable)
    }

    /// Legge dai buffer leggibili, visti come spazio contiguo, a partire da
    /// `offset`. Restituisce i byte letti (meno di `out.len()` alla fine).
    pub fn read(&self, ram: &dyn GuestRam, offset: u64, out: &mut [u8]) -> Result<usize, RamError> {
        let len = out.len();
        for_each_piece(&self.readable, offset, len, |addr, a, b| ram.read(addr, &mut out[a..b]))
    }

    /// Tutti i byte leggibili da `offset` alla fine.
    pub fn read_to_vec(&self, ram: &dyn GuestRam, offset: u64) -> Result<Vec<u8>, RamError> {
        let len = self.readable_len().saturating_sub(offset) as usize;
        let mut v = vec![0; len];
        self.read(ram, offset, &mut v)?;
        Ok(v)
    }

    /// Scrive nei buffer scrivibili, visti come spazio contiguo, a partire
    /// da `offset`. Restituisce i byte scritti (meno se lo spazio finisce).
    pub fn write(&self, ram: &mut dyn GuestRam, offset: u64, data: &[u8]) -> Result<usize, RamError> {
        for_each_piece(&self.writable, offset, data.len(), |addr, a, b| ram.write(addr, &data[a..b]))
    }
}

/// Descrittore come sta in memoria.
#[derive(Clone, Copy, Debug)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

fn read_desc(ram: &dyn GuestRam, table: u64, i: u16) -> Result<Desc, RamError> {
    let mut b = [0u8; 16];
    ram.read(table + 16 * u64::from(i), &mut b)?;
    Ok(Desc {
        addr: u64::from_le_bytes(b[0..8].try_into().unwrap()),
        len: u32::from_le_bytes(b[8..12].try_into().unwrap()),
        flags: u16::from_le_bytes(b[12..14].try_into().unwrap()),
        next: u16::from_le_bytes(b[14..16].try_into().unwrap()),
    })
}

/// `vring_need_event` (§2.7.10): serve un evento se `event` sta in
/// `[old, new)` modulo 2^16.
pub fn need_event(event: u16, new: u16, old: u16) -> bool {
    new.wrapping_sub(event).wrapping_sub(1) < new.wrapping_sub(old)
}

/// Stato di una coda split lato dispositivo.
#[derive(Clone, Debug)]
pub struct Virtqueue {
    max_size: u16,
    size: u16,
    ready: bool,
    desc: u64,
    driver: u64,
    device: u64,
    /// Prossimo indice dell'available ring da consumare.
    last_avail: u16,
    /// Prossimo indice dello used ring da scrivere (copia di `used.idx`).
    used_idx: u16,
    /// `used.idx` all'ultima decisione sull'interrupt.
    signalled_used: u16,
    pub(crate) event_idx: bool,
    pub(crate) indirect: bool,
}

impl Virtqueue {
    pub fn new(max_size: u16) -> Self {
        Self {
            max_size,
            size: max_size,
            ready: false,
            desc: 0,
            driver: 0,
            device: 0,
            last_avail: 0,
            used_idx: 0,
            signalled_used: 0,
            event_idx: false,
            indirect: false,
        }
    }

    /// Torna allo stato di reset (dimensione = massima, non pronta).
    pub fn reset(&mut self) {
        *self = Self::new(self.max_size);
    }

    pub fn max_size(&self) -> u16 {
        self.max_size
    }
    pub fn size(&self) -> u16 {
        self.size
    }
    pub fn ready(&self) -> bool {
        self.ready
    }
    /// Indirizzi di tabella, area del driver e area del dispositivo.
    pub fn addrs(&self) -> (u64, u64, u64) {
        (self.desc, self.driver, self.device)
    }

    pub(crate) fn set_size(&mut self, n: u16) {
        self.size = n;
    }
    pub(crate) fn set_desc(&mut self, a: u64) {
        self.desc = a;
    }
    pub(crate) fn set_driver(&mut self, a: u64) {
        self.driver = a;
    }
    pub(crate) fn set_device(&mut self, a: u64) {
        self.device = a;
    }

    /// QueueReady: la coda diventa pronta solo con una configurazione valida
    /// (dimensione potenza di 2 non oltre il massimo, aree allineate come
    /// chiede §2.7: 16, 2 e 4 byte). Restituisce lo stato risultante.
    pub(crate) fn set_ready(&mut self, ready: bool) -> bool {
        let valid = self.size != 0
            && self.size.is_power_of_two()
            && self.size <= self.max_size
            && self.desc.is_multiple_of(16)
            && self.driver.is_multiple_of(2)
            && self.device.is_multiple_of(4);
        self.ready = ready && valid;
        if self.ready {
            self.last_avail = 0;
            self.used_idx = 0;
            self.signalled_used = 0;
        }
        self.ready
    }

    fn avail_idx(&self, ram: &dyn GuestRam) -> Result<u16, QueueError> {
        let idx = ram.read_u16(self.driver + 2)?;
        if idx.wrapping_sub(self.last_avail) > self.size {
            return Err(QueueError::AvailIdx { last: self.last_avail, avail: idx });
        }
        Ok(idx)
    }

    /// Numero di catene disponibili e non ancora consumate.
    pub fn available(&self, ram: &dyn GuestRam) -> Result<u16, QueueError> {
        if !self.ready {
            return Ok(0);
        }
        Ok(self.avail_idx(ram)?.wrapping_sub(self.last_avail))
    }

    /// Con EVENT_IDX: chiede al driver una notifica quando pubblica
    /// l'indice `last_avail` (campo avail_event dello used ring).
    fn publish_avail_event(&self, ram: &mut dyn GuestRam) -> Result<(), RamError> {
        if self.event_idx {
            ram.write_u16(self.device + 4 + 8 * u64::from(self.size), self.last_avail)?;
        }
        Ok(())
    }

    /// Estrae la prossima catena disponibile, se c'è.
    pub fn pop(&mut self, ram: &mut dyn GuestRam) -> Result<Option<DescChain>, QueueError> {
        if !self.ready {
            return Ok(None);
        }
        let avail = self.avail_idx(ram)?;
        if avail == self.last_avail {
            self.publish_avail_event(ram)?;
            return Ok(None);
        }
        let slot = u64::from(self.last_avail % self.size);
        let head = ram.read_u16(self.driver + 4 + 2 * slot)?;
        let chain = self.walk(ram, head)?;
        self.last_avail = self.last_avail.wrapping_add(1);
        self.publish_avail_event(ram)?;
        Ok(Some(chain))
    }

    /// Rimette le ultime `n` catene estratte nell'available ring (il
    /// dispositivo non le ha usate, es. un frame che non ci stava).
    pub fn rewind(&mut self, ram: &mut dyn GuestRam, n: u16) -> Result<(), RamError> {
        self.last_avail = self.last_avail.wrapping_sub(n);
        self.publish_avail_event(ram)
    }

    fn walk(&self, ram: &dyn GuestRam, head: u16) -> Result<DescChain, QueueError> {
        if head >= self.size {
            return Err(QueueError::HeadOutOfRange(head));
        }
        let mut chain = DescChain { head, ..DescChain::default() };
        let mut d = read_desc(ram, self.desc, head)?;
        let (table, table_len) = if d.flags & DESC_F_INDIRECT != 0 {
            if !self.indirect {
                return Err(QueueError::IndirectNotNegotiated);
            }
            if d.len == 0 || d.len % 16 != 0 || d.len / 16 > u32::from(MAX_QUEUE_SIZE) {
                return Err(QueueError::IndirectLen(d.len));
            }
            let t = (d.addr, (d.len / 16) as u16);
            d = read_desc(ram, t.0, 0)?;
            t
        } else {
            (self.desc, self.size)
        };
        let mut visited = 0u32;
        loop {
            visited += 1;
            if visited > u32::from(table_len) {
                return Err(QueueError::ChainLoop);
            }
            if d.flags & DESC_F_INDIRECT != 0 {
                return Err(QueueError::IndirectMisplaced);
            }
            let buf = Buf { addr: d.addr, len: d.len };
            if d.flags & DESC_F_WRITE != 0 {
                chain.writable.push(buf);
            } else if chain.writable.is_empty() {
                chain.readable.push(buf);
            } else {
                return Err(QueueError::ReadableAfterWritable);
            }
            if d.flags & DESC_F_NEXT == 0 {
                return Ok(chain);
            }
            if d.next >= table_len {
                return Err(QueueError::NextOutOfRange(d.next));
            }
            d = read_desc(ram, table, d.next)?;
        }
    }

    /// Restituisce la catena `head` al driver con `len` byte scritti.
    pub fn push_used(&mut self, ram: &mut dyn GuestRam, head: u16, len: u32) -> Result<(), RamError> {
        let slot = u64::from(self.used_idx % self.size);
        let elem = self.device + 4 + 8 * slot;
        ram.write_u32(elem, u32::from(head))?;
        ram.write_u32(elem + 4, len)?;
        self.used_idx = self.used_idx.wrapping_add(1);
        ram.write_u16(self.device + 2, self.used_idx)
    }

    /// Dopo una serie di `push_used`: il driver va avvisato? Senza
    /// EVENT_IDX decide `avail.flags` (NO_INTERRUPT); con EVENT_IDX serve
    /// che `used_event` sia stato superato da quest'ultima serie.
    pub(crate) fn should_notify(&mut self, ram: &dyn GuestRam) -> Result<bool, RamError> {
        let (old, new) = (self.signalled_used, self.used_idx);
        if !self.ready || old == new {
            return Ok(false);
        }
        self.signalled_used = new;
        if self.event_idx {
            let event = ram.read_u16(self.driver + 4 + 2 * u64::from(self.size))?;
            Ok(need_event(event, new, old))
        } else {
            Ok(ram.read_u16(self.driver)? & AVAIL_F_NO_INTERRUPT == 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio::VecRam;

    const DESC: u64 = 0x1000;
    const AVAIL: u64 = 0x2000;
    const USED: u64 = 0x3000;

    fn ram() -> VecRam {
        VecRam::new(0, 0x10000)
    }

    fn queue(size: u16) -> Virtqueue {
        let mut q = Virtqueue::new(256);
        q.set_size(size);
        q.set_desc(DESC);
        q.set_driver(AVAIL);
        q.set_device(USED);
        assert!(q.set_ready(true));
        q
    }

    fn put_desc(r: &mut VecRam, table: u64, i: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let mut b = [0u8; 16];
        b[0..8].copy_from_slice(&addr.to_le_bytes());
        b[8..12].copy_from_slice(&len.to_le_bytes());
        b[12..14].copy_from_slice(&flags.to_le_bytes());
        b[14..16].copy_from_slice(&next.to_le_bytes());
        r.write(table + 16 * u64::from(i), &b).unwrap();
    }

    fn offer(r: &mut VecRam, size: u16, head: u16) {
        let idx = r.read_u16(AVAIL + 2).unwrap();
        r.write_u16(AVAIL + 4 + 2 * u64::from(idx % size), head).unwrap();
        r.write_u16(AVAIL + 2, idx.wrapping_add(1)).unwrap();
    }

    #[test]
    fn need_event_come_linux() {
        assert!(need_event(0, 1, 0));
        assert!(!need_event(1, 1, 0));
        assert!(need_event(5, 8, 3));
        assert!(!need_event(8, 8, 3));
        assert!(need_event(0xFFFF, 2, 0xFFFE)); // a cavallo dell'overflow
    }

    #[test]
    fn ready_rifiuta_configurazioni_invalide() {
        let mut q = Virtqueue::new(8);
        q.set_size(6);
        assert!(!q.set_ready(true), "non potenza di 2");
        q.set_size(16);
        assert!(!q.set_ready(true), "oltre il massimo");
        q.set_size(8);
        q.set_desc(0x1008);
        assert!(!q.set_ready(true), "tabella non allineata a 16");
        q.set_desc(0x1000);
        q.set_device(0x3002);
        assert!(!q.set_ready(true), "used ring non allineato a 4");
        q.set_device(0x3000);
        assert!(q.set_ready(true));
        assert!(!q.set_ready(false));
    }

    #[test]
    fn catena_diretta_leggibili_poi_scrivibili() {
        let mut r = ram();
        let mut q = queue(8);
        put_desc(&mut r, DESC, 3, 0x5000, 4, DESC_F_NEXT, 5);
        put_desc(&mut r, DESC, 5, 0x6000, 3, DESC_F_NEXT, 1);
        put_desc(&mut r, DESC, 1, 0x7000, 8, DESC_F_WRITE, 0);
        r.write(0x5000, b"abcd").unwrap();
        r.write(0x6000, b"efg").unwrap();
        offer(&mut r, 8, 3);
        assert_eq!(q.available(&r), Ok(1));
        let c = q.pop(&mut r).unwrap().unwrap();
        assert_eq!(c.head, 3);
        assert_eq!(c.readable_len(), 7);
        assert_eq!(c.writable, [Buf { addr: 0x7000, len: 8 }]);
        assert_eq!(c.read_to_vec(&r, 2).unwrap(), b"cdefg");
        assert_eq!(c.write(&mut r, 6, b"xyz").unwrap(), 2);
        assert_eq!(&r.bytes[0x7006..0x7008], b"xy");
        assert_eq!(q.pop(&mut r), Ok(None));
    }

    #[test]
    fn used_ring_e_notifica_senza_event_idx() {
        let mut r = ram();
        let mut q = queue(4);
        for head in [2, 0, 1, 3, 2] {
            put_desc(&mut r, DESC, head, 0x5000, 1, DESC_F_WRITE, 0);
            offer(&mut r, 4, head);
            let c = q.pop(&mut r).unwrap().unwrap();
            q.push_used(&mut r, c.head, 1).unwrap();
        }
        assert_eq!(r.read_u16(USED + 2), Ok(5));
        // Il quinto elemento torna nello slot 0.
        assert_eq!(r.read_u32(USED + 4), Ok(2));
        assert_eq!(r.read_u32(USED + 8), Ok(1));
        assert_eq!(q.should_notify(&r), Ok(true));
        assert_eq!(q.should_notify(&r), Ok(false), "niente di nuovo");
        r.write_u16(AVAIL, AVAIL_F_NO_INTERRUPT).unwrap();
        offer(&mut r, 4, 0);
        let c = q.pop(&mut r).unwrap().unwrap();
        q.push_used(&mut r, c.head, 0).unwrap();
        assert_eq!(q.should_notify(&r), Ok(false));
    }

    #[test]
    fn event_idx_rispetta_used_event_e_pubblica_avail_event() {
        let mut r = ram();
        let mut q = queue(8);
        q.event_idx = true;
        let used_event = AVAIL + 4 + 2 * 8;
        let avail_event = USED + 4 + 8 * 8;
        // Il driver vuole un interrupt solo dopo il terzo buffer (idx 2).
        r.write_u16(used_event, 2).unwrap();
        for i in 0..3u16 {
            put_desc(&mut r, DESC, i, 0x5000, 1, 0, 0);
            offer(&mut r, 8, i);
            let c = q.pop(&mut r).unwrap().unwrap();
            assert_eq!(r.read_u16(avail_event), Ok(i + 1));
            q.push_used(&mut r, c.head, 0).unwrap();
            assert_eq!(q.should_notify(&r), Ok(i == 2), "dopo il buffer {i}");
        }
        // Due buffer insieme che scavalcano used_event = 4: una notifica.
        r.write_u16(used_event, 4).unwrap();
        for i in 3..5u16 {
            put_desc(&mut r, DESC, i, 0x5000, 1, 0, 0);
            offer(&mut r, 8, i);
            let c = q.pop(&mut r).unwrap().unwrap();
            q.push_used(&mut r, c.head, 0).unwrap();
        }
        assert_eq!(q.should_notify(&r), Ok(true));
    }

    #[test]
    fn catena_indiretta() {
        let mut r = ram();
        let mut q = queue(8);
        q.indirect = true;
        let table = 0x8000;
        put_desc(&mut r, table, 0, 0x5000, 2, DESC_F_NEXT, 2);
        put_desc(&mut r, table, 2, 0x5100, 2, DESC_F_NEXT, 1);
        put_desc(&mut r, table, 1, 0x5200, 4, DESC_F_WRITE, 0);
        // WRITE sul descrittore indiretto va ignorato, NEXT anche.
        put_desc(&mut r, DESC, 6, table, 48, DESC_F_INDIRECT | DESC_F_WRITE | DESC_F_NEXT, 7);
        offer(&mut r, 8, 6);
        let c = q.pop(&mut r).unwrap().unwrap();
        assert_eq!(c.head, 6);
        assert_eq!(c.readable, [Buf { addr: 0x5000, len: 2 }, Buf { addr: 0x5100, len: 2 }]);
        assert_eq!(c.writable, [Buf { addr: 0x5200, len: 4 }]);
    }

    fn pop_err(setup: impl FnOnce(&mut VecRam, &mut Virtqueue)) -> QueueError {
        let mut r = ram();
        let mut q = queue(4);
        setup(&mut r, &mut q);
        q.pop(&mut r).unwrap_err()
    }

    #[test]
    fn errori_delle_catene() {
        assert_eq!(pop_err(|r, _| offer(r, 4, 4)), QueueError::HeadOutOfRange(4));
        assert_eq!(
            pop_err(|r, _| {
                put_desc(r, DESC, 0, 0, 1, DESC_F_NEXT, 9);
                offer(r, 4, 0);
            }),
            QueueError::NextOutOfRange(9)
        );
        assert_eq!(
            pop_err(|r, _| {
                put_desc(r, DESC, 0, 0, 1, DESC_F_NEXT, 1);
                put_desc(r, DESC, 1, 0, 1, DESC_F_NEXT, 0);
                offer(r, 4, 0);
            }),
            QueueError::ChainLoop
        );
        assert_eq!(
            pop_err(|r, _| {
                put_desc(r, DESC, 0, 0, 1, DESC_F_WRITE | DESC_F_NEXT, 1);
                put_desc(r, DESC, 1, 0, 1, 0, 0);
                offer(r, 4, 0);
            }),
            QueueError::ReadableAfterWritable
        );
        assert_eq!(
            pop_err(|r, _| {
                put_desc(r, DESC, 0, 0x8000, 16, DESC_F_INDIRECT, 0);
                offer(r, 4, 0);
            }),
            QueueError::IndirectNotNegotiated
        );
        assert_eq!(
            pop_err(|r, q| {
                q.indirect = true;
                put_desc(r, DESC, 0, 0x8000, 24, DESC_F_INDIRECT, 0);
                offer(r, 4, 0);
            }),
            QueueError::IndirectLen(24)
        );
        assert_eq!(
            pop_err(|r, q| {
                q.indirect = true;
                put_desc(r, DESC, 0, 0x8000, 16, DESC_F_INDIRECT, 0);
                put_desc(r, 0x8000, 0, 0x9000, 16, DESC_F_INDIRECT, 0);
                offer(r, 4, 0);
            }),
            QueueError::IndirectMisplaced
        );
        assert_eq!(
            pop_err(|r, q| {
                q.indirect = true;
                put_desc(r, 0x8000, 0, 0, 1, DESC_F_NEXT, 0);
                put_desc(r, DESC, 0, 0x8000, 16, DESC_F_INDIRECT, 0);
                offer(r, 4, 0);
            }),
            QueueError::ChainLoop
        );
        assert_eq!(
            pop_err(|r, _| r.write_u16(AVAIL + 2, 5).unwrap()),
            QueueError::AvailIdx { last: 0, avail: 5 }
        );
        assert!(matches!(
            pop_err(|r, _| {
                put_desc(r, DESC, 0, 0, 1, DESC_F_NEXT, 1);
                offer(r, 4, 0);
                r.bytes.truncate(0x2000);
            }),
            QueueError::Ram(_)
        ));
    }

    #[test]
    fn rewind_rimette_le_catene() {
        let mut r = ram();
        let mut q = queue(4);
        put_desc(&mut r, DESC, 0, 0x5000, 1, 0, 0);
        put_desc(&mut r, DESC, 1, 0x5000, 1, 0, 0);
        offer(&mut r, 4, 0);
        offer(&mut r, 4, 1);
        q.pop(&mut r).unwrap().unwrap();
        q.pop(&mut r).unwrap().unwrap();
        q.rewind(&mut r, 2).unwrap();
        assert_eq!(q.available(&r), Ok(2));
        assert_eq!(q.pop(&mut r).unwrap().unwrap().head, 0);
    }
}
