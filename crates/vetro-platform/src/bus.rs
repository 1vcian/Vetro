//! Bus MMIO: instrada letture e scritture fisiche verso i dispositivi in
//! base a intervalli di indirizzi.

use core::any::Any;
use core::fmt;

/// Dispositivo mappato in memoria. `offset` è relativo alla base della
/// regione; `size` è 1, 2, 4 o 8 byte. I valori stanno nei bit bassi.
///
/// `Any` come supertrait permette all'host di ritrovare il tipo concreto
/// con [`Bus::device_mut`] (es. per svuotare l'uscita della UART).
pub trait MmioDevice: Any {
    fn read(&mut self, offset: u64, size: u8) -> u64;
    fn write(&mut self, offset: u64, size: u8, value: u64);
}

/// Identificativo stabile di un dispositivo mappato (ordine di inserimento).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DeviceId(usize);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BusError {
    /// La regione ha dimensione zero o supera la fine dello spazio a 64 bit.
    InvalidRange { base: u64, size: u64 },
    /// La regione si sovrappone a una già mappata.
    Overlap { base: u64, size: u64, existing: &'static str },
}

impl fmt::Display for BusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BusError::InvalidRange { base, size } => {
                write!(f, "intervallo non valido: base {base:#x}, dimensione {size:#x}")
            }
            BusError::Overlap { base, size, existing } => {
                write!(f, "la regione {base:#x}+{size:#x} si sovrappone a {existing}")
            }
        }
    }
}

struct Region {
    base: u64,
    size: u64,
    name: &'static str,
    device: Box<dyn MmioDevice>,
}

impl Region {
    fn end(&self) -> u64 {
        self.base + self.size
    }
}

/// Bus MMIO. Le regioni non si sovrappongono; una lettura o scrittura deve
/// stare tutta dentro una regione, altrimenti non raggiunge nessuno.
#[derive(Default)]
pub struct Bus {
    /// Dispositivi in ordine di inserimento (l'indice è il `DeviceId`).
    regions: Vec<Region>,
    /// Indici di `regions` ordinati per base, per la ricerca binaria.
    by_base: Vec<usize>,
}

impl Bus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mappa `device` su `[base, base + size)`.
    pub fn map(
        &mut self,
        base: u64,
        size: u64,
        name: &'static str,
        device: Box<dyn MmioDevice>,
    ) -> Result<DeviceId, BusError> {
        if size == 0 || base.checked_add(size).is_none() {
            return Err(BusError::InvalidRange { base, size });
        }
        let end = base + size;
        if let Some(r) = self.regions.iter().find(|r| base < r.end() && r.base < end) {
            return Err(BusError::Overlap { base, size, existing: r.name });
        }
        let id = self.regions.len();
        self.regions.push(Region { base, size, name, device });
        let pos = self.by_base.partition_point(|&i| self.regions[i].base < base);
        self.by_base.insert(pos, id);
        Ok(DeviceId(id))
    }

    /// Dispositivo e offset per un accesso di `size` byte a `addr`.
    pub fn find(&self, addr: u64, size: u8) -> Option<(DeviceId, u64)> {
        let pos = self.by_base.partition_point(|&i| self.regions[i].base <= addr);
        let idx = *self.by_base.get(pos.checked_sub(1)?)?;
        let r = &self.regions[idx];
        let last = addr.checked_add(u64::from(size.max(1)) - 1)?;
        (last < r.end()).then_some((DeviceId(idx), addr - r.base))
    }

    /// Lettura MMIO. `None` se nessun dispositivo copre l'accesso: la CPU lo
    /// tratterà come errore esterno (SError/abort sincrono) in M3.
    pub fn read(&mut self, addr: u64, size: u8) -> Option<u64> {
        let (DeviceId(i), off) = self.find(addr, size)?;
        Some(self.regions[i].device.read(off, size) & size_mask(size))
    }

    /// Scrittura MMIO. `false` se nessun dispositivo copre l'accesso.
    pub fn write(&mut self, addr: u64, size: u8, value: u64) -> bool {
        match self.find(addr, size) {
            Some((DeviceId(i), off)) => {
                self.regions[i].device.write(off, size, value & size_mask(size));
                true
            }
            None => false,
        }
    }

    /// Nome, base e dimensione di una regione.
    pub fn region(&self, id: DeviceId) -> Option<(&'static str, u64, u64)> {
        self.regions.get(id.0).map(|r| (r.name, r.base, r.size))
    }

    /// Accesso tipizzato a un dispositivo mappato.
    pub fn device<T: MmioDevice>(&self, id: DeviceId) -> Option<&T> {
        let dev: &dyn Any = self.regions.get(id.0)?.device.as_ref();
        dev.downcast_ref::<T>()
    }

    /// Accesso tipizzato e mutabile a un dispositivo mappato.
    pub fn device_mut<T: MmioDevice>(&mut self, id: DeviceId) -> Option<&mut T> {
        let dev: &mut dyn Any = self.regions.get_mut(id.0)?.device.as_mut();
        dev.downcast_mut::<T>()
    }
}

