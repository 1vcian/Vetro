//! Record & replay in the browser (ABI 8, M10, ADR 0019 and 0023).
//!
//! Recording and replay are those of `vetro_machine` (a single
//! entry point, log with events and keyframes, fingerprint at the end). Here:
//!
//! - the finished log (or loaded from a file) stays in the [`Vm`]; the keyframes
//!   (full snapshots, ~10 MB each) can be **moved out** one by
//!   one ([`vetro_log_keyframe_take`]: JS writes them into OPFS) and
//!   **put back** when needed ([`vetro_log_keyframe_put`]): in the log
//!   only their position remains (instruction, console count);
//! - the log file ([`vetro_log_encode`]) contains the keyframes present at
//!   that moment; [`vetro_log_load`] reads it back;
//! - [`vetro_replay_start`] redoes the recording from the keyframe closest to
//!   an instruction (it must be present): JS then runs as always,
//!   `vetro_run` stops at the events and at the end compares the fingerprint. The
//!   jump to an instruction is the same start followed by `vetro_run` with the
//!   quantum limited up to there (like `Machine::goto`, but with the disks served
//!   by JS along the way);
//! - reading the state at the point reached: registers as text, virtual
//!   and physical memory.
//!
//! During replay the JS inputs are ignored (`Reply::Ignored`);
//! the file manager client is closed (JS reopens it at the end).

use vetro_analysis::timeline::UserInput;
use vetro_machine::record::{EventKind, Keyframe};
use vetro_machine::{Log, RecordOptions, ReplayStatus};

use crate::Vm;
use crate::analysis::{Describer, vm_ref};

/// States of [`vetro_rr_status`].
pub mod rr {
    pub const IDLE: u32 = 0;
    pub const RECORDING: u32 = 1;
    pub const REPLAYING: u32 = 2;
    /// Replay arrived at the end with the same state: identical replay.
    pub const FINISHED: u32 = 3;
    /// Replay stopped on a difference (reason in the message).
    pub const DIVERGED: u32 = 4;
}

/// Codes of [`vetro_replay_start`].
pub mod replay_start {
    pub const OK: u32 = 0;
    /// No log (neither recorded nor loaded).
    pub const NO_LOG: u32 = 1;
    /// The keyframe to start from is out (`vetro_log_keyframe_for` says
    /// which): it must be put back with `vetro_log_keyframe_put`.
    pub const KEYFRAME_MISSING: u32 = 2;
    /// The log doesn't apply to this machine (reason in the message).
    pub const REFUSED: u32 = 3;
}

impl Vm {
    /// After the machine state has changed from outside (restore,
    /// keyframe): unread console output discarded, overlay to
    /// realign in full.
    pub(crate) fn after_state_change(&mut self) {
        self.out.clear();
        self.out_pos = 0;
        for o in self.overlays.iter_mut().flatten() {
            o.full_sync = true;
        }
    }

    /// Replay timeline: the user inputs of the log from `from` on.
    fn timeline_from_log(&mut self, from: usize) {
        self.timeline.clear();
        self.describer = Describer::default();
        let Some(log) = &self.log else { return };
        let mut d = Describer::default();
        for e in &log.events[from.min(log.events.len())..] {
            if let EventKind::Input(i) = &e.kind
                && let Some((kind, label, weak)) = d.describe(i)
            {
                self.timeline.push_input(UserInput::new(e.step, kind, label, weak));
            }
        }
    }

    /// Log without the keyframe bytes (to count their sizes).
    fn log_events_only(log: &Log) -> Log {
        Log {
            config_hash: log.config_hash,
            config: log.config.clone(),
            snapshot_version: log.snapshot_version,
            jit: log.jit,
            keyframe_every: log.keyframe_every,
            start: log.start,
            events: log.events.clone(),
            keyframes: log
                .keyframes
                .iter()
                .map(|k| Keyframe {
                    step: k.step,
                    console_len: k.console_len,
                    console_hash: k.console_hash,
                    snapshot: Vec::new(),
                })
                .collect(),
            end: log.end,
        }
    }

