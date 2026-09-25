//! Immagini di avvio Android (M5): `boot.img` (header v0–v4),
//! `vendor_boot.img` (v3 e v4, con la tabella dei ramdisk e la sezione
//! bootconfig) e `init_boot.img`, trasformate in quello che il caricatore di
//! Linux ([`crate::boot`], [`crate::Machine::load_linux`]) si aspetta: un
//! `Image` non compresso, un initrd e una riga di comando. È il lavoro del
//! bootloader (u-boot di Cuttlefish, ABL): specifica in
//! `docs/specs/android-boot.md`, decisioni in ADR 0018.
//!
//! Modulo puro: niente file, niente memoria del guest. Formati di
//! riferimento: `system/tools/mkbootimg` di AOSP (`bootimg.h`,
//! `mkbootimg.py`, copia in `tools/mkbootimg/`) e source.android.com
//! (Boot image header, Vendor boot partitions, Implement bootconfig).
//!
//! Il bootloader di Vetro:
//! - decomprime il kernel se è gzip o LZ4 ([`decompress`]);
//! - mette in fila i ramdisk del vendor (v4: quelli della tabella, in ordine,
//!   senza quelli di tipo recovery salvo avvio in recovery) e subito dopo,
//!   senza allineamento, il ramdisk generico (da `init_boot` se c'è,
//!   altrimenti da `boot`): il kernel apre gli archivi concatenati uno dopo
//!   l'altro e il generico si sovrappone al vendor;
//! - con `vendor_boot` v4 sposta i parametri `androidboot.*` del bootloader
//!   ([`BootOptions::params`]) nel bootconfig, dopo la sezione bootconfig
//!   del vendor, e chiude l'initrd col blocco bootconfig ([`bootconfig`]);
//!   aggiunge `bootconfig` alla riga di comando se manca;
//! - riga di comando: `boot`, poi `vendor_boot`, poi i parametri del
//!   bootloader rimasti.
//!
//! Il resto delle immagini non serve alla macchina virt e si ignora: gli
//! indirizzi di caricamento (il layout è quello di QEMU, [`crate::boot`]),
//! il DTB del vendor e di `boot` v2 (Vetro genera il suo), `second`,
//! `recovery_dtbo` e la firma GKI.

pub mod bootconfig;
pub mod decompress;

use crate::boot::BootError;
use decompress::{DecompressError, Format};
use std::fmt;

pub const BOOT_MAGIC: &[u8; 8] = b"ANDROID!";
pub const VENDOR_BOOT_MAGIC: &[u8; 8] = b"VNDRBOOT";
/// Pagina fissa di `boot.img` e `init_boot.img` dalla versione 3.
pub const BOOT_V3_PAGE_SIZE: u32 = 4096;

const BOOT_V0_HEADER: usize = 1632;
const BOOT_V1_HEADER: usize = 1648;
const BOOT_V2_HEADER: usize = 1660;
const BOOT_V3_HEADER: usize = 1580;
const BOOT_V4_HEADER: usize = 1584;
const VENDOR_V3_HEADER: usize = 2112;
const VENDOR_V4_HEADER: usize = 2128;
/// Dimensione minima di una voce della tabella dei ramdisk (v4).
const RAMDISK_ENTRY_V4: usize = 108;

/// Quale immagine: per i messaggi d'errore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    Boot,
    VendorBoot,
    InitBoot,
}

