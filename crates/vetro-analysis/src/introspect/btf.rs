//! Parser BTF (BPF Type Format) del kernel Linux, senza dipendenze.
//!
//! Il BTF descrive i tipi del kernel (strutture, unioni, typedef, enum):
//! da lì si ricavano gli offset dei campi che servono all'introspezione
//! ([`super::layout`]). Formato: `Documentation/bpf/btf.rst` del kernel.
//! Si legge sia un file `.btf` staccato (`pahole --btf_encode_detached`)
//! sia il blob dentro un `Image` del kernel ([`Btf::find_in`]), che il
//! kernel porta fra `__start_BTF` e `__stop_BTF` con
//! `CONFIG_DEBUG_INFO_BTF`.
//!
//! Nessun panic su byte arbitrari: gli errori sono [`BtfError`].

use std::collections::BTreeMap;

/// Magia dell'intestazione (little endian).
pub const MAGIC: u16 = 0xeb9f;

/// Tipo del BTF (`BTF_KIND_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Void,
    Int,
    Ptr,
    Array,
    Struct,
    Union,
    Enum,
    Fwd,
    Typedef,
    Volatile,
    Const,
    Restrict,
    Func,
    FuncProto,
    Var,
    Datasec,
    Float,
    DeclTag,
    TypeTag,
    Enum64,
}

impl Kind {
    fn from_raw(k: u32) -> Option<Kind> {
        use Kind::*;
        Some(match k {
            1 => Int,
            2 => Ptr,
            3 => Array,
            4 => Struct,
            5 => Union,
            6 => Enum,
            7 => Fwd,
            8 => Typedef,
            9 => Volatile,
            10 => Const,
            11 => Restrict,
            12 => Func,
            13 => FuncProto,
            14 => Var,
            15 => Datasec,
            16 => Float,
            17 => DeclTag,
            18 => TypeTag,
            19 => Enum64,
            _ => return None,
        })
    }
}

/// Errore di lettura del BTF.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BtfError {
    /// Intestazione mancante o con la magia sbagliata.
    Header,
    /// Sezioni fuori dal blob.
    Bounds,
    /// Tipo sconosciuto o troncato (id del tipo).
    Type(u32),
}

impl core::fmt::Display for BtfError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BtfError::Header => write!(f, "intestazione BTF non valida"),
            BtfError::Bounds => write!(f, "sezioni BTF fuori dal blob"),
            BtfError::Type(id) => write!(f, "tipo BTF {id} non valido"),
        }
    }
}

/// Un tipo, con i dati che seguono l'intestazione ancora da leggere.
#[derive(Clone, Copy, Debug)]
struct Ty {
    kind: Kind,
    name_off: u32,
    vlen: u16,
    kflag: bool,
    /// `size` (INT, STRUCT, UNION, ENUM, DATASEC, FLOAT) o `type`.
    size_type: u32,
    /// Posizione dei dati dopo i 12 byte dell'intestazione, nella sezione
    /// dei tipi.
    extra: usize,
}

/// Un campo di una struttura o unione.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Member {
    /// Offset in bit dall'inizio della struttura esterna (anche attraverso
    /// le unioni e strutture anonime).
    pub bit_offset: u64,
    /// Larghezza del campo di bit (0 = campo normale).
    pub bit_size: u32,
    /// Tipo del campo.
    pub ty: u32,
}

impl Member {
    /// Offset in byte (per i campi di bit, del byte che contiene il primo bit).
    pub fn offset(&self) -> u64 {
        self.bit_offset / 8
    }
}

/// I tipi di un BTF.
#[derive(Clone, Debug)]
pub struct Btf {
    types: Vec<u8>,
    strings: Vec<u8>,
    /// Indice: id → tipo (l'id 0 è `void`).
    tys: Vec<Ty>,
    /// Nome → id dei tipi con quel nome.
    names: BTreeMap<String, Vec<u32>>,
}

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

