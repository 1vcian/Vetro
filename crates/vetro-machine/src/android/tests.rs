//! Tests of the `android` module. The images here are built by a small
//! writer that follows `mkbootimg.py` (edge cases and errors); the comparison
//! with images produced by the real `mkbootimg.py` lives in
//! `crates/vetro-machine/tests/mkbootimg.rs`, the guest kernel boot in
//! `tests/boot/tests/android.rs`.

use super::decompress::{self, DecompressError, Format};
use super::*;

const TEXT: &[u8] = b"Vetro Vetro Vetro: emulatore ARM64, Vetro Vetro Vetro!\n";
// Produced with `gzip -n -9` (1.14 on macOS) and `lz4 -9` 1.10.0 from TEXT.
const TEXT_GZ: &[u8] = &[
    0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x03, 0x0b, 0x4b, 0x2d, 0x29, 0xca, 0x57, 0x08,
    0x43, 0x90, 0x56, 0x0a, 0xa9, 0xb9, 0xa5, 0x39, 0x89, 0x25, 0xf9, 0x45, 0xa9, 0x0a, 0x8e, 0x41, 0xbe,
    0x66, 0x26, 0x3a, 0xc8, 0xb2, 0x10, 0x52, 0x91, 0x0b, 0x00, 0xe0, 0xd7, 0xda, 0xcc, 0x37, 0x00, 0x00,
    0x00,
];
/// `lz4 -l`.
const TEXT_LZ4_LEGACY: &[u8] = &[
    0x02, 0x21, 0x4c, 0x18, 0x28, 0x00, 0x00, 0x00, 0x67, 0x56, 0x65, 0x74, 0x72, 0x6f, 0x20, 0x06, 0x00,
    0xf8, 0x03, 0x3a, 0x20, 0x65, 0x6d, 0x75, 0x6c, 0x61, 0x74, 0x6f, 0x72, 0x65, 0x20, 0x41, 0x52, 0x4d,
    0x36, 0x34, 0x2c, 0x1e, 0x00, 0x80, 0x20, 0x56, 0x65, 0x74, 0x72, 0x6f, 0x21, 0x0a,
];
/// `lz4 --content-size -BD`: linked blocks, size and checksum of the
/// content.
const TEXT_LZ4_FRAME: &[u8] = &[
    0x04, 0x22, 0x4d, 0x18, 0x6c, 0x40, 0x37, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xde, 0x28, 0x00,
    0x00, 0x00, 0x67, 0x56, 0x65, 0x74, 0x72, 0x6f, 0x20, 0x06, 0x00, 0xf8, 0x03, 0x3a, 0x20, 0x65, 0x6d,
    0x75, 0x6c, 0x61, 0x74, 0x6f, 0x72, 0x65, 0x20, 0x41, 0x52, 0x4d, 0x36, 0x34, 0x2c, 0x1e, 0x00, 0x80,
    0x20, 0x56, 0x65, 0x74, 0x72, 0x6f, 0x21, 0x0a, 0x00, 0x00, 0x00, 0x00, 0xd7, 0xf1, 0x41, 0x8b,
];
/// `lz4 -BX`: block checksum.
const TEXT_LZ4_FRAME_BX: &[u8] = &[
    0x04, 0x22, 0x4d, 0x18, 0x74, 0x40, 0xbd, 0x28, 0x00, 0x00, 0x00, 0x67, 0x56, 0x65, 0x74, 0x72, 0x6f,
    0x20, 0x06, 0x00, 0xf8, 0x03, 0x3a, 0x20, 0x65, 0x6d, 0x75, 0x6c, 0x61, 0x74, 0x6f, 0x72, 0x65, 0x20,
    0x41, 0x52, 0x4d, 0x36, 0x34, 0x2c, 0x1e, 0x00, 0x80, 0x20, 0x56, 0x65, 0x74, 0x72, 0x6f, 0x21, 0x0a,
    0x68, 0xc0, 0x73, 0x0e, 0x00, 0x00, 0x00, 0x00, 0xd7, 0xf1, 0x41, 0x8b,
];

