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
}

/// Opcode usati dal traduttore (Core spec, sezione 5.4).
pub mod op {
    pub const UNREACHABLE: u8 = 0x00;
    pub const IF: u8 = 0x04;
    pub const ELSE: u8 = 0x05;
    pub const END: u8 = 0x0b;
    pub const RETURN: u8 = 0x0f;
    pub const CALL: u8 = 0x10;
    pub const DROP: u8 = 0x1a;
    pub const SELECT: u8 = 0x1b;
    pub const LOCAL_GET: u8 = 0x20;
    pub const LOCAL_SET: u8 = 0x21;
    pub const LOCAL_TEE: u8 = 0x22;
    pub const I32_LOAD: u8 = 0x28;
    pub const I64_LOAD: u8 = 0x29;
    pub const I32_STORE: u8 = 0x36;
    pub const I64_STORE: u8 = 0x37;
    pub const I32_CONST: u8 = 0x41;
    pub const I64_CONST: u8 = 0x42;

    pub const I32_EQZ: u8 = 0x45;
    pub const I32_EQ: u8 = 0x46;
    pub const I32_NE: u8 = 0x47;
    pub const I64_EQZ: u8 = 0x50;
    pub const I64_EQ: u8 = 0x51;
    pub const I64_NE: u8 = 0x52;
    pub const I64_LT_U: u8 = 0x54;
    pub const I64_LE_U: u8 = 0x58;

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
    pub fn call(&mut self, f: u32) -> &mut Self {
        self.code.push(op::CALL);
        uleb(&mut self.code, f as u64);
        self
    }
    /// `if` con tipo di blocco `bt` (`BLOCK_EMPTY` o un [`ValType`]).
    pub fn if_(&mut self, bt: u8) -> &mut Self {
        self.code.push(op::IF);
        self.code.push(bt);
        self
    }
    pub fn else_(&mut self) -> &mut Self {
        self.op(op::ELSE)
    }
    pub fn end(&mut self) -> &mut Self {
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
        let n = self.func_imports.len() + self.memory.is_some() as usize;
        uleb(&mut sec, n as u64);
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
