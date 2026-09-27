//! File-backed virtio-blk disks for `vetro boot` (native runner).
//!
//! The file is only read: the guest's writes end up in an in-memory
//! [`CowBackend`] (like QEMU's `snapshot=on`), so the image
//! never changes and two boots start from the same disk.
//!
//! With `--overlay=FILE` (M6, ADR 0017) the guest's writes are kept
//! in FILE, in the same format as the browser's overlay
//! (`vetro_snapshot::overlay`): on the next boot they are reapplied. FILE
//! remembers the base image ([`base_identity`]): with a different base it is discarded.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;

use vetro_machine::vetro_snapshot::overlay::{LoadError, Overlay, Patches};
use vetro_platform::virtio::{BlockBackend, BlockError, CowBackend};

/// Image in a file, read-only. The size is rounded down
/// to 512 bytes (like QEMU for raw disks).
pub struct FileBackend {
    file: File,
    size: u64,
}

impl FileBackend {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = File::open(path)?;
        let size = file.metadata()?.len() / 512 * 512;
        Ok(FileBackend { file, size })
    }
}

impl BlockBackend for FileBackend {
    fn size(&self) -> u64 {
        self.size
    }
    fn read_only(&self) -> bool {
        true
    }
    fn read_sectors(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let start = sector.checked_mul(512).ok_or(BlockError::OutOfRange)?;
        let end = start.checked_add(buf.len() as u64).ok_or(BlockError::OutOfRange)?;
        if end > self.size || !buf.len().is_multiple_of(512) {
            return Err(BlockError::OutOfRange);
        }
        self.file.read_exact_at(buf, start).map_err(|_| BlockError::Io)
    }
    fn write_sectors(&mut self, _sector: u64, _data: &[u8]) -> Result<(), BlockError> {
        Err(BlockError::ReadOnly)
    }
    fn flush(&mut self) -> Result<(), BlockError> {
        Ok(())
    }
}

/// Guest-writable disk on top of a file that stays intact.
pub fn cow_disk(path: &Path) -> std::io::Result<CowBackend<FileBackend>> {
    FileBackend::open(path).map(CowBackend::new)
}

/// Identity of the base image for the overlay: file name, size and
/// modification date (in ns). It changes if the image is replaced or
/// modified; it does not change if it is moved to another folder.
pub fn base_identity(path: &Path) -> std::io::Result<Vec<u8>> {
    let meta = std::fs::metadata(path)?;
    let mtime = meta.modified()?.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(format!("file:{name}|{}|{mtime}", meta.len()).into_bytes())
}

/// Persistent overlay of a file-backed disk.
pub struct FileOverlay {
    file: File,
    overlay: Overlay,
    /// After a restore all clusters are compared, not just the written ones.
    full_sync: bool,
}

impl FileOverlay {
    /// Opens (or creates) the overlay `path` for the disk `cow` with base
    /// `identity` and loads its clusters into the copy-on-write. The second value
    /// says why an existing file was discarded (different base, unreadable).
    pub fn open<B: BlockBackend>(
        path: &Path,
        identity: &[u8],
        cow: &mut CowBackend<B>,
    ) -> std::io::Result<(Self, Option<LoadError>)> {
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        let bytes = std::fs::read(path)?;
        let (overlay, discarded) = match Overlay::load(&bytes, identity, cow.size()) {
            Ok(l) => {
                for &(c, data) in &l.clusters {
                    cow.load_cluster(c, data)
                        .map_err(|e| std::io::Error::other(format!("cluster {c}: {e:?}")))?;
                }
                (l.overlay, None)
            }
            Err(e) => (Overlay::new(identity, cow.size()), Some(e)),
        };
        Ok((FileOverlay { file, overlay, full_sync: false }, discarded))
    }

    /// Writes to the file the clusters written by the guest since last time (all
    /// those that differ from the file after [`after_restore`](Self::after_restore)).
    /// Returns whether it wrote anything.
    pub fn persist<B: BlockBackend>(&mut self, cow: &mut CowBackend<B>) -> std::io::Result<bool> {
        let dirty = cow.take_dirty();
        let p = if core::mem::take(&mut self.full_sync) {
            self.overlay.sync(cow.clusters())
        } else {
            self.overlay.update(dirty.into_iter().map(|c| (c, cow.cluster(c))))
        };
        if p.is_empty() {
            return Ok(false);
        }
        apply(&self.file, &p)?;
        Ok(true)
    }

    /// The in-memory clusters come from a snapshot: on the next
    /// [`persist`](Self::persist) the file is realigned to all of them.
    pub fn after_restore(&mut self) {
        self.full_sync = true;
    }

    pub fn overlay(&self) -> &Overlay {
        &self.overlay
    }
}

