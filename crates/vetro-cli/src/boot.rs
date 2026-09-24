//! Caricatore del kernel Linux arm64 per la modalità sistema (M3).
//!
//! Modulo puro: riceve i byte dell'`Image` e le dimensioni di initramfs e
//! device tree, restituisce dove va ogni cosa in RAM fisica e lo stato dei
//! registri all'ingresso del kernel. Non tocca memoria del guest né file
//! dell'host: chi esegue la CPU in modalità sistema copia i segmenti e imposta
//! i registri.
//!
//! Il layout copia quello di QEMU (`hw/arm/boot.c`) per la macchina virt,
//! così un avvio sotto Vetro e uno sotto l'oracolo partono dagli stessi
//! indirizzi:
//! - RAM da `0x4000_0000`;
//! - kernel a `base + text_offset`, spostato di 2 MiB se cadrebbe nei primi
//!   4 KiB (QEMU li riserva al suo stub di avvio): con i kernel moderni,
//!   `text_offset = 0`, l'ingresso è `0x4020_0000`;
//! - initramfs a `base + min(ram/2, 128 MiB)`, e comunque dopo la fine del
//!   kernel;
//! - DTB subito dopo l'initramfs, allineato a 2 MiB (il kernel mappa il DTB a
//!   blocchi di 2 MiB).
//!
//! Riferimento del protocollo: `Documentation/arch/arm64/booting.rst`.

use std::fmt;

/// Dimensione dell'header dell'`Image` arm64.
pub const HEADER_LEN: usize = 64;
/// `"ARM\x64"` letto come u32 little-endian all'offset 56.
pub const MAGIC: u32 = 0x644d_5241;
/// Inizio della RAM sulla macchina virt.
pub const VIRT_RAM_BASE: u64 = 0x4000_0000;
/// Dimensione massima del DTB ammessa dal protocollo di avvio.
pub const MAX_DTB: u64 = 2 * MIB;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
/// `text_offset` implicito dei kernel senza `image_size` (prima di 3.17).
const LEGACY_TEXT_OFFSET: u64 = 0x8_0000;
/// Spazio che QEMU riserva in fondo alla RAM al suo stub di avvio.
const BOOTLOADER_MAX: u64 = 4 * KIB;
const PAGE: u64 = 4 * KIB;

/// Granulo di pagina dichiarato dal kernel (flags, bit 1-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageSize {
    Unspecified,
    K4,
    K16,
    K64,
}

/// Header dell'`Image` arm64, già validato.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageHeader {
    /// Offset dell'immagine da una base allineata a 2 MiB.
    pub text_offset: u64,
    /// Memoria occupata dal kernel, bss compresa (0 nei kernel < 3.17).
    pub image_size: u64,
    /// Campo flags grezzo.
    pub flags: u64,
    pub page_size: PageSize,
    /// Flags bit 3: il kernel può stare ovunque in RAM (altrimenti il più
    /// vicino possibile all'inizio della RAM).
    pub phys_anywhere: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootError {
    /// Meno di 64 byte.
    Truncated,
    /// Manca il magic `ARM\x64`.
    BadMagic(u32),
    /// Kernel big-endian (flags bit 0).
    BigEndian,
    /// Granulo di 16 KiB: la Cortex-A53 non lo implementa.
    UnsupportedPageSize(PageSize),
    /// Base della RAM non allineata a 2 MiB.
    MisalignedRam(u64),
    /// DTB più grande di 2 MiB.
    DtbTooLarge(u64),
    /// Il pezzo indicato non sta nella RAM.
    DoesNotFit(&'static str),
}

impl fmt::Display for BootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BootError::Truncated => write!(f, "Image troncata: meno di {HEADER_LEN} byte"),
            BootError::BadMagic(m) => write!(f, "non è un Image arm64 (magic {m:#010x})"),
            BootError::BigEndian => write!(f, "kernel big-endian non supportato"),
            BootError::UnsupportedPageSize(p) => write!(f, "granulo di pagina non supportato: {p:?}"),
            BootError::MisalignedRam(b) => write!(f, "base della RAM {b:#x} non allineata a 2 MiB"),
            BootError::DtbTooLarge(n) => write!(f, "DTB di {n} byte, oltre i 2 MiB"),
            BootError::DoesNotFit(what) => write!(f, "{what} non sta nella RAM"),
        }
    }
}

impl std::error::Error for BootError {}

fn le64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