impl fmt::Display for Which {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Which::Boot => "boot.img",
            Which::VendorBoot => "vendor_boot.img",
            Which::InitBoot => "init_boot.img",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AndroidError {
    /// Più corta dell'header.
    Truncated(Which),
    BadMagic(Which),
    UnsupportedVersion(Which, u32),
    /// Pagina non potenza di due fra 2 e 16 KiB (i valori di `mkbootimg`).
    BadPageSize(Which, u32),
    /// Una sezione dichiarata esce dall'immagine.
    OutOfBounds(Which, &'static str),
    /// Tabella dei ramdisk del vendor incoerente.
    BadRamdiskTable(&'static str),
    /// `init_boot.img` con un kernel o con header prima della v4.
    BadInitBoot(&'static str),
    /// `vendor_boot` insieme a un `boot.img` v0–v2, o `init_boot` senza v4.
    Mismatch(&'static str),
    /// `boot.img` senza kernel.
    NoKernel,
    /// Kernel compresso in un formato che non si sa aprire, o dati rotti.
    Kernel(Format, DecompressError),
    Bootconfig(String),
    /// Il kernel non si carica (header dell'`Image`, RAM).
    Load(BootError),
}

impl fmt::Display for AndroidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AndroidError::Truncated(w) => write!(f, "{w}: più corta dell'header"),
            AndroidError::BadMagic(w) => write!(f, "{w}: magic sbagliato"),
            AndroidError::UnsupportedVersion(w, v) => write!(f, "{w}: header versione {v} non supportata"),
            AndroidError::BadPageSize(w, p) => write!(f, "{w}: dimensione di pagina {p} non valida"),
            AndroidError::OutOfBounds(w, s) => write!(f, "{w}: la sezione {s} esce dall'immagine"),
            AndroidError::BadRamdiskTable(m) => write!(f, "vendor_boot.img: tabella dei ramdisk: {m}"),
            AndroidError::BadInitBoot(m) => write!(f, "init_boot.img: {m}"),
            AndroidError::Mismatch(m) => write!(f, "immagini incompatibili: {m}"),
            AndroidError::NoKernel => write!(f, "boot.img: nessun kernel"),
            AndroidError::Kernel(fmt_, e) => write!(f, "kernel ({fmt_}): {e}"),
            AndroidError::Bootconfig(m) => write!(f, "bootconfig: {m}"),
            AndroidError::Load(e) => write!(f, "kernel: {e}"),
        }
    }
}

impl std::error::Error for AndroidError {}

impl From<BootError> for AndroidError {
    fn from(e: BootError) -> Self {
        AndroidError::Load(e)
    }
}

fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

fn le64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
}

/// Stringa terminata da NUL (o dal campo pieno).
fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

fn round_up(n: u64, page: u64) -> u64 {
    n.div_ceil(page) * page
}

/// Sezione `[off, off + len)` dell'immagine; `off` avanza alla pagina dopo.
fn take<'a>(
    img: &'a [u8],
    off: &mut u64,
    len: u32,
    page: u32,
    w: Which,
    what: &'static str,
) -> Result<&'a [u8], AndroidError> {
    let start = *off;
    let end = start + len as u64;
    if end > img.len() as u64 {
        return Err(AndroidError::OutOfBounds(w, what));
    }
    *off = round_up(end, page as u64);
    Ok(&img[start as usize..end as usize])
}

fn check_page(w: Which, page: u32) -> Result<(), AndroidError> {
    if page.is_power_of_two() && (2048..=16384).contains(&page) {
        Ok(())
    } else {
        Err(AndroidError::BadPageSize(w, page))
    }
}

/// Versione di Android e livello delle patch di sicurezza (campo
/// `os_version`: `a<<25 | b<<18 | c<<11 | (anno-2000)<<4 | mese`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OsVersion {
    pub major: u8,
    pub minor: u8,
    pub patch: u8,
    /// 0 se non indicato.
    pub year: u16,
    pub month: u8,
}

impl OsVersion {
    pub fn from_raw(v: u32) -> Self {
        let ver = v >> 11;
        let lvl = v & 0x7ff;
        OsVersion {
            major: (ver >> 14) as u8 & 0x7f,
            minor: (ver >> 7) as u8 & 0x7f,
            patch: ver as u8 & 0x7f,
            year: if lvl == 0 { 0 } else { 2000 + (lvl >> 4) as u16 },
            month: (lvl & 0xf) as u8,
        }
    }
}

