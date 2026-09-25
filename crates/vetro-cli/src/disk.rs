//! Dischi virtio-blk da file per `vetro boot` (runner nativo).
//!
//! Il file si legge soltanto: le scritture del guest finiscono in un
//! [`CowBackend`] in memoria (come `snapshot=on` di QEMU), quindi l'immagine
//! non cambia mai e due avvii partono dallo stesso disco.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

use vetro_platform::virtio::{BlockBackend, BlockError, CowBackend};

/// Immagine su file, in sola lettura. La dimensione è arrotondata per difetto
/// a 512 byte (come QEMU per i dischi raw).
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

/// Disco scrivibile dal guest sopra un file che resta intatto.
pub fn cow_disk(path: &Path) -> std::io::Result<CowBackend<FileBackend>> {
    FileBackend::open(path).map(CowBackend::new)
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
        // 1536 + 100 byte: la coda sotto i 512 non fa parte del disco.
        let mut f = data.clone();
        f.extend_from_slice(&[0xee; 100]);
        std::fs::write(&path, &f).unwrap();

        let mut d = cow_disk(&path).unwrap();
        assert_eq!(d.size(), 1536);
        assert!(!d.read_only(), "il copy-on-write accetta le scritture");
        let mut buf = vec![0; 1024];
        d.read_sectors(1, &mut buf).unwrap();
        assert_eq!(buf, data[512..]);
        d.write_sectors(2, &[7; 512]).unwrap();
        d.read_sectors(1, &mut buf).unwrap();
        assert_eq!(&buf[..512], &data[512..1024]);
        assert_eq!(&buf[512..], &[7; 512][..]);
        assert_eq!(d.read_sectors(3, &mut [0; 512]), Err(BlockError::OutOfRange));
        assert_eq!(std::fs::read(&path).unwrap(), f, "il file non cambia");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