#[test]
fn decomprime_vettori_degli_strumenti_veri() {
    assert_eq!(decompress::detect(TEXT_GZ), Format::Gzip);
    assert_eq!(decompress::gunzip(TEXT_GZ).unwrap(), TEXT);
    assert_eq!(decompress::detect(TEXT_LZ4_LEGACY), Format::Lz4Legacy);
    assert_eq!(decompress::lz4_legacy(TEXT_LZ4_LEGACY).unwrap(), TEXT);
    assert_eq!(decompress::detect(TEXT_LZ4_FRAME), Format::Lz4Frame);
    assert_eq!(decompress::lz4_frame(TEXT_LZ4_FRAME).unwrap(), TEXT);
    assert_eq!(decompress::lz4_frame(TEXT_LZ4_FRAME_BX).unwrap(), TEXT);
    // Two concatenated gzip members and zeros at the end (like an image on a
    // partition).
    let mut two = TEXT_GZ.to_vec();
    two.extend_from_slice(TEXT_GZ);
    two.extend_from_slice(&[0; 7]);
    assert_eq!(decompress::gunzip(&two).unwrap(), [TEXT, TEXT].concat());
    // Two concatenated legacy lz4 streams (like the GKI ramdisks).
    let mut two = TEXT_LZ4_LEGACY.to_vec();
    two.extend_from_slice(TEXT_LZ4_LEGACY);
    assert_eq!(decompress::lz4_legacy(&two).unwrap(), [TEXT, TEXT].concat());
    assert_eq!(decompress::crc32(b"123456789"), 0xcbf4_3926);
}

#[test]
fn decompressione_rileva_gli_errori() {
    let mut bad = TEXT_GZ.to_vec();
    let n = bad.len();
    bad[n - 8] ^= 1;
    assert!(matches!(decompress::gunzip(&bad), Err(DecompressError::Crc { .. })));
    let mut bad = TEXT_GZ.to_vec();
    bad[n - 4] ^= 1;
    assert!(matches!(decompress::gunzip(&bad), Err(DecompressError::Length { .. })));
    assert_eq!(decompress::gunzip(&TEXT_GZ[..20]), Err(DecompressError::Truncated));
    assert_eq!(decompress::lz4_legacy(&TEXT_LZ4_LEGACY[..20]), Err(DecompressError::Truncated));
    // Distance before the start: block "1 literal, match at distance 5".
    let block = [0x10, b'x', 5, 0, 0x00];
    assert!(matches!(decompress::lz4_block(&block, &mut Vec::new()), Err(DecompressError::BadData(_))));
    // Reserved deflate block type (BFINAL=1, BTYPE=3).
    assert!(matches!(decompress::inflate(&[0x07]), Err(DecompressError::BadData(_))));
    assert!(decompress::decompress(b"nothing known").is_err());
}

/// Test data with repetitive parts and random parts (dynamic and stored
/// deflate blocks).
fn sample(n: usize) -> Vec<u8> {
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut v = Vec::with_capacity(n);
    while v.len() < n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        if (v.len() / 4096) % 3 == 0 {
            v.push(x as u8);
        } else {
            v.extend_from_slice(format!("riga {} di vetro\n", x % 97).as_bytes());
        }
    }
    v.truncate(n);
    v
}

/// Compresses with the system tool, if present.
fn compress_with(tool: &str, args: &[&str], data: &[u8]) -> Option<Vec<u8>> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new(tool)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let data = data.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&data));
    let out = child.wait_with_output().ok()?;
    writer.join().ok()?.ok()?;
    out.status.success().then_some(out.stdout)
}

#[test]
fn andata_e_ritorno_con_gzip_e_lz4_di_sistema() {
    let data = sample(700_000);
    for level in ["-1", "-6", "-9"] {
        match compress_with("gzip", &["-n", level, "-c"], &data) {
            Some(gz) => assert_eq!(decompress::gunzip(&gz).unwrap(), data, "gzip {level}"),
            None => eprintln!("SKIP: gzip assente"),
        }
    }
    for args in [&["-l", "-9", "-c"][..], &["-1", "-c"], &["-BD", "-B4", "-c"], &["-BX", "-c"]] {
        match compress_with("lz4", args, &data) {
            Some(lz) => assert_eq!(&*decompress::decompress(&lz).unwrap(), &data[..], "lz4 {args:?}"),
            None => eprintln!("SKIP: lz4 assente"),
        }
    }
}

