//! MMIO bus: routes physical reads and writes to the devices based
//! on address ranges.

use core::any::Any;
use core::fmt;

/// Memory-mapped device. `offset` is relative to the base of the
/// region; `size` is 1, 2, 4 or 8 bytes. Values sit in the low bits.
///
/// `Any` as a supertrait lets the host recover the concrete type
/// with [`Bus::device_mut`] (e.g. to drain the UART output).
pub trait MmioDevice: Any {
    fn read(&mut self, offset: u64, size: u8) -> u64;
    fn write(&mut self, offset: u64, size: u8, value: u64);
}

/// Stable identifier of a mapped device (insertion order).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DeviceId(usize);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BusError {
    /// The region has zero size or goes past the end of the 64-bit space.
    InvalidRange { base: u64, size: u64 },
    /// The region overlaps an already mapped one.
    Overlap { base: u64, size: u64, existing: &'static str },
}

impl fmt::Display for BusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BusError::InvalidRange { base, size } => {
                write!(f, "invalid range: base {base:#x}, size {size:#x}")
            }
            BusError::Overlap { base, size, existing } => {
                write!(f, "region {base:#x}+{size:#x} overlaps {existing}")
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

/// MMIO bus. Regions do not overlap; a read or write must
/// lie entirely inside one region, otherwise it reaches nobody.
#[derive(Default)]
pub struct Bus {
    /// Devices in insertion order (the index is the `DeviceId`).
    regions: Vec<Region>,
    /// Indices into `regions` sorted by base, for binary search.
    by_base: Vec<usize>,
}

impl Bus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Maps `device` onto `[base, base + size)`.
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

    /// Device and offset for an access of `size` bytes at `addr`.
    pub fn find(&self, addr: u64, size: u8) -> Option<(DeviceId, u64)> {
        let pos = self.by_base.partition_point(|&i| self.regions[i].base <= addr);
        let idx = *self.by_base.get(pos.checked_sub(1)?)?;
        let r = &self.regions[idx];
        let last = addr.checked_add(u64::from(size.max(1)) - 1)?;
        (last < r.end()).then_some((DeviceId(idx), addr - r.base))
    }

    /// MMIO read. `None` if no device covers the access: the CPU will
    /// treat it as an external error (SError/synchronous abort) in M3.
    pub fn read(&mut self, addr: u64, size: u8) -> Option<u64> {
        let (DeviceId(i), off) = self.find(addr, size)?;
        Some(self.regions[i].device.read(off, size) & size_mask(size))
    }

    /// MMIO write. `false` if no device covers the access.
    pub fn write(&mut self, addr: u64, size: u8, value: u64) -> bool {
        match self.find(addr, size) {
            Some((DeviceId(i), off)) => {
                self.regions[i].device.write(off, size, value & size_mask(size));
                true
            }
            None => false,
        }
    }

    /// Name, base and size of a region.
    pub fn region(&self, id: DeviceId) -> Option<(&'static str, u64, u64)> {
        self.regions.get(id.0).map(|r| (r.name, r.base, r.size))
    }

    /// Typed access to a mapped device.
    pub fn device<T: MmioDevice>(&self, id: DeviceId) -> Option<&T> {
        let dev: &dyn Any = self.regions.get(id.0)?.device.as_ref();
        dev.downcast_ref::<T>()
    }

    /// Typed, mutable access to a mapped device.
    pub fn device_mut<T: MmioDevice>(&mut self, id: DeviceId) -> Option<&mut T> {
        let dev: &mut dyn Any = self.regions.get_mut(id.0)?.device.as_mut();
        dev.downcast_mut::<T>()
    }
}

/// Mask of the valid bits for an access of `size` bytes.
pub fn size_mask(size: u8) -> u64 {
    match size {
        1 => 0xFF,
        2 => 0xFFFF,
        4 => 0xFFFF_FFFF,
        _ => u64::MAX,
    }
}

/// 32-bit register inside an access: returns the value read from a
/// 4-byte aligned register when the access is 1, 2 or 4 bytes (the right
/// bytes already shifted down). Useful for 32-bit AMBA devices.
pub(crate) fn sub_word(word: u32, offset: u64, size: u8) -> u64 {
    let shift = (offset & 3) * 8;
    (u64::from(word) >> shift) & size_mask(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test device: 16 bytes of memory and a record of the last access.
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
        // Access straddling the end of the region.
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
        // Adjacent region: allowed.
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
