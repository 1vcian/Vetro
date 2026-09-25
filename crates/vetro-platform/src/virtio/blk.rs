//! virtio-blk (virtio v1.2, §5.2) e backend a blocchi.
//!
//! Una coda di richieste. Ogni richiesta: intestazione leggibile di 16 byte
//! (tipo, riservato, settore), dati, un byte di stato scrivibile in coda.
//! Tipi gestiti: IN, OUT, FLUSH, GET_ID; gli altri rispondono UNSUPP.
//! Settori sempre da 512 byte, qualunque sia `blk_size`.
//!
//! Scelte:
//! - richiesta senza intestazione completa o senza byte di stato: errore
//!   della coda (DEVICE_NEEDS_RESET), come `virtio_error` di QEMU;
//! - dati non multipli di 512, accesso oltre la capacità, scrittura su
//!   disco in sola lettura, errore del backend: stato IOERR;
//! - la lunghezza nello used ring è quella davvero scritta (dati + stato);
//! - l'I/O procede a pezzi da 64 KiB, senza allocare l'intera
//!   richiesta;
//! - [`BlockError::NotReady`] (dati non ancora arrivati, es. immagine
//!   scaricata a pezzi in M5) lascia la richiesta in sospeso: si riprova da
//!   capo al prossimo `service`, prima di estrarne altre. Le richieste sono
//!   idempotenti, quindi ripeterle è sicuro.

use core::any::Any;
use std::collections::{BTreeMap, BTreeSet};

use super::*;

pub const BLK_SECTOR_SIZE: u64 = 512;

pub const T_IN: u32 = 0;
pub const T_OUT: u32 = 1;
pub const T_FLUSH: u32 = 4;
pub const T_GET_ID: u32 = 8;

pub const S_OK: u8 = 0;
pub const S_IOERR: u8 = 1;
pub const S_UNSUPP: u8 = 2;

pub const F_SIZE_MAX: u64 = 1 << 1;
pub const F_SEG_MAX: u64 = 1 << 2;
pub const F_RO: u64 = 1 << 5;
pub const F_BLK_SIZE: u64 = 1 << 6;
pub const F_FLUSH: u64 = 1 << 9;

/// Lunghezza dell'identificativo di GET_ID.
pub const ID_BYTES: usize = 20;
/// Pezzo massimo per ogni chiamata al backend.
const CHUNK: u64 = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockError {
    /// Errore di I/O del backend.
    Io,
    /// Settori oltre la fine del disco.
    OutOfRange,
    /// Scrittura su un backend in sola lettura.
    ReadOnly,
    /// Dati non ancora disponibili: riprovare più tardi.
    NotReady,
}

/// Disco visto dal dispositivo. `sector` in unità da 512 byte; i buffer
/// sono multipli di 512.
pub trait BlockBackend: Any {
    /// Dimensione in byte (multiplo di 512).
    fn size(&self) -> u64;
    fn read_only(&self) -> bool {
        false
    }
    fn read_sectors(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError>;
    fn write_sectors(&mut self, sector: u64, data: &[u8]) -> Result<(), BlockError>;
    fn flush(&mut self) -> Result<(), BlockError>;
    /// Stato del backend negli snapshot (M6, ADR 0015). Di norma nessuno: i
    /// dati stanno fuori (un file, un'immagine via HTTP) e il backend è un
    /// collegamento che l'host ricrea prima del ripristino. Chi tiene dati
    /// propri scritti dal guest (disco in memoria, livello copy-on-write) li
    /// salva qui.
    fn save_state(&self, _w: &mut vetro_snapshot::Writer) {}
    fn restore_state(&mut self, _r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        Ok(())
    }
}

/// Intervallo di byte di `len` byte dal settore `sector`, se sta in `size`.
fn byte_range(size: u64, sector: u64, len: usize) -> Result<core::ops::Range<usize>, BlockError> {
    let start = sector.checked_mul(BLK_SECTOR_SIZE).ok_or(BlockError::OutOfRange)?;
    let end = start.checked_add(len as u64).ok_or(BlockError::OutOfRange)?;
    if end > size || !(len as u64).is_multiple_of(BLK_SECTOR_SIZE) {
        return Err(BlockError::OutOfRange);
    }
    Ok(start as usize..end as usize)
}

/// Disco in memoria.
#[derive(Clone, Debug, Default)]
pub struct MemBackend {
    data: Vec<u8>,
    read_only: bool,
}

impl MemBackend {
    /// Disco di `size` byte a zero (arrotondato a 512 in su).
    pub fn new(size: u64) -> Self {
        Self::from_vec(vec![0; size as usize])
    }