/// The guest kernel's initramfs (gzip -9 in `build.sh`): CRC32 and length
/// checked by the decompressor.
#[test]
fn initramfs_vero_del_kernel_guest() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/guest-kernel/initramfs.cpio.gz");
    let Ok(gz) = std::fs::read(path) else {
        eprintln!("SKIP initramfs_vero_del_kernel_guest: esegui tools/guest-kernel/build.sh");
        return;
    };
    let cpio = decompress::gunzip(&gz).unwrap();
    assert!(cpio.starts_with(b"070701"), "not a newc cpio");
}

#[test]
fn versione_e_livello_delle_patch() {
    // mkbootimg --os_version 15.0.1 --os_patch_level 2025-03.
    let raw = ((15 << 14 | 1) << 11) | (25 << 4 | 3);
    assert_eq!(OsVersion::from_raw(raw), OsVersion { major: 15, minor: 0, patch: 1, year: 2025, month: 3 });
    assert_eq!(OsVersion::from_raw(0), OsVersion::default());
}

// ---------------------------------------------------------------------------
// Minimal image writer, same layout as mkbootimg.py.

fn pad(v: &mut Vec<u8>, page: usize) {
    v.resize(v.len().next_multiple_of(page), 0);
}

fn put32(v: &mut [u8], off: usize, x: u32) {
    v[off..off + 4].copy_from_slice(&x.to_le_bytes());
}

fn put_str(v: &mut [u8], off: usize, s: &str) {
    v[off..off + s.len()].copy_from_slice(s.as_bytes());
}

fn boot_v4(kernel: &[u8], ramdisk: &[u8], cmdline: &str) -> Vec<u8> {
    let mut v = vec![0u8; BOOT_V4_HEADER];
    v[..8].copy_from_slice(BOOT_MAGIC);
    put32(&mut v, 8, kernel.len() as u32);
    put32(&mut v, 12, ramdisk.len() as u32);
    put32(&mut v, 20, BOOT_V4_HEADER as u32);
    put32(&mut v, 40, 4);
    put_str(&mut v, 44, cmdline);
    pad(&mut v, 4096);
    v.extend_from_slice(kernel);
    pad(&mut v, 4096);
    v.extend_from_slice(ramdisk);
    pad(&mut v, 4096);
    v
}

fn boot_v2(kernel: &[u8], ramdisk: &[u8], cmdline: &str, page: usize) -> Vec<u8> {
    let mut v = vec![0u8; BOOT_V2_HEADER];
    v[..8].copy_from_slice(BOOT_MAGIC);
    put32(&mut v, 8, kernel.len() as u32);
    put32(&mut v, 16, ramdisk.len() as u32);
    put32(&mut v, 36, page as u32);
    put32(&mut v, 40, 2);
    let (a, b) = cmdline.split_at(cmdline.len().min(511));
    put_str(&mut v, 64, a);
    put_str(&mut v, 608, b);
    put32(&mut v, 1644, BOOT_V2_HEADER as u32);
    put32(&mut v, 1648, 3);
    pad(&mut v, page);
    v.extend_from_slice(kernel);
    pad(&mut v, page);
    v.extend_from_slice(ramdisk);
    pad(&mut v, page);
    v.extend_from_slice(b"dtb");
    pad(&mut v, page);
    v
}

