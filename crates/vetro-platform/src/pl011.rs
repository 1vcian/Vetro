//! PL011 UART (ARM DDI0183).
//!
//! Functional model, like QEMU: transmission is instantaneous (the transmit
//! FIFO is always empty) and bytes end up in a buffer read by the host;
//! reception draws from an input queue filled by the host. Baud rate and
//! line format are stored but have no effect.
//!
//! Choices, where the manual leaves room or QEMU simplifies:
//! - transmission happens even with UARTEN or TXE off (needed by the
//!   earlycon, as in QEMU);
//! - reception happens only with UARTEN and RXE on: until then the
//!   bytes stay in the host queue, they are not lost;
//! - the RX interrupt fires as soon as the FIFO holds one byte (threshold 1, like
//!   QEMU) and drops when the FIFO empties; IFLS is only stored;
//! - TXRIS rises on every write to DR and drops only with ICR.

use std::collections::VecDeque;

use crate::bus::{MmioDevice, sub_word};

pub const DR: u64 = 0x000;
pub const RSR_ECR: u64 = 0x004;
pub const FR: u64 = 0x018;
pub const ILPR: u64 = 0x020;
pub const IBRD: u64 = 0x024;
pub const FBRD: u64 = 0x028;
pub const LCR_H: u64 = 0x02C;
pub const CR: u64 = 0x030;
pub const IFLS: u64 = 0x034;
pub const IMSC: u64 = 0x038;
pub const RIS: u64 = 0x03C;
pub const MIS: u64 = 0x040;
pub const ICR: u64 = 0x044;
pub const DMACR: u64 = 0x048;
/// First identification register (PeriphID0); PeriphID1-3 and CellID0-3 follow.
pub const PERIPH_ID0: u64 = 0xFE0;

/// FR bits.
pub const FR_BUSY: u32 = 1 << 3;
pub const FR_RXFE: u32 = 1 << 4;
pub const FR_TXFF: u32 = 1 << 5;
pub const FR_RXFF: u32 = 1 << 6;
pub const FR_TXFE: u32 = 1 << 7;

/// LCR_H bit: FIFO enable.
pub const LCR_H_FEN: u32 = 1 << 4;

/// CR bits.
pub const CR_UARTEN: u32 = 1 << 0;
pub const CR_LBE: u32 = 1 << 7;
pub const CR_TXE: u32 = 1 << 8;
pub const CR_RXE: u32 = 1 << 9;

/// Interrupt bits (IMSC, RIS, MIS, ICR).
pub const INT_RX: u32 = 1 << 4;
pub const INT_TX: u32 = 1 << 5;
pub const INT_RT: u32 = 1 << 6;
pub const INT_MASK: u32 = 0x7FF;

/// Depth of the receive FIFO with LCR_H.FEN on.
pub const FIFO_DEPTH: usize = 16;

/// PeriphID0-3 and CellID0-3 (the same values as QEMU).
const ID: [u8; 8] = [0x11, 0x10, 0x14, 0x00, 0x0D, 0xF0, 0x05, 0xB1];

#[derive(Clone, Debug)]
pub struct Pl011 {
    /// Bytes the host wants to deliver to the guest, not yet in the FIFO.
    input: VecDeque<u8>,
    /// Receive FIFO (8-bit data + 4 error bits, always zero here).
    rx_fifo: VecDeque<u16>,
    /// Bytes transmitted by the guest, waiting for the host to read them.
    output: Vec<u8>,
    rsr: u32,
    ilpr: u32,
    ibrd: u32,
    fbrd: u32,
    lcr_h: u32,
    cr: u32,
    ifls: u32,
    imsc: u32,
    ris: u32,
    dmacr: u32,
}

impl Default for Pl011 {
    fn default() -> Self {
        Self::new()
    }
}

