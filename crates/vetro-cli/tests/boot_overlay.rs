//! `vetro boot --disk=FILE --overlay=FILE` (M6, ADR 0017): the guest's
//! writes to the disk are kept in the overlay and come back at the next
//! boot, in another process; the base image doesn't change; if the base
//! changes (here: the modification time) the overlay is discarded and the guest rereads
//! the base.
//!
//! In release (`cargo test --release -p vetro-cli`), like the other boots.

use std::process::Command;
use std::time::Duration;

use vetro_boot_tests::{Console, SHELL_PROMPT, guest_kernel, normalize, skip_or_fail, timeout};

const AT: u64 = 300_000;
const TEXT: &str = "VETRO-OVERLAY-42";

/// Starts `vetro boot` with `args`, gives the commands to the shell one at a time,
/// powers off; returns the log.
fn session(args: &[String], commands: &[String]) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vetro"));
    cmd.arg("boot").args(args);
    let mut c = Console::spawn(cmd).expect("vetro boot");
    let mut at = c.wait_for(SHELL_PROMPT, 0, timeout()).unwrap_or_else(|| panic!("no shell:\n{}", c.log()));
    for command in commands {
        c.send(&format!("{command}\n"));
        at = c
            .wait_for(SHELL_PROMPT, at, timeout())
            .unwrap_or_else(|| panic!("{command}: no prompt:\n{}", normalize(&c.log())));
    }
    c.send("poweroff -f\n");
    assert!(c.finish(Duration::from_secs(60)), "poweroff -f did not stop vetro:\n{}", c.log());
    normalize(&c.log())
}

#[test]
fn scritture_conservate_fra_due_avvii() {
    if cfg!(debug_assertions) {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "boot under Vetro only in release");
    }
    let Some((image, initrd)) = guest_kernel() else {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "target/guest-kernel missing");
    };
    let dir = std::env::temp_dir().join(format!("vetro-boot-overlay-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (base, ov) = (dir.join("base.img"), dir.join("disco.cow"));
    let data: Vec<u8> = (0..1024 * 1024u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
    std::fs::write(&base, &data).unwrap();
    let args = vec![
        format!("--kernel={}", image.display()),
        format!("--initrd={}", initrd.display()),
        "--append=console=ttyAMA0 vetro.noautotest".to_string(),
        "--mem=512".to_string(),
        "--no-devices".to_string(),
        format!("--disk={}", base.display()),
        format!("--overlay={}", ov.display()),
    ];
    let read = format!(
        "echo 3 > /proc/sys/vm/drop_caches; echo LETTO-$(dd if=/dev/vda bs=1 skip={AT} count={} 2>/dev/null)-FINE",
        TEXT.len()
    );
    let original = String::from_utf8_lossy(&data[AT as usize..AT as usize + TEXT.len()]).into_owned();
    assert!(!original.contains(TEXT));

    let first = session(
        &args,
        &[
            format!("printf {TEXT} | dd of=/dev/vda bs=1 seek={AT} conv=notrunc 2>/dev/null; sync"),
            read.clone(),
        ],
    );
    assert!(first.contains(&format!("LETTO-{TEXT}-FINE")), "write not read back:\n{first}");
    assert!(std::fs::metadata(&ov).unwrap().len() > 4096, "overlay without clusters");
    assert_eq!(std::fs::read(&base).unwrap(), data, "the base doesn't change");

    let second = session(&args, std::slice::from_ref(&read));
    assert!(second.contains(&format!("LETTO-{TEXT}-FINE")), "write lost between the two boots:\n{second}");

    // The base changes (same bytes, another modification time): overlay discarded.
    let f = std::fs::File::options().write(true).open(&base).unwrap();
    f.set_modified(std::time::SystemTime::now() + Duration::from_secs(10)).unwrap();
    drop(f);
    let third = session(&args, std::slice::from_ref(&read));
    assert!(!third.contains(&format!("LETTO-{TEXT}-FINE")), "overlay of another base applied:\n{third}");
    assert_eq!(std::fs::metadata(&ov).unwrap().len(), 4096, "overlay discarded: only the header");
    std::fs::remove_dir_all(&dir).unwrap();
}
