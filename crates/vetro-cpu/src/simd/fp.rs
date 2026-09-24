//! Aritmetica in virgola mobile in software, secondo il pseudocodice Arm
//! (shared/functions/float: FPUnpack, FPRound, FPProcessNaNs, FPAdd, ...).
//!
//! Bit-exact e deterministica su ogni host, WASM compreso: niente float
//! dell'host, i cui NaN non hanno bit garantiti. I valori sono sempre
//! pattern di bit (`u64`) nel formato `Fmt`; un valore finito diverso da zero
//! si rappresenta esattamente come `mant · 2^exp`.

/// Formato IEEE: bit totali, bit di esponente, bit di frazione.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fmt {
    pub n: u32,
    pub e: u32,
    pub f: u32,
}

pub const H: Fmt = Fmt { n: 16, e: 5, f: 10 };
pub const S: Fmt = Fmt { n: 32, e: 8, f: 23 };
pub const D: Fmt = Fmt { n: 64, e: 11, f: 52 };

impl Fmt {
    fn bias(self) -> i32 {
        (1 << (self.e - 1)) - 1
    }
    fn min_exp(self) -> i32 {
        1 - self.bias()
    }
    fn exp_mask(self) -> u64 {
        (1 << self.e) - 1
    }
    fn frac_mask(self) -> u64 {
        (1 << self.f) - 1
    }
    fn sign_bit(self) -> u64 {
        1 << (self.n - 1)
    }
    pub fn zero(self, sign: bool) -> u64 {
        if sign { self.sign_bit() } else { 0 }
    }
    pub fn infinity(self, sign: bool) -> u64 {
        self.zero(sign) | (self.exp_mask() << self.f)
    }
    pub fn max_normal(self, sign: bool) -> u64 {
        self.zero(sign) | ((self.exp_mask() - 1) << self.f) | self.frac_mask()
    }
    pub fn default_nan(self) -> u64 {
        (self.exp_mask() << self.f) | (1 << (self.f - 1))
    }
    /// Valore `m · 2^k` con `m` piccolo, esatto (per 1.0, 1.5, 2.0 ...).
    fn small(self, sign: bool, value_x2: u64) -> u64 {
        // value_x2 = valore * 2 (2 → 1.0, 3 → 1.5, 4 → 2.0)
        let msb = 63 - value_x2.leading_zeros() as i32;
        let e = msb - 1; // esponente del valore
        let frac = (value_x2 << (self.f as i32 - msb)) & self.frac_mask();
        self.zero(sign) | (((e + self.bias()) as u64) << self.f) | frac
    }
    pub fn neg(self, x: u64) -> u64 {
        x ^ self.sign_bit()
    }
    pub fn abs(self, x: u64) -> u64 {
        x & !self.sign_bit()
    }
    pub fn sign(self, x: u64) -> bool {
        x & self.sign_bit() != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rounding {
    TieEven,
    PosInf,
    NegInf,
    Zero,
    TieAway,
    Odd,
}

// Bit di FPSR.
pub const IOC: u32 = 1 << 0;
pub const DZC: u32 = 1 << 1;
pub const OFC: u32 = 1 << 2;
pub const UFC: u32 = 1 << 3;
pub const IXC: u32 = 1 << 4;
pub const IDC: u32 = 1 << 7;

/// Contesto: FPCR in ingresso e flag cumulativi in uscita.
#[derive(Clone, Copy, Debug)]
pub struct Ctx {
    pub fpcr: u32,
    pub flags: u32,
}

impl Ctx {
    pub fn new(fpcr: u32) -> Self {
        Ctx { fpcr, flags: 0 }
    }
    pub fn rounding(&self) -> Rounding {
        match (self.fpcr >> 22) & 3 {
            0 => Rounding::TieEven,
            1 => Rounding::PosInf,
            2 => Rounding::NegInf,
            _ => Rounding::Zero,
        }
    }
    fn fz(&self, f: Fmt) -> bool {
        // FZ16 non esiste su ARMv8.0: la mezza precisione non si azzera.
        f.n != 16 && self.fpcr & (1 << 24) != 0
    }
    fn dn(&self) -> bool {
        self.fpcr & (1 << 25) != 0
    }
    fn ahp(&self) -> bool {
        self.fpcr & (1 << 26) != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Zero,
    Normal,
    Inf,
    QNaN,
    SNaN,
}

/// Operando spacchettato: per `Normal`, valore = (-1)^sign · mant · 2^exp.
#[derive(Clone, Copy, Debug)]
pub struct Unpacked {
    pub class: Class,
    pub sign: bool,
    pub mant: u64,
    pub exp: i32,
}

/// FPUnpack: i denormali con FZ diventano zero e segnalano IDC.
pub fn unpack(f: Fmt, x: u64, ctx: &mut Ctx) -> Unpacked {
    unpack_ahp(f, x, ctx, true)
}

fn unpack_ahp(f: Fmt, x: u64, ctx: &mut Ctx, honor_ahp: bool) -> Unpacked {
    let sign = f.sign(x);
    let e = (x >> f.f) & f.exp_mask();
    let frac = x & f.frac_mask();
    let z = Unpacked { class: Class::Zero, sign, mant: 0, exp: 0 };
    if e == 0 {
        if frac == 0 {
            return z;
        }
        if ctx.fz(f) {
            ctx.flags |= IDC;
            return z;
        }
        return Unpacked { class: Class::Normal, sign, mant: frac, exp: f.min_exp() - f.f as i32 };
    }
    let alt_half = f.n == 16 && honor_ahp && ctx.ahp();
    if e == f.exp_mask() && !alt_half {
        if frac == 0 {
            return Unpacked { class: Class::Inf, ..z };
        }
        let quiet = frac & (1 << (f.f - 1)) != 0;
        return Unpacked { class: if quiet { Class::QNaN } else { Class::SNaN }, ..z };
    }
    Unpacked { class: Class::Normal, sign, mant: frac | (1 << f.f), exp: e as i32 - f.bias() - f.f as i32 }
}

fn is_nan(c: Class) -> bool {
    matches!(c, Class::QNaN | Class::SNaN)
}

/// FPProcessNaN: silenzia un SNaN (IOC) o usa il NaN di default con DN.
pub fn process_nan(f: Fmt, c: Class, x: u64, ctx: &mut Ctx) -> u64 {
    let mut r = x;
    if c == Class::SNaN {
        r |= 1 << (f.f - 1);
        ctx.flags |= IOC;
    }
    if ctx.dn() { f.default_nan() } else { r }
}

fn process_nans(f: Fmt, a: (Class, u64), b: (Class, u64), ctx: &mut Ctx) -> Option<u64> {
    if a.0 == Class::SNaN {
        Some(process_nan(f, a.0, a.1, ctx))
    } else if b.0 == Class::SNaN {
        Some(process_nan(f, b.0, b.1, ctx))
    } else if a.0 == Class::QNaN {
        Some(process_nan(f, a.0, a.1, ctx))
    } else if b.0 == Class::QNaN {
        Some(process_nan(f, b.0, b.1, ctx))
    } else {
        None
    }
}

fn process_nans3(f: Fmt, a: (Class, u64), b: (Class, u64), c: (Class, u64), ctx: &mut Ctx) -> Option<u64> {
    for x in [a, b, c] {
        if x.0 == Class::SNaN {
            return Some(process_nan(f, x.0, x.1, ctx));
        }
    }
    for x in [a, b, c] {
        if x.0 == Class::QNaN {
            return Some(process_nan(f, x.0, x.1, ctx));
        }
    }
    None
}

/// Valore esatto con segno: (mant + ε) · 2^exp, con ε ∈ (0,1) se `sticky`.
#[derive(Clone, Copy, Debug)]
pub struct Exact {
    pub sign: bool,
    pub mant: u128,
    pub exp: i32,
    pub sticky: bool,
}

impl Exact {
    fn is_zero(&self) -> bool {
        self.mant == 0 && !self.sticky
    }
    fn from(u: &Unpacked) -> Exact {
        Exact { sign: u.sign, mant: u.mant as u128, exp: u.exp, sticky: false }
    }
    /// Porta il bit più alto alla posizione 125 (lascia spazio per i riporti).
    fn normalized(self) -> Exact {
        if self.mant == 0 {
            return self;
        }
        let msb = 127 - self.mant.leading_zeros() as i32;
        let sh = 125 - msb;
        if sh >= 0 {
            Exact { mant: self.mant << sh, exp: self.exp - sh, ..self }
        } else {
            let (m, st) = shr_sticky(self.mant, (-sh) as u32);
            Exact { mant: m, exp: self.exp - sh, sticky: self.sticky || st, ..self }
        }
    }
}

/// Shift a destra che raccoglie i bit persi in un flag.
fn shr_sticky(m: u128, n: u32) -> (u128, bool) {
    if n == 0 {
        (m, false)
    } else if n >= 128 {
        (0, m != 0)
    } else {
        (m >> n, m & ((1u128 << n) - 1) != 0)
    }
}

/// Somma esatta (a meno dello sticky) di due valori.
fn add_exact(a: Exact, b: Exact) -> Exact {
    if a.is_zero() {
        return b;
    }
    if b.is_zero() {
        return a;
    }
    let (a, b) = (a.normalized(), b.normalized());
    // big = quello di modulo maggiore
    let a_bigger = (a.exp, a.mant) >= (b.exp, b.mant);
    let (big, small) = if a_bigger { (a, b) } else { (b, a) };
    let d = (big.exp - small.exp) as u32;
    let (sm, st_shift) = shr_sticky(small.mant, d);
    let sticky = st_shift || small.sticky || big.sticky;
    if big.sign == small.sign {
        Exact { sign: big.sign, mant: big.mant + sm, exp: big.exp, sticky }
    } else if sticky && (st_shift || small.sticky) && !big.sticky {
        // big - (sm + ε) = (big - sm - 1) + (1 - ε)
        Exact { sign: big.sign, mant: big.mant - sm - 1, exp: big.exp, sticky: true }
    } else {
        Exact { sign: big.sign, mant: big.mant - sm, exp: big.exp, sticky }
    }
}

/// FPRound (con arrotondamento esplicito). Il valore non deve essere zero.
pub fn round(f: Fmt, v: Exact, ctx: &mut Ctx, rounding: Rounding) -> u64 {
    round_ahp(f, v, ctx, rounding, true)
}

fn round_ahp(f: Fmt, v: Exact, ctx: &mut Ctx, rounding: Rounding, honor_ahp: bool) -> u64 {
    debug_assert!(!v.is_zero());
    let v = if v.mant == 0 { Exact { mant: 1, exp: v.exp - 200, sticky: true, ..v } } else { v };
    let sign = v.sign;
    let msb = 127 - v.mant.leading_zeros() as i32;
    let exponent = msb + v.exp; // valore in [2^exponent, 2^(exponent+1))
    let min_exp = f.min_exp();
    if ctx.fz(f) && exponent < min_exp {
        ctx.flags |= UFC;
        return f.zero(sign);
    }
    let mut biased = (exponent - min_exp + 1).max(0) as u64;
    // Posizione (in potenze di 2) dell'ultimo bit del risultato.
    let lsb_exp = if biased == 0 { min_exp - f.f as i32 } else { exponent - f.f as i32 };
    let s = lsb_exp - v.exp; // bit da scartare
    let (mut int_mant, rem_gt_half, rem_eq_half, inexact) = if s <= 0 {
        (v.mant << (-s) as u32, false, false, v.sticky)
    } else if s >= 128 {
        // tutto sotto la metà dell'ulp (v.mant < 2^127 ≤ 2^(s-1))
        (0u128, false, false, true)
    } else {
        let s = s as u32;
        let rem = v.mant & ((1u128 << s) - 1);
        let half = 1u128 << (s - 1);
        (v.mant >> s, rem > half || rem == half && v.sticky, rem == half && !v.sticky, rem != 0 || v.sticky)
    };
    let mut error_nonzero = inexact;
    if biased == 0 && error_nonzero {
        ctx.flags |= UFC;
    }
    let odd = int_mant & 1 == 1;
    let (round_up, overflow_to_inf) = match rounding {
        Rounding::TieEven => (rem_gt_half || rem_eq_half && odd, true),
        Rounding::TieAway => (rem_gt_half || rem_eq_half, true),
        Rounding::PosInf => (error_nonzero && !sign, !sign),
        Rounding::NegInf => (error_nonzero && sign, sign),
        Rounding::Zero | Rounding::Odd => (false, false),
    };
    if round_up {
        int_mant += 1;
        if int_mant == 1u128 << f.f {
            biased = 1;
        }
        if int_mant == 1u128 << (f.f + 1) {
            biased += 1;
            int_mant >>= 1;
        }
    }
    if error_nonzero && rounding == Rounding::Odd {
        int_mant |= 1;
    }
    let result;
    if !(f.n == 16 && honor_ahp && ctx.ahp()) {
        if biased >= f.exp_mask() {
            result = if overflow_to_inf { f.infinity(sign) } else { f.max_normal(sign) };
            ctx.flags |= OFC;
            error_nonzero = true;
        } else {
            result = f.zero(sign) | (biased << f.f) | (int_mant as u64 & f.frac_mask());
        }
    } else if biased >= 1 << f.e {
        result = f.zero(sign) | (f.sign_bit() - 1);
        ctx.flags |= IOC;
        error_nonzero = false;
    } else {
        result = f.zero(sign) | (biased << f.f) | (int_mant as u64 & f.frac_mask());
    }
    if error_nonzero {
        ctx.flags |= IXC;
    }
    result
}

fn exact_zero_sign(ctx: &Ctx) -> bool {
    ctx.rounding() == Rounding::NegInf
}

pub fn add(f: Fmt, a: u64, b: u64, ctx: &mut Ctx) -> u64 {
    addsub(f, a, b, false, ctx)
}

pub fn sub(f: Fmt, a: u64, b: u64, ctx: &mut Ctx) -> u64 {
    addsub(f, a, b, true, ctx)
}

fn addsub(f: Fmt, a: u64, b: u64, subtract: bool, ctx: &mut Ctx) -> u64 {
    let (ua, mut ub) = (unpack(f, a, ctx), unpack(f, b, ctx));
    if let Some(r) = process_nans(f, (ua.class, a), (ub.class, b), ctx) {
        return r;
    }
    if subtract {
        ub.sign = !ub.sign;
    }
    let (inf1, inf2) = (ua.class == Class::Inf, ub.class == Class::Inf);
    let (zero1, zero2) = (ua.class == Class::Zero, ub.class == Class::Zero);
    if inf1 && inf2 && ua.sign != ub.sign {
        ctx.flags |= IOC;
        return f.default_nan();
    }
    if inf1 && !ua.sign || inf2 && !ub.sign {
        return f.infinity(false);
    }
    if inf1 || inf2 {
        return f.infinity(true);
    }
    if zero1 && zero2 && ua.sign == ub.sign {
        return f.zero(ua.sign);
    }
    let r = add_exact(Exact::from(&ua), Exact::from(&ub));
    if r.is_zero() {
        return f.zero(exact_zero_sign(ctx));
    }
    let rm = ctx.rounding();
    round(f, r, ctx, rm)
}

fn mul_exact(a: &Unpacked, b: &Unpacked) -> Exact {
    Exact { sign: a.sign != b.sign, mant: a.mant as u128 * b.mant as u128, exp: a.exp + b.exp, sticky: false }
}

pub fn mul(f: Fmt, a: u64, b: u64, ctx: &mut Ctx) -> u64 {
    mul_x(f, a, b, false, ctx)
}

/// FMUL (`x = false`) o FMULX (∞ × 0 = ±2).
pub fn mul_x(f: Fmt, a: u64, b: u64, x: bool, ctx: &mut Ctx) -> u64 {
    let (ua, ub) = (unpack(f, a, ctx), unpack(f, b, ctx));
    if let Some(r) = process_nans(f, (ua.class, a), (ub.class, b), ctx) {
        return r;
    }
    let sign = ua.sign != ub.sign;
    let (inf1, inf2) = (ua.class == Class::Inf, ub.class == Class::Inf);
    let (zero1, zero2) = (ua.class == Class::Zero, ub.class == Class::Zero);
    if inf1 && zero2 || zero1 && inf2 {
        if x {
            return f.small(sign, 4);
        }
        ctx.flags |= IOC;
        return f.default_nan();
    }
    if inf1 || inf2 {
        return f.infinity(sign);
    }
    if zero1 || zero2 {
        return f.zero(sign);
    }
    let rm = ctx.rounding();
    round(f, mul_exact(&ua, &ub), ctx, rm)
}

pub fn div(f: Fmt, a: u64, b: u64, ctx: &mut Ctx) -> u64 {
    let (ua, ub) = (unpack(f, a, ctx), unpack(f, b, ctx));
    if let Some(r) = process_nans(f, (ua.class, a), (ub.class, b), ctx) {
        return r;
    }
    let sign = ua.sign != ub.sign;
    let (inf1, inf2) = (ua.class == Class::Inf, ub.class == Class::Inf);
    let (zero1, zero2) = (ua.class == Class::Zero, ub.class == Class::Zero);
    if inf1 && inf2 || zero1 && zero2 {
        ctx.flags |= IOC;
        return f.default_nan();
    }
    if inf1 || zero2 {
        if !inf1 {
            ctx.flags |= DZC;
        }
        return f.infinity(sign);
    }
    if zero1 || inf2 {
        return f.zero(sign);
    }
    // Normalizza le mantisse al bit 63 e dividi con 64 bit di guardia.
    let (sa, sb) = (ua.mant.leading_zeros(), ub.mant.leading_zeros());
    let (ma, mb) = ((ua.mant << sa) as u128, (ub.mant << sb) as u128);
    let q = (ma << 64) / mb;
    let r = (ma << 64) % mb;
    let exp = (ua.exp - sa as i32) - (ub.exp - sb as i32) - 64;
    let rm = ctx.rounding();
    round(f, Exact { sign, mant: q, exp, sticky: r != 0 }, ctx, rm)
}

fn isqrt(n: u128) -> u128 {
    if n < 2 {
        return n;
    }
    let mut x = 1u128 << ((128 - n.leading_zeros()).div_ceil(2));
    loop {
        let y = (x + n / x) / 2;
        if y >= x {
            return x;
        }
        x = y;
    }
}

pub fn sqrt(f: Fmt, a: u64, ctx: &mut Ctx) -> u64 {
    let u = unpack(f, a, ctx);
    match u.class {
        Class::QNaN | Class::SNaN => return process_nan(f, u.class, a, ctx),
        Class::Zero => return f.zero(u.sign),
        Class::Inf if !u.sign => return f.infinity(false),
        _ => {}
    }
    if u.sign {
        ctx.flags |= IOC;
        return f.default_nan();
    }
    // mant · 2^exp con exp pari e mant di ~120 bit.
    let lz = (u.mant as u128).leading_zeros() as i32;
    let mut sh = lz - 7; // porta il bit alto a 120
    if (u.exp - sh) % 2 != 0 {
        sh += 1;
    }
    let m = (u.mant as u128) << sh;
    let e = u.exp - sh;
    let r = isqrt(m);
    let rm = ctx.rounding();
    round(f, Exact { sign: false, mant: r, exp: e / 2, sticky: r * r != m }, ctx, rm)
}

/// FPMulAdd(addend, op1, op2) = addend + op1 · op2, arrotondato una volta.
pub fn mul_add(f: Fmt, addend: u64, a: u64, b: u64, ctx: &mut Ctx) -> u64 {
    let ux = unpack(f, addend, ctx);
    let (u1, u2) = (unpack(f, a, ctx), unpack(f, b, ctx));
    let (inf1, inf2) = (u1.class == Class::Inf, u2.class == Class::Inf);
    let (zero1, zero2) = (u1.class == Class::Zero, u2.class == Class::Zero);
    let nan = process_nans3(f, (ux.class, addend), (u1.class, a), (u2.class, b), ctx);
    if ux.class == Class::QNaN && (inf1 && zero2 || zero1 && inf2) {
        ctx.flags |= IOC;
        return f.default_nan();
    }
    if let Some(r) = nan {
        return r;
    }
    let (infa, zeroa) = (ux.class == Class::Inf, ux.class == Class::Zero);
    let signp = u1.sign != u2.sign;
    let (infp, zerop) = (inf1 || inf2, zero1 || zero2);
    if inf1 && zero2 || zero1 && inf2 || infa && infp && ux.sign != signp {
        ctx.flags |= IOC;
        return f.default_nan();
    }
    if infa && !ux.sign || infp && !signp {
        return f.infinity(false);
    }
    if infa || infp {
        return f.infinity(true);
    }
    if zeroa && zerop && ux.sign == signp {
        return f.zero(ux.sign);
    }
    let p = if zerop { Exact { sign: signp, mant: 0, exp: 0, sticky: false } } else { mul_exact(&u1, &u2) };
    let r = add_exact(Exact::from(&ux), p);
    if r.is_zero() {
        return f.zero(exact_zero_sign(ctx));
    }
    let rm = ctx.rounding();
    round(f, r, ctx, rm)
}

/// FRECPS: 2 - op1·op2 (fuso). FRSQRTS: (3 - op1·op2) / 2 (fuso).
pub fn step_fused(f: Fmt, a: u64, b: u64, sqrt_step: bool, ctx: &mut Ctx) -> u64 {
    let a = f.neg(a);
    let (u1, u2) = (unpack(f, a, ctx), unpack(f, b, ctx));
    if let Some(r) = process_nans(f, (u1.class, a), (u2.class, b), ctx) {
        return r;
    }
    let (inf1, inf2) = (u1.class == Class::Inf, u2.class == Class::Inf);
    let (zero1, zero2) = (u1.class == Class::Zero, u2.class == Class::Zero);
    if inf1 && zero2 || zero1 && inf2 {
        return f.small(false, if sqrt_step { 3 } else { 4 });
    }
    if inf1 || inf2 {
        return f.infinity(u1.sign != u2.sign);
    }
    let base = Exact { sign: false, mant: if sqrt_step { 3 } else { 2 }, exp: 0, sticky: false };
    let p = if zero1 || zero2 {
        Exact { sign: false, mant: 0, exp: 0, sticky: false }
    } else {
        mul_exact(&u1, &u2)
    };
    let mut r = add_exact(base, p);
    if r.is_zero() {
        return f.zero(exact_zero_sign(ctx));
    }
    if sqrt_step {
        r.exp -= 1;
    }
    let rm = ctx.rounding();
    round(f, r, ctx, rm)
}

/// FPCompare: NZCV (0110 uguali, 1000 minore, 0010 maggiore, 0011 non ordinati).
pub fn compare(f: Fmt, a: u64, b: u64, signal_nans: bool, ctx: &mut Ctx) -> u32 {
    let (ua, ub) = (unpack(f, a, ctx), unpack(f, b, ctx));
    if is_nan(ua.class) || is_nan(ub.class) {
        if signal_nans || ua.class == Class::SNaN || ub.class == Class::SNaN {
            ctx.flags |= IOC;
        }
        return 0b0011;
    }
    match cmp_values(&ua, &ub) {
        std::cmp::Ordering::Equal => 0b0110,
        std::cmp::Ordering::Less => 0b1000,
        std::cmp::Ordering::Greater => 0b0010,
    }
}

/// Confronto numerico di due valori non NaN.
fn cmp_values(a: &Unpacked, b: &Unpacked) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    let key = |u: &Unpacked| -> (i32, i32, u64) {
        // (classe di grandezza, esponente normalizzato, mantissa normalizzata)
        match u.class {
            Class::Zero => (0, 0, 0),
            Class::Inf => (2, 0, 0),
            _ => {
                let lz = u.mant.leading_zeros();
                (1, u.exp - lz as i32, u.mant << lz)
            }
        }
    };
    let (ka, kb) = (key(a), key(b));
    let (za, zb) = (a.class == Class::Zero, b.class == Class::Zero);
    if za && zb {
        return Equal;
    }
    let sa = !za && a.sign;
    let sb = !zb && b.sign;
    match (sa, sb) {
        (false, true) => Greater,
        (true, false) => Less,
        (false, false) => ka.cmp(&kb),
        (true, true) => kb.cmp(&ka),
    }
}

/// Confronti vettoriali: EQ, GE, GT. Restituisce true/false.
pub fn compare_eq(f: Fmt, a: u64, b: u64, ctx: &mut Ctx) -> bool {
    let (ua, ub) = (unpack(f, a, ctx), unpack(f, b, ctx));
    if is_nan(ua.class) || is_nan(ub.class) {
        if ua.class == Class::SNaN || ub.class == Class::SNaN {
            ctx.flags |= IOC;
        }
        return false;
    }
    cmp_values(&ua, &ub) == std::cmp::Ordering::Equal
}

pub fn compare_ge(f: Fmt, a: u64, b: u64, gt: bool, ctx: &mut Ctx) -> bool {
    let (ua, ub) = (unpack(f, a, ctx), unpack(f, b, ctx));
    if is_nan(ua.class) || is_nan(ub.class) {
        ctx.flags |= IOC;
        return false;
    }
    let o = cmp_values(&ua, &ub);
    if gt { o == std::cmp::Ordering::Greater } else { o != std::cmp::Ordering::Less }
}

/// FPMax/FPMin (`max`), con o senza la semantica "numero" (FMAXNM/FMINNM).
pub fn max_min(f: Fmt, a: u64, b: u64, max: bool, num: bool, ctx: &mut Ctx) -> u64 {
    let (mut a, mut b) = (a, b);
    if num {
        let mut scratch = *ctx;
        let (ca, cb) = (unpack(f, a, &mut scratch).class, unpack(f, b, &mut scratch).class);
        // Un solo QNaN: diventa -∞ (per il massimo) o +∞ (per il minimo).
        if ca == Class::QNaN && cb != Class::QNaN {
            a = f.infinity(max);
        } else if ca != Class::QNaN && cb == Class::QNaN {
            b = f.infinity(max);
        }
    }
    let (ua, ub) = (unpack(f, a, ctx), unpack(f, b, ctx));
    if let Some(r) = process_nans(f, (ua.class, a), (ub.class, b), ctx) {
        return r;
    }
    let o = cmp_values(&ua, &ub);
    let pick_a = if max { o == std::cmp::Ordering::Greater } else { o == std::cmp::Ordering::Less };
    let u = if pick_a { ua } else { ub };
    match u.class {
        Class::Zero => {
            // max: segno più positivo (AND), min: più negativo (OR).
            let (sa, sb) = (ua.sign, ub.sign);
            let both_zero = ua.class == Class::Zero && ub.class == Class::Zero;
            let sign = if !both_zero {
                u.sign
            } else if max {
                sa && sb
            } else {
                sa || sb
            };
            f.zero(sign)
        }
        Class::Inf => f.infinity(u.sign),
        _ => {
            // FPRound anche se esatto: con FZ un denormale diventa zero.
            let rm = ctx.rounding();
            round(f, Exact::from(&u), ctx, rm)
        }
    }
}

/// FPRoundInt: all'intero secondo `rounding`; `exact` segnala l'inesattezza.
pub fn round_int(f: Fmt, a: u64, rounding: Rounding, exact: bool, ctx: &mut Ctx) -> u64 {
    let u = unpack(f, a, ctx);
    match u.class {
        Class::QNaN | Class::SNaN => return process_nan(f, u.class, a, ctx),
        Class::Inf => return f.infinity(u.sign),
        Class::Zero => return f.zero(u.sign),
        Class::Normal => {}
    }
    if u.exp >= 0 {
        return f.zero(u.sign) | (a & !f.sign_bit()); // già intero
    }
    let (int_part, frac_nonzero, gt_half, eq_half) = split_frac(u.mant as u128, (-u.exp) as u32);
    let odd = int_part & 1 == 1;
    let round_up = round_up_int(rounding, u.sign, frac_nonzero, gt_half, eq_half, odd);
    let mag = int_part + round_up as u128;
    if mag == 0 {
        if frac_nonzero && exact {
            ctx.flags |= IXC;
        }
        return f.zero(u.sign);
    }
    let mut c = Ctx { flags: 0, ..*ctx };
    let r = round(f, Exact { sign: u.sign, mant: mag, exp: 0, sticky: false }, &mut c, Rounding::Zero);
    if frac_nonzero && exact {
        ctx.flags |= IXC;
    }
    r
}

/// Parte intera di m · 2^-n (modulo) e informazioni sulla parte frazionaria.
fn split_frac(m: u128, n: u32) -> (u128, bool, bool, bool) {
    if n >= 128 {
        return (0, m != 0, false, false);
    }
    let int_part = m >> n;
    let frac = m & ((1u128 << n) - 1);
    let half = 1u128 << (n - 1);
    (int_part, frac != 0, frac > half, frac == half)
}

/// Arrotondamento all'intero del modulo, dato il segno (come il pseudocodice
/// su RoundDown, riscritto sul valore assoluto).
fn round_up_int(r: Rounding, sign: bool, nonzero: bool, gt: bool, eq: bool, odd: bool) -> bool {
    match r {
        Rounding::TieEven => gt || eq && odd,
        Rounding::TieAway => gt || eq,
        Rounding::PosInf => nonzero && !sign,
        Rounding::NegInf => nonzero && sign,
        Rounding::Zero | Rounding::Odd => false,
    }
}

/// FPToFixed: `op · 2^fbits` arrotondato a intero di `bits` bit, saturato.
pub fn to_fixed(
    f: Fmt,
    a: u64,
    fbits: u32,
    unsigned: bool,
    bits: u32,
    rounding: Rounding,
    ctx: &mut Ctx,
) -> u64 {
    let u = unpack(f, a, ctx);
    let (max, min): (i128, i128) =
        if unsigned { ((1i128 << bits) - 1, 0) } else { ((1i128 << (bits - 1)) - 1, -(1i128 << (bits - 1))) };
    let sat = |v: i128, ctx: &mut Ctx| -> u64 {
        ctx.flags |= IOC;
        (v.clamp(min, max) as u64) & crate::bits::ones(bits)
    };
    match u.class {
        Class::QNaN | Class::SNaN => {
            ctx.flags |= IOC;
            return 0;
        }
        Class::Inf => return sat(if u.sign { min } else { max }, ctx),
        Class::Zero => return 0,
        Class::Normal => {}
    }
    let e = u.exp + fbits as i32;
    let (mag, nonzero, gt, eq) = if e >= 0 {
        if e > 70 {
            return sat(if u.sign { min } else { max }, ctx);
        }
        ((u.mant as u128) << e, false, false, false)
    } else {
        split_frac(u.mant as u128, (-e) as u32)
    };
    let up = round_up_int(rounding, u.sign, nonzero, gt, eq, mag & 1 == 1);
    let mag = mag + up as u128;
    if mag > (1u128 << 100) {
        return sat(if u.sign { min } else { max }, ctx);
    }
    let v = if u.sign { -(mag as i128) } else { mag as i128 };
    if v > max || v < min {
        return sat(v, ctx);
    }
    if nonzero {
        ctx.flags |= IXC;
    }
    (v as u64) & crate::bits::ones(bits)
}

/// FixedToFP: intero di `bits` bit (con o senza segno) diviso 2^fbits.
pub fn from_fixed(
    f: Fmt,
    x: u64,
    fbits: u32,
    unsigned: bool,
    bits: u32,
    rounding: Rounding,
    ctx: &mut Ctx,
) -> u64 {
    let x = x & crate::bits::ones(bits);
    let val: i128 = if unsigned { x as i128 } else { crate::bits::sext(x, bits) as i128 };
    if val == 0 {
        return f.zero(false);
    }
    round(
        f,
        Exact { sign: val < 0, mant: val.unsigned_abs(), exp: -(fbits as i32), sticky: false },
        ctx,
        rounding,
    )
}

/// FPConvert tra formati (FCVT), con le regole dei NaN.
pub fn convert(from: Fmt, to: Fmt, a: u64, rounding: Rounding, ctx: &mut Ctx) -> u64 {
    let u = unpack_ahp(from, a, ctx, true);
    let alt_to = to.n == 16 && ctx.ahp();
    match u.class {
        Class::QNaN | Class::SNaN => {
            if alt_to {
                // Mezza precisione alternativa: niente NaN, risultato zero.
                ctx.flags |= IOC;
                return to.zero(u.sign);
            }
            if u.class == Class::SNaN {
                ctx.flags |= IOC;
            }
            if ctx.dn() {
                return to.default_nan();
            }
            // Conserva segno e bit alti della frazione, silenziato.
            let frac = a & from.frac_mask();
            let top = if from.f >= to.f { frac >> (from.f - to.f) } else { frac << (to.f - from.f) };
            to.zero(u.sign) | (to.exp_mask() << to.f) | (1 << (to.f - 1)) | (top & to.frac_mask())
        }
        Class::Inf => {
            if alt_to {
                ctx.flags |= IOC;
                return to.zero(u.sign) | (to.sign_bit() - 1);
            }
            to.infinity(u.sign)
        }
        Class::Zero => to.zero(u.sign),
        Class::Normal => round(to, Exact::from(&u), ctx, rounding),
    }
}

/// FRECPE: stima del reciproco (tabella di RecipEstimate).
pub fn recip_estimate(f: Fmt, a: u64, ctx: &mut Ctx) -> u64 {
    let u = unpack(f, a, ctx);
    match u.class {
        Class::QNaN | Class::SNaN => return process_nan(f, u.class, a, ctx),
        Class::Inf => return f.zero(u.sign),
        Class::Zero => {
            ctx.flags |= DZC;
            return f.infinity(u.sign);
        }
        Class::Normal => {}
    }
    let lz = u.mant.leading_zeros() as i32;
    let vexp = u.exp + 63 - lz; // |valore| in [2^vexp, 2^(vexp+1))
    let tiny = match f.n {
        16 => vexp < -16,
        32 => vexp < -128,
        _ => vexp < -1024,
    };
    if tiny {
        let to_inf = match ctx.rounding() {
            Rounding::TieEven => true,
            Rounding::PosInf => !u.sign,
            Rounding::NegInf => u.sign,
            _ => false,
        };
        ctx.flags |= OFC | IXC;
        return if to_inf { f.infinity(u.sign) } else { f.max_normal(u.sign) };
    }
    let big = match f.n {
        16 => vexp >= 14,
        32 => vexp >= 126,
        _ => vexp >= 1022,
    };
    if ctx.fz(f) && big {
        ctx.flags |= UFC;
        return f.zero(u.sign);
    }
    // Frazione estesa a 52 bit ed esponente, come per la doppia precisione.
    let mut fraction = (a & f.frac_mask()) << (52 - f.f);
    let mut exp = ((a >> f.f) & f.exp_mask()) as i64;
    if exp == 0 {
        if fraction >> 51 & 1 == 0 {
            exp = -1;
            fraction = (fraction << 2) & ((1 << 52) - 1);
        } else {
            fraction = (fraction << 1) & ((1 << 52) - 1);
        }
    }
    let scaled = 256 | (fraction >> 44);
    let result_exp: i64 = match f.n {
        16 => 29 - exp,
        32 => 253 - exp,
        _ => 2045 - exp,
    };
    let est = {
        let a = scaled * 2 + 1;
        let b = (1u64 << 19) / a;
        b.div_ceil(2)
    };
    let mut fraction = (est & 0xff) << 44;
    let mut rexp = result_exp;
    if rexp == 0 {
        fraction = (1 << 51) | (fraction >> 1);
    } else if rexp == -1 {
        fraction = (1 << 50) | (fraction >> 2);
        rexp = 0;
    }
    f.zero(u.sign) | ((rexp as u64 & f.exp_mask()) << f.f) | (fraction >> (52 - f.f))
}

/// FRSQRTE: stima della radice reciproca.
pub fn rsqrt_estimate(f: Fmt, a: u64, ctx: &mut Ctx) -> u64 {
    let u = unpack(f, a, ctx);
    match u.class {
        Class::QNaN | Class::SNaN => return process_nan(f, u.class, a, ctx),
        Class::Zero => {
            ctx.flags |= DZC;
            return f.infinity(u.sign);
        }
        _ if u.sign => {
            ctx.flags |= IOC;
            return f.default_nan();
        }
        Class::Inf => return f.zero(false),
        Class::Normal => {}
    }
    let mut fraction = (a & f.frac_mask()) << (52 - f.f);
    let mut exp = ((a >> f.f) & f.exp_mask()) as i64;
    if exp == 0 {
        while fraction >> 51 & 1 == 0 {
            fraction = (fraction << 1) & ((1 << 52) - 1);
            exp -= 1;
        }
        fraction = (fraction << 1) & ((1 << 52) - 1);
    }
    let scaled = if exp & 1 == 0 { 256 | (fraction >> 44) } else { 128 | (fraction >> 45) };
    let result_exp: i64 = match f.n {
        16 => (44 - exp).div_euclid(2),
        32 => (380 - exp).div_euclid(2),
        _ => (3068 - exp).div_euclid(2),
    };
    let est = {
        let mut a = scaled;
        if a < 256 {
            a = a * 2 + 1;
        } else {
            a = (a >> 1) << 1;
            a = (a + 1) * 2;
        }
        let mut b = 512u64;
        while a * (b + 1) * (b + 1) < (1 << 28) {
            b += 1;
        }
        b.div_ceil(2)
    };
    ((result_exp as u64 & f.exp_mask()) << f.f) | ((est & 0xff) << (f.f - 8))
}

/// FRECPX: reciproco dell'esponente.
pub fn recpx(f: Fmt, a: u64, ctx: &mut Ctx) -> u64 {
    let u = unpack(f, a, ctx);
    if is_nan(u.class) {
        return process_nan(f, u.class, a, ctx);
    }
    let exp = (a >> f.f) & f.exp_mask();
    let sign = f.zero(u.sign);
    if exp == 0 { sign | ((f.exp_mask() - 1) << f.f) } else { sign | ((!exp & f.exp_mask()) << f.f) }
}

/// Stime intere URECPE / URSQRTE (su 32 bit).
pub fn unsigned_recip_estimate(x: u32) -> u32 {
    if x >> 31 == 0 {
        return 0xffff_ffff;
    }
    let a = (x >> 23) as u64; // 256..511
    let a = a * 2 + 1;
    let b = (1u64 << 19) / a;
    let r = b.div_ceil(2);
    (r as u32) << 23
}

pub fn unsigned_rsqrt_estimate(x: u32) -> u32 {
    if x >> 30 == 0 {
        return 0xffff_ffff;
    }
    let mut a = (x >> 23) as u64; // 128..511
    if a < 256 {
        a = a * 2 + 1;
    } else {
        a = (a >> 1) << 1;
        a = (a + 1) * 2;
    }
    let mut b = 512u64;
    while a * (b + 1) * (b + 1) < (1 << 28) {
        b += 1;
    }
    ((b.div_ceil(2)) as u32) << 23
}

/// VFPExpandImm: immediato di FMOV.
pub fn expand_imm(f: Fmt, imm8: u64) -> u64 {
    let sign = imm8 >> 7;
    let b6 = (imm8 >> 6) & 1;
    let exp =
        ((b6 ^ 1) << (f.e - 1)) | (if b6 == 1 { ((1 << (f.e - 3)) - 1) << 2 } else { 0 }) | ((imm8 >> 4) & 3);
    let frac = (imm8 & 0xf) << (f.f - 4);
    (sign << (f.n - 1)) | (exp << f.f) | frac
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c() -> Ctx {
        Ctx::new(0)
    }

    #[test]
    fn basic_ops_match_ieee() {
        let mut k = c();
        let one = 1.0f64.to_bits();
        let three = 3.0f64.to_bits();
        assert_eq!(f64::from_bits(add(D, one, three, &mut k)), 4.0);
        assert_eq!(f64::from_bits(div(D, one, three, &mut k)), 1.0 / 3.0);
        assert!(k.flags & IXC != 0);
        let mut k = c();
        assert_eq!(f64::from_bits(sqrt(D, 2.0f64.to_bits(), &mut k)), 2.0f64.sqrt());
        assert_eq!(
            f32::from_bits(mul(S, 1.5f32.to_bits() as u64, 2.5f32.to_bits() as u64, &mut k) as u32),
            3.75
        );
        assert_eq!(f64::from_bits(sub(D, one, one, &mut k)), 0.0);
        assert!(!f64::from_bits(sub(D, one, one, &mut k)).is_sign_negative());
        let fma = mul_add(D, 1.0f64.to_bits(), 0.1f64.to_bits(), 10.0f64.to_bits(), &mut k);
        assert_eq!(
            f64::from_bits(fma),
            1.0f64 + 0.1f64 * 10.0f64 - (0.1f64 * 10.0f64 - 0.1f64.mul_add(10.0, 0.0))
        );
    }

    #[test]
    fn nan_and_specials() {
        let mut k = c();
        let snan = 0x7ff0_0000_0000_0001u64;
        assert_eq!(add(D, snan, 0, &mut k), 0x7ff8_0000_0000_0001);
        assert!(k.flags & IOC != 0);
        let mut k = c();
        assert_eq!(div(D, 1.0f64.to_bits(), 0, &mut k), D.infinity(false));
        assert_eq!(k.flags, DZC);
        assert_eq!(compare(D, snan, 0, false, &mut k), 0b0011);
        assert_eq!(expand_imm(D, 0x70), 1.0f64.to_bits());
        assert_eq!(expand_imm(S, 0x00) as u32, 2.0f32.to_bits());
    }

    #[test]
    fn conversions() {
        let mut k = c();
        assert_eq!(
            to_fixed(D, (-2.5f64).to_bits(), 0, false, 32, Rounding::TieEven, &mut k) as u32 as i32,
            -2
        );
        assert_eq!(
            to_fixed(D, (-2.5f64).to_bits(), 0, false, 32, Rounding::TieAway, &mut k) as u32 as i32,
            -3
        );
        assert_eq!(to_fixed(D, 1e20f64.to_bits(), 0, false, 32, Rounding::Zero, &mut k) as u32, 0x7fff_ffff);
        assert_eq!(
            from_fixed(D, (-7i64) as u64, 1, false, 64, Rounding::TieEven, &mut k),
            (-3.5f64).to_bits()
        );
        assert_eq!(convert(D, S, 0.1f64.to_bits(), Rounding::TieEven, &mut k) as u32, 0.1f32.to_bits());
        assert_eq!(round_int(D, 2.5f64.to_bits(), Rounding::TieEven, false, &mut k), 2.0f64.to_bits());
        assert_eq!(round_int(D, (-0.4f64).to_bits(), Rounding::TieEven, false, &mut k), (-0.0f64).to_bits());
    }
}
