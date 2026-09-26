//! Encoder minimo di moduli WebAssembly (formato binario 1.0 con le
//! estensioni di segno, "sign-extension ops", presenti in ogni motore).
//!
//! Solo ciò che serve ai moduli del JIT: tipi di funzione, import di una
//! memoria e di funzioni, funzioni con variabili locali, export e codice.

/// Tipi di valore.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValType {
    I32 = 0x7f,
    I64 = 0x7e,
    F32 = 0x7d,
    F64 = 0x7c,
    V128 = 0x7b,
}

/// Opcode usati dal traduttore (Core spec, sezione 5.4).
pub mod op {
    pub const UNREACHABLE: u8 = 0x00;
    pub const BLOCK: u8 = 0x02;
    pub const LOOP: u8 = 0x03;
    pub const IF: u8 = 0x04;
    pub const ELSE: u8 = 0x05;
    pub const END: u8 = 0x0b;
    pub const BR: u8 = 0x0c;
    pub const BR_IF: u8 = 0x0d;
    pub const BR_TABLE: u8 = 0x0e;
    pub const RETURN: u8 = 0x0f;
    pub const CALL: u8 = 0x10;
    pub const CALL_INDIRECT: u8 = 0x11;
    pub const DROP: u8 = 0x1a;
    pub const SELECT: u8 = 0x1b;
    pub const LOCAL_GET: u8 = 0x20;
    pub const LOCAL_SET: u8 = 0x21;
    pub const LOCAL_TEE: u8 = 0x22;
    pub const I32_LOAD: u8 = 0x28;
    pub const I64_LOAD: u8 = 0x29;
    pub const I64_LOAD8_U: u8 = 0x31;
    pub const I64_LOAD16_U: u8 = 0x33;
    pub const I64_LOAD32_U: u8 = 0x35;
    pub const I32_STORE: u8 = 0x36;
    pub const I64_STORE: u8 = 0x37;
    pub const I64_STORE8: u8 = 0x3c;
    pub const I64_STORE16: u8 = 0x3d;
    pub const I64_STORE32: u8 = 0x3e;
    pub const I32_CONST: u8 = 0x41;
    pub const I64_CONST: u8 = 0x42;

    pub const I32_EQZ: u8 = 0x45;
    pub const I32_EQ: u8 = 0x46;
    pub const I32_NE: u8 = 0x47;
    pub const I32_LT_U: u8 = 0x49;
    pub const I32_GT_U: u8 = 0x4b;
    pub const I32_LE_S: u8 = 0x4c;
    pub const I32_MUL: u8 = 0x6c;
    // Virgola mobile (ADR 0026).
    pub const F32_LOAD: u8 = 0x2a;
    pub const F64_LOAD: u8 = 0x2b;
    pub const F32_EQ: u8 = 0x5b;
    pub const F32_NE: u8 = 0x5c;
    pub const F32_LT: u8 = 0x5d;
    pub const F32_GT: u8 = 0x5e;
    pub const F32_LE: u8 = 0x5f;
    pub const F32_GE: u8 = 0x60;
    pub const F64_EQ: u8 = 0x61;
    pub const F64_NE: u8 = 0x62;
    pub const F64_LT: u8 = 0x63;
    pub const F64_GT: u8 = 0x64;
    pub const F64_LE: u8 = 0x65;
    pub const F64_GE: u8 = 0x66;
    pub const F32_ABS: u8 = 0x8b;
    pub const F32_NEG: u8 = 0x8c;
    pub const F32_CEIL: u8 = 0x8d;
    pub const F32_FLOOR: u8 = 0x8e;
    pub const F32_TRUNC: u8 = 0x8f;
    pub const F32_NEAREST: u8 = 0x90;
    pub const F32_SQRT: u8 = 0x91;
    pub const F32_ADD: u8 = 0x92;
    pub const F32_SUB: u8 = 0x93;
    pub const F32_MUL: u8 = 0x94;
    pub const F32_DIV: u8 = 0x95;
    pub const F32_MIN: u8 = 0x96;
    pub const F32_MAX: u8 = 0x97;
    pub const F64_ABS: u8 = 0x99;
    pub const F64_NEG: u8 = 0x9a;
    pub const F64_CEIL: u8 = 0x9b;
    pub const F64_FLOOR: u8 = 0x9c;
    pub const F64_TRUNC: u8 = 0x9d;
    pub const F64_NEAREST: u8 = 0x9e;
    pub const F64_SQRT: u8 = 0x9f;
    pub const F64_ADD: u8 = 0xa0;
    pub const F64_SUB: u8 = 0xa1;
    pub const F64_MUL: u8 = 0xa2;
    pub const F64_DIV: u8 = 0xa3;
    pub const F64_MIN: u8 = 0xa4;
    pub const F64_MAX: u8 = 0xa5;
    pub const F32_CONVERT_I32_S: u8 = 0xb2;
    pub const F32_CONVERT_I32_U: u8 = 0xb3;
    pub const F32_CONVERT_I64_S: u8 = 0xb4;
    pub const F32_CONVERT_I64_U: u8 = 0xb5;
    pub const F32_DEMOTE_F64: u8 = 0xb6;
    pub const F64_CONVERT_I32_S: u8 = 0xb7;
    pub const F64_CONVERT_I32_U: u8 = 0xb8;
    pub const F64_CONVERT_I64_S: u8 = 0xb9;
    pub const F64_CONVERT_I64_U: u8 = 0xba;
    pub const F64_PROMOTE_F32: u8 = 0xbb;
    pub const I32_REINTERPRET_F32: u8 = 0xbc;
    pub const I64_REINTERPRET_F64: u8 = 0xbd;
    pub const F32_REINTERPRET_I32: u8 = 0xbe;
    pub const F64_REINTERPRET_I64: u8 = 0xbf;
    pub const I32_LT_S: u8 = 0x48;
    pub const I32_GT_S: u8 = 0x4a;
    pub const I32_LE_U: u8 = 0x4d;
    pub const I32_GE_S: u8 = 0x4e;
    pub const I32_GE_U: u8 = 0x4f;
    pub const I64_EQZ: u8 = 0x50;
    pub const I64_EQ: u8 = 0x51;
    pub const I64_NE: u8 = 0x52;
    pub const I64_LT_S: u8 = 0x53;
    pub const I64_LT_U: u8 = 0x54;
    pub const I64_GT_S: u8 = 0x55;
    pub const I64_GT_U: u8 = 0x56;
    pub const I64_LE_U: u8 = 0x58;
    pub const I64_GE_S: u8 = 0x59;
    pub const I64_GE_U: u8 = 0x5a;

