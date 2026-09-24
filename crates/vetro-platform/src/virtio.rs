//! Trasporto virtio-mmio vuoto (virtio v1.2, sezione 4.2.2).
//!
//! Come gli slot liberi di QEMU virt: MagicValue e Version sono validi e
//! DeviceID vale 0, così il driver Linux riconosce il trasporto e lo salta.
//! I dispositivi veri (blk, net) sostituiranno questi slot in M3.

use crate::bus::{MmioDevice, sub_word};

pub const MAGIC_VALUE: u64 = 0x000;
pub const VERSION: u64 = 0x004;
pub const DEVICE_ID: u64 = 0x008;
pub const VENDOR_ID: u64 = 0x00C;

/// "virt" in little endian.
pub const MAGIC: u32 = 0x7472_6976;

#[derive(Clone, Copy, Debug, Default)]
pub struct VirtioMmioEmpty;

impl MmioDevice for VirtioMmioEmpty {
    fn read(&mut self, offset: u64, size: u8) -> u64 {
        let word = match offset & !3 {
            MAGIC_VALUE => MAGIC,
            VERSION => 2,
            _ => 0,
        };
        if size > 4 { 0 } else { sub_word(word, offset, size) }
    }

    fn write(&mut self, _offset: u64, _size: u8, _value: u64) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_vuoto_riconoscibile() {
        let mut v = VirtioMmioEmpty;
        assert_eq!(v.read(MAGIC_VALUE, 4), 0x7472_6976);
        assert_eq!(v.read(VERSION, 4), 2);
        assert_eq!(v.read(DEVICE_ID, 4), 0);
        v.write(0x070, 4, 0xF);
        assert_eq!(v.read(0x070, 4), 0);
    }
}
