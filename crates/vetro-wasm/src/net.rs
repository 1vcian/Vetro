//! Connessioni TCP dal JS verso i servizi del guest (ABI 5): l'inoltro di
//! porte di `vetro-net` (`Stack::host_connect`, come `hostfwd` di QEMU),
//! la base per un client ADB in JS verso adbd sulla 5555 del guest.
//!
//! Aprire, scrivere, chiudere e leggere byte arrivati sono ingressi: passano
//! da `Machine::net` e arrivano al guest prima della prossima istruzione
//! (per il replay di M10 vanno registrati con il numero di istruzione).
//! Lo stato e una lettura senza byte pronti non toccano la macchina.

use vetro_machine::vetro_net::{CloseReason, HostConnState};
use vetro_machine::{HostNetOp, Input, Reply};

use crate::Vm;

/// Codici di stato di [`vetro_net_state`].
pub mod state {
    /// Connessione sconosciuta (o già rilasciata), o macchina senza rete.
    pub const UNKNOWN: u32 = 0;
    pub const CONNECTING: u32 = 1;
    pub const OPEN: u32 = 2;
    pub const CLOSED: u32 = 3;
}

/// Motivi di chiusura in [`vetro_net_state`] (`out[0]`).
pub mod reason {
    pub const NONE: u32 = 0;
    pub const NORMAL: u32 = 1;
    pub const GUEST_RESET: u32 = 2;
    pub const REMOTE_RESET: u32 = 3;
    pub const REFUSED: u32 = 4;
    pub const TIMEOUT: u32 = 5;
}

fn reason_code(r: CloseReason) -> u32 {
    match r {
        CloseReason::Normal => reason::NORMAL,
        CloseReason::GuestReset => reason::GUEST_RESET,
        CloseReason::RemoteReset => reason::REMOTE_RESET,
        CloseReason::Refused => reason::REFUSED,
        CloseReason::Timeout | CloseReason::Idle => reason::TIMEOUT,
    }
}

impl Vm {
    /// Byte del guest pronti per la connessione (senza toccare la macchina).
    fn net_readable(&self, conn: u64) -> usize {
        self.m.net_view(|s| s.host_conn(conn).map_or(0, |i| i.readable)).unwrap_or(0)
    }

    /// La connessione esiste (senza toccare la macchina).
    fn net_known(&self, conn: u64) -> bool {
        self.m.net_view(|s| s.host_conn(conn).is_some()).unwrap_or(false)
    }

    /// Un'operazione sulle connessioni dell'host: ingresso della macchina,
    /// registrato per il replay (M10, ADR 0019).
    fn host(&mut self, op: HostNetOp) -> Reply {
        self.m.input(Input::HostNet(op))
    }
}

/// Apre una connessione TCP verso `guest_port` del guest (10.0.2.15, dal
/// gateway 10.0.2.2); il SYN parte prima della prossima istruzione.
/// Restituisce l'id (> 0), o 0 senza rete o con una porta non valida.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_net_connect(vm: *mut Vm, guest_port: u32) -> u64 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    let Ok(port) = u16::try_from(guest_port) else { return 0 };
    if port == 0 {
        return 0;
    }
    match vm.host(HostNetOp::Connect(port)) {
        Reply::HostConn(Some(id)) => id,
        _ => 0,
    }
}

/// Mette in coda `len` byte per il guest; restituisce quanti ne ha presi
/// (al più 256 KiB in coda: il resto va riproposto dopo un quanto).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_net_send(vm: *mut Vm, conn: u64, src: *const u8, len: usize) -> usize {
    // SAFETY: `vm` viene da `vetro_machine_new`, `src` vale per `len` byte.
    let vm = unsafe { &mut *vm };
    if len == 0 {
        return 0;
    }
    let data = unsafe { core::slice::from_raw_parts(src, len) };
    match vm.host(HostNetOp::Send(conn, data.to_vec())) {
        Reply::Accepted(n) => n as usize,
        _ => 0,
    }
}

/// Copia e consuma al più `cap` byte arrivati dal guest; 0 = niente (la
/// fine del flusso è nello stato).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_net_recv(vm: *mut Vm, conn: u64, dst: *mut u8, cap: usize) -> usize {
    // SAFETY: `vm` viene da `vetro_machine_new`, `dst` vale per `cap` byte.
    let vm = unsafe { &mut *vm };
    if cap == 0 || vm.net_readable(conn) == 0 {
        return 0;
    }
    let buf = unsafe { core::slice::from_raw_parts_mut(dst, cap) };
    match vm.host(HostNetOp::Recv(conn, cap as u64)) {
        Reply::Data(d) => {
            buf[..d.len()].copy_from_slice(&d);
            d.len()
        }
        _ => 0,
    }
}

