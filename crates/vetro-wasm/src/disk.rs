//! Dischi virtio-blk i cui dati arrivano dall'host JavaScript (M5): un
//! file locale, un URL letto a pezzi con HTTP Range, una cache OPFS.
//!
//! [`HostDisk`] conosce solo la dimensione del disco e i blocchi (da
//! `block_size` byte, allineati) che il JS gli ha già dato. Una lettura che
//! tocca un blocco mancante risponde [`BlockError::NotReady`] e mette il
//! blocco nella lista dei richiesti: virtio-blk tiene la richiesta in
//! sospeso, la macchina si ferma con `Stop::Blocked` senza eseguire altre
//! istruzioni, il JS prende la lista ([`HostDisk::take_wanted`]), procura i
//! blocchi (OPFS, poi rete) e li consegna ([`HostDisk::fill`]); al quanto
//! successivo la richiesta si ripete da capo e si completa allo stesso
//! numero di istruzioni di un disco locale (ADR 0014).
//!
//! Le scritture del guest vanno nel [`CowBackend`] sopra (in memoria, M6
//! le renderà persistenti): [`HostDisk`] è in sola lettura.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use vetro_platform::virtio::{BLK_SECTOR_SIZE, BlockBackend, BlockError};

/// Contatori di un disco, nell'ordine di `vetro_disk_stats`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiskStats {
    /// Letture che hanno trovato un blocco mancante (ogni ripetizione conta).
    pub misses: u64,
    /// Blocchi consegnati dal JS.
    pub fills: u64,
    /// Blocchi tolti dalla cache per far posto.
    pub evictions: u64,
    /// Blocchi che il JS non è riuscito a procurare (errore di I/O al guest).
    pub failures: u64,
}

/// Disco in sola lettura con i dati forniti dall'host a blocchi.
pub struct HostDisk {
    size: u64,
    block: u64,
    /// Blocchi presenti.
    cache: BTreeMap<u64, Box<[u8]>>,
    /// Ordine d'arrivo, per togliere i più vecchi oltre `max_blocks`.
    order: VecDeque<u64>,
    /// 0 = nessun limite.
    max_blocks: usize,
    /// Richiesti e non ancora consegnati.
    requested: BTreeSet<u64>,
    /// Richiesti dopo l'ultimo [`take_wanted`](Self::take_wanted).
    wanted: Vec<u64>,
    /// Blocchi che il JS non ha potuto procurare.
    failed: BTreeSet<u64>,
    pub stats: DiskStats,
}

/// Perché [`HostDisk::new`] o [`HostDisk::fill`] rifiuta.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskError {
    /// `block_size` non è una potenza di due multipla di 512.
    BadBlockSize,
    /// Blocco oltre la fine del disco.
    OutOfRange,
    /// Lunghezza diversa da quella del blocco (l'ultimo può essere corto).
    BadLength,
}

impl HostDisk {
    /// Disco di `size` byte (arrotondati per difetto a 512, come QEMU per i
    /// dischi raw) a blocchi da `block_size` byte; al più `max_blocks`
    /// blocchi in memoria (0 = nessun limite).
    pub fn new(size: u64, block_size: u32, max_blocks: usize) -> Result<Self, DiskError> {
        let block = u64::from(block_size);
        if !block.is_power_of_two() || block < BLK_SECTOR_SIZE {
            return Err(DiskError::BadBlockSize);
        }
        Ok(HostDisk {
            size: size / BLK_SECTOR_SIZE * BLK_SECTOR_SIZE,
            block,
            cache: BTreeMap::new(),
            order: VecDeque::new(),
            max_blocks,
            requested: BTreeSet::new(),
            wanted: Vec::new(),
            failed: BTreeSet::new(),
            stats: DiskStats::default(),
        })
    }

    pub fn block_size(&self) -> u64 {
        self.block
    }

