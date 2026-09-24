use std::cell::RefCell;
use std::rc::Rc;

use super::*;
use crate::virtio::testdrv::{B, Driver, Transport};

const ALL: u64 = u64::MAX;

fn transport(backend: impl BlockBackend + 'static, cfg: VirtioBlkConfig) -> VirtioMmio {
    VirtioMmio::new(Box::new(VirtioBlk::new(Box::new(backend), cfg)))
}

fn driver(backend: impl BlockBackend + 'static, cfg: VirtioBlkConfig) -> Driver<VirtioMmio> {
    let mut d = Driver::new(transport(backend, cfg));
    d.init(ALL, 16);
    d
}

fn blk(d: &mut Driver<VirtioMmio>) -> &mut VirtioBlk {
    d.t.device_as_mut::<VirtioBlk>().unwrap()
}

fn hdr(kind: u32, sector: u64) -> [u8; 16] {
    let mut h = [0u8; 16];
    h[0..4].copy_from_slice(&kind.to_le_bytes());
    h[8..16].copy_from_slice(&sector.to_le_bytes());
    h
}

/// Pubblica una richiesta: intestazione, dati in uscita, `in_len` byte di
/// dati in entrata e il byte di stato, ognuno nel suo descrittore.
/// Restituisce gli indirizzi dei dati in entrata e dello stato.
fn submit(
    d: &mut Driver<VirtioMmio>,
    kind: u32,
    sector: u64,
    out: &[u8],
    in_len: u32,
    ind: bool,
) -> (u64, u64) {
    let h = d.buf(&hdr(kind, sector));
    let mut bufs: Vec<B> = vec![(h, 16, false)];
    if !out.is_empty() {
        bufs.push((d.buf(out), out.len() as u32, false));
    }
    let data_in = d.alloc(u64::from(in_len), 8);
    if in_len > 0 {
        bufs.push((data_in, in_len, true));
    }
    let status = d.buf(&[0xFF]);
    bufs.push((status, 1, true));
    if ind {
        d.add_indirect(0, &bufs);
    } else {
        d.add(0, &bufs);
    }
    (data_in, status)
}

/// Richiesta completa: (stato, lunghezza nello used ring, dati letti).
fn request(
    d: &mut Driver<VirtioMmio>,
    kind: u32,
    sector: u64,
    out: &[u8],
    in_len: u32,
) -> (u8, u32, Vec<u8>) {
    let (data_in, status) = submit(d, kind, sector, out, in_len, false);
    d.service();
    assert_eq!(d.irq() & INT_VRING, INT_VRING);
    let (_, len) = d.pop_used(0).expect("richiesta non completata");
    (d.mem(status, 1)[0], len, d.mem(data_in, in_len as usize))
}

#[test]
fn configurazione_e_feature() {
    let mut d = driver(MemBackend::new(1 << 20), VirtioBlkConfig::default());
    assert_eq!(d.t.cfg(0, 4) | d.t.cfg(4, 4) << 32, 2048, "capacity");
    assert_eq!(d.t.cfg(0, 8), 2048, "capacity a 64 bit");
    assert_eq!(d.t.cfg(8, 4), 1 << 20, "size_max");
    assert_eq!(d.t.cfg(12, 4), 254, "seg_max");
    assert_eq!(d.t.cfg(20, 4), 512, "blk_size");
    assert_eq!(d.t.cfg(34, 2), 1, "num_queues");
    assert_eq!(d.t.cfg(0x40, 4), 0);
    let want = F_SIZE_MAX | F_SEG_MAX | F_BLK_SIZE | F_FLUSH | F_VERSION_1 | F_INDIRECT_DESC | F_EVENT_IDX;
    assert_eq!(d.features, want);
    assert_eq!(d.t.rd(DEVICE_ID), ID_BLOCK);
    d.t.wr(QUEUE_SEL, 0);
    assert_eq!(d.t.rd(QUEUE_NUM_MAX), 256);
    let d = driver(MemBackend::new(4096).read_only(), VirtioBlkConfig::default());
    assert_ne!(d.features & F_RO, 0);
    let d = driver(MemBackend::new(4096), VirtioBlkConfig { read_only: true, ..Default::default() });
    assert_ne!(d.features & F_RO, 0);
}

