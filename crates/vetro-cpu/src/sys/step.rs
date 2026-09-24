//! Un passo in modalità sistema: interrupt, fetch con MMU, istruzioni di
//! sistema, interprete comune e consegna delle eccezioni al guest.

use crate::bits::field;
use crate::decode::{AtOp, Insn, PstateField, SysOp, SysReg, decode};
use crate::exec::Exception;
use crate::mem::{Access, Memory};
use crate::state::Cpu;

use super::except::{ExceptionKind, ec, esr};
use super::mem::{Pending, SysMem};
use super::regs::Deny;
use super::state::{Mode, PsciConduit, daif, sctlr};
use super::{AtResult, BusFault, CpuEnv, SysBus, SysEvent};

/// ISS di una trap WFI da EL0 (EC 0x01): CV = 1, COND = 0b1110, TI = 0.
const ISS_WFI: u64 = 1 << 24 | 0xe << 20;
/// ISS di una trap FP/SIMD da AArch64 (EC 0x07): CV = 1, COND = 0b1110.
const ISS_FP: u64 = 1 << 24 | 0xe << 20;

/// ISS di una trap di MSR/MRS/SYS (EC 0x18) ricavato dalla codifica:
/// Op0, Op2, Op1, CRn, Rt, CRm, direzione (1 = lettura).
fn sysreg_iss(raw: u32) -> u64 {
    let f = |hi, lo| u64::from(field(raw, hi, lo));
    f(20, 19) << 20 | f(7, 5) << 17 | f(18, 16) << 14 | f(15, 12) << 10 | f(4, 0) << 5 | f(11, 8) << 1 | f(21, 21)
}

/// Istruzioni soggette alla trap di CPACR_EL1.FPEN.
fn is_fp(insn: &Insn) -> bool {
    matches!(
        insn,
        Insn::Simd(_)
            | Insn::Mrs { reg: SysReg::Fpcr | SysReg::Fpsr, .. }
            | Insn::Msr { reg: SysReg::Fpcr | SysReg::Fpsr, .. }
    )
}

/// Scritture che possono dare un fault di allineamento dall'interprete
/// (esclusive e store-release): servono per WnR.
fn is_store(insn: &Insn) -> bool {
    matches!(insn, Insn::Exclusive { load: false, .. } | Insn::StoreRelease { .. })
}

impl Cpu {
    /// Esegue un'istruzione in modalità sistema, oppure prende un interrupt
    /// o un'eccezione. Richiede [`Cpu::reset_system`] (o
    /// `sys.mode = Mode::System`) prima del primo passo.
    pub fn step_system<B: SysBus + ?Sized, E: CpuEnv + ?Sized>(&mut self, bus: &mut B, env: &mut E) -> SysEvent {
        debug_assert_eq!(self.sys.mode, Mode::System, "step_system in modalità utente");
        if let Some(ev) = self.take_interrupt(env) {
            return ev;
        }
        let pc = self.pc;
        if pc & 3 != 0 {
            return self.sync(esr(ec::PC_ALIGN, 0), Some(pc), pc);
        }
        let el = self.sys.el;
        let mut mem = SysMem::new(bus, self.translation_regs(), el);
        let raw = match mem.fetch(pc) {
            Ok(w) => w,
            Err(_) => return self.deliver_fault(mem.fault.take(), 0, false),
        };
        if self.sys.il {
            return self.sync(esr(ec::ILLEGAL_STATE, 0), None, pc);
        }
        let insn = decode(raw);
        if let Some(ev) = self.exec_system(insn, raw, &mut mem, env) {
            return ev;
        }
        match self.execute(insn, raw, &mut mem) {
            Ok(target) => {
                self.pc = match target {
                    Some(t) => self.branch_addr(t),
                    None => pc.wrapping_add(4),
                };
                if let Insn::Svc { imm } = insn {
                    return self.sync(esr(ec::SVC, u64::from(imm)), None, self.pc);
                }
                SysEvent::Executed
            }
            Err(e) => match e {
                Exception::Undefined(_) => self.undefined(),
                Exception::Unimplemented { raw, what } => SysEvent::Unimplemented { raw, what },
                Exception::Breakpoint(imm) => self.sync(esr(ec::BRK, u64::from(imm)), None, pc),
                Exception::DataAbort { .. } | Exception::InstructionAbort { .. } => {
                    self.deliver_fault(mem.fault.take(), raw, false)
                }
                Exception::Alignment { addr } => {
                    let access = if is_store(&insn) { Access::Write } else { Access::Read };
                    let f = Pending::Abort { va: addr, fsc: BusFault::FSC_ALIGNMENT, ea: false, access };
                    self.deliver_fault(Some(f), raw, false)
                }
                Exception::Svc(_) | Exception::PcAlignment { .. } => {
                    unreachable!("non prodotte dall'interprete comune")
                }
            },
        }
    }