    /// Numero di blocchi del disco (l'ultimo può essere corto).
    pub fn blocks(&self) -> u64 {
        self.size.div_ceil(self.block)
    }

    /// Byte del blocco `b`.
    pub fn block_len(&self, b: u64) -> usize {
        (self.size - b * self.block).min(self.block) as usize
    }

    pub fn cached_blocks(&self) -> usize {
        self.cache.len()
    }

    /// I blocchi richiesti dall'ultima chiamata: ognuno compare una volta
    /// sola finché non viene consegnato o dichiarato fallito.
    pub fn take_wanted(&mut self) -> Vec<u64> {
        core::mem::take(&mut self.wanted)
    }

    /// Rimette `b` nella lista dei richiesti (non consegnato da chi l'aveva
    /// preso).
    pub fn requeue(&mut self, b: u64) {
        if self.requested.contains(&b) && !self.wanted.contains(&b) {
            self.wanted.push(b);
        }
    }

    /// Consegna il blocco `b` (anche non richiesto: lettura anticipata).
    pub fn fill(&mut self, b: u64, data: &[u8]) -> Result<(), DiskError> {
        if b >= self.blocks() {
            return Err(DiskError::OutOfRange);
        }
        if data.len() != self.block_len(b) {
            return Err(DiskError::BadLength);
        }
        self.requested.remove(&b);
        self.failed.remove(&b);
        self.stats.fills += 1;
        if self.cache.insert(b, data.into()).is_none() {
            self.order.push_back(b);
        }
        while self.max_blocks > 0 && self.cache.len() > self.max_blocks {
            let Some(old) = self.order.pop_front() else { break };
            self.cache.remove(&old);
            self.stats.evictions += 1;
        }
        Ok(())
    }

    /// Il JS non è riuscito a procurare il blocco `b`: la richiesta che lo
    /// aspetta finisce con un errore di I/O (stato IOERR al guest).
    pub fn fail(&mut self, b: u64) {
        self.requested.remove(&b);
        self.failed.insert(b);
        self.stats.failures += 1;
    }
}

impl BlockBackend for HostDisk {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_only(&self) -> bool {
        true
    }

    fn read_sectors(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let start = sector.checked_mul(BLK_SECTOR_SIZE).ok_or(BlockError::OutOfRange)?;
        let end = start.checked_add(buf.len() as u64).ok_or(BlockError::OutOfRange)?;
        if end > self.size || !(buf.len() as u64).is_multiple_of(BLK_SECTOR_SIZE) {
            return Err(BlockError::OutOfRange);
        }
        if buf.is_empty() {
            return Ok(());
        }
        let (first, last) = (start / self.block, (end - 1) / self.block);
        // Prima tutti i mancanti dell'intervallo, così il JS li chiede
        // insieme.
        let mut missing = false;
        for b in first..=last {
            if self.failed.contains(&b) {
                return Err(BlockError::Io);
            }
            if !self.cache.contains_key(&b) {
                missing = true;
                if self.requested.insert(b) {
                    self.wanted.push(b);
                }
            }
        }
        if missing {
            self.stats.misses += 1;
            return Err(BlockError::NotReady);
        }
        for b in first..=last {
            let data = &self.cache[&b];
            let at = b * self.block;
            let (lo, hi) = (start.max(at), end.min(at + data.len() as u64));
            buf[(lo - start) as usize..(hi - start) as usize]
                .copy_from_slice(&data[(lo - at) as usize..(hi - at) as usize]);
        }
        Ok(())
    }

    fn write_sectors(&mut self, _sector: u64, _data: &[u8]) -> Result<(), BlockError> {
        Err(BlockError::ReadOnly)
    }