/// Applies the writes in order (the header last, after the
/// data is on disk).
fn apply(file: &File, p: &Patches) -> std::io::Result<()> {
    if let Some(n) = p.truncate {
        file.set_len(n)?;
    }
    let (header, data): (Vec<_>, Vec<_>) = p.writes.iter().partition(|(at, _)| *at == 0);
    for (at, bytes) in data {
        file.write_all_at(bytes, *at)?;
    }
    if !header.is_empty() {
        file.sync_data()?;
        for (at, bytes) in header {
            file.write_all_at(bytes, *at)?;
        }
    }
    file.sync_data()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scritture_in_memoria_file_intatto() {
        let dir = std::env::temp_dir().join(format!("vetro-disk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("disco.img");
        let data: Vec<u8> = (0..1536u32).map(|i| (i % 251) as u8).collect();
        // 1536 + 100 bytes: the tail below 512 is not part of the disk.
        let mut f = data.clone();
        f.extend_from_slice(&[0xee; 100]);
        std::fs::write(&path, &f).unwrap();

        let mut d = cow_disk(&path).unwrap();
        assert_eq!(d.size(), 1536);
        assert!(!d.read_only(), "the copy-on-write accepts writes");
        let mut buf = vec![0; 1024];
        d.read_sectors(1, &mut buf).unwrap();
        assert_eq!(buf, data[512..]);
        d.write_sectors(2, &[7; 512]).unwrap();
        d.read_sectors(1, &mut buf).unwrap();
        assert_eq!(&buf[..512], &data[512..1024]);
        assert_eq!(&buf[512..], &[7; 512][..]);
        assert_eq!(d.read_sectors(3, &mut [0; 512]), Err(BlockError::OutOfRange));
        assert_eq!(std::fs::read(&path).unwrap(), f, "the file does not change");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `--overlay`: the writes come back on reopening; with a different base
    /// the overlay is discarded and the file is rewritten; after a restore the file
    /// is realigned.
    #[test]
    fn overlay_su_file() {
        let dir = std::env::temp_dir().join(format!("vetro-overlay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (base, ov) = (dir.join("base.img"), dir.join("disco.cow"));
        let data: Vec<u8> = (0..32768u32).map(|i| (i % 253) as u8).collect();
        std::fs::write(&base, &data).unwrap();
        let id = base_identity(&base).unwrap();
        assert!(String::from_utf8_lossy(&id).starts_with("file:base.img|32768|"));

        let mut d = cow_disk(&base).unwrap();
        let (mut o, discarded) = FileOverlay::open(&ov, &id, &mut d).unwrap();
        assert!(discarded.is_none());
        d.write_sectors(3, &[0xaa; 512]).unwrap();
        d.write_sectors(20, &[0xbb; 1024]).unwrap();
        assert!(o.persist(&mut d).unwrap());
        assert!(!o.persist(&mut d).unwrap(), "nothing new");
        let saved = {
            let mut w = vetro_machine::vetro_snapshot::Writer::new();
            d.save_state(&mut w);
            w.into_bytes()
        };
        d.write_sectors(3, &[0xcc; 512]).unwrap();
        assert!(o.persist(&mut d).unwrap());
        assert_eq!(o.overlay().generation(), 2);

        let mut d2 = cow_disk(&base).unwrap();
        let (mut o2, discarded) = FileOverlay::open(&ov, &id, &mut d2).unwrap();
        assert!(discarded.is_none());
        let mut buf = vec![0u8; 512];
        d2.read_sectors(3, &mut buf).unwrap();
        assert_eq!(buf, [0xcc; 512]);
        d2.read_sectors(21, &mut buf).unwrap();
        assert_eq!(buf, [0xbb; 512]);
        d2.read_sectors(0, &mut buf).unwrap();
        assert_eq!(buf, data[..512]);
        d2.restore_state(&mut vetro_machine::vetro_snapshot::Reader::new(&saved)).unwrap();
        o2.after_restore();
        assert!(o2.persist(&mut d2).unwrap());
        let mut d3 = cow_disk(&base).unwrap();
        FileOverlay::open(&ov, &id, &mut d3).unwrap();
        d3.read_sectors(3, &mut buf).unwrap();
        assert_eq!(buf, [0xaa; 512], "realigned to the snapshot");

        let mut d4 = cow_disk(&base).unwrap();
        let (mut o4, discarded) = FileOverlay::open(&ov, b"file:altro|32768|1", &mut d4).unwrap();
        assert!(discarded.unwrap().to_string().contains("another base image"));
        d4.read_sectors(3, &mut buf).unwrap();
        assert_eq!(buf, data[3 * 512..4 * 512]);
        assert!(o4.persist(&mut d4).unwrap());
        assert_eq!(std::fs::metadata(&ov).unwrap().len(), 4096, "rewritten from scratch: header only");
        assert_eq!(std::fs::read(&base).unwrap(), data, "the base does not change");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
