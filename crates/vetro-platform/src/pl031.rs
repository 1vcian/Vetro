//! PL031 RTC (ARM DDI0224).
//!
//! Time comes from outside with [`Pl031::set_time`] (seconds since the Unix
//! epoch, supplied by the engine's single recordable point): the device
//! never reads the host clock. DR is `time + offset`, 32
//! bits with modular arithmetic; writing LR changes the offset.
//!
//! Like QEMU: CR always reads 1 (RTC always started) and ignores
//! writes; any write to ICR clears the interrupt; the alarm
//! fires when DR reaches MR moving forward, or immediately if after a
//! write of MR or LR the two coincide.

use crate::bus::{MmioDevice, sub_word};

pub const DR: u64 = 0x00;
pub const MR: u64 = 0x04;
pub const LR: u64 = 0x08;
pub const CR: u64 = 0x0C;
pub const IMSC: u64 = 0x10;
pub const RIS: u64 = 0x14;
pub const MIS: u64 = 0x18;
pub const ICR: u64 = 0x1C;
pub const PERIPH_ID0: u64 = 0xFE0;

/// PeriphID0-3 and CellID0-3 (the same values as QEMU).
const ID: [u8; 8] = [0x31, 0x10, 0x14, 0x00, 0x0D, 0xF0, 0x05, 0xB1];

#[derive(Clone, Debug, Default)]
pub struct Pl031 {
    /// Last time received from outside, in seconds.
    now: u64,
    /// DR = (now + offset) mod 2^32.
    offset: u32,
    mr: u32,
    lr: u32,
    imsc: u32,
    ris: u32,
}

impl Pl031 {
    /// RTC with DR equal to `now_secs` (truncated to 32 bits).
    pub fn new(now_secs: u64) -> Self {
        Self { now: now_secs, ..Self::default() }
    }

    /// Current value of the counter (DR).
    pub fn count(&self) -> u32 {
        (self.now as u32).wrapping_add(self.offset)
    }

    /// Updates the time. If the counter moves forward until it reaches or
    /// passes MR, the alarm goes off. A time that goes backwards does not
    /// trigger anything.
    pub fn set_time(&mut self, now_secs: u64) {
        let old = self.count();
        let advance = now_secs.saturating_sub(self.now);
        self.now = now_secs;
        if advance == 0 {
            return;
        }
        let to_match = u64::from(self.mr.wrapping_sub(old));
        if (to_match != 0 && to_match <= advance) || advance >= 1 << 32 {
            self.ris = 1;
        }
    }

    /// Seconds left before the alarm, if the interrupt is not already active:
    /// the host uses it to schedule the next `set_time`.
    pub fn seconds_to_alarm(&self) -> Option<u32> {
        (self.ris == 0).then(|| self.mr.wrapping_sub(self.count())).filter(|&s| s != 0)
    }

    /// Level of the IRQ line to the GIC (RTCINTR).
    pub fn irq_level(&self) -> bool {
        self.ris & self.imsc != 0
    }

    fn check_alarm_now(&mut self) {
        if self.mr == self.count() {
            self.ris = 1;
        }
    }

    fn read_reg(&self, reg: u64) -> u32 {
        match reg {
            DR => self.count(),
            MR => self.mr,
            LR => self.lr,
            CR => 1,
            IMSC => self.imsc,
            RIS => self.ris,
            MIS => self.ris & self.imsc,
            0xFE0..=0xFFC => u32::from(ID[((reg - PERIPH_ID0) / 4) as usize]),
            _ => 0,
        }
    }

    fn write_reg(&mut self, reg: u64, v: u32) {
        match reg {
            MR => {
                self.mr = v;
                self.check_alarm_now();
            }
            LR => {
                self.offset = v.wrapping_sub(self.now as u32);
                self.lr = v;
                self.check_alarm_now();
            }
            IMSC => self.imsc = v & 1,
            ICR => self.ris = 0,
            _ => {}
        }
    }
}

