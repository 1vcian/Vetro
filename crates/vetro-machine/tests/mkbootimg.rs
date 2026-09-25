//! Il lettore di immagini Android contro le immagini costruite da
//! `mkbootimg.py` di AOSP (copia fissata in `tools/mkbootimg/`): header v0–v4
//! di `boot.img`, `vendor_boot.img` v3 e v4 (ramdisk multipli, board id,
//! bootconfig), `init_boot.img`. Ogni sezione letta deve essere il file dato
//! a mkbootimg, byte per byte.
//!
//! Serve `python3`: senza, il test si salta, salvo `VETRO_REQUIRE_ORACLE=1`
//! (come in CI), dove fallisce.

use std::path::{Path, PathBuf};
use std::process::Command;
use vetro_machine::android::{
    AndroidBoot, BootImage, BootOptions, OsVersion, RamdiskType, VendorBoot, bootconfig, decompress::Format,
};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

fn python() -> Option<&'static str> {
    Command::new("python3").arg("--version").output().ok().filter(|o| o.status.success()).map(|_| "python3")
}

struct Work {
    dir: PathBuf,
    py: &'static str,
}

impl Work {
    fn new(name: &str) -> Option<Self> {
        let Some(py) = python() else {
            if std::env::var("VETRO_REQUIRE_ORACLE").is_ok_and(|v| v == "1") {
                panic!("python3 assente: serve a tools/mkbootimg/mkbootimg.py (VETRO_REQUIRE_ORACLE=1)");
            }
            eprintln!("SKIP: python3 assente, niente mkbootimg.py");
            return None;
        };
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("mkbootimg-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Some(Work { dir, py })
    }

    fn file(&self, name: &str, data: &[u8]) -> String {
        let p = self.dir.join(name);
        std::fs::write(&p, data).unwrap();
        p.to_str().unwrap().to_string()
    }

    fn out(&self, name: &str) -> String {
        self.dir.join(name).to_str().unwrap().to_string()
    }

    fn mkbootimg(&self, args: &[&str]) {
        let script = repo_root().join("tools/mkbootimg/mkbootimg.py");
        let o = Command::new(self.py)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .arg(script)
            .args(args)
            .output()
            .expect("python3");
        assert!(o.status.success(), "mkbootimg.py {args:?}:\n{}", String::from_utf8_lossy(&o.stderr));
    }

    fn read(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.dir.join(name)).unwrap()
    }
}

/// Byte di prova riconoscibili.
fn blob(tag: u8, n: usize) -> Vec<u8> {
    (0..n).map(|i| tag ^ (i as u8).wrapping_mul(31)).collect()
}

/// Un `Image` arm64 finto (header valido).
fn image(n: usize) -> Vec<u8> {
    let mut v = blob(0x4b, n);
    v[8..16].fill(0);
    v[16..24].copy_from_slice(&(n as u64 * 2).to_le_bytes());
    v[24..32].copy_from_slice(&0b1010u64.to_le_bytes());
    v[56..60].copy_from_slice(b"ARM\x64");
    v
}

