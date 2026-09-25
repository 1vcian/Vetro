//! `vetro boot --hostfwd=tcp:127.0.0.1:0-:5555`: un vero socket in ascolto
//! sull'host inoltra le connessioni al servizio del guest (BusyBox
//! `nc -l -p 5555 -e cat`, l'eco), come `hostfwd` di QEMU.
//!
//! - eco di 200 KB da un `TcpStream` del test, chiusura ordinata nei due
//!   versi (il test chiude il suo verso e legge fino alla fine);
//! - senza servizio nel guest la connessione si chiude subito (come slirp);
//! - un client che interrompe con RST: il servizio del guest vede il reset.
//!
//! In release (`cargo test --release -p vetro-cli`): in debug l'interprete è
//! troppo lento e il test si salta.

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
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "avvio sotto Vetro solo in release");
    }
    let Some((image, initrd)) = guest_kernel() else {
        return skip_or_fail("VETRO_REQUIRE_GUEST_KERNEL", "target/guest-kernel mancante");
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
    let (at, line) = c
        .wait_line("vetro: hostfwd tcp ", 0, limit)
        .unwrap_or_else(|| panic!("niente hostfwd:\n{}", c.log()));
    let addr = line["vetro: hostfwd tcp ".len()..].split(' ').next().unwrap().to_string();
    assert!(line.ends_with("-> 10.0.2.15:5555"), "{line}");
    assert!(addr.starts_with("127.0.0.1:"), "{addr}");
    let at = c.wait_for(SHELL_PROMPT, at, limit).unwrap_or_else(|| panic!("niente shell:\n{}", c.log()));
    c.send("udhcpc -i eth0 -n -q >/dev/null 2>&1; nc -n -v -l -p 5555 -e cat\n");
    let at = c.wait_for("listening on", at, limit).unwrap_or_else(|| panic!("nc non ascolta:\n{}", c.log()));

    // Eco di 200 KB: un thread scrive e chiude il suo verso, qui si legge
    // fino alla fine del flusso.
    let data = payload();
    let mut s = TcpStream::connect(&addr).expect("connessione a --hostfwd");
    s.set_read_timeout(Some(limit)).unwrap();
    let mut w = s.try_clone().unwrap();
    let out = data.clone();
    let writer = std::thread::spawn(move || {
        w.write_all(&out).unwrap();
        w.shutdown(std::net::Shutdown::Write).unwrap();
    });
    let mut echo = Vec::new();
    s.read_to_end(&mut echo).expect("eco");
    writer.join().unwrap();
    assert_eq!(echo.len(), data.len(), "byte dell'eco");
    assert!(echo == data, "eco di 200 KB diversa");
    let (at, conn) =
        c.wait_line("connect to ", at, limit).unwrap_or_else(|| panic!("niente riga di nc:\n{}", c.log()));
    assert!(conn.starts_with("connect to 10.0.2.15:5555 from 10.0.2.2:"), "{conn}");
    let at = c.wait_for(SHELL_PROMPT, at, limit).unwrap_or_else(|| panic!("nc non è uscito:\n{}", c.log()));

    // Nessun servizio: il guest risponde RST, il client vede la chiusura.
    let mut s = TcpStream::connect(&addr).expect("connessione");
    s.set_read_timeout(Some(limit)).unwrap();
    let mut buf = [0u8; 16];
    assert!(matches!(s.read(&mut buf), Ok(0) | Err(_)), "connessione chiusa senza servizio");

    // Il client interrompe con RST: `cat` nel guest vede il reset.
    c.send("nc -n -v -l -p 5555 -e cat\n");
    let at = c.wait_for("listening on", at, limit).unwrap_or_else(|| panic!("nc non ascolta:\n{}", c.log()));
    let mut s = TcpStream::connect(&addr).expect("connessione");
    s.set_read_timeout(Some(limit)).unwrap();
    s.write_all(b"prima\n").unwrap();
    let mut got = [0u8; 6];
    s.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"prima\n");
    rst(&s);
    drop(s);
    let at = c
        .wait_for("Connection reset by peer", at, limit)
        .unwrap_or_else(|| panic!("il guest non ha visto il reset:\n{}", c.log()));
    let _ = c.wait_for(SHELL_PROMPT, at, limit).unwrap_or_else(|| panic!("niente prompt:\n{}", c.log()));
    c.send("poweroff -f\n");
    assert!(c.finish(Duration::from_secs(60)), "poweroff -f non ha fermato vetro:\n{}", c.log());
}

/// SO_LINGER a zero: la chiusura manda RST.
fn rst(s: &TcpStream) {
    use std::os::fd::AsRawFd;
    let l = libc::linger { l_onoff: 1, l_linger: 0 };
    // SAFETY: descrittore valido, struttura della dimensione passata.
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
