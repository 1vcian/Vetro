//! File dell'overlay copy-on-write persistente di un disco (M6, ADR 0016).
//!
//! Le scritture del guest su un disco con un'immagine base in sola lettura
//! (un file, un URL letto con HTTP Range) stanno in cluster da
//! [`CLUSTER`] byte (`CowBackend` di `vetro-platform`). Questo modulo dà il
//! formato del file che li conserva fra una sessione e l'altra, uguale per
//! la CLI (`vetro boot --disk=... --overlay=FILE`) e per il browser (OPFS):
//!
//! ```text
//! intestazione, HEADER_LEN = 4096 byte:
//!   "VETROCOW"  u32 versione  u32 CLUSTER  u64 dimensione del disco
//!   u64 generazione  u64 slot  u32 lunghezza dell'identità  identità
//!   ... zeri ...  u64 hash64 dei primi 4088 byte (all'offset 4088)
//! slot k all'offset HEADER_LEN + k * SLOT_LEN:
//!   u64 cluster (FREE = slot libero)  u64 controllo  CLUSTER byte di dati
//! ```
//!
//! - **Identità della base**: una stringa scelta dall'host (URL, dimensione
//!   ed ETag nel browser; nome, dimensione e data di modifica nella CLI).
//!   Un overlay di un'altra base, o di un disco di un'altra dimensione, si
//!   scarta ([`LoadError::Mismatch`]): applicato a un'altra immagine sarebbe
//!   un filesystem rovinato.
//! - **Scritture sul posto**: ogni cluster ha il suo slot; riscriverlo
//!   riscrive lo slot, un cluster nuovo prende uno slot libero o uno nuovo in
//!   fondo. Niente registro da compattare: il file è grande quanto i cluster
//!   vivi (più gli slot liberati).
//! - **Ordine**: [`Overlay::update`] dà le scritture da fare ([`Patches`]),
//!   con l'intestazione (generazione e numero di slot) per ultima. Un'
//!   interruzione prima dell'intestazione lascia gli slot nuovi fuori dal
//!   conto; uno slot scritto a metà ha il controllo sbagliato e si ignora
//!   (quel cluster torna quello della base).
//! - **Generazione**: cresce a ogni gruppo di scritture che cambia qualcosa.
//!   Il browser la mette accanto allo snapshot della macchina: uno snapshot
//!   vale solo con l'overlay alla stessa generazione (ADR 0016).
//!
//! Il modulo non fa I/O: chi chiama legge il file intero per
//! [`Overlay::load`] e applica le [`Patches`] (JS con
//! `FileSystemSyncAccessHandle`, la CLI con `write_at`).

use std::collections::{BTreeMap, BTreeSet};

use crate::hash64;

/// Primi 8 byte del file.
pub const MAGIC: [u8; 8] = *b"VETROCOW";
/// Versione del formato del file.
pub const VERSION: u32 = 1;
/// Byte di un cluster (quelli di `CowBackend::CLUSTER`).
pub const CLUSTER: u64 = 4096;
/// Byte dell'intestazione.
pub const HEADER_LEN: u64 = 4096;
/// Byte di uno slot: cluster, controllo, dati.
pub const SLOT_LEN: u64 = 16 + CLUSTER;
/// Cluster di uno slot libero.
pub const FREE: u64 = u64::MAX;
/// Lunghezza massima dell'identità della base.
pub const MAX_IDENTITY: usize = HEADER_LEN as usize - 64;

/// Perché un file non si può usare: in tutti i casi l'overlay si scarta e
/// si riparte da uno vuoto (le [`Patches`] successive troncano il file).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadError {
    /// File valido di un'altra immagine base (o di un disco di un'altra
    /// dimensione).
    Mismatch(String),
    /// Non è un overlay leggibile (magia, versione, intestazione rovinata).
    Corrupt(String),
}

impl core::fmt::Display for LoadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            LoadError::Mismatch(why) => write!(f, "overlay di un'altra immagine base ({why}): scartato"),
            LoadError::Corrupt(why) => write!(f, "overlay illeggibile ({why}): scartato"),
        }
    }
}

/// Scritture da fare sul file, in ordine: prima l'eventuale troncamento,
/// poi i byte ai loro offset (l'intestazione per ultima).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Patches {
    /// Tronca il file a questa lunghezza prima delle scritture.
    pub truncate: Option<u64>,
    /// (offset, byte).
    pub writes: Vec<(u64, Vec<u8>)>,
}

impl Patches {
    pub fn is_empty(&self) -> bool {
        self.truncate.is_none() && self.writes.is_empty()
    }