#[test]
fn scrittura_e_lettura_dirette_e_indirette() {
    let mut d = driver(MemBackend::new(1 << 20), VirtioBlkConfig::default());
    let data: Vec<u8> = (0..1024).map(|i| (i * 7) as u8).collect();
    let (st, len, _) = request(&mut d, T_OUT, 3, &data, 0);
    assert_eq!((st, len), (S_OK, 1));
    let mut disk = vec![0u8; 1024];
    blk(&mut d).backend_mut().read_sectors(3, &mut disk).unwrap();
    assert_eq!(disk, data);

    // Lettura con tabella indiretta e dati divisi in due descrittori.
    let h = d.buf(&hdr(T_IN, 3));
    let (a, b) = (d.alloc(600, 8), d.alloc(424, 8));
    let st = d.buf(&[0xFF]);
    let head = d.add_indirect(0, &[(h, 16, false), (a, 600, true), (b, 424, true), (st, 1, true)]);
    d.service();
    assert_eq!(d.irq(), INT_VRING);
    assert_eq!(d.pop_used(0), Some((head, 1025)));
    assert_eq!(d.mem(st, 1), [S_OK]);
    assert_eq!([d.mem(a, 600), d.mem(b, 424)].concat(), data);

    // Intestazione divisa in due descrittori, stato in coda ai dati.
    let h = d.buf(&hdr(T_IN, 4));
    let buf = d.alloc(513, 8);
    let head = d.add(0, &[(h, 10, false), (h + 10, 6, false), (buf, 513, true)]);
    d.service();
    assert_eq!(d.pop_used(0), Some((head, 513)));
    assert_eq!(d.mem(buf, 513), [&data[512..], &[S_OK]].concat());
}

#[test]
fn piu_richieste_in_un_servizio() {
    let mut d = Driver::new(transport(MemBackend::new(1 << 16), VirtioBlkConfig::default()));
    d.init(ALL, 32);
    let mut stati = Vec::new();
    for s in 0..10u64 {
        stati.push(submit(&mut d, T_OUT, s, &[s as u8; 512], 0, s % 2 == 0).1);
    }
    d.service();
    assert_eq!(d.irq(), INT_VRING);
    for _ in 0..10 {
        assert_eq!(d.pop_used(0).map(|u| u.1), Some(1));
    }
    assert_eq!(d.pop_used(0), None);
    for (s, st) in stati.into_iter().enumerate() {
        assert_eq!(d.mem(st, 1), [S_OK]);
        let mut b = [0u8; 512];
        blk(&mut d).backend_mut().read_sectors(s as u64, &mut b).unwrap();
        assert_eq!(b, [s as u8; 512]);
    }
}

#[test]
fn richiesta_piu_grande_di_un_pezzo() {
    let mut d = driver(MemBackend::new(1 << 20), VirtioBlkConfig::default());
    let data: Vec<u8> = (0..200 * 1024).map(|i| (i % 251) as u8).collect();
    assert_eq!(request(&mut d, T_OUT, 8, &data, 0).0, S_OK);
    let (st, len, back) = request(&mut d, T_IN, 8, &[], data.len() as u32);
    assert_eq!((st, len), (S_OK, data.len() as u32 + 1));
    assert_eq!(back, data);
}