    /// Disco con il contenuto dato, completato con zeri fino a 512.
    pub fn from_vec(mut data: Vec<u8>) -> Self {
        let pad = data.len().next_multiple_of(BLK_SECTOR_SIZE as usize);
        data.resize(pad, 0);
        Self { data, read_only: false }
    }

    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

impl BlockBackend for MemBackend {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }
    fn read_only(&self) -> bool {
        self.read_only
    }
    fn read_sectors(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let r = byte_range(self.size(), sector, buf.len())?;
        buf.copy_from_slice(&self.data[r]);
        Ok(())
    }
    fn write_sectors(&mut self, sector: u64, data: &[u8]) -> Result<(), BlockError> {
        if self.read_only {
            return Err(BlockError::ReadOnly);
        }
        let r = byte_range(self.size(), sector, data.len())?;
        self.data[r].copy_from_slice(data);
        Ok(())
    }
    fn flush(&mut self) -> Result<(), BlockError> {
        Ok(())
    }
    /// Scrivibile: il contenuto intero (il guest può averlo scritto), a
    /// blocchi compressi. In sola lettura (la base di un copy-on-write) è
    /// un'immagine che non cambia: solo il suo hash, per controllare che al
    /// ripristino sia collegata la stessa.
    fn save_state(&self, w: &mut vetro_snapshot::Writer) {
        w.u64(u64::from(self.read_only));
        if self.read_only {
            w.u64(vetro_snapshot::hash64(&self.data));
        } else {
            vetro_snapshot::compress(w, &self.data);
        }
    }
    fn restore_state(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        r.expect_u64("sola lettura del disco in memoria", u64::from(self.read_only))?;
        if self.read_only {
            return r.expect_u64("hash del disco in memoria", vetro_snapshot::hash64(&self.data));
        }
        self.data.fill(0);
        vetro_snapshot::decompress_into(r, &mut self.data, |_| {})
    }
}

/// Strato copy-on-write sopra un backend usato solo in lettura (es.
/// un'immagine scaricata a pezzi): le scritture finiscono in cluster da
/// [`CowBackend::CLUSTER`] byte in memoria, le letture preferiscono i
/// cluster scritti. Una scrittura parziale di un cluster lo copia prima
/// dalla base.
///
/// Per l'overlay persistente (M6, ADR 0017) tiene anche l'insieme dei
/// cluster scritti dall'ultima [`take_dirty`](Self::take_dirty): chi
/// conserva i cluster su file (CLI, OPFS nel browser) scrive solo quelli.
/// È contabilità dell'host, non stato del guest: non entra negli snapshot,
/// e dopo un ripristino si svuota (chi persiste confronta tutti i cluster).
pub struct CowBackend<B: BlockBackend> {
    base: B,
    clusters: BTreeMap<u64, Box<[u8]>>,
    dirty: BTreeSet<u64>,
}

impl<B: BlockBackend> CowBackend<B> {
    pub const CLUSTER: u64 = 4096;

    pub fn new(base: B) -> Self {
        Self { base, clusters: BTreeMap::new(), dirty: BTreeSet::new() }
    }

    pub fn base(&self) -> &B {
        &self.base
    }

    pub fn base_mut(&mut self) -> &mut B {
        &mut self.base
    }

    /// Numero di cluster scritti.
    pub fn dirty_clusters(&self) -> usize {
        self.clusters.len()
    }

    /// I cluster scritti dal guest dall'ultima chiamata, in ordine.
    pub fn take_dirty(&mut self) -> Vec<u64> {
        core::mem::take(&mut self.dirty).into_iter().collect()
    }

    /// I dati del cluster `c`, se è stato scritto.
    pub fn cluster(&self, c: u64) -> Option<&[u8]> {
        self.clusters.get(&c).map(|d| &d[..])
    }

    /// Tutti i cluster scritti, in ordine di indice.
    pub fn clusters(&self) -> impl Iterator<Item = (u64, &[u8])> {
        self.clusters.iter().map(|(&c, d)| (c, &d[..]))
    }