impl Btf {
    /// Legge un blob BTF che inizia a `bytes[0]` (le sezioni seguono
    /// l'intestazione; il resto dei byte si ignora).
    pub fn parse(bytes: &[u8]) -> Result<Btf, BtfError> {
        let (hdr_len, type_off, type_len, str_off, str_len) = Self::header(bytes).ok_or(BtfError::Header)?;
        let base = hdr_len as usize;
        let t0 = base.checked_add(type_off as usize).ok_or(BtfError::Bounds)?;
        let t1 = t0.checked_add(type_len as usize).ok_or(BtfError::Bounds)?;
        let s0 = base.checked_add(str_off as usize).ok_or(BtfError::Bounds)?;
        let s1 = s0.checked_add(str_len as usize).ok_or(BtfError::Bounds)?;
        let types = bytes.get(t0..t1).ok_or(BtfError::Bounds)?.to_vec();
        let strings = bytes.get(s0..s1).ok_or(BtfError::Bounds)?.to_vec();
        let mut tys =
            vec![Ty { kind: Kind::Void, name_off: 0, vlen: 0, kflag: false, size_type: 0, extra: 0 }];
        let mut at = 0usize;
        while at < types.len() {
            let id = tys.len() as u32;
            let name_off = u32_at(&types, at).ok_or(BtfError::Type(id))?;
            let info = u32_at(&types, at + 4).ok_or(BtfError::Type(id))?;
            let size_type = u32_at(&types, at + 8).ok_or(BtfError::Type(id))?;
            let kind = Kind::from_raw(info >> 24 & 0x1f).ok_or(BtfError::Type(id))?;
            let vlen = (info & 0xffff) as u16;
            let extra = at + 12;
            let n = usize::from(vlen);
            let more = match kind {
                Kind::Int | Kind::Var | Kind::DeclTag => 4,
                Kind::Array => 12,
                Kind::Struct | Kind::Union | Kind::Datasec | Kind::Enum64 => 12 * n,
                Kind::Enum | Kind::FuncProto => 8 * n,
                _ => 0,
            };
            if extra + more > types.len() {
                return Err(BtfError::Type(id));
            }
            tys.push(Ty { kind, name_off, vlen, kflag: info >> 31 != 0, size_type, extra });
            at = extra + more;
        }
        let mut btf = Btf { types, strings, tys, names: BTreeMap::new() };
        for id in 1..btf.tys.len() as u32 {
            let name = btf.name(id);
            if !name.is_empty() {
                btf.names.entry(name.to_string()).or_default().push(id);
            }
        }
        Ok(btf)
    }

    fn header(b: &[u8]) -> Option<(u32, u32, u32, u32, u32)> {
        if u16_at(b, 0)? != MAGIC || *b.get(2)? != 1 {
            return None;
        }
        let hdr_len = u32_at(b, 4)?;
        if hdr_len < 24 {
            return None;
        }
        Some((hdr_len, u32_at(b, 8)?, u32_at(b, 12)?, u32_at(b, 16)?, u32_at(b, 20)?))
    }

    /// Cerca il BTF del kernel dentro un `Image` (o qualunque blob): la
    /// prima intestazione valida che si legge per intero con almeno la
    /// struttura `task_struct`. Restituisce anche l'offset nel blob.
    pub fn find_in(image: &[u8]) -> Option<(usize, Btf)> {
        let pat = [0x9f, 0xeb, 0x01, 0x00];
        let mut i = 0;
        while i + 24 <= image.len() {
            let Some(p) = image[i..].windows(4).position(|w| w == pat) else { break };
            let at = i + p;
            if let Ok(btf) = Btf::parse(&image[at..])
                && btf.struct_id("task_struct").is_some()
            {
                return Some((at, btf));
            }
            i = at + 1;
        }
        None
    }