/// `boot.img` o `init_boot.img` letta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootImage<'a> {
    pub header_version: u32,
    /// 4096 dalla v3; dall'header prima.
    pub page_size: u32,
    pub kernel: &'a [u8],
    pub ramdisk: &'a [u8],
    /// v0–v2 (ignorato).
    pub second: &'a [u8],
    /// v1–v2 (ignorato).
    pub recovery_dtbo: &'a [u8],
    /// v2 (ignorato: Vetro genera il suo DTB).
    pub dtb: &'a [u8],
    /// Riga di comando; v0–v2: `cmdline` seguita da `extra_cmdline`, senza
    /// separatore (mkbootimg spezza una riga lunga a 511 byte).
    pub cmdline: String,
    /// Nome del prodotto (v0–v2).
    pub name: String,
    pub os_version: OsVersion,
    /// Firma GKI della v4 (ignorata).
    pub signature: &'a [u8],
}

impl<'a> BootImage<'a> {
    pub fn parse(img: &'a [u8]) -> Result<Self, AndroidError> {
        Self::parse_as(img, Which::Boot)
    }

    /// `init_boot.img`: header v4 senza kernel, solo il ramdisk generico.
    pub fn parse_init_boot(img: &'a [u8]) -> Result<Self, AndroidError> {
        let b = Self::parse_as(img, Which::InitBoot)?;
        if b.header_version < 4 {
            return Err(AndroidError::BadInitBoot("header prima della versione 4"));
        }
        if !b.kernel.is_empty() {
            return Err(AndroidError::BadInitBoot("contiene un kernel"));
        }
        Ok(b)
    }

    fn parse_as(img: &'a [u8], w: Which) -> Result<Self, AndroidError> {
        if img.len() < 44 {
            return Err(AndroidError::Truncated(w));
        }
        if &img[..8] != BOOT_MAGIC {
            return Err(AndroidError::BadMagic(w));
        }
        let version = le32(img, 40);
        match version {
            0..=2 => Self::parse_v0(img, version, w),
            3 | 4 => Self::parse_v3(img, version, w),
            v => Err(AndroidError::UnsupportedVersion(w, v)),
        }
    }

    fn parse_v0(img: &'a [u8], version: u32, w: Which) -> Result<Self, AndroidError> {
        let hdr = [BOOT_V0_HEADER, BOOT_V1_HEADER, BOOT_V2_HEADER][version as usize];
        if img.len() < hdr {
            return Err(AndroidError::Truncated(w));
        }
        let page = le32(img, 36);
        check_page(w, page)?;
        let mut off = round_up(hdr as u64, page as u64);
        let kernel = take(img, &mut off, le32(img, 8), page, w, "kernel")?;
        let ramdisk = take(img, &mut off, le32(img, 16), page, w, "ramdisk")?;
        let second = take(img, &mut off, le32(img, 24), page, w, "second")?;
        let mut recovery_dtbo: &[u8] = &[];
        let mut dtb: &[u8] = &[];
        if version >= 1 {
            let (size, at) = (le32(img, 1632), le64(img, 1636));
            if size != 0 {
                let mut o = at;
                recovery_dtbo = take(img, &mut o, size, page, w, "recovery_dtbo")?;
                off = off.max(o);
            }
        }
        if version == 2 {
            dtb = take(img, &mut off, le32(img, 1648), page, w, "dtb")?;
        }
        let mut cmdline = cstr(&img[64..576]);
        cmdline.push_str(&cstr(&img[608..1632]));
        Ok(BootImage {
            header_version: version,
            page_size: page,
            kernel,
            ramdisk,
            second,
            recovery_dtbo,
            dtb,
            cmdline,
            name: cstr(&img[48..64]),
            os_version: OsVersion::from_raw(le32(img, 44)),
            signature: &[],
        })
    }