    pub const I32_CLZ: u8 = 0x67;
    pub const I32_ADD: u8 = 0x6a;
    pub const I32_SUB: u8 = 0x6b;
    pub const I32_AND: u8 = 0x71;
    pub const I32_OR: u8 = 0x72;
    pub const I32_XOR: u8 = 0x73;
    pub const I32_SHL: u8 = 0x74;
    pub const I32_SHR_S: u8 = 0x75;
    pub const I32_SHR_U: u8 = 0x76;
    pub const I32_ROTL: u8 = 0x77;
    pub const I32_ROTR: u8 = 0x78;

    pub const I64_CLZ: u8 = 0x79;
    pub const I64_ADD: u8 = 0x7c;
    pub const I64_SUB: u8 = 0x7d;
    pub const I64_MUL: u8 = 0x7e;
    pub const I64_DIV_S: u8 = 0x7f;
    pub const I64_DIV_U: u8 = 0x80;
    pub const I64_AND: u8 = 0x83;
    pub const I64_OR: u8 = 0x84;
    pub const I64_XOR: u8 = 0x85;
    pub const I64_SHL: u8 = 0x86;
    pub const I64_SHR_S: u8 = 0x87;
    pub const I64_SHR_U: u8 = 0x88;
    pub const I64_ROTL: u8 = 0x89;
    pub const I64_ROTR: u8 = 0x8a;

    pub const I32_WRAP_I64: u8 = 0xa7;
    pub const I64_EXTEND_I32_U: u8 = 0xad;
    pub const I64_EXTEND8_S: u8 = 0xc2;
    pub const I64_EXTEND16_S: u8 = 0xc3;
    pub const I64_EXTEND32_S: u8 = 0xc4;
}

/// Tipo di blocco di `if`: vuoto o con un risultato.
pub const BLOCK_EMPTY: u8 = 0x40;

/// Conversioni saturanti (prefisso 0xfc).
pub mod sat {
    pub const I32_TRUNC_SAT_F32_S: u32 = 0;
    pub const I32_TRUNC_SAT_F32_U: u32 = 1;
    pub const I32_TRUNC_SAT_F64_S: u32 = 2;
    pub const I32_TRUNC_SAT_F64_U: u32 = 3;
    pub const I64_TRUNC_SAT_F32_S: u32 = 4;
    pub const I64_TRUNC_SAT_F32_U: u32 = 5;
    pub const I64_TRUNC_SAT_F64_S: u32 = 6;
    pub const I64_TRUNC_SAT_F64_U: u32 = 7;
}