    fn flush(&mut self) -> Result<(), BlockError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 13 + i / 4096) as u8).collect()
    }

    fn feed(d: &mut HostDisk, img: &[u8]) -> usize {
        let wanted = d.take_wanted();
        for &b in &wanted {
            let at = (b * d.block_size()) as usize;
            d.fill(b, &img[at..at + d.block_len(b)]).unwrap();
        }
        wanted.len()
    }

    /// Lettura a cavallo di due blocchi: prima NotReady con entrambi i
    /// blocchi richiesti una volta sola, poi i dati giusti.
    #[test]
    fn blocchi_mancanti_richiesti_poi_letti() {
        let img = image(3 * 4096 + 1024 + 100); // ultimo blocco corto, coda non allineata
        let mut d = HostDisk::new(img.len() as u64, 4096, 0).unwrap();
        assert_eq!(d.size(), 3 * 4096 + 1024);
        assert_eq!(d.blocks(), 4);
        assert_eq!(d.block_len(3), 1024);
        let mut buf = vec![0u8; 2048];
        assert_eq!(d.read_sectors(7, &mut buf), Err(BlockError::NotReady));
        assert_eq!(d.read_sectors(7, &mut buf), Err(BlockError::NotReady));
        assert_eq!(d.take_wanted(), [0, 1], "ogni blocco richiesto una volta sola");
        assert!(d.take_wanted().is_empty());
        d.fill(0, &img[..4096]).unwrap();
        assert_eq!(d.read_sectors(7, &mut buf), Err(BlockError::NotReady));
        assert!(d.take_wanted().is_empty(), "il blocco 1 è già stato chiesto");
        d.fill(1, &img[4096..8192]).unwrap();
        assert_eq!(d.read_sectors(7, &mut buf), Ok(()));
        assert_eq!(buf, img[7 * 512..7 * 512 + 2048]);
        let mut tail = vec![0u8; 1024];
        assert_eq!(d.read_sectors(24, &mut tail), Err(BlockError::NotReady));
        assert_eq!(feed(&mut d, &img), 1);
        assert_eq!(d.read_sectors(24, &mut tail), Ok(()));
        assert_eq!(tail, img[3 * 4096..3 * 4096 + 1024]);
        assert_eq!(d.read_sectors(26, &mut [0u8; 512]), Err(BlockError::OutOfRange));
        assert_eq!(d.stats.misses, 4);
    }

    #[test]
    fn consegne_sbagliate_e_blocchi_falliti() {
        assert_eq!(HostDisk::new(4096, 1000, 0).err(), Some(DiskError::BadBlockSize));
        assert_eq!(HostDisk::new(4096, 256, 0).err(), Some(DiskError::BadBlockSize));
        let mut d = HostDisk::new(8192, 4096, 0).unwrap();
        assert_eq!(d.fill(2, &[0; 4096]), Err(DiskError::OutOfRange));
        assert_eq!(d.fill(1, &[0; 512]), Err(DiskError::BadLength));
        let mut buf = [0u8; 512];
        assert_eq!(d.read_sectors(8, &mut buf), Err(BlockError::NotReady));
        assert_eq!(d.take_wanted(), [1]);
        d.fail(1);
        assert_eq!(d.read_sectors(8, &mut buf), Err(BlockError::Io));
        assert_eq!(d.write_sectors(0, &buf), Err(BlockError::ReadOnly));
    }

    /// Oltre il limite si tolgono i blocchi più vecchi, che poi si
    /// richiedono di nuovo.
    #[test]
    fn cache_limitata() {
        let img = image(4 * 4096);
        let mut d = HostDisk::new(img.len() as u64, 4096, 2).unwrap();
        let mut buf = vec![0u8; 4096];
        for s in [0, 8, 16] {
            assert_eq!(d.read_sectors(s, &mut buf), Err(BlockError::NotReady));
            feed(&mut d, &img);
            assert_eq!(d.read_sectors(s, &mut buf), Ok(()));
        }
        assert_eq!(d.cached_blocks(), 2);
        assert_eq!(d.stats.evictions, 1);
        assert_eq!(d.read_sectors(0, &mut buf), Err(BlockError::NotReady));
        assert_eq!(d.take_wanted(), [0]);
    }
}