    fn set_log(&mut self, log: Log) {
        self.kf_sizes = log.keyframes.iter().map(|k| k.snapshot.len() as u64).collect();
        self.log = Some(log);
    }
}

/// Starts recording from here: every input from now on goes into the log, with
/// a keyframe every `keyframe_every` instructions (0 = none; in the browser
/// at least the initial one is needed to redo the session, so the page
/// always asks for them). A replay in progress ends.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_record_start(vm: *mut Vm, keyframe_every: u64) {
    let vm = unsafe { vm_ref(vm) };
    vm.replay_active = false;
    vm.m.start_recording(RecordOptions { keyframe_every });
}

/// Ends the recording; the log stays in the machine (replacing
/// the one that was there). 1 done, 0 it wasn't recording.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_record_stop(vm: *mut Vm) -> u32 {
    let vm = unsafe { vm_ref(vm) };
    match vm.m.stop_recording() {
        Some(log) => {
            vm.set_log(log);
            1
        }
        None => 0,
    }
}

/// Recording and replay state (codes of [`rr`]); in `out` (at most
/// `cap`): events recorded so far (or next replay event), events
/// of the log, keyframes of the log, start and end instruction of the log, 1
/// if there is a log. With `DIVERGED` the reason is in the message.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_rr_status(vm: *mut Vm, out: *mut u64, cap: usize) -> u32 {
    let vm = unsafe { vm_ref(vm) };
    let (code, progress) = if vm.m.is_recording() {
        (rr::RECORDING, vm.m.recorded_events() as u64)
    } else if vm.replay_active {
        match vm.m.replay_status() {
            Some(ReplayStatus::Running { next }) => (rr::REPLAYING, *next as u64),
            Some(ReplayStatus::Finished) => {
                (rr::FINISHED, vm.log.as_ref().map_or(0, |l| l.events.len() as u64))
            }
            Some(ReplayStatus::Diverged(d)) => {
                vm.message = d.to_string();
                (rr::DIVERGED, 0)
            }
            None => (rr::IDLE, 0),
        }
    } else {
        (rr::IDLE, 0)
    };
    let l = vm.log.as_ref();
    let v = [
        progress,
        l.map_or(0, |l| l.events.len() as u64),
        l.map_or(0, |l| l.keyframes.len() as u64),
        l.map_or(0, |l| l.start.steps),
        l.map_or(0, |l| l.end.steps),
        u64::from(l.is_some()),
    ];
    unsafe { crate::write_u64s(out, cap, &v) };
    code
}

/// The log file (format of `docs/specs/replay.md`) in the result
/// buffer, with the keyframes present at this moment; 0 without a log.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_log_encode(vm: *mut Vm) -> usize {
    let vm = unsafe { vm_ref(vm) };
    let bytes = vm.log.as_ref().map(Log::encode).unwrap_or_default();
    vm.set_result(bytes)
}

/// Reads a log file (replacing the log that was there). 0 done, 1 invalid
/// file (reason in the message: another version, damaged, truncated).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_log_load(vm: *mut Vm, data: *const u8, len: usize) -> u32 {
    let vm = unsafe { vm_ref(vm) };
    match Log::decode(unsafe { crate::bytes(data, len) }) {
        Ok(log) => {
            vm.set_log(log);
            0
        }
        Err(e) => {
            vm.message = e.to_string();
            1
        }
    }
}

/// Log data in `out` (at most `cap`): start instruction, end instruction,
/// events, keyframes, keyframe interval, 1 if recorded with the JIT, 1
/// if it is of a machine configured like this one, bytes of the events.
/// Returns how many values (0 without a log).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_log_info(vm: *mut Vm, out: *mut u64, cap: usize) -> usize {
    let vm = unsafe { vm_ref(vm) };
    let Some(l) = &vm.log else { return 0 };
    let events_len = Vm::log_events_only(l).encode().len() as u64;
    let v = [
        l.start.steps,
        l.end.steps,
        l.events.len() as u64,
        l.keyframes.len() as u64,
        l.keyframe_every,
        u64::from(l.jit),
        u64::from(l.config_hash == vm.m.config_hash()),
        events_len,
    ];
    unsafe { crate::write_u64s(out, cap, &v) }
}