    fn parse_v3(img: &'a [u8], version: u32, w: Which) -> Result<Self, AndroidError> {
        let hdr = if version == 3 { BOOT_V3_HEADER } else { BOOT_V4_HEADER };
        if img.len() < hdr {
            return Err(AndroidError::Truncated(w));
        }
        let page = BOOT_V3_PAGE_SIZE;
        let mut off = page as u64;
        let kernel = take(img, &mut off, le32(img, 8), page, w, "kernel")?;
        let ramdisk = take(img, &mut off, le32(img, 12), page, w, "ramdisk")?;
        let signature =
            if version == 4 { take(img, &mut off, le32(img, 1580), page, w, "boot_signature")? } else { &[] };
        Ok(BootImage {
            header_version: version,
            page_size: page,
            kernel,
            ramdisk,
            second: &[],
            recovery_dtbo: &[],
            dtb: &[],
            cmdline: cstr(&img[44..1580]),
            name: String::new(),
            os_version: OsVersion::from_raw(le32(img, 16)),
            signature,
        })
    }
}

/// Tipo di un ramdisk del vendor (`VENDOR_RAMDISK_TYPE_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RamdiskType {
    None,
    /// Da caricare sempre.
    Platform,
    /// Solo per l'avvio in recovery.
    Recovery,
    /// Moduli del kernel.
    Dlkm,
    Other(u32),
}

impl RamdiskType {
    pub fn from_raw(v: u32) -> Self {
        match v {
            0 => RamdiskType::None,
            1 => RamdiskType::Platform,
            2 => RamdiskType::Recovery,
            3 => RamdiskType::Dlkm,
            v => RamdiskType::Other(v),
        }
    }
}

/// Un ramdisk della tabella del vendor (v3: uno solo, tutta la sezione).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorRamdisk<'a> {
    pub kind: RamdiskType,
    pub name: String,
    pub board_id: [u32; 16],
    pub data: &'a [u8],
}

/// `vendor_boot.img` letta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorBoot<'a> {
    pub header_version: u32,
    pub page_size: u32,
    pub cmdline: String,
    /// Nome del prodotto.
    pub name: String,
    /// Tutta la sezione dei ramdisk.
    pub ramdisk_section: &'a [u8],
    pub ramdisks: Vec<VendorRamdisk<'a>>,
    /// Ignorato: Vetro genera il suo DTB.
    pub dtb: &'a [u8],
    /// Sezione bootconfig (v4), testo `chiave=valore` una riga per parametro.
    pub bootconfig: &'a [u8],
}

