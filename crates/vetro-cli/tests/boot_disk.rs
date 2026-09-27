//! `vetro boot --no-devices --disk=FILE`: the M3 guest kernel sees the file
//! as /dev/vda (first disk, virtio-mmio slot 31), reads it, writes to it, and the
//! file stays intact (in-memory copy-on-write, like QEMU's `snapshot=on`).
//! It is the mechanism used to boot the Android emulator image
//! (`tools/android-emu`, `docs/research/m5-gki-boot.md`).
//!
//! In release (`cargo test --release -p vetro-cli`): in debug the interpreter is
//! too slow and the test is skipped.

use std::process::Command;
use std::time::Duration;

use vetro_boot_tests::{BOOT_MARKER, Console, SHELL_PROMPT, guest_kernel, skip_or_fail, timeout};

#[test]
fn boot_con_disco_da_file() {
    if cfg!(debug_assertions) {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "boot under Vetro only in release");
    }
    let Some((image, initrd)) = guest_kernel() else {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "target/guest-kernel mancante");
    };
    let dir = std::env::temp_dir().join(format!("vetro-boot-disk-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let disk = dir.join("disco.img");
    let mut data = vec![0u8; 1 << 20];
    data[..13].copy_from_slice(b"VETRO-DISCO-7");
    data[512 * 1000..512 * 1000 + 6].copy_from_slice(b"SETTOR");
    std::fs::write(&disk, &data).unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vetro"));
    cmd.arg("boot")
        .arg(format!("--kernel={}", image.display()))
        .arg(format!("--initrd={}", initrd.display()))
        .arg("--append=console=ttyAMA0 vetro.noautotest")
        .arg("--mem=256")
        .arg("--no-devices")
        .arg(format!("--disk={}", disk.display()));
    let mut c = Console::spawn(cmd).expect("vetro boot");
    let limit = timeout();
    let at = c.wait_for(BOOT_MARKER, 0, limit).unwrap_or_else(|| panic!("no /init:\n{}", c.log()));
    let at = c.wait_for(SHELL_PROMPT, at, limit).unwrap_or_else(|| panic!("no shell:\n{}", c.log()));
    // The markers are written "D"IM= etc.: the echo of the command doesn't contain them.
    c.send(concat!(
        "echo \"D\"IM=$(cat /sys/block/vda/size) DEV=$(basename $(readlink /sys/block/vda/device)); ",
        "echo \"P\"RIMO=$(head -c 13 /dev/vda) $(dd if=/dev/vda bs=512 skip=1000 count=1 2>/dev/null | head -c 6); ",
        "echo SCRITTO | dd of=/dev/vda bs=512 seek=5 conv=sync 2>/dev/null; sync; echo 3 > /proc/sys/vm/drop_caches; ",
        "echo \"D\"OPO=$(dd if=/dev/vda bs=512 skip=5 count=1 2>/dev/null | head -c 7); poweroff -f\n"
    ));
    let (_, dim) = c.wait_line("DIM=", at, limit).unwrap_or_else(|| panic!("no DIM:\n{}", c.log()));
    assert_eq!(dim, "DIM=2048 DEV=virtio0", "1 MiB = 2048 settori, primo dispositivo virtio");
    let (_, primo) = c.wait_line("PRIMO=", at, limit).unwrap_or_else(|| panic!("no PRIMO:\n{}", c.log()));
    assert_eq!(primo, "PRIMO=VETRO-DISCO-7 SETTOR");
    let (_, dopo) = c.wait_line("DOPO=", at, limit).unwrap_or_else(|| panic!("no DOPO:\n{}", c.log()));
    assert_eq!(dopo, "DOPO=SCRITTO", "the guest rereads what it wrote");
    assert!(c.finish(Duration::from_secs(60)), "poweroff -f did not stop vetro:\n{}", c.log());
    assert_eq!(std::fs::read(&disk).unwrap(), data, "the disk file doesn't change");
    std::fs::remove_dir_all(&dir).unwrap();
}
