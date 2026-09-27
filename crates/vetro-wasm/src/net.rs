//! TCP connections from JS to the guest's services (ABI 5): the port
//! forwarding of `vetro-net` (`Stack::host_connect`, like QEMU's `hostfwd`),
//! the basis for an ADB client in JS towards adbd on the guest's 5555.
//!
//! Opening, writing, closing and reading arrived bytes are inputs: they go
//! through `Machine::net` and reach the guest before the next instruction
//! (for M10's replay they must be recorded with the instruction count).
//! The state and a read with no bytes ready don't touch the machine.

use vetro_machine::vetro_net::{CloseReason, HostConnState};
use vetro_machine::{HostNetOp, Input, Reply};

use crate::Vm;

/// Status codes of [`vetro_net_state`].
pub mod state {
    /// Unknown connection (or already released), or machine without a network.
    pub const UNKNOWN: u32 = 0;
    pub const CONNECTING: u32 = 1;
    pub const OPEN: u32 = 2;
    pub const CLOSED: u32 = 3;
}

/// Close reasons in [`vetro_net_state`] (`out[0]`).
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
    /// Guest bytes ready for the connection (without touching the machine).
    fn net_readable(&self, conn: u64) -> usize {
        self.m.net_view(|s| s.host_conn(conn).map_or(0, |i| i.readable)).unwrap_or(0)
    }

    /// The connection exists (without touching the machine).
    fn net_known(&self, conn: u64) -> bool {
        self.m.net_view(|s| s.host_conn(conn).is_some()).unwrap_or(false)
    }

    /// An operation on the host connections: a machine input,
    /// recorded for replay (M10, ADR 0019).
    fn host(&mut self, op: HostNetOp) -> Reply {
        self.m.input(Input::HostNet(op))
    }
}

/// Opens a TCP connection to the guest's `guest_port` (10.0.2.15, from the
/// gateway 10.0.2.2); the SYN leaves before the next instruction.
/// Returns the id (> 0), or 0 without a network or with an invalid port.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_net_connect(vm: *mut Vm, guest_port: u32) -> u64 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
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

/// Queues `len` bytes for the guest; returns how many it took
/// (at most 256 KiB queued: the rest must be offered again after a quantum).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_net_send(vm: *mut Vm, conn: u64, src: *const u8, len: usize) -> usize {
    // SAFETY: `vm` comes from `vetro_machine_new`, `src` is valid for `len` bytes.
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

/// Copies and consumes at most `cap` bytes arrived from the guest; 0 = nothing (the
/// end of the stream is in the state).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_net_recv(vm: *mut Vm, conn: u64, dst: *mut u8, cap: usize) -> usize {
    // SAFETY: `vm` comes from `vetro_machine_new`, `dst` is valid for `cap` bytes.
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

/// Closes the JS→guest direction (FIN after the queued bytes). 1 = done.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_net_shutdown(vm: *mut Vm, conn: u64) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    if !vm.net_known(conn) {
        return 0;
    }
    (vm.host(HostNetOp::Shutdown(conn)) == Reply::Done) as u32
}

/// Aborts the connection (RST to the guest). 1 = done.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_net_abort(vm: *mut Vm, conn: u64) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    if !vm.net_known(conn) {
        return 0;
    }
    (vm.host(HostNetOp::Abort(conn)) == Reply::Done) as u32
}

/// Forgets the connection (if it is still alive, it aborts it first). To be
/// called when the state is `CLOSED` and the bytes have been read. 1 = done.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_net_release(vm: *mut Vm, conn: u64) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    if !vm.net_known(conn) {
        return 0;
    }
    (vm.host(HostNetOp::Release(conn)) == Reply::Done) as u32
}

/// State of the connection (codes of [`state`]); in `out` (at most `cap`
/// values): close reason ([`reason`]), readable bytes, room for
/// `vetro_net_send`, end of stream from the guest (1 = the guest has closed and
/// everything has been read), queued bytes not yet taken by the guest. It doesn't touch
/// the machine.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_net_state(vm: *const Vm, conn: u64, out: *mut u32, cap: usize) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`, `out` is valid for `cap` values.
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

    /// Without a kernel nobody answers the SYN: the connection keeps waiting;
    /// the abort closes it without packets if the SYN hasn't left.
    #[test]
    fn connessioni_dall_api() {
        let vm = vetro_machine_new_with(64 << 20, 0, 0, dev::NET, 0, 0);
        let none = vetro_machine_new_with(64 << 20, 0, 0, 0, 0, 0);
        unsafe {
            assert_eq!(vetro_net_connect(none, 5555), 0, "without a network");
            assert_eq!(vetro_net_connect(vm, 0), 0);
            assert_eq!(vetro_net_connect(vm, 70000), 0);
            let c = vetro_net_connect(vm, 5555);
            assert!(c > 0);
            let mut st = [9u32; 5];
            assert_eq!(vetro_net_state(vm, c, st.as_mut_ptr(), 5), state::CONNECTING);
            assert_eq!(st, [reason::NONE, 0, 256 * 1024, 0, 0]);
            assert_eq!(vetro_net_send(vm, c, b"ciao".as_ptr(), 4), 4);
            assert_eq!(vetro_net_state(vm, c, st.as_mut_ptr(), 5), state::CONNECTING);
            assert_eq!(st[4], 4, "queued");
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