    /// FIQ, IRQ e SError non mascherati, in quest'ordine (come QEMU per FIQ
    /// e IRQ). Si prendono fra un'istruzione e l'altra, con ELR = PC.
    fn take_interrupt<E: CpuEnv + ?Sized>(&mut self, env: &mut E) -> Option<SysEvent> {
        let masks = self.sys.daif;
        let from_el = self.sys.el;
        let kind = if masks & daif::F == 0 && env.fiq_line() {
            ExceptionKind::Fiq
        } else if masks & daif::I == 0 && env.irq_line() {
            ExceptionKind::Irq
        } else if masks & daif::A == 0
            && let Some(iss) = self.sys.serror_pending.take()
        {
            let e = esr(ec::SERROR, u64::from(iss) & 0x1ff_ffff);
            self.take_exception(ExceptionKind::SError, Some(e), None, self.pc);
            return Some(SysEvent::Exception { kind: ExceptionKind::SError, esr: e, from_el });
        } else {
            return None;
        };
        self.take_exception(kind, None, None, self.pc);
        Some(SysEvent::Exception { kind, esr: 0, from_el })
    }

    /// Eccezione sincrona con ELR = `preferred`.
    fn sync(&mut self, esr: u64, far: Option<u64>, preferred: u64) -> SysEvent {
        let from_el = self.sys.el;
        self.take_exception(ExceptionKind::Sync, Some(esr), far, preferred);
        SysEvent::Exception { kind: ExceptionKind::Sync, esr, from_el }
    }

    /// Istruzione UNDEFINED al PC corrente (EC 0x00).
    fn undefined(&mut self) -> SysEvent {
        self.sync(esr(ec::UNKNOWN, 0), None, self.pc)
    }

    /// Trap di MSR/MRS/SYS a EL1 (EC 0x18).
    fn sysreg_trap(&mut self, raw: u32) -> SysEvent {
        self.sync(esr(ec::SYSREG, sysreg_iss(raw)), None, self.pc)
    }

    fn deny(&mut self, d: Deny, raw: u32) -> SysEvent {
        match d {
            Deny::Undefined => self.undefined(),
            Deny::Trap => self.sysreg_trap(raw),
        }
    }

    /// Consegna un fault di memoria come Instruction o Data Abort (o fault
    /// di allineamento), oppure lo restituisce come limite di Vetro. `cm`
    /// marca le istruzioni di manutenzione (AT).
    fn deliver_fault(&mut self, f: Option<Pending>, raw: u32, cm: bool) -> SysEvent {
        let lower = self.sys.el == 0;
        match f {
            Some(Pending::Abort { va, fsc, ea, access }) => {
                let fsc = u64::from(fsc);
                let ea = u64::from(ea) << 9;
                let e = match access {
                    Access::Fetch => {
                        esr(if lower { ec::INSN_ABORT_LOWER } else { ec::INSN_ABORT_SAME }, ea | fsc)
                    }
                    Access::Read | Access::Write => {
                        let wnr = u64::from(access == Access::Write) << 6;
                        let cm = u64::from(cm) << 8;
                        esr(if lower { ec::DATA_ABORT_LOWER } else { ec::DATA_ABORT_SAME }, ea | cm | wnr | fsc)
                    }
                };
                self.sync(e, Some(va), self.pc)
            }
            Some(Pending::Unimplemented(what)) => SysEvent::Unimplemented { raw, what },
            None => SysEvent::Unimplemented { raw, what: "fault di memoria senza dettagli" },
        }
    }

