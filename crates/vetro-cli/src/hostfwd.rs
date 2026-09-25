//! `vetro boot --hostfwd=tcp:[ADDR]:PORTA-[:]PORTA_GUEST`: inoltro di porte
//! dall'host verso servizi TCP del guest, come `hostfwd` della rete user di
//! QEMU (per esempio `adb connect 127.0.0.1:PORTA` verso adbd sulla 5555).
//!
//! I socket veri stanno qui, nel runner nativo, non nel core: un thread per
//! regola accetta le connessioni, un thread per connessione legge dal
//! socket e uno ci scrive. Tutto arriva al ciclo principale su un canale
//! ([`Input`], insieme alla console), e il ciclo lo passa allo stack di
//! rete (`Stack::host_connect`, `host_send`, `host_recv`, …) **tra un quanto
//! di istruzioni e l'altro**, con `Machine::input` e
//! `Input::HostNet` (M10, ADR 0019): è lì che gli ingressi dell'host entrano
//! nella macchina, e con `vetro boot --record` si registrano con il numero
//! d'istruzione, come i byte della console. Il momento in cui arrivano dal
//! socket dipende dall'host: senza registrazione due esecuzioni con
//! `--hostfwd` non sono ripetibili, con la registrazione il replay le rifà
//! identiche (senza connessioni `--hostfwd` non tocca la macchina). Solo le
//! letture che trovano byte pronti diventano ingressi (`net_view` guarda
//! prima), così il log non si riempie di letture vuote.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use vetro_machine::vetro_net::{CloseReason, ConnId, HostConnState};
use vetro_machine::{HostNetOp, Machine, Reply};

/// Un'operazione sulle connessioni dell'host, come ingresso della macchina.
fn host(m: &mut Machine, op: HostNetOp) -> Reply {
    m.input(vetro_machine::Input::HostNet(op))
}

/// Byte letti da un socket e non ancora presi dallo stack oltre i quali il
/// thread di lettura si ferma (contropressione verso il client).
const READ_AHEAD: usize = 1 << 20;
/// Byte presi dallo stack e non ancora scritti sul socket oltre i quali non
/// se ne prendono altri (la finestra del guest si chiude).
const WRITE_BEHIND: usize = 1 << 20;

/// Una regola `--hostfwd`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rule {
    pub host: SocketAddrV4,
    pub guest_port: u16,
}

/// Interpreta `tcp:[ADDR]:PORTA-[IP_GUEST]:PORTA_GUEST` (la sintassi di
/// QEMU). Senza indirizzo l'host ascolta su 127.0.0.1 (QEMU ascolta su tutte
/// le interfacce: qui, per prudenza, solo localhost). L'indirizzo del guest,
/// se c'è, dev'essere 10.0.2.15. `PORTA` 0 = scelta dal sistema.
pub fn parse_rule(s: &str) -> Result<Rule, String> {
    let bad = || format!("--hostfwd={s}: atteso tcp:[ADDR]:PORTA-[IP_GUEST]:PORTA_GUEST");
    let rest = s.strip_prefix("tcp:").ok_or_else(bad)?;
    let (host, guest) = rest.split_once('-').ok_or_else(bad)?;
    let (haddr, hport) = host.rsplit_once(':').ok_or_else(bad)?;
    let (gaddr, gport) = guest.rsplit_once(':').ok_or_else(bad)?;
    let haddr = if haddr.is_empty() { Ipv4Addr::LOCALHOST } else { haddr.parse().map_err(|_| bad())? };
    if !gaddr.is_empty() && gaddr.parse::<Ipv4Addr>() != Ok(Ipv4Addr::new(10, 0, 2, 15)) {
        return Err(format!("--hostfwd={s}: il guest è 10.0.2.15"));
    }
    let hport = hport.parse::<u16>().map_err(|_| bad())?;
    let gport = gport.parse::<u16>().map_err(|_| bad())?;
    if gport == 0 {
        return Err(bad());
    }
    Ok(Rule { host: SocketAddrV4::new(haddr, hport), guest_port: gport })
}

/// Ingressi dell'host per il ciclo principale.
pub enum Input {
    Console(Vec<u8>),
    ConsoleClosed,
    Accepted {
        rule: usize,
        stream: TcpStream,
    },
    Data {
        key: u64,
        data: Vec<u8>,
    },
    /// Il client ha chiuso il suo verso.
    Eof {
        key: u64,
    },
    /// Errore di lettura (di solito un RST del client).
    Broken {
        key: u64,
    },
}

enum WriterMsg {
    Data(Vec<u8>),
    /// FIN verso il client dopo i dati.
    Shutdown,
    /// RST verso il client.
    Abort,
}

/// Un client collegato a una connessione verso il guest.
struct Bridge {
    key: u64,
    id: ConnId,
    /// Byte del client non ancora presi dallo stack.
    pending: VecDeque<u8>,
    read_ahead: Arc<AtomicUsize>,
    write_behind: Arc<AtomicUsize>,
    writer: Sender<WriterMsg>,
    client_eof: bool,
    client_broken: bool,
    shut_to_guest: bool,
    shut_to_client: bool,
}