/// vendor_boot v4: (dati, tipo, nome) per ogni ramdisk.
fn vendor_v4(ramdisks: &[(&[u8], u32, &str)], cmdline: &str, bootconfig: &[u8], page: usize) -> Vec<u8> {
    let total: usize = ramdisks.iter().map(|r| r.0.len()).sum();
    let mut v = vec![0u8; VENDOR_V4_HEADER];
    v[..8].copy_from_slice(VENDOR_BOOT_MAGIC);
    put32(&mut v, 8, 4);
    put32(&mut v, 12, page as u32);
    put32(&mut v, 24, total as u32);
    put_str(&mut v, 28, cmdline);
    put32(&mut v, 2096, VENDOR_V4_HEADER as u32);
    put32(&mut v, 2100, 4);
    put32(&mut v, 2112, (ramdisks.len() * RAMDISK_ENTRY_V4) as u32);
    put32(&mut v, 2116, ramdisks.len() as u32);
    put32(&mut v, 2120, RAMDISK_ENTRY_V4 as u32);
    put32(&mut v, 2124, bootconfig.len() as u32);
    pad(&mut v, page);
    for r in ramdisks {
        v.extend_from_slice(r.0);
    }
    pad(&mut v, page);
    v.extend_from_slice(b"DTB!");
    pad(&mut v, page);
    let mut at = 0;
    for (i, r) in ramdisks.iter().enumerate() {
        let mut e = vec![0u8; RAMDISK_ENTRY_V4];
        put32(&mut e, 0, r.0.len() as u32);
        put32(&mut e, 4, at);
        put32(&mut e, 8, r.1);
        put_str(&mut e, 12, r.2);
        put32(&mut e, 44, i as u32 + 7);
        v.extend_from_slice(&e);
        at += r.0.len() as u32;
    }
    pad(&mut v, page);
    v.extend_from_slice(bootconfig);
    pad(&mut v, page);
    v
}

fn vendor_v3(ramdisk: &[u8], cmdline: &str, page: usize) -> Vec<u8> {
    let mut v = vec![0u8; VENDOR_V3_HEADER];
    v[..8].copy_from_slice(VENDOR_BOOT_MAGIC);
    put32(&mut v, 8, 3);
    put32(&mut v, 12, page as u32);
    put32(&mut v, 24, ramdisk.len() as u32);
    put_str(&mut v, 28, cmdline);
    put32(&mut v, 2096, VENDOR_V3_HEADER as u32);
    pad(&mut v, page);
    v.extend_from_slice(ramdisk);
    pad(&mut v, page);
    v
}

/// Un `Image` finto: header valido, 8 KiB.
fn fake_image() -> Vec<u8> {
    let mut v = vec![0u8; 8192];
    v[16..24].copy_from_slice(&0x10000u64.to_le_bytes());
    v[24..32].copy_from_slice(&0b10u64.to_le_bytes());
    v[56..60].copy_from_slice(b"ARM\x64");
    v[100] = 0xaa;
    v
}

#[test]
fn v4_con_ramdisk_multipli_bootconfig_e_init_boot() {
    let kernel = fake_image();
    let boot = boot_v4(&kernel, b"", "console=ttyAMA0");
    let vendor = vendor_v4(
        &[(b"AAAA", 1, "plat"), (b"RRR", 2, "reco"), (b"DD", 3, "dlkm")],
        "vendor.x=1 bootconfig",
        b"androidboot.hardware=vetro\n",
        2048,
    );
    let init = boot_v4(b"", b"GENERIC", "");

    let v = VendorBoot::parse(&vendor).unwrap();
    assert_eq!(v.header_version, 4);
    assert_eq!(v.ramdisk_section, b"AAAARRRDD");
    assert_eq!(v.dtb, b"DTB!");
    assert_eq!(v.ramdisks.len(), 3);
    assert_eq!(v.ramdisks[1].kind, RamdiskType::Recovery);
    assert_eq!(v.ramdisks[1].name, "reco");
    assert_eq!(v.ramdisks[1].data, b"RRR");
    assert_eq!(v.ramdisks[2].board_id[0], 9);

    let opts =
        BootOptions { params: "androidboot.serialno=V1 quiet androidboot.a=\"x y\"".into(), recovery: false };
    let a = AndroidBoot::from_images(&boot, Some(&vendor), Some(&init), &opts).unwrap();
    assert_eq!(a.kernel, kernel);
    assert_eq!(a.kernel_format, Format::Image);
    // The vendor already says `bootconfig`: it is not repeated.
    assert_eq!(a.cmdline, "console=ttyAMA0 vendor.x=1 bootconfig quiet");
    let text = "androidboot.hardware=vetro\nandroidboot.serialno = \"V1\"\nandroidboot.a = \"x y\"\n";
    assert_eq!(a.bootconfig, text);
    let (len, bc) = bootconfig::split(&a.initrd).unwrap();
    assert_eq!(&a.initrd[..len], b"AAAADDGENERIC", "recovery excluded, generic from init_boot at the end");
    assert_eq!(bc, text.as_bytes());
    assert_eq!(a.ramdisks.len(), 3);

    // In recovery also the recovery-type ramdisk, in table order.
    let opts = BootOptions { recovery: true, ..BootOptions::default() };
    let a = AndroidBoot::from_images(&boot, Some(&vendor), Some(&init), &opts).unwrap();
    let (len, _) = bootconfig::split(&a.initrd).unwrap();
    assert_eq!(&a.initrd[..len], b"AAAARRRDDGENERIC");
}