impl Pl011 {
    /// UART in its reset state (CR = 0x300, IFLS = 0x12).
    pub fn new() -> Self {
        Self {
            input: VecDeque::new(),
            rx_fifo: VecDeque::new(),
            output: Vec::new(),
            rsr: 0,
            ilpr: 0,
            ibrd: 0,
            fbrd: 0,
            lcr_h: 0,
            cr: CR_TXE | CR_RXE,
            ifls: 0x12,
            imsc: 0,
            ris: 0,
            dmacr: 0,
        }
    }

    /// Queues input bytes for the guest.
    pub fn push_input(&mut self, bytes: &[u8]) {
        self.input.extend(bytes);
        self.fill_rx();
    }

    /// Input bytes not yet read by the guest (host queue + FIFO).
    pub fn pending_input(&self) -> usize {
        self.input.len() + self.rx_fifo.len()
    }

    /// Output accumulated by the guest, without consuming it.
    pub fn output(&self) -> &[u8] {
        &self.output
    }

    /// Consumes and returns the accumulated output.
    pub fn take_output(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.output)
    }

    /// Level of the IRQ line to the GIC (UARTINTR): high if an
    /// unmasked interrupt is active.
    pub fn irq_level(&self) -> bool {
        self.ris & self.imsc != 0
    }

    fn fifo_depth(&self) -> usize {
        if self.lcr_h & LCR_H_FEN != 0 { FIFO_DEPTH } else { 1 }
    }

    fn rx_enabled(&self) -> bool {
        self.cr & (CR_UARTEN | CR_RXE) == CR_UARTEN | CR_RXE
    }

    /// Moves bytes from the host queue to the FIFO while there is room.
    fn fill_rx(&mut self) {
        if !self.rx_enabled() {
            return;
        }
        while self.rx_fifo.len() < self.fifo_depth() {
            match self.input.pop_front() {
                Some(b) => self.rx_fifo.push_back(u16::from(b)),
                None => break,
            }
        }
        if !self.rx_fifo.is_empty() {
            self.ris |= INT_RX;
        }
    }

    fn flags(&self) -> u32 {
        let mut fr = FR_TXFE;
        if self.rx_fifo.is_empty() {
            fr |= FR_RXFE;
        }
        if self.rx_fifo.len() >= self.fifo_depth() {
            fr |= FR_RXFF;
        }
        fr
    }

    fn read_reg(&mut self, reg: u64) -> u32 {
        match reg {
            DR => {
                let v = self.rx_fifo.pop_front().unwrap_or(0);
                if self.rx_fifo.is_empty() {
                    self.ris &= !(INT_RX | INT_RT);
                }
                self.fill_rx();
                u32::from(v)
            }
            RSR_ECR => self.rsr,
            FR => self.flags(),
            ILPR => self.ilpr,
            IBRD => self.ibrd,
            FBRD => self.fbrd,
            LCR_H => self.lcr_h,
            CR => self.cr,
            IFLS => self.ifls,
            IMSC => self.imsc,
            RIS => self.ris,
            MIS => self.ris & self.imsc,
            DMACR => self.dmacr,
            0xFE0..=0xFFC => u32::from(ID[((reg - PERIPH_ID0) / 4) as usize]),
            _ => 0,
        }
    }

    fn write_reg(&mut self, reg: u64, v: u32) {
        match reg {
            DR => {
                let byte = v as u8;
                if self.cr & CR_LBE != 0 {
                    // Loopback: the byte goes back into the receive FIFO.
                    if self.rx_fifo.len() < self.fifo_depth() {
                        self.rx_fifo.push_back(u16::from(byte));
                        self.ris |= INT_RX;
                    }
                } else {
                    self.output.push(byte);
                }
                self.ris |= INT_TX;
            }
            RSR_ECR => self.rsr = 0,
            ILPR => self.ilpr = v & 0xFF,
            IBRD => self.ibrd = v & 0xFFFF,
            FBRD => self.fbrd = v & 0x3F,
            LCR_H => {
                if (self.lcr_h ^ v) & LCR_H_FEN != 0 {
                    // Changing FEN empties the FIFO (like QEMU); bytes already
                    // moved to the FIFO are lost, the host queue is not.
                    self.rx_fifo.clear();
                    self.ris &= !(INT_RX | INT_RT);
                }
                self.lcr_h = v & 0xFF;
                self.fill_rx();
            }
            CR => {
                self.cr = v & 0xFF87;
                self.fill_rx();
            }
            IFLS => self.ifls = v & 0x3F,
            IMSC => self.imsc = v & INT_MASK,
            ICR => self.ris &= !(v & INT_MASK),
            DMACR => self.dmacr = v & 0x7,
            _ => {}
        }
    }
}

