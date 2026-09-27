//! PL061 GPIO (ARM DDI0190), like QEMU's `hw/gpio/pl061.c` in the virt
//! machine: eight lines, line 3 wired to the power key (`gpio-keys`,
//! KEY_POWER) in the device tree.
//!
//! The input lines are driven by the host with [`Pl061::set_input`] (from the engine's single
//! recordable point); undriven ones read 0 (QEMU's virt
//! sets `pullups = 0`, `pulldowns = 0xff`). Same interrupt logic
//! as QEMU (`pl061_update`): a change of an input with IS = 0
//! (edge) accumulates in RIS the edge chosen by IBE/IEV; with IS = 1 (level)
//! RIS turns back on as long as the level stays active; IC clears RIS bits.
//! The line to the GIC is `RIS & IE != 0`.
//!
//! DATA is addressed with the mask in bits 9:2 of the offset: only the mask
//! bits are read and written, and only output lines
//! (DIR = 1) are written. The Linux driver uses byte accesses (`readb`/`writeb`).

use crate::bus::{MmioDevice, sub_word};

pub const DATA: u64 = 0x000;
pub const DIR: u64 = 0x400;
pub const IS: u64 = 0x404;
pub const IBE: u64 = 0x408;
pub const IEV: u64 = 0x40C;
pub const IE: u64 = 0x410;
pub const RIS: u64 = 0x414;
pub const MIS: u64 = 0x418;
pub const IC: u64 = 0x41C;
pub const AFSEL: u64 = 0x420;
pub const PERIPH_ID0: u64 = 0xFE0;

/// Power key line in virt (`gpio-keys`, KEY_POWER).
pub const POWER_KEY_LINE: u32 = 3;

/// PeriphID0-3 and CellID0-3 (the same values as QEMU).
const ID: [u8; 8] = [0x61, 0x10, 0x04, 0x00, 0x0D, 0xF0, 0x05, 0xB1];

#[derive(Clone, Debug, Default)]
pub struct Pl061 {
    /// Line values: outputs written by the guest, inputs by the host.
    data: u8,
    /// Inputs already seen by the interrupt logic.
    old_in: u8,
    dir: u8,
    is: u8,
    ibe: u8,
    iev: u8,
    ie: u8,
    ris: u8,
    afsel: u8,
}

impl Pl061 {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drives input line `line` (0..8). Ignored for lines
    /// configured as outputs (like QEMU).
    pub fn set_input(&mut self, line: u32, level: bool) {
        let mask = 1u8 << (line & 7);
        if self.dir & mask == 0 {
            self.data = (self.data & !mask) | if level { mask } else { 0 };
            self.update();
        }
    }

    /// Value of the output lines (DIR = 1); the others read 0.
    pub fn outputs(&self) -> u8 {
        self.data & self.dir
    }

    /// Level of the IRQ line to the GIC (GPIOINTR).
    pub fn irq_level(&self) -> bool {
        self.ris & self.ie != 0
    }

    fn update(&mut self) {
        let changed = (self.old_in ^ self.data) & !self.dir;
        if changed != 0 {
            self.old_in = self.data;
            let edge = changed & !self.is;
            // Any edge with IBE, otherwise the one chosen by IEV
            // (1 = rising): the bit goes into RIS if the new level is IEV.
            self.ris |= edge & (self.ibe | !(self.data ^ self.iev));
        }
        self.ris |= !(self.data ^ self.iev) & self.is;
    }

    fn read_reg(&self, reg: u64) -> u8 {
        match reg {
            0x000..=0x3FC => self.data & (reg >> 2) as u8,
            DIR => self.dir,
            IS => self.is,
            IBE => self.ibe,
            IEV => self.iev,
            IE => self.ie,
            RIS => self.ris,
            MIS => self.ris & self.ie,
            AFSEL => self.afsel,
            0xFE0..=0xFFC => ID[((reg - PERIPH_ID0) / 4) as usize],
            _ => 0,
        }
    }

    fn write_reg(&mut self, reg: u64, v: u8) {
        match reg {
            0x000..=0x3FC => {
                let mask = (reg >> 2) as u8 & self.dir;
                self.data = (self.data & !mask) | (v & mask);
            }
            DIR => self.dir = v,
            IS => self.is = v,
            IBE => self.ibe = v,
            IEV => self.iev = v,
            IE => self.ie = v,
            IC => self.ris &= !v,
            AFSEL => self.afsel = v,
            _ => return,
        }
        self.update();
    }
}

impl MmioDevice for Pl061 {
    fn read(&mut self, offset: u64, size: u8) -> u64 {
        if offset >= 0x1000 || size > 4 {
            return 0;
        }
        sub_word(u32::from(self.read_reg(offset & !3)), offset, size)
    }

    fn write(&mut self, offset: u64, size: u8, value: u64) {
        if offset >= 0x1000 || size > 4 || offset & 3 != 0 {
            return;
        }
        self.write_reg(offset, value as u8);
    }
}

// ---- Snapshot (M6, ADR 0015) -------------------------------------------------