/// Keyframe `index` in `out` (at most `cap`): instruction, console bytes and hash,
/// snapshot size, 1 if present (0 if it has been
/// taken). Returns how many values (0 = it doesn't exist).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_log_keyframe(vm: *mut Vm, index: u32, out: *mut u64, cap: usize) -> usize {
    let vm = unsafe { vm_ref(vm) };
    let Some(k) = vm.log.as_ref().and_then(|l| l.keyframes.get(index as usize)) else { return 0 };
    let size = vm.kf_sizes.get(index as usize).copied().unwrap_or(0);
    let v = [k.step, k.console_len, k.console_hash, size, u64::from(!k.snapshot.is_empty())];
    unsafe { crate::write_u64s(out, cap, &v) }
}

/// Moves the bytes of keyframe `index` into the result buffer (its position
/// stays in the log); returns the length, 0 if it doesn't exist or is already
/// out.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_log_keyframe_take(vm: *mut Vm, index: u32) -> usize {
    let vm = unsafe { vm_ref(vm) };
    let bytes = vm
        .log
        .as_mut()
        .and_then(|l| l.keyframes.get_mut(index as usize))
        .map(|k| core::mem::take(&mut k.snapshot))
        .unwrap_or_default();
    vm.set_result(bytes)
}

/// Puts back the bytes of keyframe `index` (taken with
/// [`vetro_log_keyframe_take`]). 1 done, 0 wrong index or length
/// different from the keyframe's (if known: a log read back from a file without
/// the keyframe bytes doesn't know it).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_log_keyframe_put(vm: *mut Vm, index: u32, data: *const u8, len: usize) -> u32 {
    let vm = unsafe { vm_ref(vm) };
    let i = index as usize;
    let Some(k) = vm.log.as_mut().and_then(|l| l.keyframes.get_mut(i)) else { return 0 };
    // Size 0 = unknown (log read back without the keyframe bytes).
    let size = vm.kf_sizes.get(i).copied().unwrap_or(0);
    if len == 0 || (size != 0 && size != len as u64) {
        return 0;
    }
    k.snapshot = unsafe { crate::bytes(data, len) }.to_vec();
    if let Some(s) = vm.kf_sizes.get_mut(i) {
        *s = len as u64;
    }
    1
}

/// Index of the keyframe from which the replay towards instruction `step` starts
/// (the last one not beyond it), -1 if none.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_log_keyframe_for(vm: *mut Vm, step: u64) -> i32 {
    let vm = unsafe { vm_ref(vm) };
    let Some(l) = &vm.log else { return -1 };
    l.keyframes.iter().rposition(|k| k.step <= step).map_or(-1, |i| i as i32)
}