impl MmioDevice for Pl011 {
    fn read(&mut self, offset: u64, size: u8) -> u64 {
        if offset >= 0x1000 || size > 4 {
            return 0;
        }
        let reg = offset & !3;
        let word = self.read_reg(reg);
        sub_word(word, offset, size)
    }

    fn write(&mut self, offset: u64, size: u8, value: u64) {
        // Only register-aligned accesses (DR also accepts 8 and 16 bits).
        if offset >= 0x1000 || size > 4 || offset & 3 != 0 {
            return;
        }
        self.write_reg(offset, value as u32);
    }
}

// ---- Snapshot (M6, ADR 0015) -------------------------------------------------

/// Registers, receive FIFO, host input not yet in the FIFO and
/// output not yet read by the host.
impl vetro_snapshot::Snapshot for Pl011 {
    fn save(&self, w: &mut vetro_snapshot::Writer) {
        w.seq(&self.input, |w, &b| w.u8(b));
        w.seq(&self.rx_fifo, |w, &v| w.u16(v));
        w.bytes(&self.output);
        for v in [
            self.rsr, self.ilpr, self.ibrd, self.fbrd, self.lcr_h, self.cr, self.ifls, self.imsc, self.ris,
            self.dmacr,
        ] {
            w.u32(v);
        }
    }

