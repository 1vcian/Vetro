//! Driver virtio di prova: fa quello che fa Linux (drivers/virtio/
//! virtio_mmio.c e virtio_ring.c) su una RAM finta. Negozia le feature,
//! prepara le code split, pubblica catene (dirette o indirette), notifica,
//! legge lo used ring e gestisce l'interrupt.

use std::collections::HashMap;

use super::queue::{DESC_F_INDIRECT, DESC_F_NEXT, DESC_F_WRITE};
use super::*;

/// Base della RAM finta: la stessa della macchina virt.
pub const RAM_BASE: u64 = 0x4000_0000;
const RAM_SIZE: usize = 8 << 20;

/// Come il driver raggiunge i registri del trasporto.
pub trait Transport {
    fn rd(&mut self, off: u64) -> u32;
    fn wr(&mut self, off: u64, v: u32);
    fn cfg(&mut self, off: u64, size: u8) -> u64;
    fn cfg_wr(&mut self, off: u64, size: u8, v: u64);
    fn service(&mut self, ram: &mut VecRam);
}

impl Transport for VirtioMmio {
    fn rd(&mut self, off: u64) -> u32 {
        self.read(off, 4) as u32
    }
    fn wr(&mut self, off: u64, v: u32) {
        self.write(off, 4, u64::from(v));
    }
    fn cfg(&mut self, off: u64, size: u8) -> u64 {
        self.read(CONFIG + off, size)
    }
    fn cfg_wr(&mut self, off: u64, size: u8, v: u64) {
        self.write(CONFIG + off, size, v);
    }
    fn service(&mut self, ram: &mut VecRam) {
        VirtioMmio::service(self, ram);
    }
}

pub struct DrvQueue {
    pub size: u16,
    pub desc: u64,
    pub avail: u64,
    pub used: u64,
    free: Vec<u16>,
    avail_idx: u16,
    last_used: u16,
    /// Descrittori (diretti) occupati da ogni catena, per testa.
    chains: HashMap<u16, Vec<u16>>,
}

pub struct Driver<T: Transport> {
    pub t: T,
    pub ram: VecRam,
    next: u64,
    pub queues: Vec<DrvQueue>,
    pub features: u64,
}

/// Buffer di una catena: (indirizzo, lunghezza, scrivibile dal dispositivo).
pub type B = (u64, u32, bool);

impl<T: Transport> Driver<T> {
    pub fn new(t: T) -> Self {
        Self { t, ram: VecRam::new(RAM_BASE, RAM_SIZE), next: RAM_BASE, queues: Vec::new(), features: 0 }
    }

    pub fn alloc(&mut self, len: u64, align: u64) -> u64 {
        let a = self.next.next_multiple_of(align);
        self.next = a + len.max(1);
        assert!(self.next <= RAM_BASE + RAM_SIZE as u64, "RAM finta esaurita");
        a
    }

    /// Buffer con il contenuto dato.
    pub fn buf(&mut self, data: &[u8]) -> u64 {
        let a = self.alloc(data.len() as u64, 8);
        self.ram.write(a, data).unwrap();
        a
    }

    pub fn mem(&self, addr: u64, len: usize) -> Vec<u8> {
        let mut v = vec![0; len];
        self.ram.read(addr, &mut v).unwrap();
        v
    }

    fn status(&mut self, bits: u32) {
        let s = self.t.rd(STATUS);
        self.t.wr(STATUS, s | bits);
    }

    /// Fino a FEATURES_OK: reset, ACKNOWLEDGE, DRIVER, feature `offerte &
    /// wanted`. Restituisce lo Status riletto.
    pub fn negotiate(&mut self, wanted: u64) -> u32 {
        assert_eq!(self.t.rd(MAGIC_VALUE), MAGIC);
        assert_eq!(self.t.rd(VERSION), 2);
        self.t.wr(STATUS, 0);
        assert_eq!(self.t.rd(STATUS), 0);
        self.status(STATUS_ACKNOWLEDGE);
        self.status(STATUS_DRIVER);
        self.t.wr(DEVICE_FEATURES_SEL, 1);
        let mut offered = u64::from(self.t.rd(DEVICE_FEATURES)) << 32;
        self.t.wr(DEVICE_FEATURES_SEL, 0);
        offered |= u64::from(self.t.rd(DEVICE_FEATURES));
        self.features = offered & wanted;
        self.t.wr(DRIVER_FEATURES_SEL, 1);
        self.t.wr(DRIVER_FEATURES, (self.features >> 32) as u32);
        self.t.wr(DRIVER_FEATURES_SEL, 0);
        self.t.wr(DRIVER_FEATURES, self.features as u32);
        self.status(STATUS_FEATURES_OK);
        self.t.rd(STATUS)
    }

    /// Negoziazione completa, code (al massimo `qsize` voci) e DRIVER_OK.
    pub fn init(&mut self, wanted: u64, qsize: u16) -> u64 {
        let st = self.negotiate(wanted);
        assert_ne!(st & STATUS_FEATURES_OK, 0, "FEATURES_OK rifiutato");
        self.queues.clear();
        for i in 0.. {
            self.t.wr(QUEUE_SEL, i);
            assert_eq!(self.t.rd(QUEUE_READY), 0);
            let max = self.t.rd(QUEUE_NUM_MAX);
            if max == 0 {
                break;
            }
            self.setup_queue((max as u16).min(qsize));
        }
        self.status(STATUS_DRIVER_OK);
        self.features
    }

