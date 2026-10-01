//! Litmus tests of the cores in parallel (ADR 0042): what the ARM memory
//! model forbids must never be seen with the cores on host threads, with the
//! interpreter and with the JIT. QEMU cannot be the oracle of a race; these
//! are the classic shapes (herd7's AArch64 names), run many times.
//!
//! - **SB+dmbs** (store buffering): each core stores 1 to its variable, DMB
//!   ISH, then loads the other's. Forbidden: both load 0. A host with store
//!   buffers (x86, ARM) shows it at once if the DMB is not a host fence.
//!
//! - **MP+dmbs** and **MP+rel+acq** (message passing): core 1 stores the
//!   data, then (after DMB ISH, or with STLR) the flag; core 0 loads the flag,
//!   then (after DMB ISH, or with LDAR) the data. Forbidden: flag 1, data 0.
//! - **Exclusive counter**: both cores increment one word with LDXR/STXR;
//!   no increment is lost (the store-exclusive is an atomic
//!   compare-and-exchange against the value the load saw).
//!
//! Every iteration uses fresh variables and starts with a barrier of both
//! cores (LDAXR/STLXR increment, LDAR spin), so the two cores race at the
//! same time.

use vetro_machine::{Core, Devices, Machine, MachineConfig, Stop};
use vetro_platform::map;

const R: u64 = map::RAM_BASE;

/// SB+dmbs (encodings from `tools/a64asm.sh`). Core 0 starts core 1 at
/// `both`; per iteration `i` < N (the word at R+0x800): a barrier on
/// R+0x10000[i], then core 0 `x[i] = 1; dmb ish; r0[i] = y[i]` and core 1
/// `y[i] = 1; dmb ish; r1[i] = x[i]` (x at R+0x20000, y at R+0x30000, r0 at
/// R+0x40000, r1 at R+0x50000, 8 bytes each); after a last barrier core 0
/// powers the machine off.
const SB: [u32; 51] = [
    0xd2a80014, // mov x20, #0x40000000        // =1073741824
    0xd2a10001, // mov x1, #0x8000000          // =134217728
    0x52800042, // mov w2, #0x2                // =2
    0xb9000022, // str w2, [x1]
    0xd2800060, // mov x0, #0x3                // =3
    0xf2b88000, // movk x0, #0xc400, lsl #16
    0xd2800021, // mov x1, #0x1                // =1
    0x10000062, // adr x2, 0x28 <both>
    0xd2800003, // mov x3, #0x0                // =0
    0xd4000002, // hvc #0
    0xd2a80014, // mov x20, #0x40000000        // =1073741824
    0xd53800ad, // mrs x13, MPIDR_EL1
    0x92401dad, // and x13, x13, #0xff
    0xd2800009, // mov x9, #0x0                // =0
    0x91404295, // add x21, x20, #0x10, lsl #12 // =0x10000
    0x91408296, // add x22, x20, #0x20, lsl #12 // =0x20000
    0x9140c297, // add x23, x20, #0x30, lsl #12 // =0x30000
    0x91410298, // add x24, x20, #0x40, lsl #12 // =0x40000
    0x91414299, // add x25, x20, #0x50, lsl #12 // =0x50000
    0xf944029a, // ldr x26, [x20, #0x800]
    0x8b090eaa, // add x10, x21, x9, lsl #3
    0xc85ffd4b, // ldaxr x11, [x10]
    0x9100056b, // add x11, x11, #0x1
    0xc80cfd4b, // stlxr w12, x11, [x10]
    0x35ffffac, // cbnz w12, 0x54 <loop+0x4>
    0xc8dffd4b, // ldar x11, [x10]
    0xf100097f, // cmp x11, #0x2
    0x54ffffcb, // b.lt 0x64 <loop+0x14>
    0xeb1a013f, // cmp x9, x26
    0x54000200, // b.eq 0xb4 <done>
    0x8b090ece, // add x14, x22, x9, lsl #3
    0x8b090eef, // add x15, x23, x9, lsl #3
    0xd2800030, // mov x16, #0x1               // =1
    0xb50000cd, // cbnz x13, 0x9c <core1>
    0xf90001d0, // str x16, [x14]
    0xd5033bbf, // dmb ish
    0xf94001f1, // ldr x17, [x15]
    0xf8297b11, // str x17, [x24, x9, lsl #3]
    0x14000005, // b 0xac <next>
    0xf90001f0, // str x16, [x15]
    0xd5033bbf, // dmb ish
    0xf94001d1, // ldr x17, [x14]
    0xf8297b31, // str x17, [x25, x9, lsl #3]
    0x91000529, // add x9, x9, #0x1
    0x17ffffe8, // b 0x50 <loop>
    0xb500008d, // cbnz x13, 0xc4 <park>
    0xd2800100, // mov x0, #0x8                // =8
    0xf2b08000, // movk x0, #0x8400, lsl #16
    0xd4000002, // hvc #0
    0xd503207f, // wfi
    0x17ffffff, // b 0xc4 <park>
];