/// SIMD a 128 bit (prefisso 0xfd, "fixed-width SIMD"): opcode usati dal
/// traduttore (ADR 0026).
pub mod v {
    pub const LOAD: u32 = 0x00;
    pub const LOAD8_SPLAT: u32 = 0x07;
    pub const LOAD16_SPLAT: u32 = 0x08;
    pub const LOAD32_SPLAT: u32 = 0x09;
    pub const LOAD64_SPLAT: u32 = 0x0a;
    pub const STORE: u32 = 0x0b;
    pub const CONST: u32 = 0x0c;
    pub const SHUFFLE: u32 = 0x0d;
    pub const SWIZZLE: u32 = 0x0e;
    pub const I8X16_SPLAT: u32 = 0x0f;
    pub const I16X8_SPLAT: u32 = 0x10;
    pub const I32X4_SPLAT: u32 = 0x11;
    pub const I64X2_SPLAT: u32 = 0x12;
    pub const F32X4_SPLAT: u32 = 0x13;
    pub const F64X2_SPLAT: u32 = 0x14;
    pub const I8X16_EXTRACT_LANE_U: u32 = 0x16;
    pub const I16X8_EXTRACT_LANE_U: u32 = 0x19;
    pub const I32X4_EXTRACT_LANE: u32 = 0x1b;
    pub const I32X4_REPLACE_LANE: u32 = 0x1c;
    pub const I64X2_EXTRACT_LANE: u32 = 0x1d;
    pub const I64X2_REPLACE_LANE: u32 = 0x1e;
    pub const F32X4_EXTRACT_LANE: u32 = 0x1f;
    pub const F64X2_EXTRACT_LANE: u32 = 0x21;
    pub const I8X16_EQ: u32 = 0x23;
    pub const I8X16_LT_U: u32 = 0x26;
    pub const I8X16_GT_S: u32 = 0x27;
    pub const I8X16_GT_U: u32 = 0x28;
    pub const I8X16_GE_S: u32 = 0x2b;
    pub const I8X16_GE_U: u32 = 0x2c;
    pub const I16X8_EQ: u32 = 0x2d;
    pub const I16X8_GT_S: u32 = 0x31;
    pub const I16X8_GT_U: u32 = 0x32;
    pub const I16X8_GE_S: u32 = 0x35;
    pub const I16X8_GE_U: u32 = 0x36;
    pub const I32X4_EQ: u32 = 0x37;
    pub const I32X4_LT_U: u32 = 0x3a;
    pub const I32X4_GT_S: u32 = 0x3b;
    pub const I32X4_GT_U: u32 = 0x3c;
    pub const I32X4_GE_S: u32 = 0x3f;
    pub const I32X4_GE_U: u32 = 0x40;
    pub const F32X4_EQ: u32 = 0x41;
    pub const F32X4_NE: u32 = 0x42;
    pub const F32X4_GT: u32 = 0x44;
    pub const F32X4_GE: u32 = 0x46;
    pub const F64X2_EQ: u32 = 0x47;
    pub const F64X2_NE: u32 = 0x48;
    pub const F64X2_GT: u32 = 0x4a;
    pub const F64X2_GE: u32 = 0x4c;
    pub const NOT: u32 = 0x4d;
    pub const AND: u32 = 0x4e;
    pub const ANDNOT: u32 = 0x4f;
    pub const OR: u32 = 0x50;
    pub const XOR: u32 = 0x51;
    pub const BITSELECT: u32 = 0x52;
    pub const ANY_TRUE: u32 = 0x53;
    pub const F32X4_DEMOTE_F64X2_ZERO: u32 = 0x5e;
    pub const F64X2_PROMOTE_LOW_F32X4: u32 = 0x5f;
    pub const I8X16_ABS: u32 = 0x60;
    pub const I8X16_NEG: u32 = 0x61;
    pub const I8X16_POPCNT: u32 = 0x62;
    pub const I8X16_ALL_TRUE: u32 = 0x63;
    pub const I8X16_SHL: u32 = 0x6b;
    pub const I8X16_SHR_S: u32 = 0x6c;
    pub const I8X16_SHR_U: u32 = 0x6d;
    pub const I8X16_ADD: u32 = 0x6e;
    pub const I8X16_SUB: u32 = 0x71;
    pub const I8X16_MIN_S: u32 = 0x76;
    pub const I8X16_MIN_U: u32 = 0x77;
    pub const I8X16_MAX_S: u32 = 0x78;
    pub const I8X16_MAX_U: u32 = 0x79;
    pub const I8X16_AVGR_U: u32 = 0x7b;
    pub const I16X8_EXTADD_PAIRWISE_I8X16_S: u32 = 0x7c;
    pub const I16X8_EXTADD_PAIRWISE_I8X16_U: u32 = 0x7d;
    pub const I32X4_EXTADD_PAIRWISE_I16X8_S: u32 = 0x7e;
    pub const I32X4_EXTADD_PAIRWISE_I16X8_U: u32 = 0x7f;
    pub const I16X8_ABS: u32 = 0x80;
    pub const I16X8_NEG: u32 = 0x81;
    pub const I16X8_ALL_TRUE: u32 = 0x83;
    pub const I16X8_EXTEND_LOW_I8X16_S: u32 = 0x87;
    pub const I16X8_EXTEND_HIGH_I8X16_S: u32 = 0x88;
    pub const I16X8_EXTEND_LOW_I8X16_U: u32 = 0x89;
    pub const I16X8_EXTEND_HIGH_I8X16_U: u32 = 0x8a;
    pub const I16X8_SHL: u32 = 0x8b;
    pub const I16X8_SHR_S: u32 = 0x8c;
    pub const I16X8_SHR_U: u32 = 0x8d;
    pub const I16X8_ADD: u32 = 0x8e;
    pub const I16X8_SUB: u32 = 0x91;
    pub const I16X8_MUL: u32 = 0x95;
    pub const I16X8_MIN_S: u32 = 0x96;
    pub const I16X8_MIN_U: u32 = 0x97;
    pub const I16X8_MAX_S: u32 = 0x98;
    pub const I16X8_MAX_U: u32 = 0x99;
    pub const I16X8_AVGR_U: u32 = 0x9b;
    pub const I16X8_EXTMUL_LOW_I8X16_S: u32 = 0x9c;
    pub const I16X8_EXTMUL_HIGH_I8X16_S: u32 = 0x9d;
    pub const I16X8_EXTMUL_LOW_I8X16_U: u32 = 0x9e;
    pub const I16X8_EXTMUL_HIGH_I8X16_U: u32 = 0x9f;
    pub const I32X4_ABS: u32 = 0xa0;
    pub const I32X4_NEG: u32 = 0xa1;
    pub const I32X4_ALL_TRUE: u32 = 0xa3;
    pub const I32X4_EXTEND_LOW_I16X8_S: u32 = 0xa7;
    pub const I32X4_EXTEND_HIGH_I16X8_S: u32 = 0xa8;
    pub const I32X4_EXTEND_LOW_I16X8_U: u32 = 0xa9;
    pub const I32X4_EXTEND_HIGH_I16X8_U: u32 = 0xaa;
    pub const I32X4_SHL: u32 = 0xab;
    pub const I32X4_SHR_S: u32 = 0xac;
    pub const I32X4_SHR_U: u32 = 0xad;
    pub const I32X4_ADD: u32 = 0xae;
    pub const I32X4_SUB: u32 = 0xb1;
    pub const I32X4_MUL: u32 = 0xb5;
    pub const I32X4_MIN_S: u32 = 0xb6;
    pub const I32X4_MIN_U: u32 = 0xb7;
    pub const I32X4_MAX_S: u32 = 0xb8;
    pub const I32X4_MAX_U: u32 = 0xb9;
    pub const I32X4_EXTMUL_LOW_I16X8_S: u32 = 0xbc;
    pub const I32X4_EXTMUL_HIGH_I16X8_S: u32 = 0xbd;
    pub const I32X4_EXTMUL_LOW_I16X8_U: u32 = 0xbe;
    pub const I32X4_EXTMUL_HIGH_I16X8_U: u32 = 0xbf;
    pub const I64X2_ABS: u32 = 0xc0;
    pub const I64X2_NEG: u32 = 0xc1;
    pub const I64X2_ALL_TRUE: u32 = 0xc3;
    pub const I64X2_EXTEND_LOW_I32X4_S: u32 = 0xc7;
    pub const I64X2_EXTEND_HIGH_I32X4_S: u32 = 0xc8;
    pub const I64X2_EXTEND_LOW_I32X4_U: u32 = 0xc9;
    pub const I64X2_EXTEND_HIGH_I32X4_U: u32 = 0xca;
    pub const I64X2_SHL: u32 = 0xcb;
    pub const I64X2_SHR_S: u32 = 0xcc;
    pub const I64X2_SHR_U: u32 = 0xcd;
    pub const I64X2_ADD: u32 = 0xce;
    pub const I64X2_SUB: u32 = 0xd1;
    pub const I64X2_MUL: u32 = 0xd5;
    pub const I64X2_EQ: u32 = 0xd6;
    pub const I64X2_GT_S: u32 = 0xd9;
    pub const I64X2_GE_S: u32 = 0xdb;
    pub const I64X2_EXTMUL_LOW_I32X4_S: u32 = 0xdc;
    pub const I64X2_EXTMUL_HIGH_I32X4_S: u32 = 0xdd;
    pub const I64X2_EXTMUL_LOW_I32X4_U: u32 = 0xde;
    pub const I64X2_EXTMUL_HIGH_I32X4_U: u32 = 0xdf;
    pub const F32X4_ABS: u32 = 0xe0;
    pub const F32X4_NEG: u32 = 0xe1;
    pub const F32X4_SQRT: u32 = 0xe3;
    pub const F32X4_ADD: u32 = 0xe4;
    pub const F32X4_SUB: u32 = 0xe5;
    pub const F32X4_MUL: u32 = 0xe6;
    pub const F32X4_DIV: u32 = 0xe7;
    pub const F32X4_MIN: u32 = 0xe8;
    pub const F32X4_MAX: u32 = 0xe9;
    pub const F64X2_ABS: u32 = 0xec;
    pub const F64X2_NEG: u32 = 0xed;
    pub const F64X2_SQRT: u32 = 0xef;
    pub const F64X2_ADD: u32 = 0xf0;
    pub const F64X2_SUB: u32 = 0xf1;
    pub const F64X2_MUL: u32 = 0xf2;
    pub const F64X2_DIV: u32 = 0xf3;
    pub const F64X2_MIN: u32 = 0xf4;
    pub const F64X2_MAX: u32 = 0xf5;
    pub const I32X4_TRUNC_SAT_F32X4_S: u32 = 0xf8;
    pub const I32X4_TRUNC_SAT_F32X4_U: u32 = 0xf9;
    pub const F32X4_CONVERT_I32X4_S: u32 = 0xfa;
    pub const F32X4_CONVERT_I32X4_U: u32 = 0xfb;
}