impl<'a> VendorBoot<'a> {
    pub fn parse(img: &'a [u8]) -> Result<Self, AndroidError> {
        let w = Which::VendorBoot;
        if img.len() < 12 {
            return Err(AndroidError::Truncated(w));
        }
        if &img[..8] != VENDOR_BOOT_MAGIC {
            return Err(AndroidError::BadMagic(w));
        }
        let version = le32(img, 8);
        let hdr = match version {
            3 => VENDOR_V3_HEADER,
            4 => VENDOR_V4_HEADER,
            v => return Err(AndroidError::UnsupportedVersion(w, v)),
        };
        if img.len() < hdr {
            return Err(AndroidError::Truncated(w));
        }
        let page = le32(img, 12);
        check_page(w, page)?;
        let mut off = round_up(hdr as u64, page as u64);
        let ramdisk_section = take(img, &mut off, le32(img, 24), page, w, "vendor ramdisk")?;
        let dtb = take(img, &mut off, le32(img, 2100), page, w, "dtb")?;
        let mut ramdisks = Vec::new();
        let mut bootconfig: &[u8] = &[];
        if version == 3 {
            ramdisks.push(VendorRamdisk {
                kind: RamdiskType::Platform,
                name: String::new(),
                board_id: [0; 16],
                data: ramdisk_section,
            });
        } else {
            let (table_size, num, entry_size) = (le32(img, 2112), le32(img, 2116), le32(img, 2120) as usize);
            let table = take(img, &mut off, table_size, page, w, "tabella dei ramdisk")?;
            bootconfig = take(img, &mut off, le32(img, 2124), page, w, "bootconfig")?;
            if num > 0 && entry_size < RAMDISK_ENTRY_V4 {
                return Err(AndroidError::BadRamdiskTable("voci più corte di 108 byte"));
            }
            if (num as usize).checked_mul(entry_size).is_none_or(|n| n > table.len()) {
                return Err(AndroidError::BadRamdiskTable("più voci di quante ne stiano nella tabella"));
            }
            for e in table.chunks_exact(entry_size.max(1)).take(num as usize) {
                let (size, at) = (le32(e, 0) as usize, le32(e, 4) as usize);
                let data = at
                    .checked_add(size)
                    .and_then(|end| ramdisk_section.get(at..end))
                    .ok_or(AndroidError::BadRamdiskTable("ramdisk fuori dalla sezione"))?;
                let mut board_id = [0u32; 16];
                for (i, b) in board_id.iter_mut().enumerate() {
                    *b = le32(e, 44 + 4 * i);
                }
                ramdisks.push(VendorRamdisk {
                    kind: RamdiskType::from_raw(le32(e, 8)),
                    name: cstr(&e[12..44]),
                    board_id,
                    data,
                });
            }
        }
        Ok(VendorBoot {
            header_version: version,
            page_size: page,
            cmdline: cstr(&img[28..2076]),
            name: cstr(&img[2080..2096]),
            ramdisk_section,
            ramdisks,
            dtb,
            bootconfig,
        })
    }
}

/// Scelte del bootloader.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BootOptions {
    /// Parametri aggiunti dal bootloader (sintassi della riga di comando):
    /// gli `androidboot.*` vanno nel bootconfig se `vendor_boot` è v4, gli
    /// altri in coda alla riga di comando.
    pub params: String,
    /// Avvio in recovery: carica anche i ramdisk del vendor di tipo recovery.
    pub recovery: bool,
}

/// Quello che il bootloader passa al kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndroidBoot {
    /// `Image` arm64 non compresso.
    pub kernel: Vec<u8>,
    /// Formato del kernel nel `boot.img`.
    pub kernel_format: Format,
    /// Ramdisk concatenati e, se c'è, il blocco bootconfig in coda.
    pub initrd: Vec<u8>,
    pub cmdline: String,
    /// Testo del bootconfig (senza NUL e trailer), vuoto se non c'è.
    pub bootconfig: String,
    /// Descrizione dei ramdisk caricati, in ordine (per i messaggi).
    pub ramdisks: Vec<String>,
}

impl AndroidBoot {
    /// Legge le immagini e le combina ([`assemble`]).
    pub fn from_images(
        boot: &[u8],
        vendor_boot: Option<&[u8]>,
        init_boot: Option<&[u8]>,
        opts: &BootOptions,
    ) -> Result<Self, AndroidError> {
        let boot = BootImage::parse(boot)?;
        let vendor = vendor_boot.map(VendorBoot::parse).transpose()?;
        let init = init_boot.map(BootImage::parse_init_boot).transpose()?;
        assemble(&boot, vendor.as_ref(), init.as_ref(), opts)
    }

    /// L'initrd per il caricatore, `None` se vuoto.
    pub fn initrd(&self) -> Option<&[u8]> {
        (!self.initrd.is_empty()).then_some(&self.initrd)
    }
}

fn ramdisk_label(r: &VendorRamdisk) -> String {
    let kind = match r.kind {
        RamdiskType::None => "none".to_string(),
        RamdiskType::Platform => "platform".to_string(),
        RamdiskType::Recovery => "recovery".to_string(),
        RamdiskType::Dlkm => "dlkm".to_string(),
        RamdiskType::Other(v) => format!("tipo {v}"),
    };
    let name = if r.name.is_empty() { "vendor" } else { &r.name };
    format!("{name} ({kind}, {} byte)", r.data.len())
}

