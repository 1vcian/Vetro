//! Android boot images (M5): `boot.img` (header v0–v4),
//! `vendor_boot.img` (v3 and v4, with the ramdisk table and the
//! bootconfig section) and `init_boot.img`, turned into what the Linux
//! loader ([`crate::boot`], [`crate::Machine::load_linux`]) expects: an
//! uncompressed `Image`, an initrd and a command line. It is the bootloader's
//! job (Cuttlefish's u-boot, ABL): specification in
//! `docs/specs/android-boot.md`, decisions in ADR 0018.
//!
//! Pure module: no files, no guest memory. Reference
//! formats: AOSP's `system/tools/mkbootimg` (`bootimg.h`,
//! `mkbootimg.py`, copy in `tools/mkbootimg/`) and source.android.com
//! (Boot image header, Vendor boot partitions, Implement bootconfig).
//!
//! Vetro's bootloader:
//! - decompresses the kernel if it is gzip or LZ4 ([`decompress`]);
//! - lines up the vendor ramdisks (v4: those of the table, in order,
//!   without the recovery-type ones unless booting into recovery) and right after,
//!   with no alignment, the generic ramdisk (from `init_boot` if present,
//!   otherwise from `boot`): the kernel opens the concatenated archives one after
//!   the other and the generic one is overlaid on the vendor one;
//! - with `vendor_boot` v4 moves the bootloader's `androidboot.*` parameters
//!   ([`BootOptions::params`]) into the bootconfig, after the vendor's bootconfig
//!   section, and closes the initrd with the bootconfig block ([`bootconfig`]);
//!   adds `bootconfig` to the command line if missing;
//! - command line: `boot`, then `vendor_boot`, then the remaining bootloader
//!   parameters.
//!
//! The rest of the images is not needed by the virt machine and is ignored: the
//! load addresses (the layout is QEMU's, [`crate::boot`]),
//! the DTB of the vendor and of `boot` v2 (Vetro generates its own), `second`,
//! `recovery_dtbo` and the GKI signature.

pub mod bootconfig;
pub mod decompress;

use crate::boot::BootError;
use decompress::{DecompressError, Format};
use std::fmt;

pub const BOOT_MAGIC: &[u8; 8] = b"ANDROID!";
pub const VENDOR_BOOT_MAGIC: &[u8; 8] = b"VNDRBOOT";
/// Fixed page of `boot.img` and `init_boot.img` from version 3.
pub const BOOT_V3_PAGE_SIZE: u32 = 4096;

const BOOT_V0_HEADER: usize = 1632;
const BOOT_V1_HEADER: usize = 1648;
const BOOT_V2_HEADER: usize = 1660;
const BOOT_V3_HEADER: usize = 1580;
const BOOT_V4_HEADER: usize = 1584;
const VENDOR_V3_HEADER: usize = 2112;
const VENDOR_V4_HEADER: usize = 2128;
/// Minimum size of an entry of the ramdisk table (v4).
const RAMDISK_ENTRY_V4: usize = 108;