/// The log events in JSON: `[{"i":0,"step":N,"kind":"console",
/// "label":"...","weak":false,"user":true}, ...]`; the events that are not
/// user actions (vsock, host network, releases) have `user: false`
/// and the input type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_log_events(vm: *mut Vm) -> usize {
    use std::fmt::Write as _;
    use vetro_analysis::net::json::quote_into;
    use vetro_machine::Input;
    let vm = unsafe { vm_ref(vm) };
    let Some(l) = &vm.log else { return vm.set_result(Vec::new()) };
    let mut d = Describer::default();
    let mut out = String::from("[");
    for (i, e) in l.events.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let (kind, label, weak, user) = match &e.kind {
            EventKind::Opaque { slot } => ("opaque", format!("opaque access to slot {slot:?}"), true, false),
            EventKind::Input(input) => match d.describe(input) {
                Some((k, label, weak)) => (k.name(), label, weak, true),
                None => {
                    let k = match input {
                        Input::Console(_) => "console",
                        Input::Keyboard(_) => "key",
                        Input::Pointer(_) => "pointer",
                        Input::Gpio { .. } => "gpio",
                        Input::Display { .. } => "display",
                        Input::NetFrame(_) | Input::NetLink(_) | Input::HostNet(_) => "network",
                        Input::Vsock(_) => "vsock",
                    };
                    (k, String::new(), true, false)
                }
            },
        };
        let _ = write!(out, "{{\"i\":{i},\"step\":{},\"kind\":\"{kind}\",\"label\":", e.step);
        quote_into(&mut out, &label);
        let _ = write!(out, ",\"weak\":{weak},\"user\":{user}}}");
    }
    out.push(']');
    vm.set_result(out.into_bytes())
}

/// Starts the replay of the log from the last keyframe not beyond `step` (0 =
/// from the start of the recording), which must be present. Codes of
/// [`replay_start`]. From here `vetro_run` redoes the session (it stops at the
/// events; at the end it compares the fingerprint: `vetro_rr_status`); to jump
/// to `step` JS runs with the quantum limited up to there. The capture and the
/// timeline restart (the timeline with the log inputs), the file manager
/// client is closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_replay_start(vm: *mut Vm, step: u64) -> u32 {
    let vm = unsafe { vm_ref(vm) };
    let Some(log) = vm.log.take() else { return replay_start::NO_LOG };
    let step = step.max(log.start.steps);
    let kf = log.keyframes.iter().rposition(|k| k.step <= step);
    if kf.is_some_and(|i| log.keyframes[i].snapshot.is_empty()) {
        vm.log = Some(log);
        vm.message =
            "the keyframe to start from is not present: put it back with vetro_log_keyframe_put".into();
        return replay_start::KEYFRAME_MISSING;
    }
    vm.files = None;
    vm.files_queue.clear();
    // Without useful keyframes start from the current state (it must be the
    // starting one).
    let r = vm.m.replay_from(&log, step);
    let from = kf.map_or(0, |i| log.events.partition_point(|e| e.step < log.keyframes[i].step));
    vm.log = Some(log);
    match r {
        Ok(()) => {
            vm.replay_active = true;
            vm.after_state_change();
            vm.capture_clear();
            vm.timeline_from_log(from);
            replay_start::OK
        }
        Err(d) => {
            vm.message = d.to_string();
            replay_start::REFUSED
        }
    }
}

/// The registers at the point reached (`Machine::registers_text`) in the result
/// buffer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_registers_text(vm: *mut Vm) -> usize {
    let vm = unsafe { vm_ref(vm) };
    let t = vm.m.registers_text();
    vm.set_result(t.into_bytes())
}

/// Reads `len` bytes at virtual address `va` (current tables, current
/// EL, RAM only, without touching the devices) into `dst`. 1 done; 0 if
/// an address is not readable: the first one goes into `fault`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_read_virt(
    vm: *mut Vm,
    va: u64,
    dst: *mut u8,
    len: usize,
    fault: *mut u64,
) -> u32 {
    let vm = unsafe { vm_ref(vm) };
    if len == 0 {
        return 1;
    }
    // SAFETY: `dst` is valid for `len` bytes (API contract).
    let buf = unsafe { core::slice::from_raw_parts_mut(dst, len) };
    match vm.m.read_virt(va, buf) {
        Ok(()) => 1,
        Err(at) => {
            if !fault.is_null() {
                unsafe { *fault = at };
            }
            0
        }
    }
}

/// Translates virtual address `va`; `u64::MAX` if it is not mapped.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_translate(vm: *mut Vm, va: u64) -> u64 {
    unsafe { vm_ref(vm) }.m.translate(va).unwrap_or(u64::MAX)
}