#[test]
fn get_id_flush_e_tipo_sconosciuto() {
    struct Contaflush(Rc<RefCell<u32>>, MemBackend);
    impl BlockBackend for Contaflush {
        fn size(&self) -> u64 {
            self.1.size()
        }
        fn read_sectors(&mut self, s: u64, b: &mut [u8]) -> Result<(), BlockError> {
            self.1.read_sectors(s, b)
        }
        fn write_sectors(&mut self, s: u64, b: &[u8]) -> Result<(), BlockError> {
            self.1.write_sectors(s, b)
        }
        fn flush(&mut self) -> Result<(), BlockError> {
            *self.0.borrow_mut() += 1;
            Ok(())
        }
    }
    let flushes = Rc::new(RefCell::new(0));
    let cfg = VirtioBlkConfig { serial: b"disco-di-prova".to_vec(), ..Default::default() };
    let mut d = driver(Contaflush(flushes.clone(), MemBackend::new(4096)), cfg);
    let (st, len, id) = request(&mut d, T_GET_ID, 0, &[], 20);
    assert_eq!((st, len), (S_OK, 21));
    assert_eq!(&id, b"disco-di-prova\0\0\0\0\0\0");
    // Buffer più corto di 20 byte: si tronca.
    let (st, len, id) = request(&mut d, T_GET_ID, 0, &[], 5);
    assert_eq!((st, len, id.as_slice()), (S_OK, 6, &b"disco"[..]));
    assert_eq!(request(&mut d, T_FLUSH, 0, &[], 0).0, S_OK);
    assert_eq!(*flushes.borrow(), 1);
    let (st, len, _) = request(&mut d, 11, 0, &[], 0);
    assert_eq!((st, len), (S_UNSUPP, 1));
}

#[test]
fn errori_di_io() {
    let mut d = driver(MemBackend::new(4096), VirtioBlkConfig::default());
    assert_eq!(request(&mut d, T_IN, 7, &[], 1024).0, S_IOERR, "oltre la capacità");
    assert_eq!(request(&mut d, T_IN, u64::MAX, &[], 512).0, S_IOERR, "overflow del settore");
    assert_eq!(request(&mut d, T_IN, 0, &[], 100).0, S_IOERR, "non multiplo di 512");
    assert_eq!(request(&mut d, T_OUT, 0, &[1; 300], 0).0, S_IOERR);
    let (st, len, _) = request(&mut d, T_IN, 7, &[], 512);
    assert_eq!((st, len), (S_OK, 513), "ultimo settore");
    let mut d = driver(MemBackend::new(4096), VirtioBlkConfig { read_only: true, ..Default::default() });
    assert_eq!(request(&mut d, T_OUT, 0, &[1; 512], 0).0, S_IOERR, "sola lettura");
    assert_eq!(request(&mut d, T_IN, 0, &[], 512).0, S_OK);
}

#[test]
fn richiesta_malformata_porta_a_needs_reset() {
    let mut d = driver(MemBackend::new(4096), VirtioBlkConfig::default());
    let h = d.buf(&[0; 8]);
    d.add(0, &[(h, 8, false)]);
    d.service();
    assert_ne!(d.t.rd(STATUS) & STATUS_DEVICE_NEEDS_RESET, 0);
    assert_eq!(d.irq(), INT_CONFIG);
    assert!(matches!(d.t.last_error(), Some(QueueError::Malformed(_))));
    // Fermo fino al reset.
    submit(&mut d, T_IN, 0, &[], 512, false);
    d.service();
    assert_eq!(d.irq(), 0);
    // Il driver resetta e riparte.
    d.init(ALL, 16);
    assert_eq!(d.t.rd(STATUS) & STATUS_DEVICE_NEEDS_RESET, 0);
    assert_eq!(d.t.last_error(), None);
    assert_eq!(request(&mut d, T_IN, 0, &[], 512).0, S_OK);

    // Senza byte di stato.
    let h = d.buf(&hdr(T_IN, 0));
    d.add(0, &[(h, 16, false)]);
    d.service();
    assert!(matches!(d.t.last_error(), Some(QueueError::Malformed(_))));
}

#[test]
fn indirect_non_negoziato_e_un_errore() {
    let mut d = Driver::new(transport(MemBackend::new(4096), Default::default()));
    let f = d.init(ALL & !F_INDIRECT_DESC, 16);
    assert_eq!(f & F_INDIRECT_DESC, 0);
    submit(&mut d, T_IN, 0, &[], 512, true);
    d.service();
    assert_eq!(d.t.last_error(), Some(QueueError::IndirectNotNegotiated));
    assert_ne!(d.t.rd(STATUS) & STATUS_DEVICE_NEEDS_RESET, 0);
}

