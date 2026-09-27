//! File of a disk's persistent copy-on-write overlay (M6, ADR 0016).
//!
//! The guest's writes to a disk with a read-only base image
//! (a file, a URL read with HTTP Range) live in clusters of
//! [`CLUSTER`] bytes (`CowBackend` in `vetro-platform`). This module gives the
//! format of the file that keeps them from one session to the next, the same for
//! the CLI (`vetro boot --disk=... --overlay=FILE`) and for the browser (OPFS):
//!
//! ```text
//! header, HEADER_LEN = 4096 bytes:
//!   "VETROCOW"  u32 version  u32 CLUSTER  u64 disk size
//!   u64 generation  u64 slots  u32 identity length  identity
//!   ... zeros ...  u64 hash64 of the first 4088 bytes (at offset 4088)
//! slot k at offset HEADER_LEN + k * SLOT_LEN:
//!   u64 cluster (FREE = free slot)  u64 check  CLUSTER bytes of data
//! ```
//!
//! - **Base identity**: a string chosen by the host (URL, size
//!   and ETag in the browser; name, size and modification date in the CLI).
//!   An overlay of another base, or of a disk of another size, is
//!   discarded ([`LoadError::Mismatch`]): applied to another image it would be
//!   a corrupted filesystem.
//! - **In-place writes**: every cluster has its slot; rewriting it
//!   rewrites the slot, a new cluster takes a free slot or a new one at the
//!   end. No log to compact: the file is as large as the live
//!   clusters (plus the freed slots).
//! - **Ordering**: [`Overlay::update`] gives the writes to perform ([`Patches`]),
//!   with the header (generation and slot count) last. An
//!   interruption before the header leaves the new slots out of the
//!   count; a half-written slot has the wrong check and is ignored
//!   (that cluster goes back to the base's).
//! - **Generation**: grows with every group of writes that changes something.
//!   The browser stores it next to the machine snapshot: a snapshot
//!   is valid only with the overlay at the same generation (ADR 0016).
//!
//! The module does no I/O: the caller reads the whole file for
//! [`Overlay::load`] and applies the [`Patches`] (JS with
//! `FileSystemSyncAccessHandle`, the CLI with `write_at`).

use std::collections::{BTreeMap, BTreeSet};

use crate::hash64;

/// First 8 bytes of the file.
pub const MAGIC: [u8; 8] = *b"VETROCOW";
/// File format version.
pub const VERSION: u32 = 1;
/// Bytes in a cluster (those of `CowBackend::CLUSTER`).
pub const CLUSTER: u64 = 4096;
/// Header bytes.
pub const HEADER_LEN: u64 = 4096;
/// Bytes in a slot: cluster, check, data.
pub const SLOT_LEN: u64 = 16 + CLUSTER;
/// Cluster of a free slot.
pub const FREE: u64 = u64::MAX;
/// Maximum length of the base identity.
pub const MAX_IDENTITY: usize = HEADER_LEN as usize - 64;

/// Why a file cannot be used: in every case the overlay is discarded and
/// we start again from an empty one (the following [`Patches`] truncate the file).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadError {
    /// Valid file of another base image (or of a disk of another
    /// size).
    Mismatch(String),
    /// Not a readable overlay (magic, version, corrupted header).
    Corrupt(String),
}

impl core::fmt::Display for LoadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            LoadError::Mismatch(why) => write!(f, "overlay of another base image ({why}): discarded"),
            LoadError::Corrupt(why) => write!(f, "unreadable overlay ({why}): discarded"),
        }
    }
}

/// Writes to perform on the file, in order: first the truncation, if any,
/// then the bytes at their offsets (the header last).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Patches {
    /// Truncates the file to this length before the writes.
    pub truncate: Option<u64>,
    /// (offset, bytes).
    pub writes: Vec<(u64, Vec<u8>)>,
}

impl Patches {
    pub fn is_empty(&self) -> bool {
        self.truncate.is_none() && self.writes.is_empty()
    }

    /// Encoding for JS (`vetro_overlay_take`): u64 length to
    /// truncate to (`u64::MAX` = none), u32 number of writes, then for
    /// each one u64 offset, u32 length and the bytes. Little endian.
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