#[test]
fn bootconfig_aggiunto_alla_riga_di_comando_se_manca() {
    let boot = boot_v4(&fake_image(), b"GEN", "console=ttyAMA0");
    let vendor = vendor_v4(&[(b"V", 0, "v")], "", b"", 4096);
    // No bootconfig and no androidboot parameters: no trailer.
    let a = AndroidBoot::from_images(&boot, Some(&vendor), None, &BootOptions::default()).unwrap();
    assert_eq!(a.initrd, b"VGEN");
    assert_eq!(a.cmdline, "console=ttyAMA0");
    assert_eq!(a.bootconfig, "");
    // With a bootloader androidboot: trailer and `bootconfig`.
    let opts = BootOptions { params: "androidboot.x=1".into(), ..BootOptions::default() };
    let a = AndroidBoot::from_images(&boot, Some(&vendor), None, &opts).unwrap();
    assert_eq!(a.cmdline, "console=ttyAMA0 bootconfig");
    assert_eq!(bootconfig::split(&a.initrd), Some((4, &b"androidboot.x = \"1\"\n"[..])));
    // A vendor section without a final newline gets the separator.
    let vendor = vendor_v4(&[(b"V", 0, "v")], "", b"androidboot.y=2", 4096);
    let a = AndroidBoot::from_images(&boot, Some(&vendor), None, &opts).unwrap();
    assert_eq!(a.bootconfig, "androidboot.y=2\nandroidboot.x = \"1\"\n");
    // A parameter with a key of the vendor section replaces that line in
    // place (ADR 0028), the others are appended.
    let vendor = vendor_v4(&[(b"V", 0, "v")], "", b"androidboot.y=2\nandroidboot.z=3\n", 4096);
    let opts = BootOptions { params: "androidboot.y=nuovo androidboot.x=1".into(), ..BootOptions::default() };
    let a = AndroidBoot::from_images(&boot, Some(&vendor), None, &opts).unwrap();
    assert_eq!(a.bootconfig, "androidboot.y = \"nuovo\"\nandroidboot.z=3\nandroidboot.x = \"1\"\n");
    // Key repeated in the parameters: the kernel would reject the whole block.
    let opts = BootOptions { params: "androidboot.x=1 androidboot.x=2".into(), ..BootOptions::default() };
    assert!(matches!(
        AndroidBoot::from_images(&boot, Some(&vendor), None, &opts),
        Err(AndroidError::Bootconfig(_))
    ));
}

