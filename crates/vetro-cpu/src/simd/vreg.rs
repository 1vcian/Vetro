//! Accesso a elementi dei registri vettoriali.

/// Elemento `idx` di `esize` bit (8, 16, 32, 64) di un valore a 128 bit.
#[inline]
pub fn elem(v: u128, idx: usize, esize: u32) -> u64 {
    let sh = idx as u32 * esize;
    let m = if esize == 64 { u64::MAX } else { (1u64 << esize) - 1 };
    ((v >> sh) as u64) & m
}

#[inline]
pub fn set_elem(v: u128, idx: usize, esize: u32, x: u64) -> u128 {
    let sh = idx as u32 * esize;
    let m: u128 = if esize == 64 { u64::MAX as u128 } else { (1u128 << esize) - 1 } << sh;
    (v & !m) | (((x as u128) << sh) & m)
}

/// Maschera dei bit bassi di `datasize` (64 o 128).
#[inline]
pub fn clip(v: u128, datasize: u32) -> u128 {
    if datasize == 128 { v } else { v & (u64::MAX as u128) }
}

/// Estensione di segno di un elemento di `esize` bit a i64.
#[inline]
pub fn sx(x: u64, esize: u32) -> i64 {
    let s = 64 - esize;
    ((x << s) as i64) >> s
}

#[inline]
pub fn emask(esize: u32) -> u64 {
    if esize == 64 { u64::MAX } else { (1u64 << esize) - 1 }
}
