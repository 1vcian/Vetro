//! `vetro boot --hostfwd=tcp:127.0.0.1:0-:5555`: a real listening socket
//! on the host forwards connections to the guest's service (BusyBox
//! `nc -l -p 5555 -e cat`, the echo), like QEMU's `hostfwd`.
//!
//! - echo of 200 KB from a `TcpStream` of the test, orderly close in both
//!   directions (the test closes its direction and reads to the end);
//! - without a service in the guest the connection closes immediately (like slirp);
//! - a client that aborts with RST: the guest's service sees the reset.
//!
//! In release (`cargo test --release -p vetro-cli`): in debug the interpreter is
//! too slow and the test is skipped.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::time::Duration;

use vetro_boot_tests::{Console, SHELL_PROMPT, guest_kernel, skip_or_fail, timeout};

fn payload() -> Vec<u8> {
    (0..200_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 7) as u8).collect()
}

#[test]
fn hostfwd_con_un_socket_vero() {
    if cfg!(debug_assertions) {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "boot under Vetro only in release");
    }
    let Some((image, initrd)) = guest_kernel() else {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "target/guest-kernel missing");
    };
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg("exec \"$0\" \"$@\" 2>&1")
        .arg(env!("CARGO_BIN_EXE_vetro"))
        .arg("boot")
        .arg(format!("--kernel={}", image.display()))
        .arg(format!("--initrd={}", initrd.display()))
        .arg("--append=console=ttyAMA0 vetro.noautotest")
        .arg("--mem=256")
        .arg("--no-devices")
        .arg("--net")
        .arg("--hostfwd=tcp:127.0.0.1:0-:5555");
    let mut c = Console::spawn(cmd).expect("vetro boot");
    let limit = timeout();
    let (at, line) =
        c.wait_line("vetro: hostfwd tcp ", 0, limit).unwrap_or_else(|| panic!("no hostfwd:\n{}", c.log()));
    let addr = line["vetro: hostfwd tcp ".len()..].split(' ').next().unwrap().to_string();
    assert!(line.ends_with("-> 10.0.2.15:5555"), "{line}");
    assert!(addr.starts_with("127.0.0.1:"), "{addr}");
    let at = c.wait_for(SHELL_PROMPT, at, limit).unwrap_or_else(|| panic!("no shell:\n{}", c.log()));
    c.send("udhcpc -i eth0 -n -q >/dev/null 2>&1; nc -n -v -l -p 5555 -e cat\n");
    let at =
        c.wait_for("listening on", at, limit).unwrap_or_else(|| panic!("nc not listening:\n{}", c.log()));

    // Echo of 200 KB: a thread writes and closes its direction, here we read
    // to the end of the stream.
    let data = payload();
    let mut s = TcpStream::connect(&addr).expect("connection to --hostfwd");
    s.set_read_timeout(Some(limit)).unwrap();
    let mut w = s.try_clone().unwrap();
    let out = data.clone();
    let writer = std::thread::spawn(move || {
        w.write_all(&out).unwrap();
        w.shutdown(std::net::Shutdown::Write).unwrap();
    });
    let mut echo = Vec::new();
    s.read_to_end(&mut echo).expect("echo");
    writer.join().unwrap();
    assert_eq!(echo.len(), data.len(), "echo bytes");
    assert!(echo == data, "200 KB echo differs");
    let (at, conn) =
        c.wait_line("connect to ", at, limit).unwrap_or_else(|| panic!("no nc line:\n{}", c.log()));
    assert!(conn.starts_with("connect to 10.0.2.15:5555 from 10.0.2.2:"), "{conn}");
    let at = c.wait_for(SHELL_PROMPT, at, limit).unwrap_or_else(|| panic!("nc did not exit:\n{}", c.log()));

    // No service: the guest answers RST, the client sees the close.
    let mut s = TcpStream::connect(&addr).expect("connection");
    s.set_read_timeout(Some(limit)).unwrap();
    let mut buf = [0u8; 16];
    assert!(matches!(s.read(&mut buf), Ok(0) | Err(_)), "connection closed without a service");

    // The client aborts with RST: `cat` in the guest sees the reset.
    c.send("nc -n -v -l -p 5555 -e cat\n");
    let at =
        c.wait_for("listening on", at, limit).unwrap_or_else(|| panic!("nc not listening:\n{}", c.log()));
    let mut s = TcpStream::connect(&addr).expect("connection");
    s.set_read_timeout(Some(limit)).unwrap();
    s.write_all(b"prima\n").unwrap();
    let mut got = [0u8; 6];
    s.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"prima\n");
    rst(&s);
    drop(s);
    let at = c
        .wait_for("Connection reset by peer", at, limit)
        .unwrap_or_else(|| panic!("the guest did not see the reset:\n{}", c.log()));
    let _ = c.wait_for(SHELL_PROMPT, at, limit).unwrap_or_else(|| panic!("no prompt:\n{}", c.log()));
    c.send("poweroff -f\n");
    assert!(c.finish(Duration::from_secs(60)), "poweroff -f did not stop vetro:\n{}", c.log());
}

/// SO_LINGER at zero: closing sends RST.
fn rst(s: &TcpStream) {
    use std::os::fd::AsRawFd;
    let l = libc::linger { l_onoff: 1, l_linger: 0 };
    // SAFETY: valid descriptor, structure of the size passed.
    unsafe {
        libc::setsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            (&l as *const libc::linger).cast(),
            std::mem::size_of::<libc::linger>() as libc::socklen_t,
        );
    }
}