    /// Applies the writes to a file held in memory (tests, and reference
    /// for whoever applies them to a real file).
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

/// Checksum of a slot: binds the data to the cluster.
fn slot_check(cluster: u64, data: &[u8]) -> u64 {
    hash64(data) ^ cluster.rotate_left(17) ^ 0x5a17_c0de_0f5e_7a11
}

/// The state of an overlay file: where every cluster is, the free slots,
/// the generation.
#[derive(Clone, Debug)]
pub struct Overlay {
    identity: Vec<u8>,
    disk_size: u64,
    generation: u64,
    /// Slots used in the file (free ones included).
    slots: u64,
    /// cluster -> (slot, data check).
    map: BTreeMap<u64, (u64, u64)>,
    free: BTreeSet<u64>,
    /// Slots with the wrong check found by [`load`](Self::load).
    damaged: u64,
    /// The file must be rewritten from scratch (new, or discarded): the next
    /// [`update`](Self::update) truncates and writes the header.
    fresh: bool,
}

/// What [`Overlay::load`] found.
pub struct Loaded<'a> {
    pub overlay: Overlay,
    /// The file's clusters (index, data as long as the cluster in the disk),
    /// in index order.
    pub clusters: Vec<(u64, &'a [u8])>,
}

impl Overlay {
    /// Empty overlay for the disk of `disk_size` bytes with base `identity`
    /// (cut to [`MAX_IDENTITY`] bytes). The file must be written from scratch.
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