/// Which image: for error messages.
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
    /// Shorter than the header.
    Truncated(Which),
    BadMagic(Which),
    UnsupportedVersion(Which, u32),
    /// Page not a power of two between 2 and 16 KiB (the `mkbootimg` values).
    BadPageSize(Which, u32),
    /// A declared section goes past the end of the image.
    OutOfBounds(Which, &'static str),
    /// Inconsistent vendor ramdisk table.
    BadRamdiskTable(&'static str),
    /// `init_boot.img` with a kernel or with a header older than v4.
    BadInitBoot(&'static str),
    /// `vendor_boot` together with a v0–v2 `boot.img`, or `init_boot` without v4.
    Mismatch(&'static str),
    /// `boot.img` without a kernel.
    NoKernel,
    /// Kernel compressed in a format we can't open, or broken data.
    Kernel(Format, DecompressError),
    Bootconfig(String),
    /// The kernel doesn't load (`Image` header, RAM).
    Load(BootError),
}

impl fmt::Display for AndroidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AndroidError::Truncated(w) => write!(f, "{w}: shorter than the header"),
            AndroidError::BadMagic(w) => write!(f, "{w}: wrong magic"),
            AndroidError::UnsupportedVersion(w, v) => write!(f, "{w}: header version {v} not supported"),
            AndroidError::BadPageSize(w, p) => write!(f, "{w}: invalid page size {p}"),
            AndroidError::OutOfBounds(w, s) => write!(f, "{w}: section {s} goes past the end of the image"),
            AndroidError::BadRamdiskTable(m) => write!(f, "vendor_boot.img: ramdisk table: {m}"),
            AndroidError::BadInitBoot(m) => write!(f, "init_boot.img: {m}"),
            AndroidError::Mismatch(m) => write!(f, "incompatible images: {m}"),
            AndroidError::NoKernel => write!(f, "boot.img: no kernel"),
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

/// NUL-terminated string (or terminated by the full field).
fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

fn round_up(n: u64, page: u64) -> u64 {
    n.div_ceil(page) * page
}

/// Section `[off, off + len)` of the image; `off` advances to the page after.
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

/// Android version and security patch level (field
/// `os_version`: `a<<25 | b<<18 | c<<11 | (year-2000)<<4 | month`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OsVersion {
    pub major: u8,
    pub minor: u8,
    pub patch: u8,
    /// 0 if not specified.
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

/// `boot.img` or `init_boot.img` as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootImage<'a> {
    pub header_version: u32,
    /// 4096 from v3; from the header before that.
    pub page_size: u32,
    pub kernel: &'a [u8],
    pub ramdisk: &'a [u8],
    /// v0–v2 (ignored).
    pub second: &'a [u8],
    /// v1–v2 (ignored).
    pub recovery_dtbo: &'a [u8],
    /// v2 (ignored: Vetro generates its own DTB).
    pub dtb: &'a [u8],
    /// Command line; v0–v2: `cmdline` followed by `extra_cmdline`, with no
    /// separator (mkbootimg splits a long line at 511 bytes).
    pub cmdline: String,
    /// Product name (v0–v2).
    pub name: String,
    pub os_version: OsVersion,
    /// v4 GKI signature (ignored).
    pub signature: &'a [u8],
}

impl<'a> BootImage<'a> {
    pub fn parse(img: &'a [u8]) -> Result<Self, AndroidError> {
        Self::parse_as(img, Which::Boot)
    }

    /// `init_boot.img`: v4 header without a kernel, only the generic ramdisk.
    pub fn parse_init_boot(img: &'a [u8]) -> Result<Self, AndroidError> {
        let b = Self::parse_as(img, Which::InitBoot)?;
        if b.header_version < 4 {
            return Err(AndroidError::BadInitBoot("header older than version 4"));
        }
        if !b.kernel.is_empty() {
            return Err(AndroidError::BadInitBoot("contains a kernel"));
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

/// Type of a vendor ramdisk (`VENDOR_RAMDISK_TYPE_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RamdiskType {
    None,
    /// Always loaded.
    Platform,
    /// Only for booting into recovery.
    Recovery,
    /// Kernel modules.
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

/// A ramdisk of the vendor table (v3: just one, the whole section).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorRamdisk<'a> {
    pub kind: RamdiskType,
    pub name: String,
    pub board_id: [u32; 16],
    pub data: &'a [u8],
}

/// `vendor_boot.img` as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorBoot<'a> {
    pub header_version: u32,
    pub page_size: u32,
    pub cmdline: String,
    /// Product name.
    pub name: String,
    /// The whole ramdisk section.
    pub ramdisk_section: &'a [u8],
    pub ramdisks: Vec<VendorRamdisk<'a>>,
    /// Ignored: Vetro generates its own DTB.
    pub dtb: &'a [u8],
    /// Bootconfig section (v4), `key=value` text one line per parameter.
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
            let table = take(img, &mut off, table_size, page, w, "ramdisk table")?;
            bootconfig = take(img, &mut off, le32(img, 2124), page, w, "bootconfig")?;
            if num > 0 && entry_size < RAMDISK_ENTRY_V4 {
                return Err(AndroidError::BadRamdiskTable("entries shorter than 108 bytes"));
            }
            if (num as usize).checked_mul(entry_size).is_none_or(|n| n > table.len()) {
                return Err(AndroidError::BadRamdiskTable("more entries than fit in the table"));
            }
            for e in table.chunks_exact(entry_size.max(1)).take(num as usize) {
                let (size, at) = (le32(e, 0) as usize, le32(e, 4) as usize);
                let data = at
                    .checked_add(size)
                    .and_then(|end| ramdisk_section.get(at..end))
                    .ok_or(AndroidError::BadRamdiskTable("ramdisk outside the section"))?;
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

/// Bootloader choices.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BootOptions {
    /// Parameters added by the bootloader (command-line syntax):
    /// the `androidboot.*` ones go into the bootconfig if `vendor_boot` is v4, the
    /// others at the end of the command line.
    pub params: String,
    /// Boot into recovery: also loads the recovery-type vendor ramdisks.
    pub recovery: bool,
}

/// What the bootloader passes to the kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndroidBoot {
    /// Uncompressed arm64 `Image`.
    pub kernel: Vec<u8>,
    /// Format of the kernel in the `boot.img`.
    pub kernel_format: Format,
    /// Concatenated ramdisks and, if present, the bootconfig block at the end.
    pub initrd: Vec<u8>,
    pub cmdline: String,
    /// Bootconfig text (without NULs and trailer), empty if there is none.
    pub bootconfig: String,
    /// Description of the loaded ramdisks, in order (for messages).
    pub ramdisks: Vec<String>,
}

impl AndroidBoot {
    /// Reads the images and combines them ([`assemble`]).
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

    /// The initrd for the loader, `None` if empty.
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
        RamdiskType::Other(v) => format!("type {v}"),
    };
    let name = if r.name.is_empty() { "vendor" } else { &r.name };
    format!("{name} ({kind}, {} byte)", r.data.len())
}

/// Combines the images like the bootloader (see the module documentation).
pub fn assemble(
    boot: &BootImage,
    vendor: Option<&VendorBoot>,
    init_boot: Option<&BootImage>,
    opts: &BootOptions,
) -> Result<AndroidBoot, AndroidError> {
    if vendor.is_some() && boot.header_version < 3 {
        return Err(AndroidError::Mismatch("vendor_boot wants boot.img v3 or v4"));
    }
    if init_boot.is_some() && boot.header_version < 4 {
        return Err(AndroidError::Mismatch("init_boot wants boot.img v4"));
    }
    if boot.kernel.is_empty() {
        return Err(AndroidError::NoKernel);
    }
    let format = decompress::detect(boot.kernel);
    let kernel =
        decompress::decompress(boot.kernel).map_err(|e| AndroidError::Kernel(format, e))?.into_owned();

    // Ramdisks: vendor in table order, then the generic one, with no gaps.
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
        ramdisks.push(format!("generic from {from} ({} bytes)", generic.len()));
    }

    // Bootloader parameters: androidboot.* into the bootconfig if there is one.
    let has_bootconfig = vendor.is_some_and(|v| v.header_version >= 4);
    let mut extra = Vec::new();
    let mut params: Vec<(&str, String)> = Vec::new();
    for p in bootconfig::split_cmdline(&opts.params) {
        if has_bootconfig && p.starts_with("androidboot.") {
            let key = bootconfig::key_value(p).0;
            if params.iter().any(|(k, _)| *k == key) {
                return Err(AndroidError::Bootconfig(format!("{key} repeated in the parameters")));
            }
            params.push((key, bootconfig::param_line(p).map_err(AndroidError::Bootconfig)?));
        } else {
            extra.push(p);
        }
    }
    // A parameter with the key of a line of the vendor section replaces it
    // (ADR 0028): repeated, the kernel would discard the whole block.
    let mut text = String::new();
    let mut used = vec![false; params.len()];
    if let Some(v) = vendor {
        let section = v.bootconfig;
        let section = &section[..section.iter().rposition(|&b| b != 0).map_or(0, |p| p + 1)];
        for line in String::from_utf8_lossy(section).lines() {
            let key = line.split('=').next().unwrap_or("").trim();
            match params.iter().position(|(k, _)| *k == key) {
                Some(i) if !key.is_empty() => {
                    text.push_str(&params[i].1);
                    used[i] = true;
                }
                _ => {
                    text.push_str(line);
                    text.push('\n');
                }
            }
        }
    }
    for (i, (_, line)) in params.iter().enumerate() {
        if !used[i] {
            text.push_str(line);
        }
    }

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
