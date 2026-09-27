//! Guest physical memory and translation of virtual addresses through the
//! guest page tables (AArch64, stage 1, 4 KiB granule),
//! without TLB and without side effects: only the descriptors in RAM are read.

/// The guest physical memory, read-only.
pub trait PhysMem {
    /// Reads `buf.len()` bytes from `pa`: false if they are not all RAM.
    fn read_phys(&self, pa: u64, buf: &mut [u8]) -> bool;
}

impl<F: Fn(u64, &mut [u8]) -> bool> PhysMem for F {
    fn read_phys(&self, pa: u64, buf: &mut [u8]) -> bool {
        self(pa, buf)
    }
}

impl PhysMem for [u8] {
    /// Memory starting at physical address 0 (for the tests).
    fn read_phys(&self, pa: u64, buf: &mut [u8]) -> bool {
        let Ok(s) = usize::try_from(pa) else { return false };
        match self.get(s..s.saturating_add(buf.len())) {
            Some(src) if src.len() == buf.len() => {
                buf.copy_from_slice(src);
                true
            }
            _ => false,
        }
    }
}

/// Translation registers of the EL1&0 regime.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Space {
    pub tcr: u64,
    pub ttbr0: u64,
    pub ttbr1: u64,
}

/// Table address in a TTBR (BADDR, without ASID or CnP).
pub fn ttbr_base(ttbr: u64) -> u64 {
    ttbr & 0x0000_ffff_ffff_fffe
}

impl Space {
    /// The same kernel space with the user table `pgd_pa` (the
    /// `mm->pgd` of a process, translated to physical).
    pub fn with_user(self, pgd_pa: u64) -> Space {
        Space { ttbr0: pgd_pa, ..self }
    }

    /// Translates `va`: physical address, or `None` if the page is not mapped
    /// (or the granule is not the 4 KiB one).
    pub fn translate(&self, mem: &(impl PhysMem + ?Sized), va: u64) -> Option<u64> {
        let hi = va >> 55 & 1 != 0;
        let (txsz, tg, ttbr) = if hi {
            (self.tcr >> 16 & 0x3f, self.tcr >> 30 & 3, self.ttbr1)
        } else {
            (self.tcr & 0x3f, self.tcr >> 14 & 3, self.ttbr0)
        };
        // 4 KiB only: TG1 = 0b10, TG0 = 0b00.
        if (hi && tg != 0b10) || (!hi && tg != 0b00) {
            return None;
        }
        let bits = 64 - txsz.clamp(16, 39) as u32;
        // The bits above the input size must all equal
        // bit 55 (not counting the top byte if TBI).
        let tbi = if hi { self.tcr >> 38 & 1 } else { self.tcr >> 37 & 1 } != 0;
        let top = if tbi { 56 } else { 64 };
        let upper = if top > bits { (va >> bits) & ((1u64 << (top - bits)) - 1) } else { 0 };
        let want = if hi { (1u64 << (top - bits)) - 1 } else { 0 };
        if upper != want {
            return None;
        }
        // Starting level: 4 - ceil((bits - 12) / 9).
        let levels = (bits - 12).div_ceil(9);
        let mut level = 4 - levels;
        let mut table = ttbr_base(ttbr);
        loop {
            let shift = 12 + 9 * (3 - level);
            let idx_bits = if level == 4 - levels { bits - shift } else { 9 };
            let idx = (va >> shift) & ((1u64 << idx_bits) - 1);
            let mut d = [0u8; 8];
            if !mem.read_phys(table + idx * 8, &mut d) {
                return None;
            }
            let desc = u64::from_le_bytes(d);
            let out = desc & 0x0000_ffff_ffff_f000;
            match desc & 3 {
                3 if level < 3 => table = out,
                3 => return Some(out | (va & 0xfff)),
                1 if level == 1 || level == 2 => {
                    let mask = (1u64 << shift) - 1;
                    return Some((out & !mask) | (va & mask));
                }
                _ => return None,
            }
            level += 1;
        }
    }

    /// Reads `buf.len()` bytes at virtual address `va`, page by
    /// page: false if a page is not mapped or is not RAM.
    pub fn read(&self, mem: &(impl PhysMem + ?Sized), va: u64, buf: &mut [u8]) -> bool {
        let mut done = 0usize;
        while done < buf.len() {
            let at = va.wrapping_add(done as u64);
            let n = (4096 - (at & 4095) as usize).min(buf.len() - done);
            let Some(pa) = self.translate(mem, at) else { return false };
            if !mem.read_phys(pa, &mut buf[done..done + n]) {
                return false;
            }
            done += n;
        }
        true
    }