    /// Mette il cluster `c` con i dati di un overlay salvato (lunghi come il
    /// cluster nel disco). Non conta come scrittura del guest.
    pub fn load_cluster(&mut self, c: u64, data: &[u8]) -> Result<(), BlockError> {
        if c >= self.size().div_ceil(Self::CLUSTER) {
            return Err(BlockError::OutOfRange);
        }
        if data.len() != self.cluster_len(c) {
            return Err(BlockError::Io);
        }
        self.clusters.insert(c, data.into());
        Ok(())
    }

    /// Dimensione del cluster `c` (l'ultimo può essere più corto).
    fn cluster_len(&self, c: u64) -> usize {
        (self.base.size() - c * Self::CLUSTER).min(Self::CLUSTER) as usize
    }

    /// Applica `f(cluster, offset nel cluster, inizio, fine)` a ogni pezzo
    /// dell'intervallo `[pos, pos + len)` diviso per cluster.
    fn pieces(
        pos: u64,
        len: usize,
        mut f: impl FnMut(u64, usize, usize, usize) -> Result<(), BlockError>,
    ) -> Result<(), BlockError> {
        let mut done = 0usize;
        while done < len {
            let at = pos + done as u64;
            let (c, off) = (at / Self::CLUSTER, (at % Self::CLUSTER) as usize);
            let n = (Self::CLUSTER as usize - off).min(len - done);
            f(c, off, done, done + n)?;
            done += n;
        }
        Ok(())
    }
}

impl<B: BlockBackend> BlockBackend for CowBackend<B> {
    fn size(&self) -> u64 {
        self.base.size()
    }

    fn read_sectors(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let r = byte_range(self.size(), sector, buf.len())?;
        Self::pieces(r.start as u64, buf.len(), |c, off, a, b| match self.clusters.get(&c) {
            Some(data) => {
                buf[a..b].copy_from_slice(&data[off..off + (b - a)]);
                Ok(())
            }
            None => {
                let at = c * Self::CLUSTER + off as u64;
                self.base.read_sectors(at / BLK_SECTOR_SIZE, &mut buf[a..b])
            }
        })
    }

    fn write_sectors(&mut self, sector: u64, data: &[u8]) -> Result<(), BlockError> {
        let r = byte_range(self.size(), sector, data.len())?;
        Self::pieces(r.start as u64, data.len(), |c, off, a, b| {
            if !self.clusters.contains_key(&c) {
                let mut fresh = vec![0u8; self.cluster_len(c)].into_boxed_slice();
                if b - a < fresh.len() {
                    self.base.read_sectors(c * Self::CLUSTER / BLK_SECTOR_SIZE, &mut fresh)?;
                }
                self.clusters.insert(c, fresh);
            }
            let cl = self.clusters.get_mut(&c).expect("cluster appena inserito");
            cl[off..off + (b - a)].copy_from_slice(&data[a..b]);
            self.dirty.insert(c);
            Ok(())
        })
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        Ok(())
    }

    /// I cluster scritti dal guest (in ordine), poi lo stato della base (di
    /// norma nessuno: la base è un collegamento, controllato solo per
    /// dimensione).
    fn save_state(&self, w: &mut vetro_snapshot::Writer) {
        w.u64(self.base.size());
        w.seq(&self.clusters, |w, (&c, data)| {
            w.u64(c);
            vetro_snapshot::compress(w, data);
        });
        self.base.save_state(w);
    }