pub fn uleb(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

pub fn sleb(out: &mut Vec<u8>, mut v: i64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        let done = (v == 0 && b & 0x40 == 0) || (v == -1 && b & 0x40 != 0);
        if done {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn name(out: &mut Vec<u8>, s: &str) {
    uleb(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

/// Corpo di una funzione: variabili locali e istruzioni.
#[derive(Clone, Debug, Default)]
pub struct Func {
    /// Gruppi (quantità, tipo) di variabili locali oltre ai parametri.
    pub locals: Vec<(u32, ValType)>,
    pub code: Vec<u8>,
    /// Blocchi (`block`, `loop`, `if`) aperti nel punto corrente: serve a
    /// calcolare le etichette di `br`.
    pub depth: u32,
}

impl Func {
    #[inline]
    pub fn op(&mut self, o: u8) -> &mut Self {
        self.code.push(o);
        self
    }
    pub fn local_get(&mut self, i: u32) -> &mut Self {
        self.code.push(op::LOCAL_GET);
        uleb(&mut self.code, i as u64);
        self
    }
    pub fn local_set(&mut self, i: u32) -> &mut Self {
        self.code.push(op::LOCAL_SET);
        uleb(&mut self.code, i as u64);
        self
    }
    pub fn local_tee(&mut self, i: u32) -> &mut Self {
        self.code.push(op::LOCAL_TEE);
        uleb(&mut self.code, i as u64);
        self
    }
    pub fn i32_const(&mut self, v: i32) -> &mut Self {
        self.code.push(op::I32_CONST);
        sleb(&mut self.code, v as i64);
        self
    }
    pub fn i64_const(&mut self, v: i64) -> &mut Self {
        self.code.push(op::I64_CONST);
        sleb(&mut self.code, v);
        self
    }
    fn memarg(&mut self, o: u8, align: u32, offset: u32) -> &mut Self {
        self.code.push(o);
        uleb(&mut self.code, align as u64);
        uleb(&mut self.code, offset as u64);
        self
    }
    pub fn i64_load(&mut self, offset: u32) -> &mut Self {
        self.memarg(op::I64_LOAD, 3, offset)
    }
    pub fn i64_store(&mut self, offset: u32) -> &mut Self {
        self.memarg(op::I64_STORE, 3, offset)
    }
    pub fn i32_load(&mut self, offset: u32) -> &mut Self {
        self.memarg(op::I32_LOAD, 2, offset)
    }
    pub fn i32_store(&mut self, offset: u32) -> &mut Self {
        self.memarg(op::I32_STORE, 2, offset)
    }
    /// Load di `bytes` byte (1, 2, 4, 8) esteso a zero in un i64, con
    /// l'allineamento naturale come suggerimento.
    pub fn i64_load_n(&mut self, bytes: u32, offset: u32) -> &mut Self {
        match bytes {
            1 => self.memarg(op::I64_LOAD8_U, 0, offset),
            2 => self.memarg(op::I64_LOAD16_U, 1, offset),
            4 => self.memarg(op::I64_LOAD32_U, 2, offset),
            _ => self.memarg(op::I64_LOAD, 3, offset),
        }
    }
    /// Store dei `bytes` byte bassi di un i64.
    pub fn i64_store_n(&mut self, bytes: u32, offset: u32) -> &mut Self {
        match bytes {
            1 => self.memarg(op::I64_STORE8, 0, offset),
            2 => self.memarg(op::I64_STORE16, 1, offset),
            4 => self.memarg(op::I64_STORE32, 2, offset),
            _ => self.memarg(op::I64_STORE, 3, offset),
        }
    }
    pub fn f32_load(&mut self, offset: u32) -> &mut Self {
        self.memarg(op::F32_LOAD, 2, offset)
    }
    pub fn f64_load(&mut self, offset: u32) -> &mut Self {
        self.memarg(op::F64_LOAD, 3, offset)
    }
    /// Istruzione con prefisso 0xfc (conversioni saturanti, [`sat`]).
    pub fn sat(&mut self, o: u32) -> &mut Self {
        self.code.push(0xfc);
        uleb(&mut self.code, o as u64);
        self
    }
    /// Istruzione SIMD senza immediati ([`v`]).
    pub fn v(&mut self, o: u32) -> &mut Self {
        self.code.push(0xfd);
        uleb(&mut self.code, o as u64);
        self
    }
    /// `v128.load` / `v128.store` (allineamento 16 come suggerimento).
    pub fn v128_load(&mut self, offset: u32) -> &mut Self {
        self.v(v::LOAD);
        uleb(&mut self.code, 4);
        uleb(&mut self.code, offset as u64);
        self
    }
    pub fn v128_store(&mut self, offset: u32) -> &mut Self {
        self.v(v::STORE);
        uleb(&mut self.code, 4);
        uleb(&mut self.code, offset as u64);
        self
    }
    /// `v128.load{8,16,32,64}_splat` di `bytes` byte.
    pub fn v128_load_splat(&mut self, bytes: u32, offset: u32) -> &mut Self {
        let (o, a) = match bytes {
            1 => (v::LOAD8_SPLAT, 0),
            2 => (v::LOAD16_SPLAT, 1),
            4 => (v::LOAD32_SPLAT, 2),
            _ => (v::LOAD64_SPLAT, 3),
        };
        self.v(o);
        uleb(&mut self.code, a);
        uleb(&mut self.code, offset as u64);
        self
    }
    /// `v128.const` dai due u64 (basso, alto).
    pub fn v128_const(&mut self, lo: u64, hi: u64) -> &mut Self {
        self.v(v::CONST);
        self.code.extend_from_slice(&lo.to_le_bytes());
        self.code.extend_from_slice(&hi.to_le_bytes());
        self
    }
    /// `i8x16.shuffle` con gli indici `lanes` (0..32).
    pub fn shuffle(&mut self, lanes: [u8; 16]) -> &mut Self {
        debug_assert!(lanes.iter().all(|&l| l < 32));
        self.v(v::SHUFFLE);
        self.code.extend_from_slice(&lanes);
        self
    }
    /// Estrazione o sostituzione di una corsia (`o` di [`v`]).
    pub fn lane(&mut self, o: u32, lane: u8) -> &mut Self {
        self.v(o);
        self.code.push(lane);
        self
    }
    /// `call_indirect` sul tipo `ty` nella tabella 0.
    pub fn call_indirect(&mut self, ty: u32) -> &mut Self {
        self.code.push(op::CALL_INDIRECT);
        uleb(&mut self.code, ty as u64);
        self.code.push(0);
        self
    }
    /// `loop` o `block` con tipo `bt`.
    pub fn loop_(&mut self, bt: u8) -> &mut Self {
        self.code.push(op::LOOP);
        self.code.push(bt);
        self.depth += 1;
        self
    }
    pub fn block(&mut self, bt: u8) -> &mut Self {
        self.code.push(op::BLOCK);
        self.code.push(bt);
        self.depth += 1;
        self
    }
    pub fn br(&mut self, depth: u32) -> &mut Self {
        self.code.push(op::BR);
        uleb(&mut self.code, depth as u64);
        self
    }
    pub fn br_if(&mut self, depth: u32) -> &mut Self {
        self.code.push(op::BR_IF);
        uleb(&mut self.code, depth as u64);
        self
    }
    /// `br_table`: salta all'etichetta `labels[i]` (i32 in cima allo
    /// stack), o a `default` se `i` è fuori.
    pub fn br_table(&mut self, labels: &[u32], default: u32) -> &mut Self {
        self.code.push(op::BR_TABLE);
        uleb(&mut self.code, labels.len() as u64);
        for &l in labels {
            uleb(&mut self.code, l as u64);
        }
        uleb(&mut self.code, default as u64);
        self
    }
    pub fn call(&mut self, f: u32) -> &mut Self {
        self.code.push(op::CALL);
        uleb(&mut self.code, f as u64);
        self
    }
    /// `if` con tipo di blocco `bt` (`BLOCK_EMPTY` o un [`ValType`]).
    pub fn if_(&mut self, bt: u8) -> &mut Self {
        self.code.push(op::IF);
        self.code.push(bt);
        self.depth += 1;
        self
    }
    pub fn else_(&mut self) -> &mut Self {
        self.op(op::ELSE)
    }
    pub fn end(&mut self) -> &mut Self {
        self.depth = self.depth.saturating_sub(1);
        self.op(op::END)
    }

    fn encode(&self, out: &mut Vec<u8>) {
        let mut body = Vec::with_capacity(self.code.len() + 16);
        uleb(&mut body, self.locals.len() as u64);
        for &(n, t) in &self.locals {
            uleb(&mut body, n as u64);
            body.push(t as u8);
        }
        body.extend_from_slice(&self.code);
        body.push(op::END);
        uleb(out, body.len() as u64);
        out.extend_from_slice(&body);
    }
}

/// Tipo di funzione: parametri e risultati.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FuncType {
    pub params: Vec<ValType>,
    pub results: Vec<ValType>,
}

/// Import di una memoria: minimo di pagine e, per una memoria condivisa
/// (thread), il massimo obbligatorio.
#[derive(Clone, Copy, Debug)]
pub struct MemoryImport {
    pub min: u32,
    pub shared_max: Option<u32>,
}

/// Modulo in costruzione. Le funzioni importate precedono quelle definite
/// nello spazio degli indici, come vuole il formato.
#[derive(Clone, Debug, Default)]
pub struct Module {
    types: Vec<FuncType>,
    memory: Option<(String, String, MemoryImport)>,
    func_imports: Vec<(String, String, u32)>,
    funcs: Vec<(u32, Func)>,
    exports: Vec<(String, u32)>,
    /// Tabella di funzioni importata: (modulo, campo, minimo di voci).
    table: Option<(String, String, u32)>,
}

impl Module {
    pub fn new() -> Self {
        Self::default()
    }

    /// Indice del tipo (riusa un tipo identico già presente).
    pub fn ty(&mut self, params: &[ValType], results: &[ValType]) -> u32 {
        let t = FuncType { params: params.to_vec(), results: results.to_vec() };
        if let Some(i) = self.types.iter().position(|x| *x == t) {
            return i as u32;
        }
        self.types.push(t);
        (self.types.len() - 1) as u32
    }

    pub fn import_memory(&mut self, module: &str, field: &str, mem: MemoryImport) {
        self.memory = Some((module.into(), field.into(), mem));
    }

    /// Importa la tabella 0 (`funcref`, almeno `min` voci): la condividono
    /// i moduli del JIT per il concatenamento dei blocchi.
    pub fn import_table(&mut self, module: &str, field: &str, min: u32) {
        self.table = Some((module.into(), field.into(), min));
    }

    /// Importa una funzione; restituisce il suo indice. Va chiamata prima di
    /// definire funzioni.
    pub fn import_func(&mut self, module: &str, field: &str, ty: u32) -> u32 {
        assert!(self.funcs.is_empty(), "import dopo le funzioni definite");
        self.func_imports.push((module.into(), field.into(), ty));
        (self.func_imports.len() - 1) as u32
    }

    /// Definisce una funzione; restituisce il suo indice.
    pub fn func(&mut self, ty: u32, f: Func) -> u32 {
        self.funcs.push((ty, f));
        (self.func_imports.len() + self.funcs.len() - 1) as u32
    }

    pub fn export_func(&mut self, name: &str, index: u32) {
        self.exports.push((name.into(), index));
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = b"\0asm".to_vec();
        out.extend_from_slice(&1u32.to_le_bytes());

        let mut sec = Vec::new();
        uleb(&mut sec, self.types.len() as u64);
        for t in &self.types {
            sec.push(0x60);
            uleb(&mut sec, t.params.len() as u64);
            sec.extend(t.params.iter().map(|&v| v as u8));
            uleb(&mut sec, t.results.len() as u64);
            sec.extend(t.results.iter().map(|&v| v as u8));
        }
        section(&mut out, 1, &sec);

        sec.clear();
        let n = self.func_imports.len() + self.memory.is_some() as usize + self.table.is_some() as usize;
        uleb(&mut sec, n as u64);
        if let Some((m, f, min)) = &self.table {
            name(&mut sec, m);
            name(&mut sec, f);
            sec.push(0x01);
            sec.push(0x70); // funcref
            sec.push(0x00);
            uleb(&mut sec, *min as u64);
        }
        if let Some((m, f, mem)) = &self.memory {
            name(&mut sec, m);
            name(&mut sec, f);
            sec.push(0x02);
            match mem.shared_max {
                None => {
                    sec.push(0x00);
                    uleb(&mut sec, mem.min as u64);
                }
                Some(max) => {
                    sec.push(0x03);
                    uleb(&mut sec, mem.min as u64);
                    uleb(&mut sec, max as u64);
                }
            }
        }
        for (m, f, ty) in &self.func_imports {
            name(&mut sec, m);
            name(&mut sec, f);
            sec.push(0x00);
            uleb(&mut sec, *ty as u64);
        }
        section(&mut out, 2, &sec);

        sec.clear();
        uleb(&mut sec, self.funcs.len() as u64);
        for (ty, _) in &self.funcs {
            uleb(&mut sec, *ty as u64);
        }
        section(&mut out, 3, &sec);

        sec.clear();
        uleb(&mut sec, self.exports.len() as u64);
        for (n, i) in &self.exports {
            name(&mut sec, n);
            sec.push(0x00);
            uleb(&mut sec, *i as u64);
        }
        section(&mut out, 7, &sec);

        sec.clear();
        uleb(&mut sec, self.funcs.len() as u64);
        for (_, f) in &self.funcs {
            f.encode(&mut sec);
        }
        section(&mut out, 10, &sec);
        out
    }
}

fn section(out: &mut Vec<u8>, id: u8, body: &[u8]) {
    out.push(id);
    uleb(out, body.len() as u64);
    out.extend_from_slice(body);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validate(bytes: &[u8]) {
        let mut v = wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::default());
        v.validate_all(bytes).expect("modulo WASM non valido");
    }

    #[test]
    fn leb128() {
        let mut o = Vec::new();
        uleb(&mut o, 624485);
        assert_eq!(o, [0xe5, 0x8e, 0x26]);
        o.clear();
        sleb(&mut o, -123456);
        assert_eq!(o, [0xc0, 0xbb, 0x78]);
        o.clear();
        sleb(&mut o, 63);
        assert_eq!(o, [0x3f]);
        o.clear();
        sleb(&mut o, 64);
        assert_eq!(o, [0xc0, 0x00]);
        o.clear();
        sleb(&mut o, i64::MIN);
        assert_eq!(o.len(), 10);
    }

    #[test]
    fn empty_module_is_valid() {
        validate(&Module::new().encode());
    }

    #[test]
    fn module_with_imports_and_exports_is_valid() {
        use ValType::*;
        let mut m = Module::new();
        let t_blk = m.ty(&[I32], &[I32]);
        let t_ld = m.ty(&[I32, I64, I32], &[I64]);
        let t_st = m.ty(&[I32, I64, I32, I64], &[I32]);
        assert_eq!(m.ty(&[I32], &[I32]), t_blk);
        m.import_memory("env", "mem", MemoryImport { min: 1, shared_max: None });
        let ld = m.import_func("env", "ld", t_ld);
        let st = m.import_func("env", "st", t_st);
        let mut f = Func { locals: vec![(2, I64), (1, I32)], ..Default::default() };
        f.local_get(0).i64_const(0x1234).i32_const(8).call(ld).local_set(1);
        f.local_get(0).local_get(1).i64_store(8);
        f.local_get(0).i64_const(-1).i32_const(4).local_get(1).call(st);
        f.if_(ValType::I32 as u8).i32_const(2).else_().i32_const(0).end();
        f.op(op::RETURN);
        let b = m.func(t_blk, f);
        assert_eq!(b, 2);
        m.export_func("b0", b);
        validate(&m.encode());
    }

    #[test]
    fn table_import_and_call_indirect_are_valid() {
        use ValType::*;
        let mut m = Module::new();
        let t = m.ty(&[I32], &[I32]);
        m.import_memory("env", "mem", MemoryImport { min: 1, shared_max: None });
        m.import_table("env", "tbl", 1 << 18);
        let mut f = Func::default();
        f.loop_(BLOCK_EMPTY).local_get(0).local_get(0).call_indirect(t).br_if(0).end();
        f.local_get(0).i64_load_n(1, 3).i64_const(0).op(op::I64_GT_U).op(op::DROP);
        f.local_get(0).i64_const(7).i64_store_n(2, 0);
        f.i32_const(0);
        m.func(t, f);
        validate(&m.encode());
    }

    #[test]
    fn shared_memory_import_is_valid() {
        let mut m = Module::new();
        m.import_memory("env", "mem", MemoryImport { min: 1, shared_max: Some(16) });
        validate(&m.encode());
    }

    #[test]
    fn invalid_body_is_rejected() {
        // Controllo del controllo: un corpo che lascia il tipo sbagliato
        // sullo stack deve essere rifiutato dal validatore.
        let mut m = Module::new();
        let t = m.ty(&[ValType::I32], &[ValType::I32]);
        let mut f = Func::default();
        f.i64_const(1);
        m.func(t, f);
        let mut v = wasmparser::Validator::new();
        assert!(v.validate_all(&m.encode()).is_err());
    }
}