    pub fn u64(&self, mem: &(impl PhysMem + ?Sized), va: u64) -> Option<u64> {
        let mut b = [0u8; 8];
        self.read(mem, va, &mut b).then(|| u64::from_le_bytes(b))
    }

    pub fn u32(&self, mem: &(impl PhysMem + ?Sized), va: u64) -> Option<u32> {
        let mut b = [0u8; 4];
        self.read(mem, va, &mut b).then(|| u32::from_le_bytes(b))
    }

    /// C string at address `va` (at most `max` bytes, without the zero).
    pub fn cstr(&self, mem: &(impl PhysMem + ?Sized), va: u64, max: usize) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let mut at = va;
        while out.len() < max {
            let n = (4096 - (at & 4095) as usize).min(max - out.len());
            let mut chunk = vec![0u8; n];
            if !self.read(mem, at, &mut chunk) {
                return if out.is_empty() { None } else { Some(out) };
            }
            if let Some(z) = chunk.iter().position(|&c| c == 0) {
                out.extend_from_slice(&chunk[..z]);
                return Some(out);
            }
            out.extend_from_slice(&chunk);
            at = at.wrapping_add(n as u64);
        }
        Some(out)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// 4-level (48-bit) tables built in a fake RAM starting at 0.
    pub(crate) struct Tables {
        pub ram: Vec<u8>,
        next: u64,
        pub root: u64,
    }

    impl Tables {
        pub(crate) fn new(size: usize) -> Self {
            let mut t = Tables { ram: vec![0; size], next: 0x1000, root: 0 };
            t.root = t.alloc();
            t
        }

        fn alloc(&mut self) -> u64 {
            let a = self.next;
            self.next += 0x1000;
            a
        }

        fn rd(&self, pa: u64) -> u64 {
            u64::from_le_bytes(self.ram[pa as usize..pa as usize + 8].try_into().unwrap())
        }

        fn wr(&mut self, pa: u64, v: u64) {
            self.ram[pa as usize..pa as usize + 8].copy_from_slice(&v.to_le_bytes());
        }

        /// Maps the page `va` → `pa` (4 KiB) starting from `root`.
        pub(crate) fn map_in(&mut self, root: u64, va: u64, pa: u64) {
            let mut table = root;
            for level in 0..3 {
                let shift = 12 + 9 * (3 - level);
                let e = table + ((va >> shift) & 511) * 8;
                let d = self.rd(e);
                table = if d & 3 == 3 {
                    d & 0x0000_ffff_ffff_f000
                } else {
                    let n = self.alloc();
                    self.wr(e, n | 3);
                    n
                };
            }
            self.wr(table + ((va >> 12) & 511) * 8, pa | 3 | 1 << 10);
        }

        pub(crate) fn map(&mut self, va: u64, pa: u64) {
            let r = self.root;
            self.map_in(r, va, pa);
        }

        pub(crate) fn space(&self) -> Space {
            // T0SZ = T1SZ = 16, TG0 = 4K, TG1 = 4K.
            Space { tcr: 16 | 16 << 16 | 0b10 << 30, ttbr0: self.root, ttbr1: self.root }
        }
    }

    #[test]
    fn tabelle_a_quattro_livelli_e_blocchi() {
        let mut t = Tables::new(1 << 20);
        let va = 0xffff_8000_8000_1000u64;
        t.map(va, 0x8_0000);
        t.map(0x40_0000, 0x9_0000);
        t.ram[0x8_0010..0x8_0016].copy_from_slice(b"ciao\0x");
        let s = t.space();
        let ram = t.ram.as_slice();
        assert_eq!(s.translate(ram, va + 0x10), Some(0x8_0010));
        assert_eq!(s.cstr(ram, va + 0x10, 64), Some(b"ciao".to_vec()));
        assert_eq!(s.translate(ram, 0x40_0123), Some(0x9_0123));
        assert_eq!(s.translate(ram, 0x40_1000), None);
        assert_eq!(s.translate(ram, 0x0001_0000_0000_0000), None, "non-canonical high bits");
        // 2 MiB block at level 2 in place of the level-3 table.
        let l0 = t.root;
        let l1 = t.rd(l0 + ((va >> 39) & 511) * 8) & !0xfff;
        let l2 = t.rd(l1 + ((va >> 30) & 511) * 8) & !0xfff;
        let e = l2 + ((va >> 21) & 511) * 8;
        t.wr(e, 0x20_0000 | 1);
        assert_eq!(t.space().translate(t.ram.as_slice(), va + 0x345), Some(0x20_1345));
        // Reads straddling unmapped pages.
        let mut b = [0u8; 8];
        assert!(!t.space().read(t.ram.as_slice(), 0x40_0ffc, &mut b));
    }
}
