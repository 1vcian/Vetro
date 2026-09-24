//! Funzioni di bit usate da decoder ed esecuzione (pseudocodice Arm ARM,
//! sezione J1 "shared/functions").

/// Campo `w<hi:lo>`.
#[inline]
pub const fn field(w: u32, hi: u32, lo: u32) -> u32 {
    (w >> lo) & (u32::MAX >> (31 - (hi - lo)))
}

#[inline]
pub const fn bit(w: u32, n: u32) -> bool {
    (w >> n) & 1 != 0
}

/// Estensione di segno dei `width` bit bassi di `v`.
#[inline]
pub const fn sext(v: u64, width: u32) -> i64 {
    let s = 64 - width;
    ((v << s) as i64) >> s
}

/// `n` bit a uno (n ≤ 64).
#[inline]
pub const fn ones(n: u32) -> u64 {
    if n >= 64 { u64::MAX } else { (1u64 << n) - 1 }
}

/// Rotazione a destra dentro un elemento di `width` bit.
#[inline]
pub fn ror(v: u64, amount: u32, width: u32) -> u64 {
    let v = v & ones(width);
    let a = amount % width;
    if a == 0 { v } else { ((v >> a) | (v << (width - a))) & ones(width) }
}

/// Replica un elemento di `esize` bit fino a riempire `width` bit.
pub fn replicate(v: u64, esize: u32, width: u32) -> u64 {
    let mut r = 0u64;
    let mut i = 0;
    while i < width {
        r |= v << i;
        i += esize;
    }
    r & ones(width)
}

/// `DecodeBitMasks(N, imms, immr, immediate, M)`: restituisce `(wmask, tmask)`
/// oppure `None` se la codifica è riservata.
pub fn decode_bit_masks(n: u32, imms: u32, immr: u32, immediate: bool, datasize: u32) -> Option<(u64, u64)> {
    let combined = (n << 6) | (!imms & 0x3f);
    if combined == 0 {
        return None;
    }
    let len = 31 - combined.leading_zeros();
    if len < 1 || datasize < (1 << len) {
        return None;
    }
    let levels = (1u32 << len) - 1;
    if immediate && (imms & levels) == levels {
        return None;
    }
    let s = imms & levels;
    let r = immr & levels;
    let d = s.wrapping_sub(r) & levels;
    let esize = 1u32 << len;
    let welem = ones(s + 1);
    let telem = ones(d + 1);
    let wmask = replicate(ror(welem, r, esize), esize, datasize);
    let tmask = replicate(telem, esize, datasize);
    Some((wmask, tmask))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields() {
        assert_eq!(field(0xD280_0540, 4, 0), 0);
        assert_eq!(field(0xD280_0540, 20, 5), 42);
        assert_eq!(field(0xFFFF_FFFF, 31, 0), 0xFFFF_FFFF);
        assert_eq!(sext(0x1FF, 9), -1);
        assert_eq!(sext(0x0FF, 9), 255);
    }

    #[test]
    fn bit_masks() {
        // and x0, x0, #0xff  → N=1 immr=0 imms=7
        assert_eq!(decode_bit_masks(1, 7, 0, true, 64).unwrap().0, 0xff);
        // orr w0, w0, #0x55555555 → N=0 immr=0 imms=0b111100
        assert_eq!(decode_bit_masks(0, 0b111100, 0, true, 32).unwrap().0, 0x5555_5555);
        // tutti uno: riservato per le istruzioni logiche
        assert!(decode_bit_masks(1, 0b111111, 0, true, 64).is_none());
        // ma valido per i bitfield (es. asr x0, x1, #63 ha imms=63)
        assert!(decode_bit_masks(1, 0b111111, 0, false, 64).is_some());
    }
}
