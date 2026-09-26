//! Riferimento nativo dei test web (tests/web): gli stessi copioni di
//! `boot-disk.mjs`, `devices.mjs` e `android-boot.mjs` (3 GiB of RAM, ADR
//! 0028), con la stessa API di vetro-wasm
//! compilata per l'host, l'interprete e un disco locale sempre pronto.
//! Scrive istruzioni e log grezzo in `target/web-test/native-*.{steps,log}`:
//! `tools/web-test.sh` lo esegue prima dei test in Node, che devono dare le
//! stesse istruzioni e lo stesso log byte per byte (col JIT in V8 e con il
//! disco servito via HTTP Range).
//!
//! Solo in release, come `vetro.rs`.

use vetro_boot_tests::*;
use vetro_platform::virtio::{CowBackend, MemBackend};
use vetro_wasm::{Vm, dev, devices_from, stop};

const QUANTUM: u64 = 1_000_000;
const PHASE_BUDGET: u64 = 6_000_000_000;
const CMDLINE: &[u8] = b"console=ttyAMA0 vetro.noautotest";
// Come boot-disk.mjs.
const SIZE: usize = 3 * 1024 * 1024 + 5 * 512 + 100;
const WRITE_AT: usize = 1_000_000;
const WRITTEN: &str = "VETRO-SCRITTO";
const BTN_LEFT: u32 = 0x110;

/// Il disco di prova di boot-disk.mjs (xorshift32, un byte per passo).
fn make_disk() -> Vec<u8> {
    let mut s: u32 = 0x9e37_79b9;
    (0..SIZE)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            s as u8
        })
        .collect()
}

/// La `Session` di tests/web/lib.mjs: quanti con confini assoluti, log e
/// ingressi solo ai confini.
struct Session {
    vm: Vm,
    log: Vec<u8>,
}

impl Session {
    fn new(image: &[u8], initrd: &[u8], setup: impl FnOnce(&mut Vm)) -> Self {
        Self::with_ram(image, initrd, vetro_machine::MachineConfig::default().ram_size, setup)
    }

    fn with_ram(image: &[u8], initrd: &[u8], ram_size: u64, setup: impl FnOnce(&mut Vm)) -> Self {
        let cfg = vetro_machine::MachineConfig { ram_size, ..vetro_machine::MachineConfig::default() };
        let mut vm = Vm::with_devices(&cfg, &devices_from(dev::DEFAULT, 0, 0));
        setup(&mut vm);
        assert_eq!(vm.load_linux(image, Some(initrd), CMDLINE), 0);
        Session { vm, log: Vec::new() }
    }

    fn quantum(&mut self) -> u32 {
        let steps = self.vm.machine().steps;
        let target = (steps / QUANTUM + 1) * QUANTUM;
        let s = self.vm.run(target - steps);
        let mut buf = [0u8; 65536];
        loop {
            let n = self.vm.console_read(&mut buf);
            if n == 0 {
                break;
            }
            self.log.extend_from_slice(&buf[..n]);
        }
        assert_ne!(s, stop::BLOCKED, "il disco locale è sempre pronto");
        s
    }

    fn until(&mut self, needle: &str, from: usize) -> usize {
        let limit = self.vm.machine().steps + PHASE_BUDGET;
        loop {
            let hay = &self.log[from.min(self.log.len())..];
            if let Some(i) = hay.windows(needle.len()).position(|w| w == needle.as_bytes()) {
                return from + i + needle.len();
            }
            assert!(self.vm.machine().steps < limit, "{needle:?} non arrivato:\n{}", self.tail());
            let s = self.quantum();
            assert_eq!(s, stop::BUDGET, "arresto {s} in attesa di {needle:?}:\n{}", self.tail());
        }
    }

    fn write(&mut self, text: &str) {
        self.vm.machine().console_input(text.as_bytes());
    }

    fn command(&mut self, cmd: &str, from: usize) -> usize {
        self.write(&format!("{cmd}\n"));
        self.until(SHELL_PROMPT, from)
    }

    fn poweroff(&mut self) {
        self.write("poweroff -f\n");
        let limit = self.vm.machine().steps + PHASE_BUDGET;
        let s = loop {
            let s = self.quantum();
            if s != stop::BUDGET || self.vm.machine().steps >= limit {
                break s;
            }
        };
        assert_eq!(s, stop::POWER_OFF, "{}", self.tail());
    }