/// Maschera dei bit validi per un accesso di `size` byte.
pub fn size_mask(size: u8) -> u64 {
    match size {
        1 => 0xFF,
        2 => 0xFFFF,
        4 => 0xFFFF_FFFF,
        _ => u64::MAX,
    }
}

/// Registro a 32 bit dentro un accesso: restituisce il valore letto da un
/// registro allineato a 4 byte quando l'accesso è di 1, 2 o 4 byte (i byte
/// giusti già spostati in basso). Utile ai dispositivi AMBA a 32 bit.
pub(crate) fn sub_word(word: u32, offset: u64, size: u8) -> u64 {
    let shift = (offset & 3) * 8;
    (u64::from(word) >> shift) & size_mask(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dispositivo di prova: memoria di 16 byte e registro dell'ultimo accesso.
    #[derive(Default)]
    struct Scratch {
        mem: [u8; 16],
        last: Option<(u64, u8)>,
    }

    impl MmioDevice for Scratch {
        fn read(&mut self, offset: u64, size: u8) -> u64 {
            self.last = Some((offset, size));
            let mut v = 0u64;
            for i in (0..u64::from(size)).rev() {
                v = (v << 8) | u64::from(self.mem[(offset + i) as usize]);
            }
            v
        }
        fn write(&mut self, offset: u64, size: u8, value: u64) {
            self.last = Some((offset, size));
            for i in 0..u64::from(size) {
                self.mem[(offset + i) as usize] = (value >> (8 * i)) as u8;
            }
        }
    }

    #[test]
    fn instrada_per_intervallo_con_offset_relativo() {
        let mut bus = Bus::new();
        let a = bus.map(0x1000, 16, "a", Box::new(Scratch::default())).unwrap();
        let b = bus.map(0x100, 16, "b", Box::new(Scratch::default())).unwrap();
        assert!(bus.write(0x1004, 4, 0xDEAD_BEEF));
        assert_eq!(bus.read(0x1004, 4), Some(0xDEAD_BEEF));
        assert_eq!(bus.read(0x1005, 1), Some(0xBE));
        assert_eq!(bus.device::<Scratch>(a).unwrap().last, Some((5, 1)));
        assert!(bus.write(0x10F, 1, 0x7F));
        assert_eq!(bus.device::<Scratch>(b).unwrap().mem[15], 0x7F);
        assert_eq!(bus.region(b), Some(("b", 0x100, 16)));
    }

    #[test]
    fn accessi_fuori_regione_non_arrivano() {
        let mut bus = Bus::new();
        bus.map(0x1000, 16, "a", Box::new(Scratch::default())).unwrap();
        assert_eq!(bus.read(0xFFF, 1), None);
        assert_eq!(bus.read(0x1010, 1), None);
        // Accesso a cavallo della fine della regione.
        assert_eq!(bus.read(0x100C, 8), None);
        assert!(!bus.write(0x2000, 4, 1));
        assert_eq!(bus.read(u64::MAX, 8), None);
    }

    #[test]
    fn rifiuta_sovrapposizioni_e_intervalli_vuoti() {
        let mut bus = Bus::new();
        bus.map(0x1000, 0x100, "a", Box::new(Scratch::default())).unwrap();
        assert!(matches!(
            bus.map(0x10FF, 0x10, "b", Box::new(Scratch::default())),
            Err(BusError::Overlap { existing: "a", .. })
        ));
        assert!(matches!(
            bus.map(0xF00, 0x101, "c", Box::new(Scratch::default())),
            Err(BusError::Overlap { .. })
        ));
        assert!(matches!(
            bus.map(0x5000, 0, "d", Box::new(Scratch::default())),
            Err(BusError::InvalidRange { .. })
        ));
        assert!(matches!(
            bus.map(u64::MAX, 2, "e", Box::new(Scratch::default())),
            Err(BusError::InvalidRange { .. })
        ));
        // Regione adiacente: ammessa.
        assert!(bus.map(0x1100, 0x10, "f", Box::new(Scratch::default())).is_ok());
    }

    #[test]
    fn downcast_col_tipo_sbagliato_fallisce() {
        struct Altro;
        impl MmioDevice for Altro {
            fn read(&mut self, _: u64, _: u8) -> u64 {
                0
            }
            fn write(&mut self, _: u64, _: u8, _: u64) {}
        }
        let mut bus = Bus::new();
        let id = bus.map(0, 4, "altro", Box::new(Altro)).unwrap();
        assert!(bus.device_mut::<Scratch>(id).is_none());
        assert!(bus.device_mut::<Altro>(id).is_some());
    }

    #[test]
    fn sub_word_estrae_i_byte() {
        assert_eq!(sub_word(0x4433_2211, 0, 4), 0x4433_2211);
        assert_eq!(sub_word(0x4433_2211, 1, 1), 0x22);
        assert_eq!(sub_word(0x4433_2211, 2, 2), 0x4433);
    }
}
