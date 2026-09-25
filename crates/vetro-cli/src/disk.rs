//! Dischi virtio-blk da file per `vetro boot` (runner nativo).
//!
//! Il file si legge soltanto: le scritture del guest finiscono in un
//! [`CowBackend`] in memoria (come `snapshot=on` di QEMU), quindi l'immagine
//! non cambia mai e due avvii partono dallo stesso disco.
//!
//! Con `--overlay=FILE` (M6, ADR 0017) le scritture del guest si conservano
//! in FILE, nello stesso formato dell'overlay del browser
//! (`vetro_snapshot::overlay`): all'avvio successivo si riapplicano. FILE
//! ricorda l'immagine base ([`base_identity`]): con un'altra base si scarta.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;

use vetro_machine::vetro_snapshot::overlay::{LoadError, Overlay, Patches};
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

/// Identità dell'immagine base per l'overlay: nome del file, dimensione e
/// data di modifica (in ns). Cambia se l'immagine si sostituisce o si
/// modifica; non cambia se si sposta in un'altra cartella.
pub fn base_identity(path: &Path) -> std::io::Result<Vec<u8>> {
    let meta = std::fs::metadata(path)?;
    let mtime = meta.modified()?.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(format!("file:{name}|{}|{mtime}", meta.len()).into_bytes())
}

/// Overlay persistente di un disco su file.
pub struct FileOverlay {
    file: File,
    overlay: Overlay,
    /// Dopo un ripristino si confrontano tutti i cluster, non solo gli scritti.
    full_sync: bool,
}

impl FileOverlay {
    /// Apre (o crea) l'overlay `path` per il disco `cow` con base
    /// `identity` e ne carica i cluster nel copy-on-write. Il secondo valore
    /// dice perché un file esistente si è scartato (altra base, illeggibile).
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

    /// Scrive nel file i cluster scritti dal guest dall'ultima volta (tutti
    /// quelli diversi dal file dopo [`after_restore`](Self::after_restore)).
    /// Restituisce se ha scritto qualcosa.
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

    /// I cluster in memoria vengono da uno snapshot: al prossimo
    /// [`persist`](Self::persist) il file si riallinea a tutti.
    pub fn after_restore(&mut self) {
        self.full_sync = true;
    }

    pub fn overlay(&self) -> &Overlay {
        &self.overlay
    }
}

/// Applica le scritture in ordine (l'intestazione per ultima, dopo che i
/// dati sono sul disco).
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

    /// `--overlay`: le scritture tornano alla riapertura; con un'altra base
    /// l'overlay si scarta e il file si riscrive; dopo un ripristino il file
    /// si riallinea.
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
        assert!(!o.persist(&mut d).unwrap(), "niente di nuovo");
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
        assert_eq!(buf, [0xaa; 512], "riallineato allo snapshot");

        let mut d4 = cow_disk(&base).unwrap();
        let (mut o4, discarded) = FileOverlay::open(&ov, b"file:altro|32768|1", &mut d4).unwrap();
        assert!(discarded.unwrap().to_string().contains("altra immagine base"));
        d4.read_sectors(3, &mut buf).unwrap();
        assert_eq!(buf, data[3 * 512..4 * 512]);
        assert!(o4.persist(&mut d4).unwrap());
        assert_eq!(std::fs::metadata(&ov).unwrap().len(), 4096, "riscritto da capo: solo l'intestazione");
        assert_eq!(std::fs::read(&base).unwrap(), data, "la base non cambia");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
