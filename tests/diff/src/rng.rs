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

    /// Valore FP a 32 bit con molti casi limite.
    pub fn fp32(&mut self) -> u32 {
        const SPECIAL: [u32; 20] = [
            0x0000_0000,
            0x8000_0000,
            0x3f80_0000,
            0xbf80_0000,
            0x7f80_0000,
            0xff80_0000,
            0x7fc0_0000,
            0xffc0_0001,
            0x7f80_0001,
            0xff90_0000, // qNaN e sNaN
            0x0000_0001,
            0x807f_ffff,
            0x0080_0000,
            0x7f7f_ffff, // denormali, min/max normali
            0x3f00_0000,
            0x3fc0_0000,
            0x4020_0000, // 0.5, 1.5, 2.5 (pareggi)
            0x4f00_0000,
            0xcf00_0000,
            0x5f00_0000, // 2^31, -2^31, 2^63
        ];
        match self.below(8) {
            0..=2 => *self.pick(&SPECIAL),
            3 => ((self.below(2001) as i32 - 1000) as f32).to_bits(), // interi piccoli
            4 => ((self.below(2001) as i32 - 1000) as f32 / 8.0).to_bits(),
            _ => self.next_u32(),
        }
    }

    /// Valore FP a 64 bit con molti casi limite.
    pub fn fp64(&mut self) -> u64 {
        const SPECIAL: [u64; 18] = [
            0,
            0x8000_0000_0000_0000,
            0x3ff0_0000_0000_0000,
            0xbff0_0000_0000_0000,
            0x7ff0_0000_0000_0000,
            0xfff0_0000_0000_0000,
            0x7ff8_0000_0000_0000,
            0xfff8_0000_0000_0001,
            0x7ff0_0000_0000_0001,
            0xfff4_0000_0000_0000,
            0x0000_0000_0000_0001,
            0x800f_ffff_ffff_ffff,
            0x0010_0000_0000_0000,
            0x7fef_ffff_ffff_ffff,
            0x3fe0_0000_0000_0000,
            0x4004_0000_0000_0000,
            0x41e0_0000_0000_0000,
            0x43e0_0000_0000_0000,
        ];
        match self.below(8) {
            0..=2 => *self.pick(&SPECIAL),
            3 => ((self.below(2001) as i64 - 1000) as f64).to_bits(),
            4 => ((self.below(2001) as i64 - 1000) as f64 / 8.0).to_bits(),
            _ => self.next_u64(),
        }
    }

    /// Registro vettoriale: corsie a 32 o 64 bit con valori FP, o bit casuali.
    pub fn fp_vector(&mut self) -> u128 {
        match self.below(4) {
            0 => (0..4).fold(0u128, |v, i| v | (self.fp32() as u128) << (32 * i)),
            1 => (self.fp64() as u128) | (self.fp64() as u128) << 64,
            2 => (0..8).fold(0u128, |v, i| v | ((self.fp32() >> 16) as u128) << (16 * i)),
            _ => (self.next_u64() as u128) | (self.next_u64() as u128) << 64,
        }
    }

    /// Registro vettoriale con corsie quasi sempre speciali.
    pub fn fp_vector_special(&mut self) -> u128 {
        const S32: [u32; 12] = [
            0x7fc0_0000,
            0xffc0_1234,
            0x7f80_0001,
            0xffa0_0000, // qNaN, sNaN
            0x7f80_0000,
            0xff80_0000,
            0x0000_0000,
            0x8000_0000,
            0x0000_0001,
            0x807f_ffff,
            0x3f80_0000,
            0x7f7f_ffff,
        ];
        const S64: [u64; 12] = [
            0x7ff8_0000_0000_0000,
            0xfff8_0000_1234_0000,
            0x7ff0_0000_0000_0001,
            0xfff4_0000_0000_0000,
            0x7ff0_0000_0000_0000,
            0xfff0_0000_0000_0000,
            0,
            0x8000_0000_0000_0000,
            1,
            0x800f_ffff_ffff_ffff,
            0x3ff0_0000_0000_0000,
            0x7fef_ffff_ffff_ffff,
        ];
        if self.chance(1, 2) {
            (0..4).fold(0u128, |v, i| {
                let x = if self.chance(3, 4) { *self.pick(&S32) } else { self.fp32() };
                v | (x as u128) << (32 * i)
            })
        } else {
            (0..2).fold(0u128, |v, i| {
                let x = if self.chance(3, 4) { *self.pick(&S64) } else { self.fp64() };
                v | (x as u128) << (64 * i)
            })
        }
    }
}