#[test]
fn boot_img_v0_v1_v2() {
    let Some(w) = Work::new("v0-v2") else { return };
    let kernel = image(10_000);
    let (ramdisk, second, dtbo, dtb) = (blob(1, 3000), blob(2, 700), blob(3, 900), blob(4, 1234));
    let k = w.file("kernel", &kernel);
    let r = w.file("ramdisk", &ramdisk);
    let s = w.file("second", &second);
    let d = w.file("dtbo", &dtbo);
    let t = w.file("dtb", &dtb);
    // Oltre 511 byte: mkbootimg ne mette una parte in extra_cmdline.
    let long: String = (0..70).map(|i| format!("vetro.p{i}=v{i} ")).collect();
    let long = long.trim_end();
    assert!(long.len() > 600);
    let cases: [(u32, &str, Vec<&str>); 3] = [
        (0, "2048", vec!["--second", &s]),
        (1, "4096", vec!["--second", &s, "--recovery_dtbo", &d]),
        (2, "16384", vec!["--recovery_dtbo", &d, "--dtb", &t]),
    ];
    for (v, page, extra) in cases {
        let out = w.out(&format!("boot-v{v}.img"));
        let ver = v.to_string();
        let mut args = vec![
            "--header_version",
            &ver,
            "--pagesize",
            page,
            "--kernel",
            &k,
            "--ramdisk",
            &r,
            "--cmdline",
            long,
            "--board",
            "vetro",
            "--os_version",
            "15.0.1",
            "--os_patch_level",
            "2025-03",
            "-o",
            &out,
        ];
        args.extend(extra);
        w.mkbootimg(&args);
        let img = w.read(&format!("boot-v{v}.img"));
        let b = BootImage::parse(&img).unwrap_or_else(|e| panic!("v{v}: {e}"));
        assert_eq!(b.header_version, v);
        assert_eq!(b.page_size, page.parse::<u32>().unwrap());
        assert_eq!(b.kernel, kernel, "v{v}");
        assert_eq!(b.ramdisk, ramdisk, "v{v}");
        assert_eq!(b.cmdline, long, "v{v}");
        assert_eq!(b.name, "vetro");
        assert_eq!(b.os_version, OsVersion { major: 15, minor: 0, patch: 1, year: 2025, month: 3 });
        assert_eq!(b.second, if v < 2 { &second[..] } else { &[] });
        assert_eq!(b.recovery_dtbo, if v >= 1 { &dtbo[..] } else { &[] });
        assert_eq!(b.dtb, if v == 2 { &dtb[..] } else { &[] });
        let a = AndroidBoot::from_images(&img, None, None, &BootOptions::default()).unwrap();
        assert_eq!(a.kernel, kernel);
        assert_eq!(a.initrd, ramdisk);
        assert_eq!(a.cmdline, long);
    }
}

#[test]
fn boot_v3_e_vendor_boot_v3() {
    let Some(w) = Work::new("v3") else { return };
    let kernel = image(5000);
    let (generic, vendor_rd) = (blob(5, 4097), blob(6, 2049));
    let k = w.file("kernel", &kernel);
    let g = w.file("generic", &generic);
    let v = w.file("vendor", &vendor_rd);
    let t = w.file("dtb", &blob(7, 100));
    let (boot, vendor) = (w.out("boot.img"), w.out("vendor_boot.img"));
    w.mkbootimg(&[
        "--header_version",
        "3",
        "--kernel",
        &k,
        "--ramdisk",
        &g,
        "--cmdline",
        "console=ttyAMA0",
        "-o",
        &boot,
        "--vendor_boot",
        &vendor,
        "--vendor_ramdisk",
        &v,
        "--vendor_cmdline",
        "androidboot.hardware=vetro",
        "--pagesize",
        "2048",
        "--dtb",
        &t,
        "--board",
        "vetro-virt",
    ]);
    let (boot, vendor) = (w.read("boot.img"), w.read("vendor_boot.img"));
    let b = BootImage::parse(&boot).unwrap();
    assert_eq!((b.header_version, b.page_size), (3, 4096));
    assert_eq!(b.kernel, kernel);
    assert_eq!(b.ramdisk, generic);
    let vb = VendorBoot::parse(&vendor).unwrap();
    assert_eq!((vb.header_version, vb.page_size), (3, 2048));
    assert_eq!(vb.name, "vetro-virt");
    assert_eq!(vb.cmdline, "androidboot.hardware=vetro");
    assert_eq!(vb.ramdisks.len(), 1);
    assert_eq!(vb.ramdisks[0].data, vendor_rd);
    assert_eq!(vb.dtb, blob(7, 100));
    let opts = BootOptions { params: "androidboot.serialno=X".into(), recovery: false };
    let a = AndroidBoot::from_images(&boot, Some(&vendor), None, &opts).unwrap();
    // v3: niente bootconfig; vendor ramdisk e generico attaccati.
    assert_eq!(a.cmdline, "console=ttyAMA0 androidboot.hardware=vetro androidboot.serialno=X");
    assert_eq!(a.initrd, [vendor_rd, generic].concat());
    assert_eq!(a.bootconfig, "");
}