impl ImageHeader {
    pub fn parse(image: &[u8]) -> Result<Self, BootError> {
        if image.len() < HEADER_LEN {
            return Err(BootError::Truncated);
        }
        let magic = u32::from_le_bytes(image[56..60].try_into().unwrap());
        if magic != MAGIC {
            return Err(BootError::BadMagic(magic));
        }
        let mut text_offset = le64(image, 8);
        let image_size = le64(image, 16);
        let flags = le64(image, 24);
        if image_size == 0 {
            // Kernel < 3.17: text_offset non è affidabile, vale 0x80000.
            text_offset = LEGACY_TEXT_OFFSET;
        }
        if flags & 1 != 0 {
            return Err(BootError::BigEndian);
        }
        let page_size = match (flags >> 1) & 3 {
            0 => PageSize::Unspecified,
            1 => PageSize::K4,
            2 => PageSize::K16,
            _ => PageSize::K64,
        };
        if page_size == PageSize::K16 {
            return Err(BootError::UnsupportedPageSize(page_size));
        }
        Ok(ImageHeader { text_offset, image_size, flags, page_size, phys_anywhere: flags & 8 != 0 })
    }
}

/// RAM fisica del guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RamConfig {
    pub base: u64,
    pub size: u64,
}

impl RamConfig {
    /// La virt di QEMU con `size` byte di RAM (`-m`).
    pub fn virt(size: u64) -> Self {
        RamConfig { base: VIRT_RAM_BASE, size }
    }

    fn end(&self) -> u64 {
        self.base + self.size
    }
}

/// Intervallo di memoria fisica `[addr, addr + len)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub addr: u64,
    pub len: u64,
}

impl Region {
    pub fn end(&self) -> u64 {
        self.addr + self.len
    }
}

/// Stato della CPU all'ingresso del kernel (booting.rst, "CPU mode").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryState {
    /// PC iniziale: primo byte dell'`Image` caricata.
    pub pc: u64,
    /// x0 = indirizzo fisico del DTB; x1, x2, x3 = 0.
    pub x: [u64; 4],
    /// Livello di eccezione: 1 (la virt senza EL2/EL3, come QEMU di default).
    pub el: u8,
    /// PSTATE: EL1h con D, A, I, F mascherati (`0x3c5`).
    pub pstate: u32,
    /// MMU (e cache dati) spenta: SCTLR_EL1.M = 0.
    pub mmu_on: bool,
}

/// PSTATE all'ingresso: M = EL1h (0b0101), DAIF = 0b1111.
pub const ENTRY_PSTATE: u32 = 0x3c5;

/// Dove va ogni pezzo in RAM e come parte la CPU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootPlan {
    pub header: ImageHeader,
    /// Byte dell'`Image` da copiare (`len` = lunghezza del file).
    pub kernel: Region,
    /// Memoria che il kernel userà all'avvio, bss compresa: deve restare
    /// libera (`len` = max(image_size, file)).
    pub kernel_footprint: Region,
    /// Initramfs, se presente: va anche in `/chosen` come
    /// `linux,initrd-start` / `linux,initrd-end`.
    pub initrd: Option<Region>,
    pub dtb: Region,
    pub entry: EntryState,
}

fn align_up(v: u64, a: u64) -> u64 {
    v.div_ceil(a) * a
}

/// Calcola il layout di avvio. `initrd_len` è `None` senza initramfs;
/// `dtb_len` è la dimensione del DTB (lo genera la piattaforma dopo aver
/// letto `initrd` da questo piano: la posizione dell'initramfs non dipende
/// dal DTB).
pub fn plan(
    ram: RamConfig,
    image: &[u8],
    initrd_len: Option<u64>,
    dtb_len: u64,
) -> Result<BootPlan, BootError> {
    let header = ImageHeader::parse(image)?;
    if !ram.base.is_multiple_of(2 * MIB) {
        return Err(BootError::MisalignedRam(ram.base));
    }
    if dtb_len > MAX_DTB {
        return Err(BootError::DtbTooLarge(dtb_len));
    }
    // Kernel: come arm_load_aarch64_image di QEMU.
    let mut offset = header.text_offset;
    if offset < BOOTLOADER_MAX {
        offset += 2 * MIB;
    }
    let file_len = image.len() as u64;
    let kernel = Region { addr: ram.base + offset, len: file_len };
    let kernel_footprint = Region { addr: kernel.addr, len: header.image_size.max(file_len) };
    if kernel_footprint.end() > ram.end() {
        return Err(BootError::DoesNotFit("kernel"));
    }
    // Initramfs: come arm_setup_direct_kernel_boot di QEMU, ma rispettando
    // anche la bss del kernel.
    let initrd_start = align_up((ram.base + (ram.size / 2).min(128 * MIB)).max(kernel_footprint.end()), PAGE);
    let initrd = initrd_len.map(|len| Region { addr: initrd_start, len });
    if initrd.is_some_and(|r| r.end() > ram.end()) {
        return Err(BootError::DoesNotFit("initramfs"));
    }
    // DTB: dopo l'initramfs, allineato a 2 MiB. Senza initramfs QEMU usa
    // lo stesso punto di partenza con dimensione zero.
    let dtb = Region { addr: align_up(initrd_start + initrd_len.unwrap_or(0), 2 * MIB), len: dtb_len };
    if dtb.end() > ram.end() {
        return Err(BootError::DoesNotFit("DTB"));
    }
    let entry =
        EntryState { pc: kernel.addr, x: [dtb.addr, 0, 0, 0], el: 1, pstate: ENTRY_PSTATE, mmu_on: false };
    Ok(BootPlan { header, kernel, kernel_footprint, initrd, dtb, entry })
}