/// Combina le immagini come il bootloader (vedi la documentazione del modulo).
pub fn assemble(
    boot: &BootImage,
    vendor: Option<&VendorBoot>,
    init_boot: Option<&BootImage>,
    opts: &BootOptions,
) -> Result<AndroidBoot, AndroidError> {
    if vendor.is_some() && boot.header_version < 3 {
        return Err(AndroidError::Mismatch("vendor_boot vuole boot.img v3 o v4"));
    }
    if init_boot.is_some() && boot.header_version < 4 {
        return Err(AndroidError::Mismatch("init_boot vuole boot.img v4"));
    }
    if boot.kernel.is_empty() {
        return Err(AndroidError::NoKernel);
    }
    let format = decompress::detect(boot.kernel);
    let kernel =
        decompress::decompress(boot.kernel).map_err(|e| AndroidError::Kernel(format, e))?.into_owned();

    // Ramdisk: vendor in ordine di tabella, poi il generico, senza spazi.
    let mut initrd = Vec::new();
    let mut ramdisks = Vec::new();
    for r in vendor.iter().flat_map(|v| &v.ramdisks) {
        if r.kind == RamdiskType::Recovery && !opts.recovery {
            continue;
        }
        initrd.extend_from_slice(r.data);
        ramdisks.push(ramdisk_label(r));
    }
    let (generic, from) = match init_boot {
        Some(i) => (i.ramdisk, "init_boot"),
        None => (boot.ramdisk, "boot"),
    };
    if !generic.is_empty() {
        initrd.extend_from_slice(generic);
        ramdisks.push(format!("generico da {from} ({} byte)", generic.len()));
    }

    // Parametri del bootloader: androidboot.* nel bootconfig se c'è.
    let has_bootconfig = vendor.is_some_and(|v| v.header_version >= 4);
    let mut extra = Vec::new();
    let mut lines = String::new();
    let mut keys: Vec<&str> = Vec::new();
    for p in bootconfig::split_cmdline(&opts.params) {
        if has_bootconfig && p.starts_with("androidboot.") {
            let key = bootconfig::key_value(p).0;
            if keys.contains(&key) {
                return Err(AndroidError::Bootconfig(format!("{key} ripetuto nei parametri")));
            }
            keys.push(key);
            lines.push_str(&bootconfig::param_line(p).map_err(AndroidError::Bootconfig)?);
        } else {
            extra.push(p);
        }
    }
    let mut text = String::new();
    if let Some(v) = vendor {
        let section = v.bootconfig;
        let section = &section[..section.iter().rposition(|&b| b != 0).map_or(0, |p| p + 1)];
        text.push_str(&String::from_utf8_lossy(section));
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
    }
    text.push_str(&lines);

    let mut parts: Vec<&str> = Vec::new();
    for s in [boot.cmdline.as_str(), vendor.map_or("", |v| v.cmdline.as_str())] {
        let s = s.trim();
        if !s.is_empty() {
            parts.push(s);
        }
    }
    let mut cmdline = parts.join(" ");
    for p in extra {
        if !cmdline.is_empty() {
            cmdline.push(' ');
        }
        cmdline.push_str(p);
    }
    if !text.is_empty() {
        bootconfig::append(&mut initrd, text.as_bytes()).map_err(AndroidError::Bootconfig)?;
        if !bootconfig::split_cmdline(&cmdline).contains(&"bootconfig") {
            if !cmdline.is_empty() {
                cmdline.push(' ');
            }
            cmdline.push_str("bootconfig");
        }
    }
    Ok(AndroidBoot { kernel, kernel_format: format, initrd, cmdline, bootconfig: text, ramdisks })
}

#[cfg(test)]
mod tests;