/// MP+dmbs: like [`SB`] with core 0 `r0[i] = flag[i]; dmb ish; r1[i] =
/// data[i]` and core 1 `data[i] = 1; dmb ish; flag[i] = 1` (flag at
/// R+0x20000, data at R+0x30000).
const MP_DMB: [u32; 51] = [
    0xd2a80014, // mov x20, #0x40000000        // =1073741824
    0xd2a10001, // mov x1, #0x8000000          // =134217728
    0x52800042, // mov w2, #0x2                // =2
    0xb9000022, // str w2, [x1]
    0xd2800060, // mov x0, #0x3                // =3
    0xf2b88000, // movk x0, #0xc400, lsl #16
    0xd2800021, // mov x1, #0x1                // =1
    0x10000062, // adr x2, 0x28 <both>
    0xd2800003, // mov x3, #0x0                // =0
    0xd4000002, // hvc #0
    0xd2a80014, // mov x20, #0x40000000        // =1073741824
    0xd53800ad, // mrs x13, MPIDR_EL1
    0x92401dad, // and x13, x13, #0xff
    0xd2800009, // mov x9, #0x0                // =0
    0x91404295, // add x21, x20, #0x10, lsl #12 // =0x10000
    0x91408296, // add x22, x20, #0x20, lsl #12 // =0x20000
    0x9140c297, // add x23, x20, #0x30, lsl #12 // =0x30000
    0x91410298, // add x24, x20, #0x40, lsl #12 // =0x40000
    0x91414299, // add x25, x20, #0x50, lsl #12 // =0x50000
    0xf944029a, // ldr x26, [x20, #0x800]
    0x8b090eaa, // add x10, x21, x9, lsl #3
    0xc85ffd4b, // ldaxr x11, [x10]
    0x9100056b, // add x11, x11, #0x1
    0xc80cfd4b, // stlxr w12, x11, [x10]
    0x35ffffac, // cbnz w12, 0x54 <loop+0x4>
    0xc8dffd4b, // ldar x11, [x10]
    0xf100097f, // cmp x11, #0x2
    0x54ffffcb, // b.lt 0x64 <loop+0x14>
    0xeb1a013f, // cmp x9, x26
    0x54000200, // b.eq 0xb4 <done>
    0x8b090ece, // add x14, x22, x9, lsl #3
    0x8b090eef, // add x15, x23, x9, lsl #3
    0xd2800030, // mov x16, #0x1               // =1
    0xb50000ed, // cbnz x13, 0xa0 <writer>
    0xf94001d1, // ldr x17, [x14]
    0xd5033bbf, // dmb ish
    0xf94001f2, // ldr x18, [x15]
    0xf8297b11, // str x17, [x24, x9, lsl #3]
    0xf8297b32, // str x18, [x25, x9, lsl #3]
    0x14000004, // b 0xac <next>
    0xf90001f0, // str x16, [x15]
    0xd5033bbf, // dmb ish
    0xf90001d0, // str x16, [x14]
    0x91000529, // add x9, x9, #0x1
    0x17ffffe8, // b 0x50 <loop>
    0xb500008d, // cbnz x13, 0xc4 <park>
    0xd2800100, // mov x0, #0x8                // =8
    0xf2b08000, // movk x0, #0x8400, lsl #16
    0xd4000002, // hvc #0
    0xd503207f, // wfi
    0x17ffffff, // b 0xc4 <park>
];