    fn restore_state(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        r.expect_u64("dimensione del disco", self.base.size())?;
        let clusters = self.size().div_ceil(Self::CLUSTER);
        self.clusters.clear();
        self.dirty.clear();
        let n = r.len_of(16)?;
        for _ in 0..n {
            let c = r.u64()?;
            if c >= clusters || self.clusters.contains_key(&c) {
                return Err(vetro_snapshot::Error::invalid(format!("cluster {c} del disco")));
            }
            let mut data = vec![0u8; self.cluster_len(c)].into_boxed_slice();
            vetro_snapshot::decompress_into(r, &mut data, |_| {})?;
            self.clusters.insert(c, data);
        }
        self.base.restore_state(r)
    }
}

/// Parametri di virtio-blk.
#[derive(Clone, Debug)]
pub struct VirtioBlkConfig {
    /// Dimensione della coda (QEMU: 256).
    pub queue_size: u16,
    /// Segmenti dati massimi per richiesta (QEMU: queue_size - 2).
    pub seg_max: u32,
    /// Byte massimi per segmento.
    pub size_max: u32,
    /// Dimensione del blocco logico annunciata al driver.
    pub blk_size: u32,
    /// Forza la sola lettura anche se il backend accetta scritture.
    pub read_only: bool,
    /// Identificativo per GET_ID (al massimo 20 byte, completato con zeri).
    pub serial: Vec<u8>,
}

impl Default for VirtioBlkConfig {
    fn default() -> Self {
        Self {
            queue_size: 256,
            seg_max: 254,
            size_max: 1 << 20,
            blk_size: 512,
            read_only: false,
            serial: b"vetro-blk".to_vec(),
        }
    }
}

/// Esito del trattamento di una richiesta.
enum Outcome {
    /// Byte scritti nei buffer del driver (dati + stato).
    Done(u32),
    /// Il backend non è pronto: riprovare.
    Retry,
}

pub struct VirtioBlk {
    backend: Box<dyn BlockBackend>,
    cfg: VirtioBlkConfig,
    queue_sizes: [u16; 1],
    pending: Option<DescChain>,
}

impl VirtioBlk {
    pub fn new(backend: Box<dyn BlockBackend>, cfg: VirtioBlkConfig) -> Self {
        let queue_sizes = [cfg.queue_size];
        Self { backend, cfg, queue_sizes, pending: None }
    }

    pub fn backend(&self) -> &dyn BlockBackend {
        self.backend.as_ref()
    }

    pub fn backend_mut(&mut self) -> &mut dyn BlockBackend {
        self.backend.as_mut()
    }

    /// Accesso tipizzato al backend.
    pub fn backend_as_mut<T: BlockBackend>(&mut self) -> Option<&mut T> {
        let b: &mut dyn Any = self.backend.as_mut();
        b.downcast_mut()
    }

    pub fn is_read_only(&self) -> bool {
        self.cfg.read_only || self.backend.read_only()
    }

    /// Capacità in settori da 512 byte.
    pub fn capacity(&self) -> u64 {
        self.backend.size() / BLK_SECTOR_SIZE
    }

    /// Una richiesta aspetta il backend.
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    fn config_bytes(&self) -> [u8; 36] {
        let mut c = [0u8; 36];
        c[0..8].copy_from_slice(&self.capacity().to_le_bytes());
        c[8..12].copy_from_slice(&self.cfg.size_max.to_le_bytes());
        c[12..16].copy_from_slice(&self.cfg.seg_max.to_le_bytes());
        // geometry (16..20) e topology (24..32) a zero, writeback (32) a 0.
        c[20..24].copy_from_slice(&self.cfg.blk_size.to_le_bytes());
        c[34..36].copy_from_slice(&1u16.to_le_bytes()); // num_queues
        c
    }

    fn handle(&mut self, c: &DescChain, ram: &mut dyn GuestRam) -> Result<Outcome, QueueError> {
        let mut hdr = [0u8; 16];
        if c.read(ram, 0, &mut hdr)? < hdr.len() {
            return Err(QueueError::Malformed("intestazione virtio-blk incompleta"));
        }
        let Some(status_at) = c.writable_len().checked_sub(1) else {
            return Err(QueueError::Malformed("byte di stato virtio-blk mancante"));
        };
        let kind = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let sector = u64::from_le_bytes(hdr[8..16].try_into().unwrap());
        let (status, written) = match kind {
            T_IN => match self.transfer(c, ram, sector, status_at, false)? {
                Some(Ok(())) => (S_OK, status_at),
                Some(Err(_)) => (S_IOERR, 0),
                None => return Ok(Outcome::Retry),
            },
            T_OUT => {
                let len = c.readable_len() - 16;
                match self.transfer(c, ram, sector, len, true)? {
                    Some(Ok(())) => (S_OK, 0),
                    Some(Err(_)) => (S_IOERR, 0),
                    None => return Ok(Outcome::Retry),
                }
            }
            T_FLUSH => match self.backend.flush() {
                Ok(()) => (S_OK, 0),
                Err(BlockError::NotReady) => return Ok(Outcome::Retry),
                Err(_) => (S_IOERR, 0),
            },
            T_GET_ID => {
                let mut id = [0u8; ID_BYTES];
                let n = self.cfg.serial.len().min(ID_BYTES);
                id[..n].copy_from_slice(&self.cfg.serial[..n]);
                let len = (status_at as usize).min(ID_BYTES);
                c.write(ram, 0, &id[..len])?;
                (S_OK, len as u64)
            }
            _ => (S_UNSUPP, 0),
        };
        c.write(ram, status_at, &[status])?;
        Ok(Outcome::Done(written as u32 + 1))
    }