    /// Reads the whole file `bytes` for the disk of `disk_size` bytes with
    /// base `identity`. An empty file gives a new overlay with no clusters.
    pub fn load<'a>(bytes: &'a [u8], identity: &[u8], disk_size: u64) -> Result<Loaded<'a>, LoadError> {
        let mut ov = Overlay::new(identity, disk_size);
        if bytes.is_empty() {
            return Ok(Loaded { overlay: ov, clusters: Vec::new() });
        }
        let corrupt = |why: &str| LoadError::Corrupt(why.to_string());
        if bytes.len() < HEADER_LEN as usize {
            return Err(corrupt("shorter than the header"));
        }
        let h = &bytes[..HEADER_LEN as usize];
        if h[..8] != MAGIC {
            return Err(corrupt("not a Vetro overlay"));
        }
        let u32_at = |o: usize| u32::from_le_bytes(h[o..o + 4].try_into().expect("4 bytes"));
        let u64_at = |o: usize| u64::from_le_bytes(h[o..o + 8].try_into().expect("8 bytes"));
        if u32_at(8) != VERSION {
            return Err(corrupt(&format!("format version {}, expected {VERSION}", u32_at(8))));
        }
        if u64_at(HEADER_LEN as usize - 8) != hash64(&h[..HEADER_LEN as usize - 8]) {
            return Err(corrupt("corrupted header"));
        }
        if u64::from(u32_at(12)) != CLUSTER {
            return Err(corrupt(&format!("clusters of {} bytes", u32_at(12))));
        }
        let id_len = u32_at(40) as usize;
        if id_len > MAX_IDENTITY {
            return Err(corrupt("identity too long"));
        }
        let found_id = &h[44..44 + id_len];
        if found_id != ov.identity.as_slice() {
            return Err(LoadError::Mismatch(format!(
                "base {:?}, expected {:?}",
                String::from_utf8_lossy(found_id),
                String::from_utf8_lossy(&ov.identity)
            )));
        }
        let size = u64_at(16);
        if size != disk_size {
            return Err(LoadError::Mismatch(format!("disk of {size} bytes, expected {disk_size}")));
        }
        ov.generation = u64_at(24);
        ov.slots = u64_at(32);
        ov.fresh = false;
        let clusters = disk_size.div_ceil(CLUSTER);
        let mut out = Vec::new();
        for k in 0..ov.slots {
            let at = HEADER_LEN + k * SLOT_LEN;
            let Some(slot) = bytes.get(at as usize..(at + SLOT_LEN) as usize) else {
                // Past the end of the file: never written, free.
                ov.free.insert(k);
                continue;
            };
            let c = u64::from_le_bytes(slot[..8].try_into().expect("8 bytes"));
            let check = u64::from_le_bytes(slot[8..16].try_into().expect("8 bytes"));
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

    /// Clusters in the file.
    pub fn clusters(&self) -> usize {
        self.map.len()
    }

    /// Slots in the file (free ones included).
    pub fn slots(&self) -> u64 {
        self.slots
    }

    /// Corrupted slots found on reading.
    pub fn damaged(&self) -> u64 {
        self.damaged
    }

    /// Length of the file after the writes given so far.
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

    /// Brings the file to the state of the given clusters: `(index, Some(data))`
    /// writes the cluster (if it changed), `(index, None)` removes it.
    /// Returns the writes to perform; if there is anything, the generation
    /// grows by one and the header is the last write.
    pub fn update<'d>(&mut self, changes: impl IntoIterator<Item = (u64, Option<&'d [u8]>)>) -> Patches {
        let mut p = Patches { truncate: self.fresh.then_some(0), writes: Vec::new() };
        let clusters = self.disk_size.div_ceil(CLUSTER);
        for (c, data) in changes {
            match data {
                Some(data) => {
                    assert!(c < clusters && data.len() as u64 <= CLUSTER, "cluster {c} outside the disk");
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

    /// Like [`update`](Self::update) with the complete state: `all` are all
    /// the written clusters of the disk; those in the file that are missing are
    /// removed. Needed after restoring a snapshot, when the clusters
    /// in memory can differ from those in the file.
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

    const SIZE: u64 = 10 * CLUSTER + 1024; // the last cluster is short

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
        assert_eq!(p.truncate, Some(0), "new file: truncated");
        assert_eq!(p.writes.last().unwrap().0, 0, "the header last");
        p.apply_to(&mut file);
        assert_eq!(file.len() as u64, HEADER_LEN + 3 * SLOT_LEN);
        assert_eq!(ov.generation(), 1);

        let l = Overlay::load(&file, b"base-1", SIZE).unwrap();
        assert_eq!(l.clusters, vec![(3, &a[..]), (7, &b[..]), (10, &last[..])]);
        let mut ov = l.overlay;
        assert_eq!((ov.generation(), ov.clusters(), ov.slots()), (1, 3, 3));
        // Same: nothing to write, generation unchanged.
        assert!(ov.update([(3, Some(&a[..]))]).is_empty());
        assert_eq!(ov.generation(), 1);
        // Changed: same slot, file of the same length.
        let a2 = fill(9, 4096);
        let p = ov.update([(3, Some(&a2[..]))]);
        assert_eq!(p.truncate, None);
        assert_eq!(p.writes.len(), 2);
        assert_eq!(p.writes[0].0, HEADER_LEN);
        p.apply_to(&mut file);
        assert_eq!(file.len() as u64, HEADER_LEN + 3 * SLOT_LEN);
        // Removed: the slot is freed and the new cluster takes it back.
        ov.update([(7, None)]).apply_to(&mut file);
        let l = Overlay::load(&file, b"base-1", SIZE).unwrap();
        assert_eq!(l.clusters, vec![(3, &a2[..]), (10, &last[..])]);
        assert_eq!(l.overlay.generation(), 3);
        let p = ov.update([(0, Some(&b[..]))]);
        assert_eq!(p.writes[0].0, HEADER_LEN + SLOT_LEN, "freed slot reused");
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
        assert!(e.to_string().contains("corrupted"), "{e}");
        assert!(matches!(Overlay::load(&file[..100], b"base-1", SIZE), Err(LoadError::Corrupt(_))));
        assert!(matches!(Overlay::load(b"other file", b"base-1", SIZE), Err(LoadError::Corrupt(_))));
        let mut v2 = file.clone();
        v2[8] = 2;
        assert!(Overlay::load(&v2, b"base-1", SIZE).err().unwrap().to_string().contains("version"));
        // After discarding we start again from a new overlay: file truncated.
        let mut fresh = Overlay::new(b"base-2", SIZE);
        let p = fresh.update(core::iter::empty());
        assert_eq!(p.truncate, Some(0));
        p.apply_to(&mut file);
        assert_eq!(file.len() as u64, HEADER_LEN);
        let l = Overlay::load(&file, b"base-2", SIZE).unwrap();
        assert!(l.clusters.is_empty());
    }

    /// A half-written slot (wrong check) is ignored: that
    /// cluster goes back to the base's, the others stay; a slot past
    /// the end of the file (interruption before the data) is free.
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
        assert_eq!(p.writes[0].0, HEADER_LEN + SLOT_LEN, "the corrupted slot is reused");
        file.truncate((HEADER_LEN + SLOT_LEN) as usize);
        let l = Overlay::load(&file, b"b", SIZE).unwrap();
        assert_eq!(l.clusters.len(), 1);
        assert_eq!(l.overlay.damaged(), 0);
    }

    /// `sync` brings the file to a complete state: changed clusters rewritten,
    /// missing ones removed, equal ones left alone; no writes if it matches.
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