/// MP+rel+acq: [`MP_DMB`] with `stlr` for the flag and `ldar` to read it.
const MP_RELACQ: [u32; 49] = [
    0xd2a80014, // mov x20, #0x40000000        // =1073741824
    0xd2a10001, // mov x1, #0x8000000          // =134217728
    0x52800042, // mov w2, #0x2                // =2
    0xb9000022, // str w2, [x1]
    0xd2800060, // mov x0, #0x3                // =3
    0xf2b88000, // movk x0, #0xc400, lsl #16
    0xd2800021, // mov x1, #0x1                // =1
    0x10000062, // adr x2, 0x28 <both>
    0xd2800003, // mov x3, #0x0                // =0
    0xd4000002, // hvc #0
    0xd2a80014, // mov x20, #0x40000000        // =1073741824
    0xd53800ad, // mrs x13, MPIDR_EL1
    0x92401dad, // and x13, x13, #0xff
    0xd2800009, // mov x9, #0x0                // =0
    0x91404295, // add x21, x20, #0x10, lsl #12 // =0x10000
    0x91408296, // add x22, x20, #0x20, lsl #12 // =0x20000
    0x9140c297, // add x23, x20, #0x30, lsl #12 // =0x30000
    0x91410298, // add x24, x20, #0x40, lsl #12 // =0x40000
    0x91414299, // add x25, x20, #0x50, lsl #12 // =0x50000
    0xf944029a, // ldr x26, [x20, #0x800]
    0x8b090eaa, // add x10, x21, x9, lsl #3
    0xc85ffd4b, // ldaxr x11, [x10]
    0x9100056b, // add x11, x11, #0x1
    0xc80cfd4b, // stlxr w12, x11, [x10]
    0x35ffffac, // cbnz w12, 0x54 <loop+0x4>
    0xc8dffd4b, // ldar x11, [x10]
    0xf100097f, // cmp x11, #0x2
    0x54ffffcb, // b.lt 0x64 <loop+0x14>
    0xeb1a013f, // cmp x9, x26
    0x540001c0, // b.eq 0xac <done>
    0x8b090ece, // add x14, x22, x9, lsl #3
    0x8b090eef, // add x15, x23, x9, lsl #3
    0xd2800030, // mov x16, #0x1               // =1
    0xb50000cd, // cbnz x13, 0x9c <writer>
    0xc8dffdd1, // ldar x17, [x14]
    0xf94001f2, // ldr x18, [x15]
    0xf8297b11, // str x17, [x24, x9, lsl #3]
    0xf8297b32, // str x18, [x25, x9, lsl #3]
    0x14000003, // b 0xa4 <next>
    0xf90001f0, // str x16, [x15]
    0xc89ffdd0, // stlr x16, [x14]
    0x91000529, // add x9, x9, #0x1
    0x17ffffea, // b 0x50 <loop>
    0xb500008d, // cbnz x13, 0xbc <park>
    0xd2800100, // mov x0, #0x8                // =8
    0xf2b08000, // movk x0, #0x8400, lsl #16
    0xd4000002, // hvc #0
    0xd503207f, // wfi
    0x17ffffff, // b 0xbc <park>
];

/// Both cores add 1 to the word at R+0x60000 N times with LDXR/STXR (with
/// a plain store to the same page in the loop, so the JIT's software TLB has
/// the page for writing and the store-exclusive runs inside the region as a
/// compare-and-exchange), then meet at R+0x10000 and core 0 powers the
/// machine off.
const ATOM: [u32; 39] = [
    0xd2a80014, // mov x20, #0x40000000        // =1073741824
    0xd2a10001, // mov x1, #0x8000000          // =134217728
    0x52800042, // mov w2, #0x2                // =2
    0xb9000022, // str w2, [x1]
    0xd2800060, // mov x0, #0x3                // =3
    0xf2b88000, // movk x0, #0xc400, lsl #16
    0xd2800021, // mov x1, #0x1                // =1
    0x10000062, // adr x2, 0x28 <both>
    0xd2800003, // mov x3, #0x0                // =0
    0xd4000002, // hvc #0
    0xd2a80014, // mov x20, #0x40000000        // =1073741824
    0xd53800ad, // mrs x13, MPIDR_EL1
    0x92401dad, // and x13, x13, #0xff
    0xf944029a, // ldr x26, [x20, #0x800]
    0x9141828a, // add x10, x20, #0x60, lsl #12 // =0x60000
    0x91404295, // add x21, x20, #0x10, lsl #12 // =0x10000
    0xd2800009, // mov x9, #0x0                // =0
    0x8b0d0d4e, // add x14, x10, x13, lsl #3
    0xf90005c9, // str x9, [x14, #0x8]
    0xc85f7d4b, // ldxr x11, [x10]
    0x9100056b, // add x11, x11, #0x1
    0xc80c7d4b, // stxr w12, x11, [x10]
    0x35ffffac, // cbnz w12, 0x4c <loop+0x4>
    0x91000529, // add x9, x9, #0x1
    0xeb1a013f, // cmp x9, x26
    0x54ffff21, // b.ne 0x48 <loop>
    0xc85ffeab, // ldaxr x11, [x21]
    0x9100056b, // add x11, x11, #0x1
    0xc80cfeab, // stlxr w12, x11, [x21]
    0x35ffffac, // cbnz w12, 0x68 <loop+0x20>
    0xb50000ed, // cbnz x13, 0x94 <park>
    0xc8dffeab, // ldar x11, [x21]
    0xf100097f, // cmp x11, #0x2
    0x54ffffcb, // b.lt 0x7c <loop+0x34>
    0xd2800100, // mov x0, #0x8                // =8
    0xf2b08000, // movk x0, #0x8400, lsl #16
    0xd4000002, // hvc #0
    0xd503207f, // wfi
    0x17ffffff, // b 0x94 <park>
];