    fn restore(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        self.input = r.seq(1, |r| r.u8())?.into();
        self.rx_fifo = r.seq(2, |r| r.u16())?.into();
        self.output = r.vec()?;
        for v in [
            &mut self.rsr,
            &mut self.ilpr,
            &mut self.ibrd,
            &mut self.fbrd,
            &mut self.lcr_h,
            &mut self.cr,
            &mut self.ifls,
            &mut self.imsc,
            &mut self.ris,
            &mut self.dmacr,
        ] {
            *v = r.u32()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> Pl011 {
        let mut u = Pl011::new();
        u.write(CR, 4, u64::from(CR_UARTEN | CR_TXE | CR_RXE));
        u.write(LCR_H, 4, u64::from(LCR_H_FEN | 0x60));
        u
    }

    #[test]
    fn registri_di_identificazione() {
        let mut u = Pl011::new();
        let id: Vec<u64> = (0..8).map(|i| u.read(PERIPH_ID0 + 4 * i, 4)).collect();
        assert_eq!(id, [0x11, 0x10, 0x14, 0x00, 0x0D, 0xF0, 0x05, 0xB1]);
    }

    #[test]
    fn valori_di_reset() {
        let mut u = Pl011::new();
        assert_eq!(u.read(FR, 4), u64::from(FR_RXFE | FR_TXFE));
        assert_eq!(u.read(CR, 4), 0x300);
        assert_eq!(u.read(IFLS, 4), 0x12);
        assert_eq!(u.read(RIS, 4), 0);
        assert!(!u.irq_level());
    }

    #[test]
    fn trasmissione_verso_l_host_anche_a_uart_spenta() {
        let mut u = Pl011::new();
        for &b in b"ciao" {
            u.write(DR, 1, u64::from(b));
        }
        assert_eq!(u.output(), b"ciao");
        assert_eq!(u.take_output(), b"ciao");
        assert!(u.output().is_empty());
        assert_eq!(u.read(RIS, 4) as u32 & INT_TX, INT_TX);
    }

    #[test]
    fn ricezione_con_interrupt_rx() {
        let mut u = enabled();
        u.write(IMSC, 4, u64::from(INT_RX | INT_RT));
        u.push_input(b"ab");
        assert_eq!(u.read(FR, 4) as u32 & FR_RXFE, 0);
        assert_eq!(u.read(MIS, 4) as u32, INT_RX);
        assert!(u.irq_level());
        assert_eq!(u.read(DR, 4), u64::from(b'a'));
        assert!(u.irq_level());
        assert_eq!(u.read(DR, 1), u64::from(b'b'));
        assert!(!u.irq_level(), "the empty FIFO lowers RX");
        assert_ne!(u.read(FR, 4) as u32 & FR_RXFE, 0);
    }

    #[test]
    fn input_trattenuto_finche_la_ricezione_e_spenta() {
        let mut u = Pl011::new();
        u.push_input(b"x");
        assert_ne!(u.read(FR, 4) as u32 & FR_RXFE, 0);
        assert_eq!(u.pending_input(), 1);
        u.write(CR, 4, u64::from(CR_UARTEN | CR_RXE));
        assert_eq!(u.read(DR, 4), u64::from(b'x'));
        assert_eq!(u.pending_input(), 0);
    }

    #[test]
    fn fifo_da_uno_senza_fen_e_da_sedici_con_fen() {
        let mut u = Pl011::new();
        u.write(CR, 4, u64::from(CR_UARTEN | CR_RXE));
        u.push_input(&[1, 2, 3]);
        assert_ne!(u.read(FR, 4) as u32 & FR_RXFF, 0, "without FEN the FIFO has one slot");
        assert_eq!(u.read(DR, 4), 1);
        assert_eq!(u.read(DR, 4), 2);
        u.write(LCR_H, 4, u64::from(LCR_H_FEN));
        u.push_input(&[0u8; 20]);
        // 3 was in the FIFO: changing FEN discards it (like QEMU).
        assert_eq!(u.rx_fifo.len(), FIFO_DEPTH);
        assert_eq!(u.pending_input(), 20);
        assert_ne!(u.read(FR, 4) as u32 & FR_RXFF, 0);
    }

    #[test]
    fn icr_e_maschere() {
        let mut u = Pl011::new();
        u.write(DR, 4, u64::from(b'z'));
        assert!(!u.irq_level(), "TX not enabled in IMSC");
        u.write(IMSC, 4, 0xFFFF_FFFF);
        assert_eq!(u.read(IMSC, 4), u64::from(INT_MASK));
        assert!(u.irq_level());
        u.write(ICR, 4, u64::from(INT_TX));
        assert_eq!(u.read(RIS, 4), 0);
        assert!(!u.irq_level());
    }

    #[test]
    fn loopback_riporta_i_byte_in_ricezione() {
        let mut u = enabled();
        u.write(CR, 4, u64::from(CR_UARTEN | CR_TXE | CR_RXE | CR_LBE));
        u.write(DR, 4, 0x55);
        assert!(u.output().is_empty());
        assert_eq!(u.read(DR, 4), 0x55);
    }

    #[test]
    fn registri_di_configurazione() {
        let mut u = Pl011::new();
        u.write(IBRD, 4, 0x1_0013);
        u.write(FBRD, 4, 0xFF);
        u.write(LCR_H, 4, 0x70);
        assert_eq!(u.read(IBRD, 4), 0x13);
        assert_eq!(u.read(FBRD, 4), 0x3F);
        assert_eq!(u.read(LCR_H, 4), 0x70);
        // Unaligned writes ignored, byte reads allowed.
        u.write(IBRD + 1, 1, 0xAA);
        assert_eq!(u.read(IBRD, 4), 0x13);
        assert_eq!(u.read(CR + 1, 1), 0x03);
    }
}