#[test]
fn event_idx_e_notifiche() {
    let mut d = driver(MemBackend::new(4096), VirtioBlkConfig::default());
    assert_ne!(d.features & F_EVENT_IDX, 0);
    // Il dispositivo chiede la notifica per il prossimo indice.
    submit(&mut d, T_IN, 0, &[], 512, false);
    d.service();
    assert_eq!(d.avail_event(0), 1);
    assert_eq!(d.irq(), INT_VRING);
    d.pop_used(0).unwrap(); // used_event = 1
    // Il driver sposta used_event avanti: due richieste senza interrupt.
    d.set_used_event(0, 3);
    for _ in 0..2 {
        submit(&mut d, T_IN, 0, &[], 512, false);
        d.service();
        assert_eq!(d.irq(), 0);
    }
    submit(&mut d, T_IN, 0, &[], 512, false);
    d.service();
    assert_eq!(d.irq(), INT_VRING, "used.idx supera used_event");
    assert_eq!(d.avail_event(0), 4);
}

#[test]
fn no_interrupt_senza_event_idx() {
    let t = transport(MemBackend::new(4096), Default::default()).without_features(F_EVENT_IDX);
    let mut d = Driver::new(t);
    assert_eq!(d.init(ALL, 16) & F_EVENT_IDX, 0);
    d.set_avail_flags(0, 1);
    submit(&mut d, T_IN, 0, &[], 512, false);
    d.service();
    assert_eq!(d.irq(), 0);
    assert!(d.pop_used(0).is_some(), "il buffer torna comunque");
    d.set_avail_flags(0, 0);
    submit(&mut d, T_IN, 0, &[], 512, false);
    d.service();
    assert_eq!(d.irq(), INT_VRING);
}

#[test]
fn niente_lavoro_prima_di_driver_ok() {
    let mut d = Driver::new(transport(MemBackend::new(4096), Default::default()));
    d.init(ALL, 16);
    let s = d.t.rd(STATUS);
    d.t.wr(STATUS, s & !STATUS_DRIVER_OK);
    submit(&mut d, T_IN, 0, &[], 512, false);
    d.service();
    assert_eq!(d.pop_used(0), None);
    d.t.wr(STATUS, s);
    d.service();
    assert!(d.pop_used(0).is_some());
}

/// Backend che non ha i dati finché non si apre il rubinetto.
struct Lento {
    pronto: Rc<RefCell<bool>>,
    disco: MemBackend,
}

impl BlockBackend for Lento {
    fn size(&self) -> u64 {
        self.disco.size()
    }
    fn read_sectors(&mut self, s: u64, b: &mut [u8]) -> Result<(), BlockError> {
        if !*self.pronto.borrow() {
            return Err(BlockError::NotReady);
        }
        self.disco.read_sectors(s, b)
    }
    fn write_sectors(&mut self, s: u64, b: &[u8]) -> Result<(), BlockError> {
        self.disco.write_sectors(s, b)
    }
    fn flush(&mut self) -> Result<(), BlockError> {
        Ok(())
    }
}

#[test]
fn backend_non_pronto_sospende_la_richiesta() {
    let pronto = Rc::new(RefCell::new(false));
    let disco = MemBackend::from_vec(vec![0xAB; 1024]);
    let mut d = driver(Lento { pronto: pronto.clone(), disco }, VirtioBlkConfig::default());
    let (a, _) = submit(&mut d, T_IN, 1, &[], 512, false);
    let (_, st2) = submit(&mut d, T_OUT, 0, &[1; 512], 0, false);
    d.service();
    assert_eq!(d.pop_used(0), None);
    assert_eq!(d.irq(), 0);
    assert!(blk(&mut d).has_pending());
    d.service();
    assert_eq!(d.pop_used(0), None, "la seconda richiesta aspetta la prima");
    *pronto.borrow_mut() = true;
    d.service();
    assert_eq!(d.irq(), INT_VRING);
    assert_eq!(d.pop_used(0).map(|u| u.1), Some(513));
    assert_eq!(d.pop_used(0).map(|u| u.1), Some(1));
    assert_eq!(d.mem(a, 512), vec![0xAB; 512]);
    assert_eq!(d.mem(st2, 1), [S_OK]);
    assert!(!blk(&mut d).has_pending());
}