impl BootPlan {
    /// Coppie (indirizzo fisico, byte) da copiare in RAM, in ordine di
    /// indirizzo. `initrd` e `dtb` devono avere le lunghezze date a [`plan`].
    pub fn segments<'a>(
        &self,
        image: &'a [u8],
        initrd: Option<&'a [u8]>,
        dtb: &'a [u8],
    ) -> Vec<(u64, &'a [u8])> {
        assert_eq!(image.len() as u64, self.kernel.len, "Image diversa da quella del piano");
        assert_eq!(initrd.map(|b| b.len() as u64), self.initrd.map(|r| r.len), "initramfs diverso dal piano");
        assert_eq!(dtb.len() as u64, self.dtb.len, "DTB diverso dal piano");
        let mut v = vec![(self.kernel.addr, image)];
        if let (Some(r), Some(b)) = (self.initrd, initrd) {
            v.push((r.addr, b));
        }
        v.push((self.dtb.addr, dtb));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Image finta: header arm64 con i campi dati, poi `len - 64` byte di
    /// "codice".
    fn fake_image(text_offset: u64, image_size: u64, flags: u64, len: usize) -> Vec<u8> {
        let mut v = vec![0u8; len];
        // code0: b +64 (salta l'header), come i kernel senza EFI stub.
        v[0..4].copy_from_slice(&0x1400_0010u32.to_le_bytes());
        v[8..16].copy_from_slice(&text_offset.to_le_bytes());
        v[16..24].copy_from_slice(&image_size.to_le_bytes());
        v[24..32].copy_from_slice(&flags.to_le_bytes());
        v[56..60].copy_from_slice(b"ARM\x64");
        v
    }

    const GIB: u64 = 1024 * MIB;

    #[test]
    fn boot_parse_header() {
        let img = fake_image(0, 0x2a0_0000, 0b1010, 4096);
        let h = ImageHeader::parse(&img).unwrap();
        assert_eq!(h.text_offset, 0);
        assert_eq!(h.image_size, 0x2a0_0000);
        assert_eq!(h.flags, 0b1010);
        assert_eq!(h.page_size, PageSize::K4);
        assert!(h.phys_anywhere);
    }

    #[test]
    fn boot_magic_constant_is_arm_x64() {
        assert_eq!(&MAGIC.to_le_bytes(), b"ARM\x64");
    }

    #[test]
    fn boot_rejects_bad_images() {
        assert_eq!(ImageHeader::parse(&[0; 63]), Err(BootError::Truncated));
        let mut img = fake_image(0, 0x10000, 0, 128);
        img[56] = b'X';
        assert!(matches!(ImageHeader::parse(&img), Err(BootError::BadMagic(_))));
        let img = fake_image(0, 0x10000, 1, 128);
        assert_eq!(ImageHeader::parse(&img), Err(BootError::BigEndian));
        let img = fake_image(0, 0x10000, 2 << 1, 128);
        assert_eq!(ImageHeader::parse(&img), Err(BootError::UnsupportedPageSize(PageSize::K16)));
        let img = fake_image(0, 0x10000, 3 << 1, 128);
        assert_eq!(ImageHeader::parse(&img).unwrap().page_size, PageSize::K64);
    }

    #[test]
    fn boot_legacy_kernel_without_image_size() {
        // image_size = 0: text_offset ignorato, vale 0x80000.
        let img = fake_image(0x1234_5000, 0, 0, 4096);
        let h = ImageHeader::parse(&img).unwrap();
        assert_eq!(h.text_offset, 0x8_0000);
        let p = plan(RamConfig::virt(GIB), &img, None, 0x1000).unwrap();
        assert_eq!(p.entry.pc, VIRT_RAM_BASE + 0x8_0000);
        assert_eq!(p.kernel_footprint.len, 4096);
    }

    /// Layout di QEMU per `-M virt -m 1G -kernel Image -initrd ...`.
    #[test]
    fn boot_layout_like_qemu_virt() {
        let img = fake_image(0, 0x2a0_0000, 0b1010, 0x1f0_0000);
        let initrd_len = 0x12_3456;
        let p = plan(RamConfig::virt(GIB), &img, Some(initrd_len), 0x10_0000).unwrap();
        assert_eq!(p.kernel, Region { addr: 0x4020_0000, len: 0x1f0_0000 });
        assert_eq!(p.kernel_footprint, Region { addr: 0x4020_0000, len: 0x2a0_0000 });
        // min(1 GiB / 2, 128 MiB) = 128 MiB sopra la base.
        assert_eq!(p.initrd, Some(Region { addr: 0x4800_0000, len: initrd_len }));
        assert_eq!(p.dtb, Region { addr: 0x4820_0000, len: 0x10_0000 });
        assert_eq!(
            p.entry,
            EntryState { pc: 0x4020_0000, x: [0x4820_0000, 0, 0, 0], el: 1, pstate: 0x3c5, mmu_on: false }
        );
    }

    #[test]
    fn boot_text_offset_above_stub_is_kept() {
        let img = fake_image(0x8_0000, 0x100_0000, 0, 4096);
        let p = plan(RamConfig::virt(GIB), &img, None, 64).unwrap();
        assert_eq!(p.entry.pc, 0x4008_0000);
        // Senza initramfs il DTB parte dallo stesso punto.
        assert_eq!(p.initrd, None);
        assert_eq!(p.dtb.addr, 0x4800_0000);
        assert_eq!(p.entry.x[0], p.dtb.addr);
    }

    #[test]
    fn boot_small_ram_puts_initrd_after_kernel() {
        // 64 MiB: ram/2 = 32 MiB cadrebbe dentro la bss del kernel (fino a
        // base + 2 MiB + 40 MiB), quindi l'initramfs va dopo il kernel.
        let img = fake_image(0, 40 * MIB, 0, 4096);
        let p = plan(RamConfig::virt(64 * MIB), &img, Some(5000), 100).unwrap();
        let initrd = p.initrd.unwrap();
        assert_eq!(initrd.addr, VIRT_RAM_BASE + 42 * MIB);
        assert_eq!(p.dtb.addr, VIRT_RAM_BASE + 44 * MIB);
        assert!(p.dtb.addr.is_multiple_of(2 * MIB));
    }

    #[test]
    fn boot_rejects_what_does_not_fit() {
        let img = fake_image(0, 0x10_0000, 0, 4096);
        let ram = RamConfig::virt(16 * MIB);
        assert_eq!(plan(ram, &img, Some(32 * MIB), 100), Err(BootError::DoesNotFit("initramfs")));
        assert_eq!(plan(ram, &img, Some(7 * MIB), 100), Err(BootError::DoesNotFit("DTB")));
        assert_eq!(plan(ram, &img, None, 3 * MIB), Err(BootError::DtbTooLarge(3 * MIB)));
        let big = fake_image(0, 15 * MIB, 0, 4096);
        assert_eq!(plan(ram, &big, None, 100), Err(BootError::DoesNotFit("kernel")));
        let odd = RamConfig { base: 0x4010_0000, size: GIB };
        assert_eq!(plan(odd, &img, None, 100), Err(BootError::MisalignedRam(0x4010_0000)));
    }

    #[test]
    fn boot_segments_in_address_order() {
        let img = fake_image(0, 0x10_0000, 0, 256);
        let initrd = vec![0xaa; 300];
        let dtb = vec![0xd0; 40];
        let p = plan(RamConfig::virt(GIB), &img, Some(300), 40).unwrap();
        let segs = p.segments(&img, Some(&initrd), &dtb);
        let addrs: Vec<u64> = segs.iter().map(|s| s.0).collect();
        assert_eq!(addrs, vec![0x4020_0000, 0x4800_0000, 0x4820_0000]);
        assert_eq!(segs[1].1, &initrd[..]);
    }

    /// L'Image vera, se `tools/guest-kernel/build.sh` è stato eseguito.
    #[test]
    fn boot_real_guest_kernel_header() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/guest-kernel/Image");
        let Ok(img) = std::fs::read(path) else {
            eprintln!("SKIP boot_real_guest_kernel_header: esegui tools/guest-kernel/build.sh");
            return;
        };
        let h = ImageHeader::parse(&img).unwrap();
        assert_eq!(h.text_offset, 0);
        assert!(h.image_size >= img.len() as u64);
        assert_eq!(h.page_size, PageSize::K4);
        let p = plan(RamConfig::virt(GIB), &img, Some(1 << 20), 1 << 16).unwrap();
        assert_eq!(p.entry.pc, 0x4020_0000);
    }
}