/// Le regole in ascolto e i client collegati.
pub struct HostFwd {
    rules: Vec<Rule>,
    tx: Sender<Input>,
    bridges: Vec<Bridge>,
    next_key: u64,
}

impl HostFwd {
    /// Apre i socket in ascolto; restituisce l'inoltro e gli indirizzi
    /// effettivi (con la porta scelta se era 0).
    pub fn listen(rules: &[Rule], tx: Sender<Input>) -> std::io::Result<(Self, Vec<SocketAddr>)> {
        let mut addrs = Vec::new();
        for (i, r) in rules.iter().enumerate() {
            let l = TcpListener::bind(r.host)?;
            addrs.push(l.local_addr()?);
            let tx = tx.clone();
            std::thread::spawn(move || {
                for stream in l.incoming() {
                    let Ok(stream) = stream else { continue };
                    if tx.send(Input::Accepted { rule: i, stream }).is_err() {
                        break;
                    }
                }
            });
        }
        Ok((HostFwd { rules: rules.to_vec(), tx, bridges: Vec::new(), next_key: 1 }, addrs))
    }

    /// Un ingresso di rete dal canale (gli altri non lo riguardano).
    pub fn input(&mut self, m: &mut Machine, input: Input) {
        match input {
            Input::Accepted { rule, stream } => self.accept(m, rule, stream),
            Input::Data { key, data } => {
                if let Some(b) = self.bridge(key) {
                    b.pending.extend(data);
                }
            }
            Input::Eof { key } => {
                if let Some(b) = self.bridge(key) {
                    b.client_eof = true;
                }
            }
            Input::Broken { key } => {
                if let Some(b) = self.bridge(key) {
                    b.client_broken = true;
                }
            }
            Input::Console(_) | Input::ConsoleClosed => {}
        }
    }

    fn bridge(&mut self, key: u64) -> Option<&mut Bridge> {
        self.bridges.iter_mut().find(|b| b.key == key)
    }

    fn accept(&mut self, m: &mut Machine, rule: usize, stream: TcpStream) {
        let port = self.rules[rule].guest_port;
        let Reply::HostConn(Some(id)) = host(m, HostNetOp::Connect(port)) else {
            // Senza rete (o senza porte effimere) si chiude subito.
            return;
        };
        let _ = stream.set_nodelay(true);
        let key = self.next_key;
        self.next_key += 1;
        let read_ahead = Arc::new(AtomicUsize::new(0));
        let write_behind = Arc::new(AtomicUsize::new(0));
        let (wtx, wrx) = channel();
        if let Ok(r) = stream.try_clone() {
            let (tx, ra) = (self.tx.clone(), read_ahead.clone());
            std::thread::spawn(move || reader(key, r, tx, ra));
        }
        let wb = write_behind.clone();
        std::thread::spawn(move || writer(stream, wrx, wb));
        self.bridges.push(Bridge {
            key,
            id,
            pending: VecDeque::new(),
            read_ahead,
            write_behind,
            writer: wtx,
            client_eof: false,
            client_broken: false,
            shut_to_guest: false,
            shut_to_client: false,
        });
    }

    /// Scambi tra i client e lo stack, tra un quanto e l'altro. Tocca la
    /// macchina (`Machine::net`) solo se c'è qualcosa da fare.
    pub fn service(&mut self, m: &mut Machine) {
        if self.bridges.is_empty() {
            return;
        }
        let work =
            |b: &Bridge| !b.pending.is_empty() || (b.client_eof && !b.shut_to_guest) || b.client_broken;
        let busy = self.bridges.iter().any(work)
            || m.net_view(|s| {
                self.bridges.iter().any(|b| match s.host_conn(b.id) {
                    None => true,
                    Some(i) => {
                        (i.readable > 0 && b.write_behind.load(Ordering::Acquire) < WRITE_BEHIND)
                            || (i.guest_eof && !b.shut_to_client)
                            || matches!(i.state, HostConnState::Closed(_))
                    }
                })
            })
            .unwrap_or(false);
        if !busy {
            return;
        }
        let mut keep = Vec::with_capacity(self.bridges.len());
        for mut b in core::mem::take(&mut self.bridges) {
            if exchange(m, &mut b) {
                keep.push(b);
            }
        }
        self.bridges = keep;
    }
}