    fn tail(&self) -> String {
        let log = normalize(&String::from_utf8_lossy(&self.log));
        let v: Vec<&str> = log.lines().rev().take(40).collect();
        v.into_iter().rev().collect::<Vec<_>>().join("\n")
    }

    fn save(&mut self, name: &str) -> u64 {
        let dir = repo_root().join("target/web-test");
        std::fs::create_dir_all(&dir).unwrap();
        let steps = self.vm.machine().steps;
        std::fs::write(dir.join(format!("native-{name}.log")), &self.log).unwrap();
        std::fs::write(dir.join(format!("native-{name}.steps")), steps.to_string()).unwrap();
        steps
    }
}

fn disk_session(image: &[u8], initrd: &[u8]) -> u64 {
    let img = make_disk();
    let mut s = Session::new(image, initrd, |vm| {
        let data = img[..SIZE / 512 * 512].to_vec();
        vm.add_disk(Box::new(CowBackend::new(MemBackend::from_vec(data).read_only())), false).unwrap();
    });
    let at = s.until(SHELL_PROMPT, 0);
    let at = s.command("md5sum /dev/vda", at);
    let cmd = format!(
        "printf {WRITTEN} | dd of=/dev/vda bs=1 seek={WRITE_AT} conv=notrunc 2>/dev/null; sync; \
         echo 3 > /proc/sys/vm/drop_caches; dd if=/dev/vda bs=1 skip={WRITE_AT} count={} 2>/dev/null; \
         echo; md5sum /dev/vda",
        WRITTEN.len()
    );
    let at = s.command(&cmd, at);
    let text = s.tail();
    // Il messaggio del kernel su drop_caches può arrivare subito dopo il testo.
    assert!(text.contains(&format!("\n{WRITTEN}")), "byte scritti non riletti:\n{text}");
    let _ = at;
    s.poweroff();
    s.save("disk")
}

fn devices_session(image: &[u8], initrd: &[u8]) -> u64 {
    let mut s = Session::new(image, initrd, |_| {});
    let at = s.until(SHELL_PROMPT, 0);
    s.write("vetro-dev drm-hold\n");
    let ready = s.until("VETRO-DRM-PRONTO", at);
    let size = s.vm.with_display(|d| d.screen(0).map(|x| (x.width, x.height, x.on))).flatten();
    assert_eq!(size, Some((1280, 800, true)));
    s.write("\n");
    let at = s.until(SHELL_PROMPT, ready);
    s.write("vetro-dev input-read /dev/input/event1 4\n");
    let r = s.until("VETRO-INPUT-PRONTO", at);
    s.vm.machine().keyboard(|k| {
        k.key(30, true);
        k.key(30, false);
    });
    let at = s.until(SHELL_PROMPT, r);
    s.write("vetro-dev input-read /dev/input/event0 5\n");
    let r = s.until("VETRO-INPUT-PRONTO", at);
    s.vm.machine().pointer(|p| p.move_abs(0x1234, 0x7000));
    s.vm.machine().pointer(|p| p.key(BTN_LEFT as u16, true));
    let at = s.until(SHELL_PROMPT, r);
    let _ = s.command("vetro-dev led /dev/input/event1 1 1", at);
    s.poweroff();
    s.save("devices")
}

/// Like android-boot.mjs: 3 GiB of RAM (on wasm32 a region outside the
/// allocator), up to the prompt, first line of /proc/meminfo, power off.
fn ram3g_session(image: &[u8], initrd: &[u8]) -> u64 {
    let mut s = Session::with_ram(image, initrd, 3 << 30, |_| {});
    let at = s.until(SHELL_PROMPT, 0);
    let _ = s.command("head -1 /proc/meminfo", at);
    s.poweroff();
    s.save("ram3g")
}

#[test]
fn riferimento_nativo_dei_test_web() {
    if cfg!(debug_assertions) {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "riferimento dei test web solo in release");
    }
    let Some((image, initrd)) = guest_kernel() else {
        return skip_or_fail(
            "VETRO_REQUIRE_GUEST_KERNEL",
            "target/guest-kernel mancante: esegui tools/guest-kernel/build.sh",
        );
    };
    let (image, initrd) = (std::fs::read(image).unwrap(), std::fs::read(initrd).unwrap());
    let d = disk_session(&image, &initrd);
    let v = devices_session(&image, &initrd);
    let r = ram3g_session(&image, &initrd);
    eprintln!(
        "riferimento nativo: disco {d} istruzioni, dispositivi {v} istruzioni, 3 GiB of RAM {r} instructions \
         (target/web-test/native-*)"
    );
}
