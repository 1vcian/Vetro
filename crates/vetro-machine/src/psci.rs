//! PSCI tramite HVC, con le stesse risposte di QEMU (`target/arm/psci.c`)
//! per la macchina virt con una sola CPU: versione 1.1.

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
/// Le varianti SMC64 hanno il bit 30 acceso.
const SMC64: u32 = 0x4000_0000;

pub const RET_SUCCESS: i64 = 0;
pub const RET_NOT_SUPPORTED: i64 = -1;
pub const RET_INVALID_PARAMS: i64 = -2;
pub const RET_ALREADY_ON: i64 = -4;
/// PSCI 1.1 (QEMU >= 8 sulla virt).
pub const VERSION_1_1: i64 = 0x1_0001;
/// MIGRATE_INFO_TYPE: nessun Trusted OS da migrare.
pub const TOS_NOT_PRESENT: i64 = 2;

/// Esito di una chiamata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Call {
    /// Valore da scrivere in x0.
    Ret(i64),
    /// CPU_SUSPEND: come una WFI (così fa QEMU), poi x0 = 0.
    Suspend,
    /// SYSTEM_OFF, o CPU_OFF dell'unica CPU.
    Off,
    SystemReset,
}

fn base(fid: u32) -> u32 {
    fid & !SMC64
}

/// `x` = x0..x3 all'HVC; `mpidr` = MPIDR_EL1 dell'unica CPU.
pub fn call(x: [u64; 4], mpidr: u64) -> Call {
    let fid = x[0] as u32;
    let is_cpu0 = |target: u64| target & 0xff_00ff_ffff == mpidr & 0xff_00ff_ffff;
    // Le funzioni con varianti a 32 e 64 bit; le altre solo a 32 bit.
    let known64 = matches!(base(fid), CPU_SUSPEND | CPU_ON | AFFINITY_INFO | MIGRATE);
    if fid & SMC64 != 0 && !known64 {
        return Call::Ret(RET_NOT_SUPPORTED);
    }
    match base(fid) {
        VERSION => Call::Ret(VERSION_1_1),
        MIGRATE_INFO_TYPE => Call::Ret(TOS_NOT_PRESENT),
        CPU_SUSPEND => Call::Suspend,
        CPU_OFF | SYSTEM_OFF => Call::Off,
        SYSTEM_RESET => Call::SystemReset,
        CPU_ON => Call::Ret(if is_cpu0(x[1]) { RET_ALREADY_ON } else { RET_INVALID_PARAMS }),
        AFFINITY_INFO => {
            // Livello 0 soltanto; 0 = ON.
            Call::Ret(if x[2] != 0 || !is_cpu0(x[1]) { RET_INVALID_PARAMS } else { 0 })
        }
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

    const MPIDR: u64 = 0x8000_0000;

    #[test]
    fn risposte_come_qemu() {
        assert_eq!(call([u64::from(VERSION), 0, 0, 0], MPIDR), Call::Ret(VERSION_1_1));
        assert_eq!(call([u64::from(SYSTEM_OFF), 0, 0, 0], MPIDR), Call::Off);
        assert_eq!(call([u64::from(SYSTEM_RESET), 0, 0, 0], MPIDR), Call::SystemReset);
        assert_eq!(call([0xc400_0003, 0, 0x4000_0000, 0], MPIDR), Call::Ret(RET_ALREADY_ON));
        assert_eq!(call([0xc400_0003, 1, 0x4000_0000, 0], MPIDR), Call::Ret(RET_INVALID_PARAMS));
        assert_eq!(call([0xc400_0004, 0, 0, 0], MPIDR), Call::Ret(0));
        assert_eq!(call([u64::from(FEATURES), u64::from(SYSTEM_OFF), 0, 0], MPIDR), Call::Ret(0));
        assert_eq!(call([u64::from(FEATURES), 0x8400_0012, 0, 0], MPIDR), Call::Ret(RET_NOT_SUPPORTED));
        assert_eq!(
            call([0xc400_0008, 0, 0, 0], MPIDR),
            Call::Ret(RET_NOT_SUPPORTED),
            "SYSTEM_OFF non ha SMC64"
        );
        assert_eq!(call([0x8600_0000, 0, 0, 0], MPIDR), Call::Ret(RET_NOT_SUPPORTED));
    }
}