/// Chiude il verso JS→guest (FIN dopo i byte in coda). 1 = fatto.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_net_shutdown(vm: *mut Vm, conn: u64) -> u32 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    if !vm.net_known(conn) {
        return 0;
    }
    (vm.host(HostNetOp::Shutdown(conn)) == Reply::Done) as u32
}

/// Interrompe la connessione (RST al guest). 1 = fatto.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_net_abort(vm: *mut Vm, conn: u64) -> u32 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    if !vm.net_known(conn) {
        return 0;
    }
    (vm.host(HostNetOp::Abort(conn)) == Reply::Done) as u32
}

/// Dimentica la connessione (se è ancora viva, prima la interrompe). Da
/// chiamare quando lo stato è `CLOSED` e i byte sono stati letti. 1 = fatto.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_net_release(vm: *mut Vm, conn: u64) -> u32 {
    // SAFETY: `vm` viene da `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    if !vm.net_known(conn) {
        return 0;
    }
    (vm.host(HostNetOp::Release(conn)) == Reply::Done) as u32
}

/// Stato della connessione (codici di [`state`]); in `out` (al più `cap`
/// valori): motivo della chiusura ([`reason`]), byte leggibili, spazio per
/// `vetro_net_send`, fine del flusso dal guest (1 = il guest ha chiuso e
/// tutto è stato letto), byte in coda non ancora presi dal guest. Non tocca
/// la macchina.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_net_state(vm: *const Vm, conn: u64, out: *mut u32, cap: usize) -> u32 {
    // SAFETY: `vm` viene da `vetro_machine_new`, `out` vale per `cap` valori.
    let vm = unsafe { &*vm };
    let Some(info) = vm.m.net_view(|s| s.host_conn(conn)).flatten() else { return state::UNKNOWN };
    let (code, why) = match info.state {
        HostConnState::Connecting => (state::CONNECTING, reason::NONE),
        HostConnState::Open => (state::OPEN, reason::NONE),
        HostConnState::Closed(r) => (state::CLOSED, reason_code(r)),
    };
    let sat = |n: usize| n.min(u32::MAX as usize) as u32;
    let v = [why, sat(info.readable), sat(info.writable), info.guest_eof as u32, sat(info.unsent)];
    let n = v.len().min(cap);
    if n > 0 {
        unsafe { core::slice::from_raw_parts_mut(out, n) }.copy_from_slice(&v[..n]);
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{dev, vetro_machine_free, vetro_machine_new_with};

    /// Senza kernel nessuno risponde al SYN: la connessione resta in attesa;
    /// l'interruzione la chiude senza pacchetti se il SYN non è partito.
    #[test]
    fn connessioni_dall_api() {
        let vm = vetro_machine_new_with(64 << 20, 0, 0, dev::NET, 0, 0);
        let none = vetro_machine_new_with(64 << 20, 0, 0, 0, 0, 0);
        unsafe {
            assert_eq!(vetro_net_connect(none, 5555), 0, "senza rete");
            assert_eq!(vetro_net_connect(vm, 0), 0);
            assert_eq!(vetro_net_connect(vm, 70000), 0);
            let c = vetro_net_connect(vm, 5555);
            assert!(c > 0);
            let mut st = [9u32; 5];
            assert_eq!(vetro_net_state(vm, c, st.as_mut_ptr(), 5), state::CONNECTING);
            assert_eq!(st, [reason::NONE, 0, 256 * 1024, 0, 0]);
            assert_eq!(vetro_net_send(vm, c, b"ciao".as_ptr(), 4), 4);
            assert_eq!(vetro_net_state(vm, c, st.as_mut_ptr(), 5), state::CONNECTING);
            assert_eq!(st[4], 4, "in coda");
            let mut buf = [0u8; 8];
            assert_eq!(vetro_net_recv(vm, c, buf.as_mut_ptr(), 8), 0);
            assert_eq!(vetro_net_abort(vm, c), 1);
            assert_eq!(vetro_net_state(vm, c, st.as_mut_ptr(), 1), state::CLOSED);
            assert_eq!(st[0], reason::REMOTE_RESET);
            assert_eq!(vetro_net_release(vm, c), 1);
            assert_eq!(vetro_net_state(vm, c, st.as_mut_ptr(), 5), state::UNKNOWN);
            assert_eq!(vetro_net_shutdown(vm, c), 0);
            vetro_machine_free(vm);
            vetro_machine_free(none);
        }
    }
}
