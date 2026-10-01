//! Several cores on one thread, deterministic (ADR 0041, step 2 of ADR 0038).
//!
//! The machine runs one core at a time, in **turns** of at most
//! [`QUANTUM`] clock steps, in round robin over the cores that are on. The
//! running core lives in `Machine::cpu` (and its interpreter state in
//! `Machine::interp`, `Machine::wfi_pending`), the others are parked here; a
//! switch swaps them. Everything is a function of the clock: a turn ends at a
//! fixed clock value, at a WFE or YIELD (QEMU's `EXCP_YIELD` with
//! single-threaded TCG) or at a WFI with nothing to do, never because of where
//! the host cuts its quanta. So recording and replay stay exact.
//!
//! **Time.** The clock (`Machine::steps`) advances by one for every
//! instruction of any core, and by the time a waiting core skips; the
//! counter (CNTPCT) is that of `steps / n`. Every core runs at the nominal
//! 100 MHz of guest time when all are busy, time is the same for all of them
//! and never goes back from one core to the next (Linux's timekeeping reads
//! the counter on any core). With one core this is exactly the machine of
//! ADR 0011.
//!
//! **Memory and TLB.** One MMU (TLB) and one JIT for all the cores: entries
//! are tagged by ASID and by table base as before, so a core never uses a
//! translation it could not have walked itself (Linux gives one ASID to one
//! address space on every core), and every TLBI, local or broadcast, reaches
//! every core at once (over-invalidating is always allowed). Exclusives
//! compare the value (QEMU's approach), so a store by another core between
//! LDXR and STXR makes the STXR fail.

use vetro_cpu::{Cpu, SysConfig};
use vetro_jit::Next;
use vetro_platform::gic::cpu_affinity;
use vetro_snapshot::{Reader, Writer};

/// Clock steps of one turn: 65536 (0.65 ms of guest time with two cores).
/// A fixed constant, part of the machine's behaviour (changing it changes
/// the interleaving, hence every SMP recording).
pub const QUANTUM: u64 = 1 << 16;

/// Most cores (one GIC cluster, [`vetro_platform::Gic::with_cpus`]).
pub const MAX_CPUS: u32 = 16;

/// MPIDR_EL1 of core `i` on QEMU virt with a GICv3: bit 31 (RES1) and the
/// affinity.
pub fn mpidr(i: usize) -> u64 {
    0x8000_0000 | cpu_affinity(i)
}

/// System-mode configuration of core `i`.
pub fn sys_config(i: usize) -> SysConfig {
    SysConfig { mpidr: mpidr(i), ..SysConfig::default() }
}

/// A core that is not running.
#[derive(Clone, Debug)]
pub(crate) struct Vcpu {
    pub cpu: Cpu,
    pub interp: Next,
    pub wfi_pending: bool,
    /// Powered on (PSCI): secondary cores start off, CPU_ON turns them on.
    pub on: bool,
}

impl Vcpu {
    /// Core `i` in its reset state, off.
    pub fn off(i: usize) -> Self {
        let mut cpu = Cpu::new();
        cpu.reset_system(sys_config(i));
        Vcpu { cpu, interp: Next::Jit, wfi_pending: false, on: false }
    }
}

/// The parked cores and the round robin.
#[derive(Clone, Debug)]
pub(crate) struct Smp {
    /// One entry per core; the running core's entry holds a stale copy
    /// (its live state is in the machine) but its `on` is current.
    pub vcpus: Vec<Vcpu>,
    /// The running core.
    pub cur: usize,
    /// Clock value at which the running core's turn ends.
    pub turn_end: u64,
}

impl Smp {
    pub fn new(n: usize) -> Self {
        let mut vcpus: Vec<Vcpu> = (0..n).map(Vcpu::off).collect();
        vcpus[0].on = true;
        Smp { vcpus, cur: 0, turn_end: QUANTUM }
    }

    /// The next core that is on after the running one (the running one
    /// itself if it is the only one), or none.
    pub fn next_on(&self) -> Option<usize> {
        let n = self.vcpus.len();
        (1..=n).map(|d| (self.cur + d) % n).find(|&i| self.vcpus[i].on)
    }

    /// The core with affinity `target` (exactly, like QEMU's
    /// `arm_get_cpu_by_id`).
    pub fn by_affinity(&self, target: u64) -> Option<usize> {
        (0..self.vcpus.len()).find(|&i| cpu_affinity(i) == target)
    }