/// Scambi di un client con lo stack; falso quando la connessione è finita.
fn exchange(m: &mut Machine, b: &mut Bridge) -> bool {
    if b.client_broken {
        host(m, HostNetOp::Release(b.id));
        let _ = b.writer.send(WriterMsg::Abort);
        return false;
    }
    let info = |m: &Machine| m.net_view(|s| s.host_conn(b.id)).flatten();
    if !b.pending.is_empty() {
        let room = info(m).map_or(0, |i| i.writable);
        let (a, _) = b.pending.as_slices();
        let offer = &a[..a.len().min(room)];
        if !offer.is_empty() {
            let n = match host(m, HostNetOp::Send(b.id, offer.to_vec())) {
                Reply::Accepted(n) => n as usize,
                _ => 0,
            };
            b.pending.drain(..n);
            b.read_ahead.fetch_sub(n, Ordering::AcqRel);
        }
    }
    if b.client_eof && b.pending.is_empty() && !b.shut_to_guest {
        host(m, HostNetOp::Shutdown(b.id));
        b.shut_to_guest = true;
    }
    while b.write_behind.load(Ordering::Acquire) < WRITE_BEHIND && info(m).is_some_and(|i| i.readable > 0) {
        let Reply::Data(d) = host(m, HostNetOp::Recv(b.id, 64 * 1024)) else { break };
        if d.is_empty() {
            break;
        }
        b.write_behind.fetch_add(d.len(), Ordering::AcqRel);
        let _ = b.writer.send(WriterMsg::Data(d));
    }
    let Some(info) = info(m) else {
        let _ = b.writer.send(WriterMsg::Abort);
        return false;
    };
    if info.guest_eof && !b.shut_to_client {
        let _ = b.writer.send(WriterMsg::Shutdown);
        b.shut_to_client = true;
    }
    match info.state {
        HostConnState::Closed(reason) if info.readable == 0 => {
            // Come slirp: chiusura ordinata anche se il guest non ha il
            // servizio (il client vede la fine del flusso); RST se la
            // connessione è stata interrotta.
            let msg = match reason {
                CloseReason::Normal | CloseReason::Refused => WriterMsg::Shutdown,
                _ => WriterMsg::Abort,
            };
            let _ = b.writer.send(msg);
            host(m, HostNetOp::Release(b.id));
            false
        }
        _ => true,
    }
}

fn reader(key: u64, mut s: TcpStream, tx: Sender<Input>, read_ahead: Arc<AtomicUsize>) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        while read_ahead.load(Ordering::Acquire) >= READ_AHEAD {
            if Arc::strong_count(&read_ahead) == 1 {
                return; // connessione già rilasciata
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        match s.read(&mut buf) {
            Ok(0) => {
                let _ = tx.send(Input::Eof { key });
                return;
            }
            Ok(n) => {
                read_ahead.fetch_add(n, Ordering::AcqRel);
                if tx.send(Input::Data { key, data: buf[..n].to_vec() }).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => {
                let _ = tx.send(Input::Broken { key });
                return;
            }
        }
    }
}

fn writer(mut s: TcpStream, rx: Receiver<WriterMsg>, write_behind: Arc<AtomicUsize>) {
    while let Ok(msg) = rx.recv() {
        match msg {
            WriterMsg::Data(d) => {
                let ok = s.write_all(&d).is_ok();
                write_behind.fetch_sub(d.len(), Ordering::AcqRel);
                if !ok {
                    // Il client se n'è andato: il lettore lo segnala.
                    continue;
                }
            }
            WriterMsg::Shutdown => {
                let _ = s.shutdown(Shutdown::Write);
            }
            WriterMsg::Abort => {
                abort(&s);
                return;
            }
        }
    }
    // Canale chiuso (connessione rilasciata dopo una chiusura ordinata): il
    // socket si chiude qui.
    let _ = s.shutdown(Shutdown::Both);
}

/// Prepara la chiusura con RST (SO_LINGER a zero): il RST parte quando si
/// chiudono entrambi i descrittori (questo e quello del lettore, che la
/// chiusura del verso di lettura sveglia senza mandare nulla sul filo).
fn abort(s: &TcpStream) {
    use std::os::fd::AsRawFd;
    let l = libc::linger { l_onoff: 1, l_linger: 0 };
    // SAFETY: descrittore valido per la durata di `s`, struttura linger
    // valida e della dimensione passata.
    unsafe {
        libc::setsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            (&l as *const libc::linger).cast(),
            std::mem::size_of::<libc::linger>() as libc::socklen_t,
        );
    }
    let _ = s.shutdown(Shutdown::Read);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regole_come_qemu() {
        let r = parse_rule("tcp:127.0.0.1:5555-:5555").unwrap();
        assert_eq!(r, Rule { host: "127.0.0.1:5555".parse().unwrap(), guest_port: 5555 });
        let r = parse_rule("tcp::0-10.0.2.15:22").unwrap();
        assert_eq!(r, Rule { host: "127.0.0.1:0".parse().unwrap(), guest_port: 22 });
        assert!(parse_rule("udp::1-:2").is_err());
        assert!(parse_rule("tcp::1-10.0.2.16:2").is_err());
        assert!(parse_rule("tcp::1-:0").is_err());
        assert!(parse_rule("tcp:1-2").is_err());
    }
}