    /// Numero di tipi (id da 1 a `len`).
    pub fn len(&self) -> u32 {
        self.tys.len() as u32 - 1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn ty(&self, id: u32) -> Option<&Ty> {
        self.tys.get(id as usize)
    }

    fn str_at(&self, off: u32) -> &str {
        let s = self.strings.get(off as usize..).unwrap_or(&[]);
        let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
        core::str::from_utf8(&s[..end]).unwrap_or("")
    }

    /// Nome del tipo (vuoto se anonimo).
    pub fn name(&self, id: u32) -> &str {
        self.ty(id).map(|t| self.str_at(t.name_off)).unwrap_or("")
    }

    /// Tipo del BTF.
    pub fn kind(&self, id: u32) -> Option<Kind> {
        self.ty(id).map(|t| t.kind)
    }

    /// Id dei tipi con questo nome.
    pub fn by_name(&self, name: &str) -> &[u32] {
        self.names.get(name).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// La struttura (o unione) completa con questo nome.
    pub fn struct_id(&self, name: &str) -> Option<u32> {
        self.by_name(name)
            .iter()
            .copied()
            .find(|&id| matches!(self.kind(id), Some(Kind::Struct | Kind::Union)))
    }

    /// Salta typedef, const, volatile, restrict e type tag.
    pub fn resolve(&self, mut id: u32) -> u32 {
        for _ in 0..64 {
            match self.ty(id) {
                Some(t)
                    if matches!(
                        t.kind,
                        Kind::Typedef | Kind::Volatile | Kind::Const | Kind::Restrict | Kind::TypeTag
                    ) =>
                {
                    id = t.size_type
                }
                _ => break,
            }
        }
        id
    }

    /// Per un puntatore, il tipo puntato (risolto).
    pub fn pointee(&self, id: u32) -> Option<u32> {
        let t = self.ty(self.resolve(id))?;
        (t.kind == Kind::Ptr).then(|| self.resolve(t.size_type))
    }

    /// Dimensione in byte del tipo.
    pub fn size_of(&self, id: u32) -> Option<u64> {
        self.size_depth(id, 0)
    }

    fn size_depth(&self, id: u32, depth: u32) -> Option<u64> {
        if depth > 16 {
            return None;
        }
        let id = self.resolve(id);
        let t = self.ty(id)?;
        match t.kind {
            Kind::Int
            | Kind::Struct
            | Kind::Union
            | Kind::Enum
            | Kind::Enum64
            | Kind::Float
            | Kind::Datasec => Some(u64::from(t.size_type)),
            Kind::Ptr => Some(8),
            Kind::Array => {
                let (elem, _, n) = self.array(id)?;
                self.size_depth(elem, depth + 1)?.checked_mul(u64::from(n))
            }
            _ => None,
        }
    }

    /// Elemento, tipo dell'indice e numero di elementi di un array.
    pub fn array(&self, id: u32) -> Option<(u32, u32, u32)> {
        let t = self.ty(self.resolve(id))?;
        if t.kind != Kind::Array {
            return None;
        }
        Some((
            u32_at(&self.types, t.extra)?,
            u32_at(&self.types, t.extra + 4)?,
            u32_at(&self.types, t.extra + 8)?,
        ))
    }

    /// Campi diretti di una struttura o unione: (nome, campo).
    pub fn members(&self, id: u32) -> Vec<(&str, Member)> {
        let id = self.resolve(id);
        let Some(t) = self.ty(id) else { return Vec::new() };
        if !matches!(t.kind, Kind::Struct | Kind::Union) {
            return Vec::new();
        }
        (0..usize::from(t.vlen))
            .filter_map(|i| {
                let o = t.extra + 12 * i;
                let name = u32_at(&self.types, o)?;
                let ty = u32_at(&self.types, o + 4)?;
                let off = u32_at(&self.types, o + 8)?;
                let (bit_offset, bit_size) = if t.kflag { (off & 0xff_ffff, off >> 24) } else { (off, 0) };
                Some((self.str_at(name), Member { bit_offset: u64::from(bit_offset), bit_size, ty }))
            })
            .collect()
    }

    /// Un campo per nome, anche dentro unioni e strutture anonime
    /// (come fa il C): offset dall'inizio di `id`.
    pub fn member(&self, id: u32, name: &str) -> Option<Member> {
        self.member_depth(id, name, 0)
    }

    fn member_depth(&self, id: u32, name: &str, depth: u32) -> Option<Member> {
        if depth > 16 {
            return None;
        }
        let ms = self.members(id);
        if let Some((_, m)) = ms.iter().find(|(n, _)| *n == name) {
            return Some(*m);
        }
        for (n, m) in ms {
            if n.is_empty()
                && let Some(inner) = self.member_depth(m.ty, name, depth + 1)
            {
                return Some(Member { bit_offset: m.bit_offset + inner.bit_offset, ..inner });
            }
        }
        None
    }

    /// Offset in byte di un cammino di campi (`"f_path.dentry"`) nella
    /// struttura `name`, con il tipo dell'ultimo campo.
    pub fn field(&self, name: &str, path: &str) -> Option<(u64, u32)> {
        let mut id = self.struct_id(name)?;
        let mut off = 0u64;
        for part in path.split('.') {
            let m = self.member(id, part)?;
            off += m.offset();
            id = m.ty;
        }
        Some((off, id))
    }

    /// Offset in byte di un cammino di campi (vedi [`Btf::field`]).
    pub fn offset_of(&self, name: &str, path: &str) -> Option<u64> {
        self.field(name, path).map(|(o, _)| o)
    }

    /// Dimensione della struttura `name`.
    pub fn struct_size(&self, name: &str) -> Option<u64> {
        self.size_of(self.struct_id(name)?)
    }

    /// Valore di un enumeratore (cerca in tutti gli enum).
    pub fn enum_value(&self, name: &str) -> Option<i64> {
        for t in &self.tys {
            let n = usize::from(t.vlen);
            match t.kind {
                Kind::Enum => {
                    for i in 0..n {
                        let o = t.extra + 8 * i;
                        if self.str_at(u32_at(&self.types, o)?) == name {
                            let v = u32_at(&self.types, o + 4)?;
                            return Some(if t.kflag { i64::from(v as i32) } else { i64::from(v) });
                        }
                    }
                }
                Kind::Enum64 => {
                    for i in 0..n {
                        let o = t.extra + 12 * i;
                        if self.str_at(u32_at(&self.types, o)?) == name {
                            let lo = u64::from(u32_at(&self.types, o + 4)?);
                            let hi = u64::from(u32_at(&self.types, o + 8)?);
                            return Some((hi << 32 | lo) as i64);
                        }
                    }
                }
                _ => {}
            }
        }
        None
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Costruttore di BTF per i test.
    #[derive(Default)]
    pub(crate) struct Builder {
        types: Vec<u8>,
        strings: Vec<u8>,
        next: u32,
    }

    impl Builder {
        pub(crate) fn new() -> Self {
            Builder { types: Vec::new(), strings: vec![0], next: 1 }
        }

        fn s(&mut self, name: &str) -> u32 {
            if name.is_empty() {
                return 0;
            }
            let off = self.strings.len() as u32;
            self.strings.extend_from_slice(name.as_bytes());
            self.strings.push(0);
            off
        }

        fn head(&mut self, name: &str, kind: u32, vlen: u32, kflag: bool, st: u32) -> u32 {
            let n = self.s(name);
            self.types.extend_from_slice(&n.to_le_bytes());
            let info = kind << 24 | vlen | u32::from(kflag) << 31;
            self.types.extend_from_slice(&info.to_le_bytes());
            self.types.extend_from_slice(&st.to_le_bytes());
            let id = self.next;
            self.next += 1;
            id
        }

        pub(crate) fn int(&mut self, name: &str, size: u32) -> u32 {
            let id = self.head(name, 1, 0, false, size);
            self.types.extend_from_slice(&(size * 8).to_le_bytes());
            id
        }

        pub(crate) fn ptr(&mut self, to: u32) -> u32 {
            self.head("", 2, 0, false, to)
        }

        pub(crate) fn typedef(&mut self, name: &str, to: u32) -> u32 {
            self.head(name, 8, 0, false, to)
        }

        pub(crate) fn array(&mut self, elem: u32, index: u32, n: u32) -> u32 {
            let id = self.head("", 3, 0, false, 0);
            for v in [elem, index, n] {
                self.types.extend_from_slice(&v.to_le_bytes());
            }
            id
        }

        /// Struttura (o unione con `union`) con campi (nome, tipo, offset in byte).
        pub(crate) fn record(
            &mut self,
            name: &str,
            union: bool,
            size: u32,
            fields: &[(&str, u32, u32)],
        ) -> u32 {
            let id = self.head(name, if union { 5 } else { 4 }, fields.len() as u32, false, size);
            for &(n, ty, off) in fields {
                let n = self.s(n);
                for v in [n, ty, off * 8] {
                    self.types.extend_from_slice(&v.to_le_bytes());
                }
            }
            id
        }

        pub(crate) fn enumeration(&mut self, name: &str, vals: &[(&str, u32)]) -> u32 {
            let id = self.head(name, 6, vals.len() as u32, false, 4);
            for &(n, v) in vals {
                let n = self.s(n);
                self.types.extend_from_slice(&n.to_le_bytes());
                self.types.extend_from_slice(&v.to_le_bytes());
            }
            id
        }

        pub(crate) fn build(&self) -> Vec<u8> {
            let mut b = Vec::new();
            b.extend_from_slice(&MAGIC.to_le_bytes());
            b.extend_from_slice(&[1, 0]);
            for v in [24u32, 0, self.types.len() as u32, self.types.len() as u32, self.strings.len() as u32] {
                b.extend_from_slice(&v.to_le_bytes());
            }
            b.extend_from_slice(&self.types);
            b.extend_from_slice(&self.strings);
            b
        }
    }

    #[test]
    fn strutture_unioni_anonime_e_typedef() {
        let mut b = Builder::new();
        let int = b.int("int", 4);
        let long = b.int("long unsigned int", 8);
        let pid_t = b.typedef("pid_t", int);
        let arr = b.array(int, int, 16);
        let inner = b.record("", false, 16, &[("vm_start", long, 0), ("vm_end", long, 8)]);
        let un = b.record("", true, 16, &[("", inner, 0), ("rcu", long, 0)]);
        let task = b.record("task_struct", false, 64, &[("pid", pid_t, 4), ("", un, 8), ("comm", arr, 24)]);
        let p = b.ptr(task);
        b.record("rq", false, 8, &[("curr", p, 0)]);
        b.enumeration("maple_type", &[("maple_dense", 0), ("maple_leaf_64", 1)]);
        let bytes = b.build();
        let mut blob = vec![0xaa; 37];
        blob.extend_from_slice(&bytes);
        let (at, btf) = Btf::find_in(&blob).unwrap();
        assert_eq!(at, 37);
        assert_eq!(btf.offset_of("task_struct", "pid"), Some(4));
        assert_eq!(btf.offset_of("task_struct", "vm_end"), Some(16), "dentro unione e struttura anonime");
        assert_eq!(btf.offset_of("task_struct", "rcu"), Some(8));
        assert_eq!(btf.struct_size("task_struct"), Some(64));
        assert_eq!(btf.size_of(arr), Some(64));
        let (_, curr) = btf.field("rq", "curr").unwrap();
        assert_eq!(btf.pointee(curr), Some(task));
        assert_eq!(btf.size_of(pid_t), Some(4));
        assert_eq!(btf.enum_value("maple_leaf_64"), Some(1));
        assert_eq!(btf.offset_of("task_struct", "nope"), None);
    }

    #[test]
    fn byte_arbitrari_senza_panic() {
        let mut b = Builder::new();
        let int = b.int("int", 4);
        b.record("s", false, 4, &[("a", int, 0)]);
        let good = b.build();
        for cut in 0..good.len() {
            let _ = Btf::parse(&good[..cut]);
        }
        let mut x = 0x1234_5678u32;
        for _ in 0..200 {
            let mut v = good.clone();
            for _ in 0..4 {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                let i = 24 + x as usize % (v.len() - 24);
                v[i] ^= (x >> 8) as u8;
            }
            if let Ok(btf) = Btf::parse(&v) {
                let _ = btf.offset_of("s", "a");
                let _ = btf.enum_value("x");
                for id in 0..btf.len() + 2 {
                    let _ = btf.size_of(id);
                    let _ = btf.members(id);
                }
            }
        }
        assert!(matches!(Btf::parse(b"nope"), Err(BtfError::Header)));
    }
}
