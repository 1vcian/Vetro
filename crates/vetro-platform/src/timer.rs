//! ARM generic timer (ARM ARM D11): physical (CNTP) and virtual (CNTV) channels
//! of one CPU.
//!
//! The counter does not live here: every function receives the value of the physical
//! counter (CNTPCT_EL0) from outside, so time stays deterministic and
//! recordable. The virtual counter is `CNTPCT - CNTVOFF`.
//!
//! Semantics as in QEMU (target/arm/helper.c, `gt_recalc_timer`):
//! - ISTATUS = ENABLE && counter >= CVAL (unsigned comparison);
//! - with ENABLE off ISTATUS reads 0 and the line is low;
//! - the IRQ line is ISTATUS && !IMASK;
//! - TVAL reads as `(CVAL - counter)[31:0]`, a write sets
//!   `CVAL = counter + SignExtend(TVAL[31:0])`.

use crate::map;

/// CNTx_CTL_EL0 bits.
pub const CTL_ENABLE: u64 = 1 << 0;
pub const CTL_IMASK: u64 = 1 << 1;
pub const CTL_ISTATUS: u64 = 1 << 2;

/// One timer channel (CTL and CVAL). The methods receive the value of the
/// counter as seen by this channel (physical or virtual).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TimerChannel {
    /// Only ENABLE and IMASK; ISTATUS is computed.
    ctl: u64,
    cval: u64,
}

impl TimerChannel {
    pub fn istatus(&self, count: u64) -> bool {
        self.ctl & CTL_ENABLE != 0 && count >= self.cval
    }

    /// Level of the output to the GIC.
    pub fn irq_level(&self, count: u64) -> bool {
        self.istatus(count) && self.ctl & CTL_IMASK == 0
    }

    pub fn read_ctl(&self, count: u64) -> u64 {
        self.ctl | if self.istatus(count) { CTL_ISTATUS } else { 0 }
    }

    /// ISTATUS is read-only.
    pub fn write_ctl(&mut self, value: u64) {
        self.ctl = value & (CTL_ENABLE | CTL_IMASK);
    }

    pub fn read_cval(&self) -> u64 {
        self.cval
    }

    pub fn write_cval(&mut self, value: u64) {
        self.cval = value;
    }

    pub fn read_tval(&self, count: u64) -> u64 {
        u64::from(self.cval.wrapping_sub(count) as u32)
    }

    pub fn write_tval(&mut self, count: u64, value: u64) {
        self.cval = count.wrapping_add(value as u32 as i32 as i64 as u64);
    }

    /// Counter value at which the line will rise, if enabled, not
    /// masked and not yet fired: the engine uses it to know how long
    /// it can run without checking the timer again.
    pub fn deadline(&self, count: u64) -> Option<u64> {
        (self.ctl & (CTL_ENABLE | CTL_IMASK) == CTL_ENABLE && count < self.cval).then_some(self.cval)
    }
}

/// Generic timer of one CPU: CNTFRQ, CNTVOFF and the two EL1 channels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenericTimer {
    /// CNTFRQ_EL0 in Hz.
    pub cntfrq: u32,
    /// CNTVOFF_EL2 (zero without EL2).
    pub cntvoff: u64,
    /// CNTP_* (PPI 30).
    pub phys: TimerChannel,
    /// CNTV_* (PPI 27).
    pub virt: TimerChannel,
}

impl Default for GenericTimer {
    fn default() -> Self {
        Self::new(map::CNTFRQ_HZ)
    }
}

impl GenericTimer {
    pub fn new(cntfrq: u32) -> Self {
        Self { cntfrq, cntvoff: 0, phys: TimerChannel::default(), virt: TimerChannel::default() }
    }

    /// CNTVCT_EL0 given CNTPCT_EL0.
    pub fn cntvct(&self, cntpct: u64) -> u64 {
        cntpct.wrapping_sub(self.cntvoff)
    }

    // System registers: the CPU will call them from MRS/MSR in M3.

    pub fn cntp_ctl(&self, cntpct: u64) -> u64 {
        self.phys.read_ctl(cntpct)
    }
    pub fn set_cntp_ctl(&mut self, value: u64) {
        self.phys.write_ctl(value);
    }
    pub fn cntp_cval(&self) -> u64 {
        self.phys.read_cval()
    }
    pub fn set_cntp_cval(&mut self, value: u64) {
        self.phys.write_cval(value);
    }
    pub fn cntp_tval(&self, cntpct: u64) -> u64 {
        self.phys.read_tval(cntpct)
    }
    pub fn set_cntp_tval(&mut self, cntpct: u64, value: u64) {
        self.phys.write_tval(cntpct, value);
    }

    pub fn cntv_ctl(&self, cntpct: u64) -> u64 {
        self.virt.read_ctl(self.cntvct(cntpct))
    }
    pub fn set_cntv_ctl(&mut self, value: u64) {
        self.virt.write_ctl(value);
    }
    pub fn cntv_cval(&self) -> u64 {
        self.virt.read_cval()
    }
    pub fn set_cntv_cval(&mut self, value: u64) {
        self.virt.write_cval(value);
    }
    pub fn cntv_tval(&self, cntpct: u64) -> u64 {
        self.virt.read_tval(self.cntvct(cntpct))
    }
    pub fn set_cntv_tval(&mut self, cntpct: u64, value: u64) {
        let count = self.cntvct(cntpct);
        self.virt.write_tval(count, value);
    }