    fn setup_queue(&mut self, size: u16) {
        let n = u64::from(size);
        let desc = self.alloc(16 * n, 16);
        let avail = self.alloc(6 + 2 * n, 2);
        let used = self.alloc(6 + 8 * n, 4);
        self.t.wr(QUEUE_NUM, u32::from(size));
        for (lo, a) in [(QUEUE_DESC_LOW, desc), (QUEUE_DRIVER_LOW, avail), (QUEUE_DEVICE_LOW, used)] {
            self.t.wr(lo, a as u32);
            self.t.wr(lo + 4, (a >> 32) as u32);
        }
        self.t.wr(QUEUE_READY, 1);
        assert_eq!(self.t.rd(QUEUE_READY), 1);
        self.queues.push(DrvQueue {
            size,
            desc,
            avail,
            used,
            free: (0..size).rev().collect(),
            avail_idx: 0,
            last_used: 0,
            chains: HashMap::new(),
        });
    }

    fn put_desc(&mut self, table: u64, i: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let mut b = [0u8; 16];
        b[0..8].copy_from_slice(&addr.to_le_bytes());
        b[8..12].copy_from_slice(&len.to_le_bytes());
        b[12..14].copy_from_slice(&flags.to_le_bytes());
        b[14..16].copy_from_slice(&next.to_le_bytes());
        self.ram.write(table + 16 * u64::from(i), &b).unwrap();
    }

    /// Pubblica la testa nell'available ring e notifica la coda.
    fn publish(&mut self, qi: usize, head: u16) {
        let q = &mut self.queues[qi];
        let slot = u64::from(q.avail_idx % q.size);
        let (avail, idx) = (q.avail, q.avail_idx.wrapping_add(1));
        q.avail_idx = idx;
        self.ram.write_u16(avail + 4 + 2 * slot, head).unwrap();
        self.ram.write_u16(avail + 2, idx).unwrap();
        self.t.wr(QUEUE_NOTIFY, qi as u32);
    }

    /// Catena di descrittori diretti.
    pub fn add(&mut self, qi: usize, bufs: &[B]) -> u16 {
        let q = &mut self.queues[qi];
        let ids: Vec<u16> = (0..bufs.len()).map(|_| q.free.pop().expect("coda piena")).collect();
        let table = q.desc;
        for (k, &(addr, len, w)) in bufs.iter().enumerate() {
            let mut flags = if w { DESC_F_WRITE } else { 0 };
            let next = ids.get(k + 1).copied().unwrap_or(0);
            if k + 1 < bufs.len() {
                flags |= DESC_F_NEXT;
            }
            self.put_desc(table, ids[k], addr, len, flags, next);
        }
        self.queues[qi].chains.insert(ids[0], ids.clone());
        self.publish(qi, ids[0]);
        ids[0]
    }

    /// Catena in una tabella indiretta (un solo descrittore nella coda).
    pub fn add_indirect(&mut self, qi: usize, bufs: &[B]) -> u16 {
        let table = self.alloc(16 * bufs.len() as u64, 16);
        for (k, &(addr, len, w)) in bufs.iter().enumerate() {
            let mut flags = if w { DESC_F_WRITE } else { 0 };
            if k + 1 < bufs.len() {
                flags |= DESC_F_NEXT;
            }
            self.put_desc(table, k as u16, addr, len, flags, k as u16 + 1);
        }
        let q = &mut self.queues[qi];
        let head = q.free.pop().expect("coda piena");
        let desc = q.desc;
        self.put_desc(desc, head, table, 16 * bufs.len() as u32, DESC_F_INDIRECT, 0);
        self.queues[qi].chains.insert(head, vec![head]);
        self.publish(qi, head);
        head
    }

    /// Prossimo elemento dello used ring (testa, byte scritti), come
    /// `virtqueue_get_buf`; con EVENT_IDX aggiorna used_event come
    /// `virtqueue_enable_cb`.
    pub fn pop_used(&mut self, qi: usize) -> Option<(u16, u32)> {
        let event_idx = self.features & F_EVENT_IDX != 0;
        let q = &mut self.queues[qi];
        let idx = self.ram.read_u16(q.used + 2).unwrap();
        if idx == q.last_used {
            return None;
        }
        let e = q.used + 4 + 8 * u64::from(q.last_used % q.size);
        let id = self.ram.read_u32(e).unwrap() as u16;
        let len = self.ram.read_u32(e + 4).unwrap();
        q.last_used = q.last_used.wrapping_add(1);
        let ids = q.chains.remove(&id).expect("id restituito due volte");
        q.free.extend(ids);
        if event_idx {
            let at = q.avail + 4 + 2 * u64::from(q.size);
            let v = q.last_used;
            self.ram.write_u16(at, v).unwrap();
        }
        Some((id, len))
    }

    /// avail_event scritto dal dispositivo (con EVENT_IDX).
    pub fn avail_event(&self, qi: usize) -> u16 {
        let q = &self.queues[qi];
        self.ram.read_u16(q.used + 4 + 8 * u64::from(q.size)).unwrap()
    }

    pub fn set_avail_flags(&mut self, qi: usize, flags: u16) {
        let a = self.queues[qi].avail;
        self.ram.write_u16(a, flags).unwrap();
    }

    pub fn set_used_event(&mut self, qi: usize, v: u16) {
        let q = &self.queues[qi];
        let at = q.avail + 4 + 2 * u64::from(q.size);
        self.ram.write_u16(at, v).unwrap();
    }

    pub fn service(&mut self) {
        self.t.service(&mut self.ram);
    }

    /// Handler dell'interrupt: legge InterruptStatus e lo riconosce.
    pub fn irq(&mut self) -> u32 {
        let s = self.t.rd(INTERRUPT_STATUS);
        self.t.wr(INTERRUPT_ACK, s);
        s
    }
}