/// Reads `len` bytes of RAM at physical address `pa`. 1 done, 0 outside
/// the RAM.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_read_phys(vm: *mut Vm, pa: u64, dst: *mut u8, len: usize) -> u32 {
    let vm = unsafe { vm_ref(vm) };
    if len == 0 {
        return 1;
    }
    // SAFETY: `dst` is valid for `len` bytes.
    let buf = unsafe { core::slice::from_raw_parts_mut(dst, len) };
    vm.m.read_phys(pa, buf) as u32
}

/// Writes `len` bytes into RAM at physical address `pa` (a bare-metal
/// program or its data, like QEMU's `-device loader`): 1 done, 0 outside the
/// RAM. With `pc` != 0 the running core starts there (EL1h, MMU off: the
/// reset state). An input outside the guest's control: not for recordings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vetro_load_raw(vm: *mut Vm, pa: u64, src: *const u8, len: usize, pc: u64) -> u32 {
    // SAFETY: `vm` comes from `vetro_machine_new*`.
    let vm = unsafe { &mut *vm };
    // SAFETY: `src` is valid for `len` bytes.
    let data = if len == 0 { &[][..] } else { unsafe { core::slice::from_raw_parts(src, len) } };
    if !vm.m.board.borrow().ram.write(pa, data) {
        return 0;
    }
    if pc != 0 {
        vm.m.cpu.pc = pc;
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::*;
    use crate::{
        dev, vetro_console_write, vetro_input_key, vetro_machine_free, vetro_machine_new_with, vetro_run,
    };
    use vetro_analysis::net::json::{self, Value};

    fn new(ram: u64) -> *mut Vm {
        vetro_machine_new_with(ram, 0, 0, dev::KEYBOARD | dev::NET, 0, 0)
    }

    fn result(vm: *mut Vm, n: usize) -> Vec<u8> {
        unsafe { crate::bytes(vetro_result_ptr(vm), n) }.to_vec()
    }

    fn status(vm: *mut Vm) -> (u32, [u64; 6]) {
        let mut v = [0u64; 6];
        let c = unsafe { vetro_rr_status(vm, v.as_mut_ptr(), 6) };
        (c, v)
    }

    /// Runs up to instruction `to` (in quanta of at most 300).
    fn run_to(vm: *mut Vm, to: u64) {
        loop {
            let now = unsafe { (*vm).m.steps };
            if now >= to {
                break;
            }
            unsafe { vetro_run(vm, (to - now).min(300)) };
        }
    }

    /// A machine without a kernel (the reset PC is not in RAM: repeated,
    /// deterministic exceptions) recorded with console and keyboard
    /// inputs and a keyframe every 500 instructions; the log goes through a
    /// file, the keyframes go out and come back in as JS would do with OPFS; the
    /// replay on another machine ends with the same state; the jump to
    /// an instruction finds the registers recorded there; a log of another
    /// machine is refused.
    #[test]
    fn registrazione_e_replay_dall_api() {
        let a = new(64 << 20);
        unsafe {
            vetro_run(a, 1000);
            vetro_record_start(a, 500);
            assert_eq!(status(a).0, rr::RECORDING);
            run_to(a, 1700);
            vetro_console_write(a, b"ab\r".as_ptr(), 3);
            run_to(a, 2000);
            vetro_input_key(a, 30, 1);
            vetro_input_key(a, 30, 0);
            run_to(a, 2500);
            let regs = (*a).m.registers_text();
            run_to(a, 4000);
            assert_eq!(vetro_record_stop(a), 1);
            assert_eq!(vetro_record_stop(a), 0);
            let (code, v) = status(a);
            assert_eq!(code, rr::IDLE);
            // Keyframes at the end of the quanta, right after 500 instructions since the
            // previous one: 1000, 1600, 2300, 2800, 3400, 4000.
            assert_eq!(v[1..], [3, 6, 1000, 4000, 1], "3 inputs, 6 keyframes");

            // Timeline: the console line and the key (not the release).
            let n = vetro_timeline_json(a, 0);
            let t = json::parse(&result(a, n)).unwrap();
            let Some(Value::Array(inputs)) = t.get("inputs") else { panic!() };
            let labels: Vec<&str> = inputs.iter().filter_map(|i| i.get("label")?.as_str()).collect();
            assert_eq!(labels, ["Enter: ab", "A"]);
            assert_eq!(inputs[0].get("step"), Some(&Value::Number("1700".into())));

            let mut info = [0u64; 8];
            assert_eq!(vetro_log_info(a, info.as_mut_ptr(), 8), 8);
            assert_eq!(info[..7], [1000, 4000, 3, 6, 500, 0, 1]);
            let n = vetro_log_events(a);
            let ev = json::parse(&result(a, n)).unwrap();
            let Value::Array(ev) = ev else { panic!() };
            assert_eq!(ev.len(), 3);
            assert_eq!(ev[2].get("user"), Some(&Value::Bool(false)), "key release");
            let n = vetro_log_encode(a);
            let file = result(a, n);
            assert_eq!(&file[..8], b"VETROREC");

            // Another machine: log from the file, keyframes out (in OPFS) and
            // put back only when needed.
            let b = new(64 << 20);
            assert_eq!(vetro_log_load(b, b"rotto".as_ptr(), 5), 1);
            assert_eq!(vetro_log_load(b, file.as_ptr(), file.len()), 0);
            let mut kfs = Vec::new();
            for (i, step) in [1000, 1600, 2300, 2800, 3400, 4000].into_iter().enumerate() {
                let i = i as u32;
                let n = vetro_log_keyframe_take(b, i);
                assert!(n > 0);
                kfs.push(result(b, n));
                let mut k = [0u64; 5];
                assert_eq!(vetro_log_keyframe(b, i, k.as_mut_ptr(), 5), 5);
                assert_eq!((k[0], k[3], k[4]), (step, n as u64, 0));
            }
            assert_eq!(vetro_log_keyframe_take(b, 0), 0, "already out");
            assert_eq!(vetro_log_keyframe_for(b, 0), -1);
            assert_eq!(vetro_log_keyframe_for(b, 2600), 2);
            assert_eq!(vetro_replay_start(b, 0), replay_start::KEYFRAME_MISSING);
            assert_eq!(vetro_log_keyframe_put(b, 0, kfs[0].as_ptr(), kfs[0].len() - 1), 0, "length");
            assert_eq!(vetro_log_keyframe_put(b, 0, kfs[0].as_ptr(), kfs[0].len()), 1);
            assert_eq!(vetro_replay_start(b, 0), replay_start::OK);
            assert_eq!((*b).m.steps, 1000);
            assert_eq!(status(b).0, rr::REPLAYING);
            // During replay the inputs are ignored and don't go into the timeline.
            vetro_input_key(b, 31, 1);
            while status(b).0 == rr::REPLAYING {
                vetro_run(b, 777);
            }
            assert_eq!(status(b).0, rr::FINISHED, "{}", (*b).message);
            assert_eq!((*b).m.steps, 4000);
            assert_eq!((*a).m.digest(), (*b).m.digest());
            let n = vetro_timeline_json(b, 0);
            let t = json::parse(&result(b, n)).unwrap();
            let Some(Value::Array(inputs)) = t.get("inputs") else { panic!() };
            assert_eq!(inputs.len(), 2, "the log inputs, not the ignored one");

            // Jump to 2500 from the keyframe at 2300: the same registers.
            let c = new(64 << 20);
            assert_eq!(vetro_log_load(c, file.as_ptr(), file.len()), 0);
            assert_eq!(vetro_replay_start(c, 2400), replay_start::OK);
            assert_eq!((*c).m.steps, 2300);
            run_to(c, 2500);
            let n = vetro_registers_text(c);
            assert_eq!(String::from_utf8(result(c, n)).unwrap(), regs);
            let mut buf = [0u8; 16];
            let mut fault = 0u64;
            let ram = vetro_platform::map::RAM_BASE;
            assert_eq!(vetro_read_virt(c, ram, buf.as_mut_ptr(), 16, &mut fault), 1, "MMU off: VA = PA");
            assert_eq!(vetro_read_virt(c, 0x1000, buf.as_mut_ptr(), 16, &mut fault), 0);
            assert_eq!(fault, 0x1000);
            assert_eq!(vetro_translate(c, ram + 8), ram + 8);
            assert_eq!(vetro_read_phys(c, ram, buf.as_mut_ptr(), 16), 1);
            assert_eq!(vetro_read_phys(c, 0, buf.as_mut_ptr(), 16), 0);

            // Another RAM: refused with the reason.
            let d = new(128 << 20);
            assert_eq!(vetro_log_load(d, file.as_ptr(), file.len()), 0);
            let mut info = [0u64; 8];
            vetro_log_info(d, info.as_mut_ptr(), 8);
            assert_eq!(info[6], 0, "different configuration");
            assert_eq!(vetro_replay_start(d, 0), replay_start::REFUSED);
            assert!((*d).message.contains("configured differently"), "{}", (*d).message);
            let e = new(64 << 20);
            assert_eq!(vetro_replay_start(e, 0), replay_start::NO_LOG);
            for vm in [a, b, c, d, e] {
                vetro_machine_free(vm);
            }
        }
    }

    /// Capture: without a network it doesn't turn on; with the network the list is empty and
    /// valid, HAR and pcapng are written. Timeline with inputs and effects
    /// annotated by JS.
    #[test]
    fn cattura_e_ispettore_dall_api() {
        let none = vetro_machine_new_with(64 << 20, 0, 0, 0, 0, 0);
        let vm = new(64 << 20);
        unsafe {
            assert_eq!(vetro_capture_set(none, 1), 0);
            assert_eq!(vetro_capture_set(vm, 1), 1);
            vetro_run(vm, 1000);
            let mut st = [9u64; 4];
            assert_eq!(vetro_capture_stats(vm, st.as_mut_ptr(), 4), 4);
            assert_eq!(st, [1, 0, 0, 0]);
            let n = vetro_inspect_requests(vm);
            let l = json::parse(&result(vm, n)).unwrap();
            assert_eq!(l.get("requests"), Some(&Value::Array(vec![])));
            assert_eq!(vetro_inspect_request(vm, 0), 0);
            let n = vetro_inspect_har(vm, 0);
            assert!(json::parse(&result(vm, n)).is_ok());
            let n = vetro_inspect_pcapng(vm, 0);
            assert_eq!(result(vm, n)[..4], [0x0a, 0x0d, 0x0d, 0x0a]);
            let v0 = vetro_timeline_version(vm);
            vetro_timeline_input(vm, 4, 0, b"scrivi /tmp/x".as_ptr(), 13);
            assert_eq!(vetro_timeline_effect(vm, 3, b"CLOSE_WRITE /tmp/x".as_ptr(), 18), 1);
            assert_eq!(vetro_timeline_effect(vm, 99, b"x".as_ptr(), 1), 0);
            assert!(vetro_timeline_version(vm) > v0);
            let n = vetro_timeline_json(vm, 1_000);
            let t = json::parse(&result(vm, n)).unwrap();
            let Some(Value::Array(e)) = t.get("effects") else { panic!() };
            assert_eq!(e[0].get("kind").and_then(Value::as_str), Some("file"));
            assert_eq!(e[0].get("cause"), Some(&Value::Number("0".into())));
            vetro_timeline_clear(vm);
            vetro_capture_clear(vm);
            assert_eq!(vetro_capture_set(vm, 0), 1);
            vetro_result_clear(vm);
            assert!(vetro_result_ptr(vm).is_null());
            vetro_machine_free(vm);
            vetro_machine_free(none);
        }
    }
}
