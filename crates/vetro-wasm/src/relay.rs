//! The network gateway (ABI 16, ADR 0046, `docs/specs/relay.md`): the frames
//! between the machine's uplink and the gateway, which the page's Worker
//! carries over WebSocket lanes (`web/node/relay.mjs`).
//!
//! `vetro_relay_take` collects what the guest's connections produced for the
//! gateway (an output: taking it does not change the execution);
//! `vetro_relay_push` gives the machine one frame of the gateway (or the
//! host's `ONLINE`/`DOWN`): an input, recorded for replay.

use vetro_machine::{Input, Reply};

use crate::Vm;

/// Codes of [`vetro_relay_push`].
pub mod push {
    pub const DONE: u32 = 0;
    /// Malformed, the wrong direction, or for an unknown connection.
    pub const REJECTED: u32 = 1;
    pub const NO_NETWORK: u32 = 2;
    /// Replay in progress: the frames come from the log.
    pub const IGNORED: u32 = 3;
    /// Recording with the machine waiting for a disk: applied later.
    pub const DEFERRED: u32 = 4;
}

/// Puts the frames for the gateway in the result buffer, each one as a `u32`
/// little-endian length and the frame; returns the total length (0 = none).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_relay_take(vm: *mut Vm) -> usize {
    // SAFETY: `vm` comes from `vetro_machine_new`.
    let vm = unsafe { &mut *vm };
    let frames = vm.m.relay_take();
    if frames.is_empty() {
        return 0;
    }
    let mut out = Vec::with_capacity(frames.iter().map(|f| f.len() + 4).sum());
    for f in frames {
        out.extend((f.len() as u32).to_le_bytes());
        out.extend(f);
    }
    vm.set_result(out)
}

/// Gives the machine one frame (`len` bytes) of the gateway: codes of [`push`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_relay_push(vm: *mut Vm, src: *const u8, len: usize) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`, `src` is valid for `len` bytes.
    let vm = unsafe { &mut *vm };
    let frame = if len == 0 { Vec::new() } else { unsafe { core::slice::from_raw_parts(src, len) }.to_vec() };
    match vm.m.input(Input::Relay(frame)) {
        Reply::Done => push::DONE,
        Reply::NoDevice => push::NO_NETWORK,
        Reply::Ignored => push::IGNORED,
        Reply::Deferred => push::DEFERRED,
        _ => push::REJECTED,
    }
}

/// The uplink's state: 1 online, 0 offline, 2 without a network. In `out`
/// (at most `cap`): connections through the gateway, frames not yet taken,
/// connections sent, refused, DNS queries sent, UDP datagrams dropped,
/// frames that did not apply. It does not touch the machine.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_relay_state(vm: *const Vm, out: *mut u32, cap: usize) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new`, `out` is valid for `cap` values.
    let vm = unsafe { &*vm };
    let Some((online, v)) = vm.m.net_view(|s| {
        let u = s.upstream();
        let st = u.stats();
        let sat = |n: u64| n.min(u64::from(u32::MAX)) as u32;
        let v = [
            sat(u.gateway_connections() as u64),
            sat(u.outbox_len() as u64),
            sat(st.connects),
            sat(st.refused),
            sat(st.dns_queries),
            sat(st.udp_dropped),
            sat(st.ignored),
        ];
        (u.online(), v)
    }) else {
        return 2;
    };
    let n = v.len().min(cap);
    if n > 0 {
        unsafe { core::slice::from_raw_parts_mut(out, n) }.copy_from_slice(&v[..n]);
    }
    online as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::vetro_result_ptr;
    use crate::{dev, vetro_machine_free, vetro_machine_new_with};
    use vetro_machine::vetro_net::{Frame, Side};

    #[test]
    fn frames_in_and_out() {
        let vm = vetro_machine_new_with(64 << 20, 0, 0, dev::NET, 0, 0);
        let none = vetro_machine_new_with(64 << 20, 0, 0, 0, 0, 0);
        let online = Frame::Online(true).encode_from(Side::Gateway);
        unsafe {
            let mut st = [9u32; 7];
            assert_eq!(vetro_relay_state(vm, st.as_mut_ptr(), 7), 0);
            assert_eq!(st, [0; 7]);
            assert_eq!(vetro_relay_push(vm, online.as_ptr(), online.len()), push::DONE);
            assert_eq!(vetro_relay_state(vm, st.as_mut_ptr(), 0), 1);
            assert_eq!(vetro_relay_push(vm, online.as_ptr(), 3), push::REJECTED);
            assert_eq!(vetro_relay_push(vm, core::ptr::null(), 0), push::REJECTED);
            let ack = Frame::Ack { id: 5, n: 1 }.encode_from(Side::Gateway);
            assert_eq!(vetro_relay_push(vm, ack.as_ptr(), ack.len()), push::REJECTED);
            assert_eq!(vetro_relay_state(vm, st.as_mut_ptr(), 7), 1);
            assert_eq!(st[6], 1, "the ACK reached the uplink and did not apply");
            assert_eq!(vetro_relay_take(vm), 0);
            assert!(vetro_result_ptr(vm).is_null());
            assert_eq!(vetro_relay_push(none, online.as_ptr(), online.len()), push::NO_NETWORK);
            assert_eq!(vetro_relay_state(none, st.as_mut_ptr(), 7), 2);
            vetro_machine_free(vm);
            vetro_machine_free(none);
        }
    }
}
