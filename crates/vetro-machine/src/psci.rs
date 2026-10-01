//! PSCI via HVC, with the same answers as QEMU 10 (`target/arm/tcg/psci.c`,
//! `target/arm/arm-powerctl.c`) for the virt machine: version 1.1. What
//! depends on the other cores (CPU_ON, AFFINITY_INFO, CPU_OFF) is decided by
//! the machine ([`Call`]).

pub const VERSION: u32 = 0x8400_0000;
pub const CPU_SUSPEND: u32 = 0x8400_0001;
pub const CPU_OFF: u32 = 0x8400_0002;
pub const CPU_ON: u32 = 0x8400_0003;
pub const AFFINITY_INFO: u32 = 0x8400_0004;
pub const MIGRATE: u32 = 0x8400_0005;
pub const MIGRATE_INFO_TYPE: u32 = 0x8400_0006;
pub const SYSTEM_OFF: u32 = 0x8400_0008;
pub const SYSTEM_RESET: u32 = 0x8400_0009;
pub const FEATURES: u32 = 0x8400_000a;
/// The SMC64 variants have bit 30 set.
const SMC64: u32 = 0x4000_0000;

pub const RET_SUCCESS: i64 = 0;
pub const RET_NOT_SUPPORTED: i64 = -1;
pub const RET_INVALID_PARAMS: i64 = -2;
pub const RET_ALREADY_ON: i64 = -4;
/// PSCI 1.1 (QEMU >= 8 on virt).
pub const VERSION_1_1: i64 = 0x1_0001;
/// MIGRATE_INFO_TYPE: no Trusted OS to migrate.
pub const TOS_NOT_PRESENT: i64 = 2;

/// AFFINITY_INFO answers.
pub const AFFINITY_ON: i64 = 0;
pub const AFFINITY_OFF: i64 = 1;

/// Outcome of a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Call {
    /// Value to write into x0.
    Ret(i64),
    /// CPU_SUSPEND: like a WFI (as QEMU does), then x0 = 0.
    Suspend,
    /// CPU_OFF: the calling core stops (never returns).
    CpuOff,
    /// SYSTEM_OFF.
    SystemOff,
    SystemReset,
    /// CPU_ON of the core with affinity `target` (exactly the MPIDR affinity
    /// bits, as QEMU's `arm_get_cpu_by_id` compares them), at `entry` (already
    /// checked: 4-byte aligned) with x0 = `context`.
    CpuOn {
        target: u64,
        entry: u64,
        context: u64,
    },
    /// AFFINITY_INFO at level 0 of the core with affinity `target`.
    AffinityInfo {
        target: u64,
    },
}

fn base(fid: u32) -> u32 {
    fid & !SMC64
}

/// `x` = x0..x3 at the HVC.
pub fn call(x: [u64; 4]) -> Call {
    let fid = x[0] as u32;
    // The functions with 32- and 64-bit variants; the others are 32-bit only.
    let known64 = matches!(base(fid), CPU_SUSPEND | CPU_ON | AFFINITY_INFO | MIGRATE);
    if fid & SMC64 != 0 && !known64 {
        return Call::Ret(RET_NOT_SUPPORTED);
    }
    match base(fid) {
        VERSION => Call::Ret(VERSION_1_1),
        MIGRATE_INFO_TYPE => Call::Ret(TOS_NOT_PRESENT),
        // Affinity levels are not supported (QEMU): only the power state.
        CPU_SUSPEND if x[1] & 0xfffe_0000 != 0 => Call::Ret(RET_INVALID_PARAMS),
        CPU_SUSPEND => Call::Suspend,
        CPU_OFF => Call::CpuOff,
        SYSTEM_OFF => Call::SystemOff,
        SYSTEM_RESET => Call::SystemReset,
        // An AArch64 entry point must be 4-byte aligned.
        CPU_ON if x[2] & 3 != 0 => Call::Ret(RET_INVALID_PARAMS),
        CPU_ON => Call::CpuOn { target: x[1], entry: x[2], context: x[3] },
        // Everything above affinity level 0 is always on.
        AFFINITY_INFO if x[2] != 0 => Call::Ret(AFFINITY_ON),
        AFFINITY_INFO => Call::AffinityInfo { target: x[1] },
        MIGRATE => Call::Ret(RET_NOT_SUPPORTED),
        FEATURES => {
            let f = x[1] as u32;
            let ok = matches!(
                base(f),
                VERSION
                    | CPU_SUSPEND
                    | CPU_OFF
                    | CPU_ON
                    | AFFINITY_INFO
                    | MIGRATE
                    | MIGRATE_INFO_TYPE
                    | SYSTEM_OFF
                    | SYSTEM_RESET
                    | FEATURES
            ) && (f & SMC64 == 0
                || matches!(base(f), CPU_SUSPEND | CPU_ON | AFFINITY_INFO | MIGRATE));
            Call::Ret(if ok { RET_SUCCESS } else { RET_NOT_SUPPORTED })
        }
        _ => Call::Ret(RET_NOT_SUPPORTED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_like_qemu() {
        assert_eq!(call([u64::from(VERSION), 0, 0, 0]), Call::Ret(VERSION_1_1));
        assert_eq!(call([u64::from(SYSTEM_OFF), 0, 0, 0]), Call::SystemOff);
        assert_eq!(call([u64::from(CPU_OFF), 0, 0, 0]), Call::CpuOff);
        assert_eq!(call([u64::from(SYSTEM_RESET), 0, 0, 0]), Call::SystemReset);
        assert_eq!(
            call([0xc400_0003, 1, 0x4000_0000, 7]),
            Call::CpuOn { target: 1, entry: 0x4000_0000, context: 7 }
        );
        assert_eq!(call([0xc400_0003, 1, 0x4000_0002, 0]), Call::Ret(RET_INVALID_PARAMS), "misaligned entry");
        assert_eq!(call([0xc400_0004, 1, 0, 0]), Call::AffinityInfo { target: 1 });
        assert_eq!(call([0xc400_0004, 1, 1, 0]), Call::Ret(AFFINITY_ON), "levels above 0 are on");
        assert_eq!(call([0xc400_0001, 0x2_0000, 0, 0]), Call::Ret(RET_INVALID_PARAMS));
        assert_eq!(call([0xc400_0001, 0, 0, 0]), Call::Suspend);
        assert_eq!(call([u64::from(FEATURES), u64::from(SYSTEM_OFF), 0, 0]), Call::Ret(0));
        assert_eq!(call([u64::from(FEATURES), 0x8400_0012, 0, 0]), Call::Ret(RET_NOT_SUPPORTED));
        assert_eq!(call([0xc400_0008, 0, 0, 0]), Call::Ret(RET_NOT_SUPPORTED), "SYSTEM_OFF has no SMC64");
        assert_eq!(call([0x8600_0000, 0, 0, 0]), Call::Ret(RET_NOT_SUPPORTED));
    }
}