impl MmioDevice for Pl031 {
    fn read(&mut self, offset: u64, size: u8) -> u64 {
        if offset >= 0x1000 || size > 4 {
            return 0;
        }
        sub_word(self.read_reg(offset & !3), offset, size)
    }

    fn write(&mut self, offset: u64, size: u8, value: u64) {
        if offset >= 0x1000 || size != 4 || offset & 3 != 0 {
            return;
        }
        self.write_reg(offset, value as u32);
    }
}

// ---- Snapshot (M6, ADR 0015) -------------------------------------------------

impl vetro_snapshot::Snapshot for Pl031 {
    fn save(&self, w: &mut vetro_snapshot::Writer) {
        w.u64(self.now);
        for v in [self.offset, self.mr, self.lr, self.imsc, self.ris] {
            w.u32(v);
        }
    }

    fn restore(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        self.now = r.u64()?;
        for v in [&mut self.offset, &mut self.mr, &mut self.lr, &mut self.imsc, &mut self.ris] {
            *v = r.u32()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identificazione_e_cr() {
        let mut r = Pl031::new(0);
        let id: Vec<u64> = (0..8).map(|i| r.read(PERIPH_ID0 + 4 * i, 4)).collect();
        assert_eq!(id, [0x31, 0x10, 0x14, 0x00, 0x0D, 0xF0, 0x05, 0xB1]);
        r.write(CR, 4, 0);
        assert_eq!(r.read(CR, 4), 1);
    }

    #[test]
    fn dr_segue_il_tempo_esterno() {
        let mut r = Pl031::new(1_700_000_000);
        assert_eq!(r.read(DR, 4), 1_700_000_000);
        r.set_time(1_700_000_042);
        assert_eq!(r.read(DR, 4), 1_700_000_042);
        // No internal clock: without set_time the value does not change.
        assert_eq!(r.read(DR, 4), 1_700_000_042);
    }

    #[test]
    fn lr_imposta_il_contatore() {
        let mut r = Pl031::new(1000);
        r.write(LR, 4, 5);
        assert_eq!(r.read(DR, 4), 5);
        assert_eq!(r.read(LR, 4), 5);
        r.set_time(1010);
        assert_eq!(r.read(DR, 4), 15);
        // 32-bit modular arithmetic.
        r.write(LR, 4, 0xFFFF_FFFF);
        r.set_time(1012);
        assert_eq!(r.read(DR, 4), 1);
    }

    #[test]
    fn allarme_su_mr_con_maschera() {
        let mut r = Pl031::new(100);
        r.write(MR, 4, 110);
        assert_eq!(r.seconds_to_alarm(), Some(10));
        r.set_time(105);
        assert_eq!(r.read(RIS, 4), 0);
        r.set_time(112);
        assert_eq!(r.read(RIS, 4), 1);
        assert_eq!(r.read(MIS, 4), 0);
        assert!(!r.irq_level());
        r.write(IMSC, 4, 0xFF);
        assert_eq!(r.read(IMSC, 4), 1);
        assert!(r.irq_level());
        assert_eq!(r.seconds_to_alarm(), None);
        r.write(ICR, 4, 1);
        assert_eq!(r.read(RIS, 4), 0);
        assert!(!r.irq_level());
    }

    #[test]
    fn allarme_immediato_se_mr_coincide() {
        let mut r = Pl031::new(50);
        r.write(MR, 4, 50);
        assert_eq!(r.read(RIS, 4), 1);
        r.write(ICR, 4, 1);
        r.write(MR, 4, 7);
        r.write(LR, 4, 7);
        assert_eq!(r.read(RIS, 4), 1);
    }

    #[test]
    fn tempo_all_indietro_non_attiva_l_allarme() {
        let mut r = Pl031::new(100);
        r.write(MR, 4, 90);
        r.set_time(80);
        assert_eq!(r.read(RIS, 4), 0);
        assert_eq!(r.read(DR, 4), 80);
    }
}
