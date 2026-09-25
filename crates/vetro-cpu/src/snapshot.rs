//! Stato della CPU negli snapshot (M6, ADR 0015): tutti i registri
//! architetturali, SIMD/FP, PSTATE, registri di sistema e monitor
//! esclusivo. La CPU non ha stato nascosto (cache di decodifica o simili):
//! quello che si salva è tutto quello che c'è.

use vetro_snapshot::{Error, Reader, Result, Snapshot, Writer};

use crate::state::{Cpu, Monitor};
use crate::sys::{Mode, PsciConduit, SysConfig, SysState};

fn mode_code(m: Mode) -> u8 {
    match m {
        Mode::User => 0,
        Mode::System => 1,
    }
}

fn psci_code(p: PsciConduit) -> u8 {
    match p {
        PsciConduit::Hvc => 0,
        PsciConduit::Smc => 1,
        PsciConduit::None => 2,
    }
}

impl Snapshot for SysConfig {
    fn save(&self, w: &mut Writer) {
        w.u8(psci_code(self.psci));
        w.u64(self.mpidr);
        w.bool(self.gicv3);
        w.u64(self.cbar);
    }

    fn restore(&mut self, r: &mut Reader<'_>) -> Result<()> {
        self.psci = match r.u8()? {
            0 => PsciConduit::Hvc,
            1 => PsciConduit::Smc,
            2 => PsciConduit::None,
            v => return Err(Error::invalid(format!("conduit PSCI {v}"))),
        };
        self.mpidr = r.u64()?;
        self.gicv3 = r.bool()?;
        self.cbar = r.u64()?;
        Ok(())
    }
}

impl Snapshot for SysState {
    fn save(&self, w: &mut Writer) {
        w.u8(mode_code(self.mode));
        self.cfg.save(w);
        w.u8(self.el);
        w.bool(self.spsel);
        w.u32(self.daif);
        w.bool(self.il);
        for v in self.sp_el {
            w.u64(v);
        }
        for v in [
            self.elr_el1,
            self.spsr_el1,
            self.vbar_el1,
            self.esr_el1,
            self.far_el1,
            self.sctlr_el1,
            self.tcr_el1,
            self.ttbr0_el1,
            self.ttbr1_el1,
            self.mair_el1,
            self.contextidr_el1,
            self.cpacr_el1,
            self.tpidr_el1,
            self.par_el1,
            self.cntkctl_el1,
            self.csselr_el1,
            self.mdscr_el1,
        ] {
            w.u64(v);
        }
        w.bool(self.oslk);
        w.u64(self.osdlr_el1);
        for v in self.dbgbvr.iter().chain(&self.dbgbcr).chain(&self.dbgwvr).chain(&self.dbgwcr) {
            w.u64(*v);
        }
        w.u8(self.dbgclaim);
        w.u64(self.pmuserenr_el0);
        w.opt(self.serror_pending, Writer::u32);
    }

    fn restore(&mut self, r: &mut Reader<'_>) -> Result<()> {
        self.mode = match r.u8()? {
            0 => Mode::User,
            1 => Mode::System,
            v => return Err(Error::invalid(format!("modalità della CPU {v}"))),
        };
        self.cfg.restore(r)?;
        self.el = r.u8()?;
        if self.el > 1 {
            return Err(Error::invalid(format!("EL{}", self.el)));
        }
        self.spsel = r.bool()?;
        self.daif = r.u32()?;
        self.il = r.bool()?;
        for v in &mut self.sp_el {
            *v = r.u64()?;
        }
        for v in [
            &mut self.elr_el1,
            &mut self.spsr_el1,
            &mut self.vbar_el1,
            &mut self.esr_el1,
            &mut self.far_el1,
            &mut self.sctlr_el1,
            &mut self.tcr_el1,
            &mut self.ttbr0_el1,
            &mut self.ttbr1_el1,
            &mut self.mair_el1,
            &mut self.contextidr_el1,
            &mut self.cpacr_el1,
            &mut self.tpidr_el1,
            &mut self.par_el1,
            &mut self.cntkctl_el1,
            &mut self.csselr_el1,
            &mut self.mdscr_el1,
        ] {
            *v = r.u64()?;
        }
        self.oslk = r.bool()?;
        self.osdlr_el1 = r.u64()?;
        for v in self
            .dbgbvr
            .iter_mut()
            .chain(self.dbgbcr.iter_mut())
            .chain(self.dbgwvr.iter_mut())
            .chain(self.dbgwcr.iter_mut())
        {
            *v = r.u64()?;
        }
        self.dbgclaim = r.u8()?;
        self.pmuserenr_el0 = r.u64()?;
        self.serror_pending = r.opt(Reader::u32)?;
        Ok(())
    }
}