    /// Codifica per il JS (`vetro_overlay_take`): u64 lunghezza a cui
    /// troncare (`u64::MAX` = niente), u32 numero di scritture, poi per
    /// ognuna u64 offset, u32 lunghezza e i byte. Little endian.
    pub fn encode(&self) -> Vec<u8> {
        let n: usize = self.writes.iter().map(|(_, b)| 12 + b.len()).sum();
        let mut out = Vec::with_capacity(12 + n);
        out.extend_from_slice(&self.truncate.unwrap_or(u64::MAX).to_le_bytes());
        out.extend_from_slice(&(self.writes.len() as u32).to_le_bytes());
        for (at, b) in &self.writes {
            out.extend_from_slice(&at.to_le_bytes());
            out.extend_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend_from_slice(b);
        }
        out
    }

    /// Applica le scritture a un file tenuto in memoria (test, e riferimento
    /// per chi le applica a un file vero).
    pub fn apply_to(&self, file: &mut Vec<u8>) {
        if let Some(n) = self.truncate {
            file.truncate(n as usize);
        }
        for (at, b) in &self.writes {
            let (at, end) = (*at as usize, *at as usize + b.len());
            if file.len() < end {
                file.resize(end, 0);
            }
            file[at..end].copy_from_slice(b);
        }
    }
}

/// Somma di controllo di uno slot: lega i dati al cluster.
fn slot_check(cluster: u64, data: &[u8]) -> u64 {
    hash64(data) ^ cluster.rotate_left(17) ^ 0x5a17_c0de_0f5e_7a11
}

/// Lo stato di un file di overlay: dove sta ogni cluster, gli slot liberi,
/// la generazione.
#[derive(Clone, Debug)]
pub struct Overlay {
    identity: Vec<u8>,
    disk_size: u64,
    generation: u64,
    /// Slot usati nel file (liberi compresi).
    slots: u64,
    /// cluster -> (slot, controllo dei dati).
    map: BTreeMap<u64, (u64, u64)>,
    free: BTreeSet<u64>,
    /// Slot con il controllo sbagliato trovati da [`load`](Self::load).
    damaged: u64,
    /// Il file va riscritto da capo (nuovo, o scartato): la prossima
    /// [`update`](Self::update) tronca e scrive l'intestazione.
    fresh: bool,
}

/// Quello che [`Overlay::load`] ha trovato.
pub struct Loaded<'a> {
    pub overlay: Overlay,
    /// I cluster del file (indice, dati lunghi come il cluster nel disco),
    /// in ordine di indice.
    pub clusters: Vec<(u64, &'a [u8])>,
}

impl Overlay {
    /// Overlay vuoto per il disco di `disk_size` byte con base `identity`
    /// (tagliata a [`MAX_IDENTITY`] byte). Il file va scritto da capo.
    pub fn new(identity: &[u8], disk_size: u64) -> Self {
        Overlay {
            identity: identity[..identity.len().min(MAX_IDENTITY)].to_vec(),
            disk_size,
            generation: 0,
            slots: 0,
            map: BTreeMap::new(),
            free: BTreeSet::new(),
            damaged: 0,
            fresh: true,
        }
    }