    /// Trasferisce `len` byte tra disco (dal settore `sector`) e catena:
    /// dati scrivibili dall'offset 0 per IN, leggibili dall'offset 16 per
    /// OUT. `None` = backend non pronto; `Some(Err)` = errore da IOERR.
    fn transfer(
        &mut self,
        c: &DescChain,
        ram: &mut dyn GuestRam,
        sector: u64,
        len: u64,
        out: bool,
    ) -> Result<Option<Result<(), BlockError>>, QueueError> {
        if out && self.is_read_only() {
            return Ok(Some(Err(BlockError::ReadOnly)));
        }
        if !len.is_multiple_of(BLK_SECTOR_SIZE) {
            return Ok(Some(Err(BlockError::OutOfRange)));
        }
        let end = sector.checked_add(len / BLK_SECTOR_SIZE);
        if end.is_none_or(|e| e > self.capacity()) {
            return Ok(Some(Err(BlockError::OutOfRange)));
        }
        let mut buf = vec![0u8; len.min(CHUNK) as usize];
        let mut done = 0u64;
        while done < len {
            let n = (len - done).min(CHUNK) as usize;
            let chunk = &mut buf[..n];
            let s = sector + done / BLK_SECTOR_SIZE;
            let r = if out {
                c.read(ram, 16 + done, chunk)?;
                self.backend.write_sectors(s, chunk)
            } else {
                let r = self.backend.read_sectors(s, chunk);
                if r.is_ok() {
                    c.write(ram, done, chunk)?;
                }
                r
            };
            match r {
                Ok(()) => {}
                Err(BlockError::NotReady) => return Ok(None),
                Err(e) => return Ok(Some(Err(e))),
            }
            done += n as u64;
        }
        Ok(Some(Ok(())))
    }
}

impl VirtioDevice for VirtioBlk {
    fn device_id(&self) -> u32 {
        ID_BLOCK
    }

    fn features(&self) -> u64 {
        let ro = if self.is_read_only() { F_RO } else { 0 };
        F_SIZE_MAX | F_SEG_MAX | F_BLK_SIZE | F_FLUSH | ro
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &self.queue_sizes
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        read_config_bytes(&self.config_bytes(), offset, data);
    }

    fn reset(&mut self) {
        self.pending = None;
    }

    fn service(&mut self, ctx: &mut ServiceCtx<'_>) -> Result<(), QueueError> {
        let (q, ram) = (&mut ctx.queues[0], &mut *ctx.ram);
        loop {
            let chain = match self.pending.take() {
                Some(c) => c,
                None => match q.pop(ram)? {
                    Some(c) => c,
                    None => return Ok(()),
                },
            };
            match self.handle(&chain, ram)? {
                Outcome::Done(len) => q.push_used(ram, chain.head, len)?,
                Outcome::Retry => {
                    self.pending = Some(chain);
                    return Ok(());
                }
            }
        }
    }

    /// La richiesta in sospeso (se il backend non era pronto) e lo stato del
    /// backend. La configurazione (capacità, coda, sola lettura) si
    /// controlla: il disco collegato al ripristino dev'essere lo stesso.
    fn save_state(&self, w: &mut vetro_snapshot::Writer) {
        w.u64(self.capacity());
        w.u64(u64::from(self.is_read_only()));
        w.bytes(&self.cfg.serial);
        w.opt(self.pending.as_ref(), |w, c| c.save(w));
        self.backend.save_state(w);
    }

    fn restore_state(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        r.expect_u64("settori del disco", self.capacity())?;
        r.expect_u64("sola lettura del disco", u64::from(self.is_read_only()))?;
        if r.bytes()? != self.cfg.serial.as_slice() {
            return Err(vetro_snapshot::Error::invalid("identificativo del disco diverso"));
        }
        self.pending = r.opt(DescChain::restore)?;
        self.backend.restore_state(r)
    }
}

#[cfg(test)]
mod tests;