fn machine(code: &[u32], n: u64) -> Machine {
    let cfg = MachineConfig { ram_size: 1 << 20, cpus: 2, ..MachineConfig::default() };
    let mut m = Machine::with_devices(&cfg, &Devices::none());
    {
        let b = m.board.borrow();
        for (i, w) in code.iter().enumerate() {
            assert!(b.ram.write(R + 4 * i as u64, &w.to_le_bytes()));
        }
        assert!(b.ram.write(R + 0x800, &n.to_le_bytes()));
    }
    m.cpu.pc = R;
    m
}

fn word(m: &Machine, pa: u64) -> u64 {
    let mut v = [0u8; 8];
    assert!(m.board.borrow().ram.read(pa, &mut v));
    u64::from_le_bytes(v)
}

/// Runs `m` with its two cores on two host threads up to the power-off,
/// with the JIT (wasmtime, threshold `jit`) on both cores or the interpreter.
fn run_parallel(m: &mut Machine, jit: Option<u32>) {
    if let Some(t) = jit {
        m.set_jit(Some(vetro_jit_native::system_jit(t)));
    }
    let cores = m.start_parallel().expect("parallel");
    std::thread::scope(|s| {
        let handles: Vec<_> = cores
            .into_iter()
            .map(|mut c: Core| {
                s.spawn(move || {
                    c.set_jit(jit.map(vetro_jit_native::system_jit));
                    while !c.stopped() {
                        match c.run(1 << 20) {
                            Stop::Budget | Stop::Idle => {}
                            Stop::PowerOff | Stop::Reset => break,
                            other => panic!("core {}: {other:?}", c.index()),
                        }
                    }
                    c.drop_jit();
                    c
                })
            })
            .collect();
        let limit = m.steps + 50_000_000_000;
        let stop = loop {
            match m.run(1 << 20) {
                Stop::Budget | Stop::Idle if m.steps < limit => {}
                other => break other,
            }
        };
        m.request_stop();
        let cores = handles.into_iter().map(|h| h.join().expect("core thread")).collect();
        m.stop_parallel(cores);
        assert_eq!(stop, Stop::PowerOff);
    });
}

/// SB+dmbs: never both loads 0 (`r0 = r1 = 0`), with the interpreter and
/// the JIT; the other three outcomes are allowed.
#[test]
fn store_buffering_with_dmb_is_forbidden() {
    const N: u64 = 4000;
    for jit in [None, Some(1)] {
        let mut seen = [0u32; 4];
        for _ in 0..3 {
            let mut m = machine(&SB, N);
            run_parallel(&mut m, jit);
            for i in 0..N {
                let r0 = word(&m, R + 0x4_0000 + 8 * i);
                let r1 = word(&m, R + 0x5_0000 + 8 * i);
                assert!(r0 <= 1 && r1 <= 1);
                seen[(r0 * 2 + r1) as usize] += 1;
            }
        }
        eprintln!(
            "SB+dmbs (JIT {jit:?}): r0r1 = 00: {}, 01: {}, 10: {}, 11: {}",
            seen[0], seen[1], seen[2], seen[3]
        );
        assert_eq!(seen[0], 0, "SB+dmbs: both loads saw 0 (JIT {jit:?})");
    }
}

/// MP+dmbs and MP+rel+acq: never the flag without the data.
#[test]
fn message_passing_is_ordered() {
    const N: u64 = 4000;
    for (name, code) in [("MP+dmbs", &MP_DMB[..]), ("MP+rel+acq", &MP_RELACQ[..])] {
        for jit in [None, Some(1)] {
            let mut seen = [0u32; 4];
            for _ in 0..3 {
                let mut m = machine(code, N);
                run_parallel(&mut m, jit);
                for i in 0..N {
                    let flag = word(&m, R + 0x4_0000 + 8 * i);
                    let data = word(&m, R + 0x5_0000 + 8 * i);
                    assert!(flag <= 1 && data <= 1);
                    seen[(flag * 2 + data) as usize] += 1;
                }
            }
            eprintln!(
                "{name} (JIT {jit:?}): flag,data = 00: {}, 01: {}, 10: {}, 11: {}",
                seen[0], seen[1], seen[2], seen[3]
            );
            assert_eq!(seen[2], 0, "{name}: the flag without the data (JIT {jit:?})");
        }
    }
}

/// The exclusive counter loses no increment with the cores in parallel.
#[test]
fn exclusive_increments_are_atomic() {
    const N: u64 = 200_000;
    for jit in [None, Some(1)] {
        let mut m = machine(&ATOM, N);
        run_parallel(&mut m, jit);
        assert_eq!(word(&m, R + 0x6_0000), 2 * N, "lost increments (JIT {jit:?})");
    }
}