    /// Istruzioni con una semantica propria della modalità sistema.
    /// `None`: l'istruzione va all'interprete comune.
    fn exec_system<B: SysBus + ?Sized, E: CpuEnv + ?Sized>(
        &mut self,
        insn: Insn,
        raw: u32,
        mem: &mut SysMem<'_, B>,
        env: &mut E,
    ) -> Option<SysEvent> {
        let el = self.sys.el;
        let sctlr = self.sys.sctlr_el1;
        if is_fp(&insn) && self.fp_trapped() {
            return Some(self.sync(esr(ec::FP_ACCESS, ISS_FP), None, self.pc));
        }
        let next = self.pc.wrapping_add(4);
        let ev = match insn {
            Insn::Wfi => {
                if el == 0 && sctlr & sctlr::NTWI == 0 {
                    return Some(self.sync(esr(ec::WFX, ISS_WFI), None, self.pc));
                }
                self.pc = next;
                SysEvent::WaitForInterrupt
            }
            Insn::Wfe => {
                self.pc = next;
                SysEvent::Executed
            }
            Insn::Hvc { imm } => {
                if el == 0 || self.sys.cfg.psci != PsciConduit::Hvc {
                    return Some(self.undefined());
                }
                self.pc = next;
                SysEvent::Hvc(imm)
            }
            Insn::Smc { imm } => {
                if el == 0 || self.sys.cfg.psci != PsciConduit::Smc {
                    return Some(self.undefined());
                }
                self.pc = next;
                SysEvent::Smc(imm)
            }
            Insn::Eret => {
                if el == 0 {
                    return Some(self.undefined());
                }
                self.exception_return();
                SysEvent::Executed
            }
            Insn::MsrImm { field, imm } => {
                match field {
                    PstateField::SpSel if el == 0 => return Some(self.undefined()),
                    PstateField::SpSel => self.set_el_sp(el, imm & 1 != 0),
                    _ if el == 0 && sctlr & sctlr::UMA == 0 => return Some(self.sysreg_trap(raw)),
                    PstateField::DaifSet => self.sys.daif |= u32::from(imm & 0xf) << 6,
                    PstateField::DaifClr => self.sys.daif &= !(u32::from(imm & 0xf) << 6),
                }
                self.pc = next;
                SysEvent::Executed
            }
            Insn::Sys { op, rt } => {
                if el == 0 {
                    return Some(self.undefined());
                }
                let xt = self.xr(rt);
                match op {
                    SysOp::Tlbi(t) => mem.bus.tlbi(t, xt),
                    SysOp::At(at) => {
                        let (access, level) = match at {
                            AtOp::S1e1r => (Access::Read, 1),
                            AtOp::S1e1w => (Access::Write, 1),
                            AtOp::S1e0r => (Access::Read, 0),
                            AtOp::S1e0w => (Access::Write, 0),
                        };
                        match mem.bus.at(&mem.regs, xt, access, level) {
                            AtResult::Par(p) => self.sys.par_el1 = p,
                            // Abort esterno sul walk: Data Abort con CM = 1 e
                            // WnR = 1, come QEMU.
                            AtResult::Abort { fsc, ea } => {
                                let f = Pending::Abort { va: xt, fsc, ea, access: Access::Write };
                                return Some(self.deliver_fault(Some(f), raw, true));
                            }
                            AtResult::Unimplemented(what) => return Some(SysEvent::Unimplemented { raw, what }),
                        }
                    }
                    // Nessuna cache modellata: come QEMU non fanno nulla.
                    SysOp::IcIall | SysOp::DcIvac | SysOp::DcSetWay => {}
                }
                self.pc = next;
                SysEvent::Executed
            }
            Insn::CacheMaint => {
                if el == 0 && sctlr & sctlr::UCI == 0 {
                    return Some(self.sysreg_trap(raw));
                }
                self.pc = next;
                SysEvent::Executed
            }
            Insn::DcZva { .. } => {
                if el == 0 && sctlr & sctlr::DZE == 0 {
                    return Some(self.sysreg_trap(raw));
                }
                return None;
            }
            Insn::Mrs { reg, rt } => {
                if let Err(d) = self.sysreg_access(reg, false) {
                    return Some(self.deny(d, raw));
                }
                let v = self.sysreg_read(reg, env);
                self.set_x(rt, v);
                self.pc = next;
                SysEvent::Executed
            }
            Insn::Msr { reg, rt } => {
                if let Err(d) = self.sysreg_access(reg, true) {
                    return Some(self.deny(d, raw));
                }
                let v = self.xr(rt);
                self.sysreg_write(reg, v, mem.bus, env);
                self.pc = next;
                SysEvent::Executed
            }
            _ => return None,
        };
        Some(ev)
    }
}