impl vetro_snapshot::Snapshot for Pl061 {
    fn save(&self, w: &mut vetro_snapshot::Writer) {
        w.raw(&[
            self.data,
            self.old_in,
            self.dir,
            self.is,
            self.ibe,
            self.iev,
            self.ie,
            self.ris,
            self.afsel,
        ]);
    }

    fn restore(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        let b = r.raw(9)?;
        [self.data, self.old_in, self.dir, self.is, self.ibe, self.iev, self.ie, self.ris, self.afsel] =
            b.try_into().expect("9 bytes");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identificazione() {
        let mut g = Pl061::new();
        let id: Vec<u64> = (0..8).map(|i| g.read(PERIPH_ID0 + 4 * i, 4)).collect();
        assert_eq!(id, [0x61, 0x10, 0x04, 0x00, 0x0D, 0xF0, 0x05, 0xB1]);
        // The Linux driver reads bytes.
        assert_eq!(g.read(PERIPH_ID0, 1), 0x61);
    }

    #[test]
    fn data_con_maschera_nell_indirizzo() {
        let mut g = Pl061::new();
        g.write(DIR, 1, 0x0F); // lines 0..3 as outputs
        g.write(0x3FC, 1, 0xFF); // full mask: only the outputs change
        assert_eq!(g.read(0x3FC, 1), 0x0F);
        g.write(0x3FC, 1, 0x00);
        g.write(0x1 << 2 | 0x2 << 2, 1, 0xFF); // mask 0b11: lines 0 and 1
        assert_eq!(g.read(0x3FC, 1), 0x03);
        assert_eq!(g.read(0x2 << 2, 1), 0x02, "read masked to line 1 only");
        assert_eq!(g.outputs(), 0x03);
        // An input driven by the host can be read; an output line cannot.
        g.set_input(5, true);
        g.set_input(1, false);
        assert_eq!(g.read(0x3FC, 1), 0x23);
        g.write(0x3FC, 1, 0x00);
        assert_eq!(g.read(0x3FC, 1), 0x20, "DATA does not write the inputs");
    }

    /// Like the Linux driver with gpio-keys (IRQ_TYPE_EDGE_BOTH: IS = 0,
    /// IBE = 1): pressing and releasing the power key give one
    /// interrupt each, cleared by IC.
    #[test]
    fn tasto_su_entrambi_i_fronti() {
        let mut g = Pl061::new();
        let m = 1 << POWER_KEY_LINE;
        g.write(IBE, 1, m);
        g.write(IE, 1, m);
        assert!(!g.irq_level());
        g.set_input(POWER_KEY_LINE, true);
        assert!(g.irq_level());
        assert_eq!(g.read(MIS, 1), m);
        g.write(IC, 1, m);
        assert!(!g.irq_level(), "edge: IC clears it even with the key held");
        g.set_input(POWER_KEY_LINE, true);
        assert!(!g.irq_level(), "no change, no edge");
        g.set_input(POWER_KEY_LINE, false);
        assert!(g.irq_level(), "the release is an edge");
        g.write(IC, 1, 0xFF);
        assert_eq!(g.read(RIS, 1), 0);
    }

    #[test]
    fn fronte_scelto_da_iev_e_maschera() {
        let mut g = Pl061::new();
        g.write(IEV, 1, 0x01); // line 0: rising; line 1: falling
        g.set_input(0, true);
        g.set_input(1, true);
        assert_eq!(g.read(RIS, 1), 0x01, "RIS turns on even without IE");
        assert_eq!(g.read(MIS, 1), 0);
        g.set_input(1, false);
        assert_eq!(g.read(RIS, 1), 0x03);
        g.write(IE, 1, 0x02);
        assert!(g.irq_level());
    }

    #[test]
    fn livello_si_riaccende_finche_attivo() {
        let mut g = Pl061::new();
        // IEV first: with IS = 1 and IEV = 0 the line at 0 is already "active"
        // (low level) and RIS turns on at once, as in QEMU.
        g.write(IEV, 1, 0x04); // high level on line 2
        g.write(IS, 1, 0x04);
        g.write(IE, 1, 0x04);
        assert!(!g.irq_level());
        g.set_input(2, true);
        assert!(g.irq_level());
        g.write(IC, 1, 0x04);
        assert!(g.irq_level(), "level still active: RIS goes back to 1");
        g.set_input(2, false);
        g.write(IC, 1, 0x04);
        assert!(!g.irq_level());
        // Active-low level (IEV = 0) with the line at 0: active immediately.
        g.write(IEV, 1, 0x00);
        assert!(g.irq_level());
    }

    #[test]
    fn ingresso_ignorato_sulle_uscite() {
        let mut g = Pl061::new();
        g.write(DIR, 1, 0x08);
        g.write(IBE, 1, 0x08);
        g.write(IE, 1, 0x08);
        g.set_input(3, true);
        assert!(!g.irq_level());
        assert_eq!(g.read(0x3FC, 1), 0);
    }
}