#[test]
fn reset_dimentica_la_richiesta_sospesa() {
    let pronto = Rc::new(RefCell::new(false));
    let mut d = driver(Lento { pronto: pronto.clone(), disco: MemBackend::new(1024) }, Default::default());
    submit(&mut d, T_IN, 0, &[], 512, false);
    d.service();
    assert!(blk(&mut d).has_pending());
    d.t.wr(STATUS, 0);
    assert!(!blk(&mut d).has_pending());
}

#[test]
fn mem_backend_limiti() {
    let mut m = MemBackend::from_vec(vec![1; 700]);
    assert_eq!(m.size(), 1024);
    assert_eq!(m.read_sectors(2, &mut [0; 512]), Err(BlockError::OutOfRange));
    assert_eq!(m.read_sectors(1, &mut [0; 100]), Err(BlockError::OutOfRange));
    let mut s = [0; 512];
    m.read_sectors(1, &mut s).unwrap();
    assert_eq!(&s[..188], &[1; 188][..]);
    assert_eq!(&s[188..], &[0; 324][..]);
    let mut ro = MemBackend::new(512).read_only();
    assert_eq!(ro.write_sectors(0, &[0; 512]), Err(BlockError::ReadOnly));
}

#[test]
fn copy_on_write_sopra_la_base() {
    // Base di 3 cluster e mezzo, in sola lettura; ogni settore vale il suo numero.
    let base: Vec<u8> = (0..14336u32).map(|i| (i / 512) as u8).collect();
    let mut cow = CowBackend::new(MemBackend::from_vec(base.clone()).read_only());
    assert_eq!(cow.size(), 14336);
    assert!(!BlockBackend::read_only(&cow));
    // Scrittura parziale a cavallo di due cluster (settori 7 e 8).
    cow.write_sectors(7, &[0xEE; 1024]).unwrap();
    assert_eq!(cow.dirty_clusters(), 2);
    let mut all = vec![0u8; 14336];
    cow.read_sectors(0, &mut all).unwrap();
    let mut expect = base.clone();
    expect[7 * 512..9 * 512].fill(0xEE);
    assert_eq!(all, expect);
    assert_eq!(cow.base().data(), &base[..], "la base non cambia");
    // Ultimo cluster corto (settori 24..28) scritto in parte.
    cow.write_sectors(27, &[0x11; 512]).unwrap();
    cow.read_sectors(24, &mut all[..2048]).unwrap();
    assert_eq!(&all[..1536], &base[24 * 512..27 * 512]);
    assert_eq!(&all[1536..2048], &[0x11; 512][..]);
    assert_eq!(cow.write_sectors(28, &[0; 512]), Err(BlockError::OutOfRange));

    // Un cluster scritto per intero non legge la base.
    struct Illeggibile;
    impl BlockBackend for Illeggibile {
        fn size(&self) -> u64 {
            8192
        }
        fn read_sectors(&mut self, _: u64, _: &mut [u8]) -> Result<(), BlockError> {
            Err(BlockError::Io)
        }
        fn write_sectors(&mut self, _: u64, _: &[u8]) -> Result<(), BlockError> {
            Err(BlockError::ReadOnly)
        }
        fn flush(&mut self) -> Result<(), BlockError> {
            Ok(())
        }
    }
    let mut cow = CowBackend::new(Illeggibile);
    cow.write_sectors(8, &[5; 4096]).unwrap();
    assert_eq!(cow.write_sectors(0, &[5; 512]), Err(BlockError::Io));
    let mut b = [0; 512];
    cow.read_sectors(15, &mut b).unwrap();
    assert_eq!(b, [5; 512]);
}

#[test]
fn disco_cow_dietro_virtio_blk() {
    let base = MemBackend::from_vec(vec![0x5A; 8192]).read_only();
    let mut d = driver(CowBackend::new(base), VirtioBlkConfig::default());
    assert_eq!(d.features & F_RO, 0);
    assert_eq!(request(&mut d, T_OUT, 1, &[0xC3; 512], 0).0, S_OK);
    let (st, _, data) = request(&mut d, T_IN, 0, &[], 1536);
    assert_eq!(st, S_OK);
    assert_eq!(&data[..512], &[0x5A; 512][..]);
    assert_eq!(&data[512..1024], &[0xC3; 512][..]);
    assert_eq!(&data[1024..], &[0x5A; 512][..]);
}