    /// Everything but the running core's registers (they are the `CPU `
    /// section): the round robin, power states and the parked cores.
    pub fn save(&self, w: &mut Writer) {
        w.len_of(self.vcpus.len());
        w.u64(self.cur as u64);
        w.u64(self.turn_end);
        for (i, v) in self.vcpus.iter().enumerate() {
            w.bool(v.on);
            if i != self.cur {
                w.bool(v.wfi_pending);
                w.put(&v.cpu);
            }
        }
    }

    pub fn restore(&mut self, r: &mut Reader<'_>) -> vetro_snapshot::Result<()> {
        r.expect_u64("cores", self.vcpus.len() as u64)?;
        let cur = r.u64()? as usize;
        if cur >= self.vcpus.len() {
            return Err(vetro_snapshot::Error::invalid(format!("running core {cur}")));
        }
        self.cur = cur;
        self.turn_end = r.u64()?;
        for (i, v) in self.vcpus.iter_mut().enumerate() {
            v.on = r.bool()?;
            v.interp = Next::Jit;
            if i != cur {
                v.wfi_pending = r.bool()?;
                r.get(&mut v.cpu)?;
            }
        }
        if !self.vcpus[cur].on {
            return Err(vetro_snapshot::Error::invalid("the running core is off"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Devices, Machine, MachineConfig, Stop};
    use vetro_platform::map;

    const R: u64 = map::RAM_BASE;

    /// Two cores meet (encodings from `tools/a64asm.sh`). Core 0 sets up the
    /// GIC, starts core 1 with PSCI CPU_ON (context 0x1234) and tries again
    /// (ALREADY_ON), asks AFFINITY_INFO, then both increment the word at R+0x800
    /// 20000 times with LDXR/STXR; core 1 then stores a flag with STLR and sends
    /// SGI 1 to core 0, which waits for the flag with LDAR + WFE, takes the
    /// SGI from ICC_IAR1_EL1 and powers the machine off (SYSTEM_OFF). Results
    /// at R+0x810.. (see `results`).
    const PROBE: [u32; 78] = [
        0xd2a80014, // mov x20, #0x40000000        // =1073741824
        0x91202295, // add x21, x20, #0x808
        0xd2a10001, // mov x1, #0x8000000          // =134217728
        0x52800042, // mov w2, #0x2                // =2
        0xb9000022, // str w2, [x1]
        0x9400003a, // bl 0xfc <gicr_init>
        0xd2800060, // mov x0, #0x3                // =3
        0xf2b88000, // movk x0, #0xc400, lsl #16
        0xd2800021, // mov x1, #0x1                // =1
        0x100003e2, // adr x2, 0xa0 <secondary>
        0xd2824683, // mov x3, #0x1234             // =4660
        0xd4000002, // hvc #0
        0xf9040a80, // str x0, [x20, #0x810]
        0xd2800060, // mov x0, #0x3                // =3
        0xf2b88000, // movk x0, #0xc400, lsl #16
        0xd2800021, // mov x1, #0x1                // =1
        0x10000302, // adr x2, 0xa0 <secondary>
        0xd4000002, // hvc #0
        0xf9040e80, // str x0, [x20, #0x818]
        0xd2800080, // mov x0, #0x4                // =4
        0xf2b88000, // movk x0, #0xc400, lsl #16
        0xd2800021, // mov x1, #0x1                // =1
        0xd2800002, // mov x2, #0x0                // =0
        0xd4000002, // hvc #0
        0xf9041280, // str x0, [x20, #0x820]
        0x9400001d, // bl 0xd8 <incr>
        0xc8dffea5, // ldar x5, [x21]
        0xb5000065, // cbnz x5, 0x78 <got_flag>
        0xd503205f, // wfe
        0x17fffffd, // b 0x68 <wait_flag>
        0xd538cc06, // mrs x6, ICC_IAR1_EL1
        0xf10004df, // cmp x6, #0x1
        0x54ffffc1, // b.ne 0x78 <got_flag>
        0xd518cc26, // msr ICC_EOIR1_EL1, x6
        0xf9041686, // str x6, [x20, #0x828]
        0xd53be047, // mrs x7, CNTVCT_EL0
        0xf9041a87, // str x7, [x20, #0x830]
        0xd2800100, // mov x0, #0x8                // =8
        0xf2b08000, // movk x0, #0x8400, lsl #16
        0xd4000002, // hvc #0
        0xd2a80014, // mov x20, #0x40000000        // =1073741824
        0x91202295, // add x21, x20, #0x808
        0xf9042280, // str x0, [x20, #0x840]
        0xd53800a7, // mrs x7, MPIDR_EL1
        0xf9042687, // str x7, [x20, #0x848]
        0x94000012, // bl 0xfc <gicr_init>
        0x94000008, // bl 0xd8 <incr>
        0xd2800025, // mov x5, #0x1                // =1
        0xc89ffea5, // stlr x5, [x21]
        0xd2800028, // mov x8, #0x1                // =1
        0xf2a02008, // movk x8, #0x100, lsl #16
        0xd518cba8, // msr ICC_SGI1R_EL1, x8
        0xd503207f, // wfi
        0x17ffffff, // b 0xd0 <park>
        0xd289c409, // mov x9, #0x4e20             // =20000
        0x9120028a, // add x10, x20, #0x800
        0xc85f7d4b, // ldxr x11, [x10]
        0x9100056b, // add x11, x11, #0x1
        0xc80c7d4b, // stxr w12, x11, [x10]
        0x35ffffac, // cbnz w12, 0xe0 <incr_loop>
        0xf1000529, // subs x9, x9, #0x1
        0x54ffff61, // b.ne 0xe0 <incr_loop>
        0xd65f03c0, // ret
        0xd53800ad, // mrs x13, MPIDR_EL1
        0x92401dad, // and x13, x13, #0xff
        0xd2a1014e, // mov x14, #0x80a0000         // =134873088
        0x8b0d45ce, // add x14, x14, x13, lsl #17
        0xb90015df, // str wzr, [x14, #0x14]
        0x914041ce, // add x14, x14, #0x10, lsl #12 // =0x10000
        0x1280000f, // mov w15, #-0x1              // =-1
        0xb90081cf, // str w15, [x14, #0x80]
        0x5280004f, // mov w15, #0x2               // =2
        0xb90101cf, // str w15, [x14, #0x100]
        0xd2801e0f, // mov x15, #0xf0              // =240
        0xd518460f, // msr ICC_PMR_EL1, x15
        0xd280002f, // mov x15, #0x1               // =1
        0xd518ccef, // msr ICC_IGRPEN1_EL1, x15
        0xd65f03c0, // ret
    ];

    fn machine(cpus: u32) -> Machine {
        let cfg = MachineConfig { ram_size: 1 << 20, cpus, ..MachineConfig::default() };
        let mut m = Machine::with_devices(&cfg, &Devices::none());
        {
            let b = m.board.borrow_mut();
            for (i, w) in PROBE.iter().enumerate() {
                assert!(b.ram.write(R + 4 * i as u64, &w.to_le_bytes()));
            }
        }
        m.cpu.pc = R;
        m
    }

    fn word(m: &Machine, off: u64) -> u64 {
        let mut v = [0u8; 8];
        assert!(m.board.borrow().ram.read(R + off, &mut v));
        u64::from_le_bytes(v)
    }

    /// Runs in host quanta of `quantum` up to the power-off.
    fn to_power_off(m: &mut Machine, quantum: u64) {
        let limit = m.steps + 50_000_000;
        loop {
            match m.run(quantum) {
                Stop::Budget if m.steps < limit => {}
                Stop::PowerOff => return,
                other => panic!("{other:?} at {} steps", m.steps),
            }
        }
    }

    #[test]
    fn two_cores_meet() {
        let mut m = machine(2);
        assert!(!m.cpu_on(1), "secondary cores start off");
        to_power_off(&mut m, 1 << 20);
        assert_eq!(word(&m, 0x800), 40_000, "every exclusive increment of both cores");
        assert_eq!(word(&m, 0x808), 1, "flag from core 1");
        assert_eq!(word(&m, 0x810) as i64, 0, "CPU_ON: SUCCESS");
        assert_eq!(word(&m, 0x818) as i64, -4, "CPU_ON again: ALREADY_ON");
        assert_eq!(word(&m, 0x820), 0, "AFFINITY_INFO: ON");
        assert_eq!(word(&m, 0x828), 1, "SGI 1 from core 1");
        assert_eq!(word(&m, 0x840), 0x1234, "x0 = context of CPU_ON");
        assert_eq!(word(&m, 0x848), 0x8000_0001, "MPIDR_EL1 of core 1");
        assert!(m.cpu_on(1));
        assert_eq!(m.cpu_of(1).map(|c| c.sys.cfg.mpidr), Some(0x8000_0001));
        // The counter is that of steps / 2: core 0 read it near the end.
        let cntvct = word(&m, 0x830);
        let half = m.steps / 2;
        assert!(cntvct <= half / 8 * 5 + 5 && cntvct + 1000 > half / 8 * 5, "{cntvct} vs {} steps", m.steps);
    }

    /// Where the host cuts its quanta does not change the interleaving: the
    /// same state at the power-off with quanta of 1, 7919 and 2^40 steps.
    #[test]
    fn host_quanta_do_not_change_the_interleaving() {
        let digest = |q: u64| {
            let mut m = machine(2);
            to_power_off(&mut m, q);
            (m.steps, m.digest())
        };
        let large = digest(1 << 40);
        assert_eq!(digest(7919), large);
        assert_eq!(digest(1), large);
    }

    /// A snapshot halfway (core 1 parked, mid-turn) restores to the same
    /// continuation.
    #[test]
    fn snapshot_with_two_cores() {
        let mut a = machine(2);
        assert_eq!(a.run(100_003), Stop::Budget);
        let snap = a.save();
        let mut b = machine(2);
        b.load_state(&snap).unwrap();
        assert_eq!(b.save(), snap, "same state");
        to_power_off(&mut a, 1 << 20);
        to_power_off(&mut b, 4093);
        assert_eq!((a.steps, a.digest()), (b.steps, b.digest()));
        // A single-core machine refuses it (configuration).
        let mut one = machine(1);
        assert!(one.load_state(&snap).is_err());
    }

    /// Record & replay with two cores (ADR 0019): a recording with keyframes
    /// and two host inputs replays identically from the start in other quanta
    /// and from a keyframe on a new machine.
    #[test]
    fn record_and_replay_two_cores() {
        use crate::record::{Input, Log, ReplayStatus, Reply};
        let mut m = machine(2);
        m.start_recording(super::super::RecordOptions { keyframe_every: 60_000 });
        let mut inputs = vec![(150_001u64, false), (77_777, true)];
        let stop = loop {
            if let Some(&(at, level)) = inputs.last()
                && m.steps >= at
            {
                assert_eq!(m.input(Input::Gpio { line: 3, level }), Reply::Done);
                inputs.pop();
            }
            let s = m.run(5_000);
            if s != Stop::Budget {
                break s;
            }
        };
        assert_eq!(stop, Stop::PowerOff);
        let log = Log::decode(&m.stop_recording().unwrap().encode()).unwrap();
        assert_eq!(log.config.cpus, 2);
        assert_eq!(log.events.len(), 2);
        assert!(log.keyframes.len() >= 3);
        let end = (m.steps, m.digest());
        let replay = |m: &mut Machine, q: u64| {
            while matches!(m.replay_status(), Some(ReplayStatus::Running { .. })) {
                m.run(q);
            }
            assert_eq!(m.replay_status(), Some(&ReplayStatus::Finished));
            (m.steps, m.digest())
        };
        for q in [997, 1 << 40] {
            let mut r = machine(2);
            r.start_replay(&log).unwrap();
            assert_eq!(replay(&mut r, q), end, "quantum {q}");
        }
        let cfg = MachineConfig { ram_size: 1 << 20, cpus: 2, ..MachineConfig::default() };
        let mut r = Machine::with_devices(&cfg, &Devices::none());
        r.replay_from(&log, 200_000).unwrap();
        assert_eq!(replay(&mut r, 4_093), end, "from a keyframe");
    }

    /// The machine is idle only when every core that is on waits with
    /// nothing to wake it: core 0 starts core 1 and both park in WFI.
    #[test]
    fn idle_when_every_core_waits() {
        let cfg = MachineConfig { ram_size: 1 << 20, cpus: 2, ..MachineConfig::default() };
        let mut m = Machine::with_devices(&cfg, &Devices::none());
        let code = [
            0xd2800060u32, // mov x0, #0x3
            0xf2b88000,    // movk x0, #0xc400, lsl #16
            0xd2800021,    // mov x1, #0x1
            0x10000042,    // adr x2, 0x14 <park>
            0xd4000002,    // hvc #0
            0xd503207f,    // wfi
            0x17ffffff,    // b 0x14 <park>
        ];
        {
            let b = m.board.borrow_mut();
            for (i, w) in code.iter().enumerate() {
                assert!(b.ram.write(R + 4 * i as u64, &w.to_le_bytes()));
            }
        }
        m.cpu.pc = R;
        assert_eq!(m.run(1 << 30), Stop::Idle);
        assert!(m.cpu_on(1));
        assert!(
            m.steps < 4 * super::QUANTUM,
            "idle within a couple of turns, not at the end of the budget: {}",
            m.steps
        );
    }
}