    /// Lines to the GIC as pairs (PPI INTID, level).
    pub fn irq_lines(&self, cntpct: u64) -> [(u32, bool); 2] {
        [
            (map::PPI_VTIMER, self.virt.irq_level(self.cntvct(cntpct))),
            (map::PPI_PTIMER, self.phys.irq_level(cntpct)),
        ]
    }

    /// Next CNTPCT value at which one of the two lines will rise.
    pub fn next_deadline(&self, cntpct: u64) -> Option<u64> {
        let v = self.virt.deadline(self.cntvct(cntpct)).map(|c| c.wrapping_add(self.cntvoff));
        let p = self.phys.deadline(cntpct);
        match (v, p) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

// ---- Snapshot (M6, ADR 0015) -------------------------------------------------

impl vetro_snapshot::Snapshot for GenericTimer {
    fn save(&self, w: &mut vetro_snapshot::Writer) {
        w.u32(self.cntfrq);
        w.u64(self.cntvoff);
        for c in [&self.phys, &self.virt] {
            w.u64(c.ctl);
            w.u64(c.cval);
        }
    }

    fn restore(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        self.cntfrq = r.u32()?;
        self.cntvoff = r.u64()?;
        for c in [&mut self.phys, &mut self.virt] {
            c.write_ctl(r.u64()?);
            c.cval = r.u64()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn istatus_e_linea_irq() {
        let mut t = TimerChannel::default();
        t.write_cval(100);
        assert!(!t.istatus(200), "disabled: ISTATUS zero");
        assert_eq!(t.read_ctl(200), 0);
        t.write_ctl(CTL_ENABLE | CTL_ISTATUS);
        assert_eq!(t.read_ctl(99), CTL_ENABLE, "ISTATUS is not writable");
        assert_eq!(t.read_ctl(100), CTL_ENABLE | CTL_ISTATUS);
        assert!(t.irq_level(100));
        t.write_ctl(CTL_ENABLE | CTL_IMASK);
        assert!(t.istatus(150));
        assert!(!t.irq_level(150), "masked");
    }

    #[test]
    fn tval_con_segno_e_troncato() {
        let mut t = TimerChannel::default();
        t.write_tval(1000, 50);
        assert_eq!(t.read_cval(), 1050);
        assert_eq!(t.read_tval(1000), 50);
        assert_eq!(t.read_tval(1060), 0xFFFF_FFF6, "-10 on 32 bits");
        t.write_tval(1000, 0xFFFF_FFFF);
        assert_eq!(t.read_cval(), 999);
        // Only the low 32 bits count.
        t.write_tval(0, 0x1_0000_0005);
        assert_eq!(t.read_cval(), 5);
    }

    #[test]
    fn confronto_senza_segno() {
        let mut t = TimerChannel::default();
        t.write_ctl(CTL_ENABLE);
        t.write_cval(u64::MAX);
        assert!(!t.istatus(1 << 63));
        assert!(t.istatus(u64::MAX));
    }

    #[test]
    fn canale_virtuale_usa_cntvoff() {
        let mut g = GenericTimer::default();
        assert_eq!(g.cntfrq, 62_500_000);
        g.cntvoff = 1000;
        g.set_cntv_ctl(CTL_ENABLE);
        g.set_cntv_tval(1500, 100); // CNTVCT = 500, CVAL = 600
        assert_eq!(g.cntv_cval(), 600);
        assert_eq!(g.cntv_tval(1500), 100);
        assert_eq!(g.irq_lines(1599), [(27, false), (30, false)]);
        assert_eq!(g.next_deadline(1500), Some(1600));
        assert_eq!(g.irq_lines(1600), [(27, true), (30, false)]);
        assert_eq!(g.cntv_ctl(1600), CTL_ENABLE | CTL_ISTATUS);
        assert_eq!(g.next_deadline(1600), None);
    }

    #[test]
    fn canale_fisico_su_ppi_30() {
        let mut g = GenericTimer::new(1_000_000);
        g.set_cntp_cval(10);
        g.set_cntp_ctl(CTL_ENABLE);
        assert_eq!(g.cntp_tval(4), 6);
        assert_eq!(g.next_deadline(4), Some(10));
        assert_eq!(g.irq_lines(10), [(27, false), (30, true)]);
        g.set_cntp_tval(10, 5);
        assert_eq!(g.cntp_cval(), 15);
        assert_eq!(g.cntp_ctl(12), CTL_ENABLE);
    }

    #[test]
    fn scadenza_minima_fra_i_canali() {
        let mut g = GenericTimer::default();
        g.set_cntp_cval(300);
        g.set_cntp_ctl(CTL_ENABLE);
        g.set_cntv_cval(200);
        g.set_cntv_ctl(CTL_ENABLE);
        assert_eq!(g.next_deadline(0), Some(200));
        g.set_cntv_ctl(CTL_ENABLE | CTL_IMASK);
        assert_eq!(g.next_deadline(0), Some(300));
    }
}