    /// Legge il file intero `bytes` per il disco di `disk_size` byte con
    /// base `identity`. Un file vuoto dà un overlay nuovo senza cluster.
    pub fn load<'a>(bytes: &'a [u8], identity: &[u8], disk_size: u64) -> Result<Loaded<'a>, LoadError> {
        let mut ov = Overlay::new(identity, disk_size);
        if bytes.is_empty() {
            return Ok(Loaded { overlay: ov, clusters: Vec::new() });
        }
        let corrupt = |why: &str| LoadError::Corrupt(why.to_string());
        if bytes.len() < HEADER_LEN as usize {
            return Err(corrupt("più corto dell'intestazione"));
        }
        let h = &bytes[..HEADER_LEN as usize];
        if h[..8] != MAGIC {
            return Err(corrupt("non è un overlay di Vetro"));
        }
        let u32_at = |o: usize| u32::from_le_bytes(h[o..o + 4].try_into().expect("4 byte"));
        let u64_at = |o: usize| u64::from_le_bytes(h[o..o + 8].try_into().expect("8 byte"));
        if u32_at(8) != VERSION {
            return Err(corrupt(&format!("formato versione {}, atteso {VERSION}", u32_at(8))));
        }
        if u64_at(HEADER_LEN as usize - 8) != hash64(&h[..HEADER_LEN as usize - 8]) {
            return Err(corrupt("intestazione rovinata"));
        }
        if u64::from(u32_at(12)) != CLUSTER {
            return Err(corrupt(&format!("cluster da {} byte", u32_at(12))));
        }
        let id_len = u32_at(40) as usize;
        if id_len > MAX_IDENTITY {
            return Err(corrupt("identità troppo lunga"));
        }
        let found_id = &h[44..44 + id_len];
        if found_id != ov.identity.as_slice() {
            return Err(LoadError::Mismatch(format!(
                "base {:?}, attesa {:?}",
                String::from_utf8_lossy(found_id),
                String::from_utf8_lossy(&ov.identity)
            )));
        }
        let size = u64_at(16);
        if size != disk_size {
            return Err(LoadError::Mismatch(format!("disco di {size} byte, atteso {disk_size}")));
        }
        ov.generation = u64_at(24);
        ov.slots = u64_at(32);
        ov.fresh = false;
        let clusters = disk_size.div_ceil(CLUSTER);
        let mut out = Vec::new();
        for k in 0..ov.slots {
            let at = HEADER_LEN + k * SLOT_LEN;
            let Some(slot) = bytes.get(at as usize..(at + SLOT_LEN) as usize) else {
                // Oltre la fine del file: mai scritto, libero.
                ov.free.insert(k);
                continue;
            };
            let c = u64::from_le_bytes(slot[..8].try_into().expect("8 byte"));
            let check = u64::from_le_bytes(slot[8..16].try_into().expect("8 byte"));
            let data = &slot[16..];
            if check != slot_check(c, data) || (c != FREE && (c >= clusters || ov.map.contains_key(&c))) {
                ov.damaged += 1;
                ov.free.insert(k);
                continue;
            }
            if c == FREE {
                ov.free.insert(k);
                continue;
            }
            ov.map.insert(c, (k, check));
            let len = (disk_size - c * CLUSTER).min(CLUSTER) as usize;
            out.push((c, &data[..len]));
        }
        out.sort_unstable_by_key(|&(c, _)| c);
        Ok(Loaded { overlay: ov, clusters: out })
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Cluster nel file.
    pub fn clusters(&self) -> usize {
        self.map.len()
    }

    /// Slot nel file (liberi compresi).
    pub fn slots(&self) -> u64 {
        self.slots
    }

    /// Slot rovinati trovati alla lettura.
    pub fn damaged(&self) -> u64 {
        self.damaged
    }

    /// Lunghezza del file dopo le scritture date finora.
    pub fn file_len(&self) -> u64 {
        if self.fresh && self.slots == 0 { 0 } else { HEADER_LEN + self.slots * SLOT_LEN }
    }

    pub fn identity(&self) -> &[u8] {
        &self.identity
    }

    fn header(&self) -> Vec<u8> {
        let mut h = vec![0u8; HEADER_LEN as usize];
        h[..8].copy_from_slice(&MAGIC);
        h[8..12].copy_from_slice(&VERSION.to_le_bytes());
        h[12..16].copy_from_slice(&(CLUSTER as u32).to_le_bytes());
        h[16..24].copy_from_slice(&self.disk_size.to_le_bytes());
        h[24..32].copy_from_slice(&self.generation.to_le_bytes());
        h[32..40].copy_from_slice(&self.slots.to_le_bytes());
        h[40..44].copy_from_slice(&(self.identity.len() as u32).to_le_bytes());
        h[44..44 + self.identity.len()].copy_from_slice(&self.identity);
        let sum = hash64(&h[..HEADER_LEN as usize - 8]);
        h[HEADER_LEN as usize - 8..].copy_from_slice(&sum.to_le_bytes());
        h
    }

    fn slot_bytes(cluster: u64, data: &[u8]) -> (u64, Vec<u8>) {
        let mut s = vec![0u8; SLOT_LEN as usize];
        s[16..16 + data.len()].copy_from_slice(data);
        let check = slot_check(cluster, &s[16..]);
        s[..8].copy_from_slice(&cluster.to_le_bytes());
        s[8..16].copy_from_slice(&check.to_le_bytes());
        (check, s)
    }

    /// Porta il file allo stato dei cluster dati: `(indice, Some(dati))`
    /// scrive il cluster (se è cambiato), `(indice, None)` lo toglie.
    /// Restituisce le scritture da fare; se c'è qualcosa, la generazione
    /// cresce di uno e l'intestazione è l'ultima scrittura.
    pub fn update<'d>(&mut self, changes: impl IntoIterator<Item = (u64, Option<&'d [u8]>)>) -> Patches {
        let mut p = Patches { truncate: self.fresh.then_some(0), writes: Vec::new() };
        let clusters = self.disk_size.div_ceil(CLUSTER);
        for (c, data) in changes {
            match data {
                Some(data) => {
                    assert!(c < clusters && data.len() as u64 <= CLUSTER, "cluster {c} fuori dal disco");
                    let (check, bytes) = Self::slot_bytes(c, data);
                    let slot = match self.map.get(&c) {
                        Some(&(_, old)) if old == check => continue,
                        Some(&(slot, _)) => slot,
                        None => self.free.pop_first().unwrap_or_else(|| {
                            self.slots += 1;
                            self.slots - 1
                        }),
                    };
                    self.map.insert(c, (slot, check));
                    p.writes.push((HEADER_LEN + slot * SLOT_LEN, bytes));
                }
                None => {
                    let Some((slot, _)) = self.map.remove(&c) else { continue };
                    self.free.insert(slot);
                    p.writes.push((HEADER_LEN + slot * SLOT_LEN, Self::slot_bytes(FREE, &[]).1));
                }
            }
        }
        if !p.writes.is_empty() || self.fresh {
            self.generation += 1;
            self.fresh = false;
            p.writes.push((0, self.header()));
        }
        p
    }

    /// Come [`update`](Self::update) con lo stato completo: `all` sono tutti
    /// i cluster scritti del disco; quelli del file che non ci sono si
    /// tolgono. Serve dopo il ripristino di uno snapshot, quando i cluster
    /// in memoria possono essere diversi da quelli del file.
    pub fn sync<'d>(&mut self, all: impl IntoIterator<Item = (u64, &'d [u8])>) -> Patches {
        let mut gone: BTreeSet<u64> = self.map.keys().copied().collect();
        let mut changes: Vec<(u64, Option<&'d [u8]>)> = Vec::new();
        for (c, d) in all {
            gone.remove(&c);
            changes.push((c, Some(d)));
        }
        changes.extend(gone.into_iter().map(|c| (c, None)));
        self.update(changes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: u64 = 10 * CLUSTER + 1024; // l'ultimo cluster è corto

    fn fill(v: u8, n: usize) -> Vec<u8> {
        vec![v; n]
    }

    #[test]
    fn scrive_rilegge_e_riscrive_sul_posto() {
        let mut file = Vec::new();
        let mut ov = Overlay::load(&file, b"base-1", SIZE).unwrap().overlay;
        assert_eq!(ov.file_len(), 0);
        let (a, b, last) = (fill(1, 4096), fill(2, 4096), fill(3, 1024));
        let p = ov.update([(3, Some(&a[..])), (7, Some(&b[..])), (10, Some(&last[..]))]);
        assert_eq!(p.truncate, Some(0), "file nuovo: si tronca");
        assert_eq!(p.writes.last().unwrap().0, 0, "l'intestazione per ultima");
        p.apply_to(&mut file);
        assert_eq!(file.len() as u64, HEADER_LEN + 3 * SLOT_LEN);
        assert_eq!(ov.generation(), 1);

        let l = Overlay::load(&file, b"base-1", SIZE).unwrap();
        assert_eq!(l.clusters, vec![(3, &a[..]), (7, &b[..]), (10, &last[..])]);
        let mut ov = l.overlay;
        assert_eq!((ov.generation(), ov.clusters(), ov.slots()), (1, 3, 3));
        // Uguale: niente da scrivere, generazione ferma.
        assert!(ov.update([(3, Some(&a[..]))]).is_empty());
        assert_eq!(ov.generation(), 1);
        // Cambiato: stesso slot, file della stessa lunghezza.
        let a2 = fill(9, 4096);
        let p = ov.update([(3, Some(&a2[..]))]);
        assert_eq!(p.truncate, None);
        assert_eq!(p.writes.len(), 2);
        assert_eq!(p.writes[0].0, HEADER_LEN);
        p.apply_to(&mut file);
        assert_eq!(file.len() as u64, HEADER_LEN + 3 * SLOT_LEN);
        // Tolto: lo slot si libera e il cluster nuovo lo riprende.
        ov.update([(7, None)]).apply_to(&mut file);
        let l = Overlay::load(&file, b"base-1", SIZE).unwrap();
        assert_eq!(l.clusters, vec![(3, &a2[..]), (10, &last[..])]);
        assert_eq!(l.overlay.generation(), 3);
        let p = ov.update([(0, Some(&b[..]))]);
        assert_eq!(p.writes[0].0, HEADER_LEN + SLOT_LEN, "slot liberato riusato");
        p.apply_to(&mut file);
        assert_eq!(file.len() as u64, HEADER_LEN + 3 * SLOT_LEN);
        let l = Overlay::load(&file, b"base-1", SIZE).unwrap();
        assert_eq!(l.clusters.iter().map(|c| c.0).collect::<Vec<_>>(), [0, 3, 10]);
    }

    #[test]
    fn altra_base_o_file_rovinato_si_scarta() {
        let mut file = Vec::new();
        let mut ov = Overlay::new(b"base-1", SIZE);
        ov.update([(1, Some(&fill(5, 4096)[..]))]).apply_to(&mut file);
        assert!(matches!(Overlay::load(&file, b"base-2", SIZE), Err(LoadError::Mismatch(_))));
        assert!(matches!(Overlay::load(&file, b"base-1", SIZE + 512), Err(LoadError::Mismatch(_))));
        let mut bad = file.clone();
        bad[100] ^= 1;
        let e = Overlay::load(&bad, b"base-1", SIZE).err().unwrap();
        assert!(matches!(e, LoadError::Corrupt(_)), "{e}");
        assert!(e.to_string().contains("rovinata"), "{e}");
        assert!(matches!(Overlay::load(&file[..100], b"base-1", SIZE), Err(LoadError::Corrupt(_))));
        assert!(matches!(Overlay::load(b"altro file", b"base-1", SIZE), Err(LoadError::Corrupt(_))));
        let mut v2 = file.clone();
        v2[8] = 2;
        assert!(Overlay::load(&v2, b"base-1", SIZE).err().unwrap().to_string().contains("versione"));
        // Dopo lo scarto si riparte da un overlay nuovo: file troncato.
        let mut fresh = Overlay::new(b"base-2", SIZE);
        let p = fresh.update(core::iter::empty());
        assert_eq!(p.truncate, Some(0));
        p.apply_to(&mut file);
        assert_eq!(file.len() as u64, HEADER_LEN);
        let l = Overlay::load(&file, b"base-2", SIZE).unwrap();
        assert!(l.clusters.is_empty());
    }

    /// Uno slot scritto a metà (controllo sbagliato) si ignora: quel
    /// cluster torna quello della base, gli altri restano; uno slot oltre
    /// la fine del file (interruzione prima dei dati) è libero.
    #[test]
    fn slot_rovinato_o_mancante() {
        let mut file = Vec::new();
        let mut ov = Overlay::new(b"b", SIZE);
        ov.update([(1, Some(&fill(1, 4096)[..])), (2, Some(&fill(2, 4096)[..]))]).apply_to(&mut file);
        let at = (HEADER_LEN + SLOT_LEN + 16 + 5) as usize;
        file[at] ^= 0xff;
        let l = Overlay::load(&file, b"b", SIZE).unwrap();
        assert_eq!(l.clusters.len(), 1);
        assert_eq!(l.clusters[0].0, 1);
        assert_eq!(l.overlay.damaged(), 1);
        let mut ov = l.overlay;
        let p = ov.update([(4, Some(&fill(4, 4096)[..]))]);
        assert_eq!(p.writes[0].0, HEADER_LEN + SLOT_LEN, "lo slot rovinato si riusa");
        file.truncate((HEADER_LEN + SLOT_LEN) as usize);
        let l = Overlay::load(&file, b"b", SIZE).unwrap();
        assert_eq!(l.clusters.len(), 1);
        assert_eq!(l.overlay.damaged(), 0);
    }

    /// `sync` porta il file a uno stato completo: cluster cambiati riscritti,
    /// mancanti tolti, uguali lasciati; nessuna scrittura se coincide.
    #[test]
    fn sync_allo_stato_completo() {
        let mut file = Vec::new();
        let mut ov = Overlay::new(b"b", SIZE);
        let (x, y, z) = (fill(1, 4096), fill(2, 4096), fill(3, 4096));
        ov.update([(1, Some(&x[..])), (2, Some(&y[..]))]).apply_to(&mut file);
        assert!(ov.sync([(1, &x[..]), (2, &y[..])]).is_empty());
        let p = ov.sync([(2, &z[..]), (5, &x[..])]);
        p.apply_to(&mut file);
        let l = Overlay::load(&file, b"b", SIZE).unwrap();
        assert_eq!(l.clusters, vec![(2, &z[..]), (5, &x[..])]);
        assert_eq!(l.overlay.generation(), 2);
        let enc = p.encode();
        assert_eq!(u64::from_le_bytes(enc[..8].try_into().unwrap()), u64::MAX);
        assert_eq!(u32::from_le_bytes(enc[8..12].try_into().unwrap()) as usize, p.writes.len());
    }
}
