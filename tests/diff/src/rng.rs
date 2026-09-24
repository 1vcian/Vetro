//! Generatore pseudocasuale deterministico (SplitMix64): stessi semi, stessi
//! programmi, su ogni macchina.

#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// Intero uniforme in `0..n` (n > 0).
    pub fn below(&mut self, n: u64) -> u64 {
        ((self.next_u64() as u128 * n as u128) >> 64) as u64
    }

    /// Vero con probabilità `num/den`.
    pub fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }

    /// Valore a 64 bit con molti casi limite (zero, segni, confini a 32 bit).
    pub fn interesting_u64(&mut self) -> u64 {
        const SPECIAL: [u64; 12] = [
            0,
            1,
            u64::MAX,
            0x7fff_ffff,
            0x8000_0000,
            0xffff_ffff,
            0x1_0000_0000,
            0x7fff_ffff_ffff_ffff,
            0x8000_0000_0000_0000,
            0xffff_ffff_8000_0000,
            0x8000_0000_0000_0001,
            0xffff_ffff_ffff_fffe,
        ];
        match self.below(8) {
            0 | 1 => *self.pick(&SPECIAL),
            2 => self.below(64),
            3 => (self.next_u64() as i8) as i64 as u64,
            4 => self.next_u32() as u64,
            _ => self.next_u64(),
        }
    }
}