#[test]
fn v3_senza_bootconfig_e_kernel_compresso() {
    let kernel = fake_image();
    let gz = compress_with("gzip", &["-n", "-9", "-c"], &kernel);
    let packed = gz.clone().unwrap_or_else(|| kernel.clone());
    let mut boot = boot_v4(&packed, b"GEN", "console=ttyAMA0");
    put32(&mut boot, 40, 3);
    let vendor = vendor_v3(b"VND", "v=1", 4096);
    let opts = BootOptions { params: "androidboot.x=1".into(), ..BootOptions::default() };
    let a = AndroidBoot::from_images(&boot, Some(&vendor), None, &opts).unwrap();
    // v3: no bootconfig, androidboot stays on the command line.
    assert_eq!(a.cmdline, "console=ttyAMA0 v=1 androidboot.x=1");
    assert_eq!(a.initrd, b"VNDGEN");
    assert_eq!(a.kernel, kernel);
    if gz.is_some() {
        assert_eq!(a.kernel_format, Format::Gzip);
    }
    // Legacy LZ4 kernel (format of the GKI boot-lz4.img): a block of only
    // literals.
    let mut lz = decompress::LZ4_LEGACY_MAGIC.to_le_bytes().to_vec();
    let mut block = vec![0xf0];
    let mut n = kernel.len() - 15;
    while n >= 255 {
        block.push(255);
        n -= 255;
    }
    block.push(n as u8);
    block.extend_from_slice(&kernel);
    lz.extend_from_slice(&(block.len() as u32).to_le_bytes());
    lz.extend_from_slice(&block);
    let boot = boot_v4(&lz, b"", "");
    let a = AndroidBoot::from_images(&boot, None, None, &BootOptions::default()).unwrap();
    assert_eq!(a.kernel_format, Format::Lz4Legacy);
    assert_eq!(a.kernel, kernel);
    assert_eq!(a.initrd(), None);
}

#[test]
fn v2_riga_di_comando_spezzata() {
    let long: String = (0..60).map(|i| format!("param{i}=valore ")).collect();
    let long = long.trim_end();
    assert!(long.len() > 511);
    let boot = boot_v2(&fake_image(), b"RD", long, 2048);
    let b = BootImage::parse(&boot).unwrap();
    assert_eq!(b.header_version, 2);
    assert_eq!(b.page_size, 2048);
    assert_eq!(b.cmdline, long);
    assert_eq!(b.dtb, b"dtb");
    let a =
        assemble(&b, None, None, &BootOptions { params: "androidboot.x=1".into(), recovery: false }).unwrap();
    assert_eq!(a.cmdline, format!("{long} androidboot.x=1"));
    assert_eq!(a.initrd, b"RD");
}

#[test]
fn errori_delle_immagini() {
    let kernel = fake_image();
    let boot = boot_v4(&kernel, b"", "");
    assert_eq!(BootImage::parse(b"ANDROID!"), Err(AndroidError::Truncated(Which::Boot)));
    assert_eq!(BootImage::parse(&vendor_v3(b"x", "", 4096)), Err(AndroidError::BadMagic(Which::Boot)));
    let mut v5 = boot.clone();
    put32(&mut v5, 40, 5);
    assert_eq!(BootImage::parse(&v5), Err(AndroidError::UnsupportedVersion(Which::Boot, 5)));
    assert_eq!(BootImage::parse(&boot[..6000]), Err(AndroidError::OutOfBounds(Which::Boot, "kernel")));
    let mut odd = boot_v2(&kernel, b"", "", 2048);
    put32(&mut odd, 36, 3000);
    assert_eq!(BootImage::parse(&odd), Err(AndroidError::BadPageSize(Which::Boot, 3000)));
    // init_boot with a kernel.
    assert_eq!(BootImage::parse_init_boot(&boot), Err(AndroidError::BadInitBoot("contiene un kernel")));
    // vendor_boot con un boot.img v2.
    let v2 = boot_v2(&kernel, b"", "", 2048);
    let vendor = vendor_v3(b"x", "", 4096);
    assert!(matches!(
        AndroidBoot::from_images(&v2, Some(&vendor), None, &BootOptions::default()),
        Err(AndroidError::Mismatch(_))
    ));
    // Ramdisk table with an entry outside the section.
    let mut vendor = vendor_v4(&[(b"AAAA", 1, "a")], "", b"", 4096);
    let table = 4096 * 3;
    put32(&mut vendor, table, 5);
    assert!(matches!(VendorBoot::parse(&vendor), Err(AndroidError::BadRamdiskTable(_))));
    // Kernel mancante o in un formato sconosciuto.
    let empty = boot_v4(b"", b"R", "");
    assert_eq!(
        AndroidBoot::from_images(&empty, None, None, &BootOptions::default()),
        Err(AndroidError::NoKernel)
    );
    let junk = boot_v4(b"this is not a kernel", b"", "");
    assert!(matches!(
        AndroidBoot::from_images(&junk, None, None, &BootOptions::default()),
        Err(AndroidError::Kernel(Format::Unknown, _))
    ));
}