impl Snapshot for Cpu {
    fn save(&self, w: &mut Writer) {
        for x in self.x {
            w.u64(x);
        }
        w.u64(self.sp);
        w.u64(self.pc);
        w.u32(self.nzcv);
        w.u64(self.tpidr_el0);
        w.u64(self.tpidrro_el0);
        w.opt(self.monitor, |w, m| {
            w.u64(m.addr);
            w.u32(m.bytes);
            w.u128(m.value);
        });
        for v in self.v {
            w.u128(v);
        }
        w.u32(self.fpcr);
        w.u32(self.fpsr);
        self.sys.save(w);
    }

    fn restore(&mut self, r: &mut Reader<'_>) -> Result<()> {
        for x in &mut self.x {
            *x = r.u64()?;
        }
        self.sp = r.u64()?;
        self.pc = r.u64()?;
        self.nzcv = r.u32()?;
        self.tpidr_el0 = r.u64()?;
        self.tpidrro_el0 = r.u64()?;
        self.monitor = r.opt(|r| Ok(Monitor { addr: r.u64()?, bytes: r.u32()?, value: r.u128()? }))?;
        for v in &mut self.v {
            *v = r.u128()?;
        }
        self.fpcr = r.u32()?;
        self.fpsr = r.u32()?;
        self.sys.restore(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ogni campo passa per lo snapshot: una CPU con tutti i registri
    /// diversi da zero torna identica (se un campo mancasse, resterebbe al
    /// valore della CPU di destinazione).
    #[test]
    fn cpu_completa_andata_e_ritorno() {
        let mut c = Cpu::new();
        c.reset_system(SysConfig { psci: PsciConduit::Smc, ..SysConfig::default() });
        let mut k = 1u64;
        let mut next = || {
            k = k.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            k
        };
        for x in &mut c.x {
            *x = next();
        }
        c.sp = next();
        c.pc = next();
        c.nzcv = 0xa000_0000;
        c.tpidr_el0 = next();
        c.tpidrro_el0 = next();
        c.monitor = Some(Monitor { addr: next(), bytes: 16, value: u128::from(next()) << 64 | 3 });
        for v in &mut c.v {
            *v = u128::from(next()) << 64 | u128::from(next());
        }
        c.fpcr = 0x0340_0000;
        c.fpsr = 0x0800_0011;
        let s = &mut c.sys;
        s.el = 0;
        s.spsel = false;
        s.daif = 0x140;
        s.il = true;
        s.sp_el = [next(), next()];
        s.elr_el1 = next();
        s.spsr_el1 = next();
        s.vbar_el1 = next();
        s.esr_el1 = next();
        s.far_el1 = next();
        s.sctlr_el1 = next();
        s.tcr_el1 = next();
        s.ttbr0_el1 = next();
        s.ttbr1_el1 = next();
        s.mair_el1 = next();
        s.contextidr_el1 = next();
        s.cpacr_el1 = next();
        s.tpidr_el1 = next();
        s.par_el1 = next();
        s.cntkctl_el1 = next();
        s.csselr_el1 = next();
        s.mdscr_el1 = next();
        s.oslk = false;
        s.osdlr_el1 = 1;
        s.dbgbvr = [1, 2, 3, 4, 5, 6];
        s.dbgbcr = [7, 8, 9, 10, 11, 12];
        s.dbgwvr = [13, 14, 15, 16];
        s.dbgwcr = [17, 18, 19, 20];
        s.dbgclaim = 0x5a;
        s.pmuserenr_el0 = 0xf;
        s.serror_pending = Some(0x1234);

        let mut w = Writer::new();
        c.save(&mut w);
        let bytes = w.into_bytes();
        let mut d = Cpu::new();
        let mut r = Reader::new(&bytes);
        d.restore(&mut r).unwrap();
        r.finish().unwrap();
        assert_eq!(d, c);
        let mut w2 = Writer::new();
        d.save(&mut w2);
        assert_eq!(w2.into_bytes(), bytes, "stessi byte dopo il ripristino");
        // Un EL impossibile si rifiuta.
        let mut bad = bytes.clone();
        // Dalla fine: SError, PMUSERENR, CLAIM, DBG*, OSDLR, OSLK, 17 registri, SP_EL*, IL, DAIF, SPSel.
        let el_at = bytes.len() - (5 + 8 + 1 + 20 * 8 + 8 + 1) - 17 * 8 - 16 - 1 - 4 - 1 - 1;
        assert_eq!(bad[el_at], 0);
        bad[el_at] = 2;
        assert!(Cpu::new().restore(&mut Reader::new(&bad)).is_err());
    }
}