#[test]
fn boot_v4_vendor_boot_v4_e_init_boot() {
    let Some(w) = Work::new("v4") else { return };
    let kernel = image(12_345);
    let (generic, plat, reco, dlkm) = (blob(8, 5000), blob(9, 3333), blob(10, 1111), blob(11, 2222));
    let k = w.file("kernel", &kernel);
    let g = w.file("generic", &generic);
    let p = w.file("plat", &plat);
    let r = w.file("reco", &reco);
    let d = w.file("dlkm", &dlkm);
    let bc =
        w.file("bootconfig", b"androidboot.hardware=vetro\nandroidboot.boot_devices=a003e00.virtio_mmio\n");
    let (boot, vendor, init) = (w.out("boot.img"), w.out("vendor_boot.img"), w.out("init_boot.img"));
    // boot.img GKI: solo il kernel; il ramdisk generico sta in init_boot.
    w.mkbootimg(&[
        "--header_version",
        "4",
        "--kernel",
        &k,
        "--cmdline",
        "console=ttyAMA0 bootconfig",
        "-o",
        &boot,
    ]);
    w.mkbootimg(&["--header_version", "4", "--ramdisk", &g, "-o", &init]);
    w.mkbootimg(&[
        "--header_version",
        "4",
        "--vendor_boot",
        &vendor,
        "--pagesize",
        "4096",
        "--vendor_cmdline",
        "vetro.vendor=1",
        "--vendor_bootconfig",
        &bc,
        "--ramdisk_type",
        "platform",
        "--ramdisk_name",
        "plat",
        "--vendor_ramdisk_fragment",
        &p,
        "--ramdisk_type",
        "recovery",
        "--ramdisk_name",
        "reco",
        "--vendor_ramdisk_fragment",
        &r,
        "--ramdisk_type",
        "dlkm",
        "--ramdisk_name",
        "dlkm",
        "--board_id0",
        "0xcafe",
        "--board_id15",
        "7",
        "--vendor_ramdisk_fragment",
        &d,
    ]);
    let (boot, vendor, init) = (w.read("boot.img"), w.read("vendor_boot.img"), w.read("init_boot.img"));
    let b = BootImage::parse(&boot).unwrap();
    assert_eq!(b.header_version, 4);
    assert_eq!(b.kernel, kernel);
    assert!(b.ramdisk.is_empty());
    assert!(b.signature.is_empty());
    let i = BootImage::parse_init_boot(&init).unwrap();
    assert_eq!(i.ramdisk, generic);
    // boot.img con un kernel non è un init_boot.
    assert!(BootImage::parse_init_boot(&boot).is_err());

    let vb = VendorBoot::parse(&vendor).unwrap();
    assert_eq!(vb.header_version, 4);
    assert_eq!(vb.cmdline, "vetro.vendor=1");
    assert_eq!(vb.ramdisk_section, [plat.clone(), reco.clone(), dlkm.clone()].concat());
    let kinds: Vec<_> = vb.ramdisks.iter().map(|r| (r.name.as_str(), r.kind)).collect();
    assert_eq!(
        kinds,
        [("plat", RamdiskType::Platform), ("reco", RamdiskType::Recovery), ("dlkm", RamdiskType::Dlkm)]
    );
    assert_eq!(vb.ramdisks[0].data, plat);
    assert_eq!(vb.ramdisks[1].data, reco);
    assert_eq!(vb.ramdisks[2].data, dlkm);
    assert_eq!(vb.ramdisks[2].board_id[0], 0xcafe);
    assert_eq!(vb.ramdisks[2].board_id[15], 7);
    assert_eq!(vb.bootconfig, b"androidboot.hardware=vetro\nandroidboot.boot_devices=a003e00.virtio_mmio\n");

    let opts =
        BootOptions { params: "androidboot.serialno=VETRO01 printk.devkmsg=on".into(), recovery: false };
    let a = AndroidBoot::from_images(&boot, Some(&vendor), Some(&init), &opts).unwrap();
    assert_eq!(a.kernel_format, Format::Image);
    assert_eq!(a.cmdline, "console=ttyAMA0 bootconfig vetro.vendor=1 printk.devkmsg=on");
    let text = "androidboot.hardware=vetro\nandroidboot.boot_devices=a003e00.virtio_mmio\n\
                androidboot.serialno = \"VETRO01\"\n";
    assert_eq!(a.bootconfig, text);
    let (len, bc) = bootconfig::split(&a.initrd).unwrap();
    assert_eq!(bc, text.as_bytes());
    assert_eq!(a.initrd[..len], [plat.clone(), dlkm.clone(), generic.clone()].concat());
    assert_eq!(a.initrd.len() % 4, 0);

    let opts = BootOptions { recovery: true, ..BootOptions::default() };
    let a = AndroidBoot::from_images(&boot, Some(&vendor), Some(&init), &opts).unwrap();
    let (len, _) = bootconfig::split(&a.initrd).unwrap();
    assert_eq!(a.initrd[..len], [plat, reco, dlkm, generic].concat());
}
