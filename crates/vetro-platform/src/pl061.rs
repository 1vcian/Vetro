//! GPIO PL061 (ARM DDI0190), come `hw/gpio/pl061.c` di QEMU nella macchina
//! virt: otto linee, la 3 collegata al tasto di spegnimento (`gpio-keys`,
//! KEY_POWER) nel device tree.
//!
//! Le linee in ingresso le pilota l'host con [`Pl061::set_input`] (dall'unico
//! punto registrabile del motore); quelle non pilotate valgono 0 (la virt di
//! QEMU imposta `pullups = 0`, `pulldowns = 0xff`). Stessa logica di
//! interrupt di QEMU (`pl061_update`): un cambio di un ingresso con IS = 0
//! (fronte) accumula in RIS il fronte scelto da IBE/IEV; con IS = 1 (livello)
//! RIS si riaccende finché il livello resta attivo; IC azzera i bit di RIS.
//! La linea verso il GIC è `RIS & IE != 0`.
//!
//! DATA si indirizza con la maschera nei bit 9:2 dell'offset: si leggono e
//! si scrivono solo i bit della maschera, e si scrivono solo le linee in
//! uscita (DIR = 1). Il driver Linux accede a byte (`readb`/`writeb`).

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

/// Linea del tasto di spegnimento nella virt (`gpio-keys`, KEY_POWER).
pub const POWER_KEY_LINE: u32 = 3;

/// PeriphID0-3 e CellID0-3 (gli stessi valori di QEMU).
const ID: [u8; 8] = [0x61, 0x10, 0x04, 0x00, 0x0D, 0xF0, 0x05, 0xB1];

#[derive(Clone, Debug, Default)]
pub struct Pl061 {
    /// Valori delle linee: uscite scritte dal guest, ingressi dall'host.
    data: u8,
    /// Ingressi già visti dalla logica di interrupt.
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

    /// Pilota la linea d'ingresso `line` (0..8). Ignorato per le linee
    /// configurate in uscita (come QEMU).
    pub fn set_input(&mut self, line: u32, level: bool) {
        let mask = 1u8 << (line & 7);
        if self.dir & mask == 0 {
            self.data = (self.data & !mask) | if level { mask } else { 0 };
            self.update();
        }
    }

    /// Valore delle linee in uscita (DIR = 1); le altre valgono 0.
    pub fn outputs(&self) -> u8 {
        self.data & self.dir
    }

    /// Livello della linea IRQ verso il GIC (GPIOINTR).
    pub fn irq_level(&self) -> bool {
        self.ris & self.ie != 0
    }

    fn update(&mut self) {
        let changed = (self.old_in ^ self.data) & !self.dir;
        if changed != 0 {
            self.old_in = self.data;
            let edge = changed & !self.is;
            // Qualunque fronte con IBE, altrimenti quello scelto da IEV
            // (1 = salita): il bit va in RIS se il nuovo livello è IEV.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identificazione() {
        let mut g = Pl061::new();
        let id: Vec<u64> = (0..8).map(|i| g.read(PERIPH_ID0 + 4 * i, 4)).collect();
        assert_eq!(id, [0x61, 0x10, 0x04, 0x00, 0x0D, 0xF0, 0x05, 0xB1]);
        // Il driver Linux legge a byte.
        assert_eq!(g.read(PERIPH_ID0, 1), 0x61);
    }

    #[test]
    fn data_con_maschera_nell_indirizzo() {
        let mut g = Pl061::new();
        g.write(DIR, 1, 0x0F); // linee 0..3 in uscita
        g.write(0x3FC, 1, 0xFF); // maschera piena: solo le uscite cambiano
        assert_eq!(g.read(0x3FC, 1), 0x0F);
        g.write(0x3FC, 1, 0x00);
        g.write(0x1 << 2 | 0x2 << 2, 1, 0xFF); // maschera 0b11: linee 0 e 1
        assert_eq!(g.read(0x3FC, 1), 0x03);
        assert_eq!(g.read(0x2 << 2, 1), 0x02, "lettura con maschera della sola linea 1");
        assert_eq!(g.outputs(), 0x03);
        // Un ingresso pilotato dall'host si legge; una linea in uscita no.
        g.set_input(5, true);
        g.set_input(1, false);
        assert_eq!(g.read(0x3FC, 1), 0x23);
        g.write(0x3FC, 1, 0x00);
        assert_eq!(g.read(0x3FC, 1), 0x20, "DATA non scrive gli ingressi");
    }

    /// Come il driver Linux con gpio-keys (IRQ_TYPE_EDGE_BOTH: IS = 0,
    /// IBE = 1): pressione e rilascio del tasto di spegnimento danno un
    /// interrupt ciascuno, azzerato da IC.
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
        assert!(!g.irq_level(), "fronte: IC lo azzera anche col tasto premuto");
        g.set_input(POWER_KEY_LINE, true);
        assert!(!g.irq_level(), "nessun cambio, nessun fronte");
        g.set_input(POWER_KEY_LINE, false);
        assert!(g.irq_level(), "il rilascio è un fronte");
        g.write(IC, 1, 0xFF);
        assert_eq!(g.read(RIS, 1), 0);
    }

    #[test]
    fn fronte_scelto_da_iev_e_maschera() {
        let mut g = Pl061::new();
        g.write(IEV, 1, 0x01); // linea 0: salita; linea 1: discesa
        g.set_input(0, true);
        g.set_input(1, true);
        assert_eq!(g.read(RIS, 1), 0x01, "RIS si accende anche senza IE");
        assert_eq!(g.read(MIS, 1), 0);
        g.set_input(1, false);
        assert_eq!(g.read(RIS, 1), 0x03);
        g.write(IE, 1, 0x02);
        assert!(g.irq_level());
    }

    #[test]
    fn livello_si_riaccende_finche_attivo() {
        let mut g = Pl061::new();
        // Prima IEV: con IS = 1 e IEV = 0 la linea a 0 è già "attiva"
        // (livello basso) e RIS si accende subito, come in QEMU.
        g.write(IEV, 1, 0x04); // livello alto sulla linea 2
        g.write(IS, 1, 0x04);
        g.write(IE, 1, 0x04);
        assert!(!g.irq_level());
        g.set_input(2, true);
        assert!(g.irq_level());
        g.write(IC, 1, 0x04);
        assert!(g.irq_level(), "livello ancora attivo: RIS torna a 1");
        g.set_input(2, false);
        g.write(IC, 1, 0x04);
        assert!(!g.irq_level());
        // Livello basso attivo (IEV = 0) con linea a 0: subito attivo.
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
