//! Salvataggio e ripristino dello stato della macchina (M6, ADR 0015).
//!
//! Questo crate non conosce la macchina: dà il formato del file e gli
//! strumenti per scriverlo e leggerlo, senza dipendenze (compila anche per
//! wasm32). Ogni crate implementa [`Snapshot`] per il proprio stato; è
//! `vetro-machine` a mettere insieme le sezioni (`Machine::save`).
//!
//! - [`Writer`] / [`Reader`]: interi little endian a larghezza fissa,
//!   booleani, byte con lunghezza, sezioni con etichetta e lunghezza;
//! - [`compress`]: dati grandi (RAM, immagini della GPU, dischi in memoria)
//!   a blocchi da 4 KiB, i blocchi a zero omessi, gli altri compressi con un
//!   LZ semplice ([`lz`]);
//! - [`encode_file`] / [`decode_file`]: intestazione (magia, versione del
//!   formato, hash della configurazione, lunghezza, somma di controllo) e
//!   contenuto.
//!
//! Determinismo: la stessa macchina nello stesso stato dà gli stessi byte.
//! Niente tabelle hash, niente orologi, niente puntatori nel formato.
//! Specifica in `docs/specs/snapshot.md`.

pub mod lz;

use core::fmt;

/// Primi 8 byte di ogni snapshot.
pub const MAGIC: [u8; 8] = *b"VETROSNP";

/// Versione del formato. Cambia a ogni modifica di ciò che si scrive (campi
/// nuovi, ordine, codifiche): uno snapshot di un'altra versione si rifiuta
/// con [`Error::Version`], senza tentare conversioni.
pub const FORMAT_VERSION: u32 = 2;

/// Byte dell'intestazione: magia, versione, hash della configurazione,
/// lunghezza del contenuto, somma di controllo del contenuto.
pub const HEADER_LEN: usize = 8 + 4 + 8 + 8 + 8;

/// Dimensione dei blocchi di [`compress`] (una pagina del guest).
pub const BLOCK: usize = 4096;

/// Perché uno snapshot non si può leggere o applicare.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Non comincia con [`MAGIC`]: non è uno snapshot di Vetro.
    BadMagic,
    /// Formato di un'altra versione di Vetro.
    Version { found: u32, expected: u32 },
    /// Snapshot di una macchina configurata in un altro modo (RAM,
    /// dispositivi, seme, ...).
    Config { found: u64, expected: u64 },
    /// Il contenuto non corrisponde alla somma di controllo (file rovinato).
    Checksum,
    /// Finito prima del previsto.
    Truncated,
    /// Sezione attesa e sezione trovata.
    Section { expected: [u8; 4], found: [u8; 4] },
    /// Byte avanzati alla fine di una sezione.
    Trailing { section: [u8; 4], bytes: usize },
    /// Un valore che lo stato non può avere.
    Invalid(String),
}

impl Error {
    /// Errore per un valore fuori dal dominio.
    pub fn invalid(what: impl Into<String>) -> Self {
        Error::Invalid(what.into())
    }
}

fn tag_str(t: &[u8; 4]) -> String {
    String::from_utf8_lossy(t).into_owned()
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BadMagic => write!(f, "non è uno snapshot di Vetro (intestazione sconosciuta)"),
            Error::Version { found, expected } => write!(
                f,
                "snapshot nel formato versione {found}, questa versione di Vetro legge solo la {expected}: \
                 va rifatto con questa versione"
            ),
            Error::Config { found, expected } => write!(
                f,
                "snapshot di una macchina configurata diversamente (hash {found:016x}, questa macchina \
                 {expected:016x}): servono la stessa RAM, gli stessi dispositivi e lo stesso seme"
            ),
            Error::Checksum => write!(f, "snapshot rovinato (somma di controllo sbagliata)"),
            Error::Truncated => write!(f, "snapshot troncato"),
            Error::Section { expected, found } => {
                write!(f, "sezione {:?} al posto di {:?}", tag_str(found), tag_str(expected))
            }
            Error::Trailing { section, bytes } => {
                write!(f, "{bytes} byte in più alla fine della sezione {:?}", tag_str(section))
            }
            Error::Invalid(what) => write!(f, "valore non valido nello snapshot: {what}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = core::result::Result<T, Error>;

/// Stato che si salva e si ripristina.
///
/// `restore` parte da un oggetto costruito con la stessa configurazione
/// (quella che non si salva: dimensioni, backend esterni) e lo porta nello
/// stato salvato. Dopo `restore`, `save` deve dare gli stessi byte letti.
pub trait Snapshot {
    fn save(&self, w: &mut Writer);
    fn restore(&mut self, r: &mut Reader<'_>) -> Result<()>;
}

// ---- Scrittura --------------------------------------------------------------

/// Buffer in scrittura: tutto little endian, a larghezza fissa.
#[derive(Clone, Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(n: usize) -> Self {
        Writer { buf: Vec::with_capacity(n) }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u128(&mut self, v: u128) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn bool(&mut self, v: bool) {
        self.buf.push(v as u8);
    }
    /// Una lunghezza o un numero di elementi (u64).
    pub fn len_of(&mut self, n: usize) {
        self.u64(n as u64);
    }
    /// Byte senza lunghezza (chi legge sa quanti sono).
    pub fn raw(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }
    /// Byte con la lunghezza davanti.
    pub fn bytes(&mut self, b: &[u8]) {
        self.len_of(b.len());
        self.raw(b);
    }
    pub fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
    /// `None` = 0; `Some` = 1 seguito dal valore.
    pub fn opt<T>(&mut self, v: Option<T>, f: impl FnOnce(&mut Self, T)) {
        match v {
            None => self.u8(0),
            Some(x) => {
                self.u8(1);
                f(self, x);
            }
        }
    }
    pub fn opt_u64(&mut self, v: Option<u64>) {
        self.opt(v, Self::u64);
    }
    /// Una sequenza: numero di elementi, poi `f` per ognuno.
    pub fn seq<I: IntoIterator>(&mut self, items: I, mut f: impl FnMut(&mut Self, I::Item))
    where
        I::IntoIter: ExactSizeIterator,
    {
        let it = items.into_iter();
        self.len_of(it.len());
        for x in it {
            f(self, x);
        }
    }
    /// Sezione: etichetta di 4 byte, lunghezza (u64), contenuto scritto da `f`.
    pub fn section(&mut self, tag: &[u8; 4], f: impl FnOnce(&mut Self)) {
        self.raw(tag);
        let at = self.buf.len();
        self.u64(0);
        f(self);
        let len = (self.buf.len() - at - 8) as u64;
        self.buf[at..at + 8].copy_from_slice(&len.to_le_bytes());
    }
    /// Uno stato che implementa [`Snapshot`].
    pub fn put<S: Snapshot + ?Sized>(&mut self, s: &S) {
        s.save(self);
    }
}

// ---- Lettura ----------------------------------------------------------------

/// Buffer in lettura; ogni lettura oltre la fine è [`Error::Truncated`].
#[derive(Clone, Debug)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    /// Etichetta della sezione (per i messaggi d'errore).
    tag: [u8; 4],
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0, tag: *b"    " }
    }

    /// Byte non ancora letti.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn raw(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(Error::Truncated);
        }
        let b = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(b)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.raw(N)?.try_into().expect("lunghezza controllata"))
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.raw(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16> {
        self.array().map(u16::from_le_bytes)
    }
    pub fn u32(&mut self) -> Result<u32> {
        self.array().map(u32::from_le_bytes)
    }
    pub fn u64(&mut self) -> Result<u64> {
        self.array().map(u64::from_le_bytes)
    }
    pub fn u128(&mut self) -> Result<u128> {
        self.array().map(u128::from_le_bytes)
    }
    /// Solo 0 o 1.
    pub fn bool(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            v => Err(Error::invalid(format!("booleano {v}"))),
        }
    }
    /// Una lunghezza, che non può superare i byte rimasti diviso `min_item`
    /// (ogni elemento occupa almeno `min_item` byte): un file rovinato non
    /// fa allocare memoria a caso.
    pub fn len_of(&mut self, min_item: usize) -> Result<usize> {
        let n = self.u64()?;
        let max = (self.remaining() / min_item.max(1)) as u64;
        if n > max {
            return Err(Error::Truncated);
        }
        Ok(n as usize)
    }
    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.len_of(1)?;
        self.raw(n)
    }
    pub fn vec(&mut self) -> Result<Vec<u8>> {
        self.bytes().map(<[u8]>::to_vec)
    }
    pub fn string(&mut self) -> Result<String> {
        String::from_utf8(self.vec()?).map_err(|_| Error::invalid("stringa non UTF-8"))
    }
    pub fn opt<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<Option<T>> {
        match self.u8()? {
            0 => Ok(None),
            1 => f(self).map(Some),
            v => Err(Error::invalid(format!("opzione {v}"))),
        }
    }
    pub fn opt_u64(&mut self) -> Result<Option<u64>> {
        self.opt(Self::u64)
    }
    /// Una sequenza scritta con [`Writer::seq`]; ogni elemento occupa almeno
    /// `min_item` byte.
    pub fn seq<T>(&mut self, min_item: usize, mut f: impl FnMut(&mut Self) -> Result<T>) -> Result<Vec<T>> {
        let n = self.len_of(min_item)?;
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push(f(self)?);
        }
        Ok(v)
    }
    /// La prossima sezione, che deve avere etichetta `tag`: il lettore del
    /// suo contenuto. Va chiusa con [`Reader::finish`].
    pub fn section(&mut self, tag: &[u8; 4]) -> Result<Reader<'a>> {
        let found: [u8; 4] = self.array()?;
        if &found != tag {
            return Err(Error::Section { expected: *tag, found });
        }
        let n = self.u64()?;
        if n > self.remaining() as u64 {
            return Err(Error::Truncated);
        }
        let body = self.raw(n as usize)?;
        Ok(Reader { buf: body, pos: 0, tag: *tag })
    }
    /// Etichetta della prossima sezione, senza consumarla.
    pub fn peek_tag(&self) -> Option<[u8; 4]> {
        self.buf.get(self.pos..self.pos + 4).map(|t| t.try_into().expect("4 byte"))
    }
    /// Controlla che la sezione sia stata letta tutta.
    pub fn finish(&self) -> Result<()> {
        match self.remaining() {
            0 => Ok(()),
            bytes => Err(Error::Trailing { section: self.tag, bytes }),
        }
    }
    /// Ripristina uno stato che implementa [`Snapshot`].
    pub fn get<S: Snapshot + ?Sized>(&mut self, s: &mut S) -> Result<()> {
        s.restore(self)
    }
    /// Controlla che un valore di configurazione salvato sia quello atteso.
    pub fn expect_u64(&mut self, what: &str, expected: u64) -> Result<()> {
        let v = self.u64()?;
        if v != expected {
            return Err(Error::invalid(format!("{what}: {v} nello snapshot, {expected} in questa macchina")));
        }
        Ok(())
    }
}

// ---- Somma di controllo e hash ---------------------------------------------

/// Hash a 64 bit non crittografico (FNV-1a su parole da 8 byte con
/// rimescolamento finale): somma di controllo del contenuto e hash della
/// configurazione. Stabile fra piattaforme e versioni del compilatore.
pub fn hash64(data: &[u8]) -> u64 {
    const P: u64 = 0x0000_0100_0000_01b3;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ data.len() as u64;
    let (words, rest) = data.as_chunks::<8>();
    for c in words {
        h = (h ^ u64::from_le_bytes(*c)).wrapping_mul(P).rotate_left(23);
    }
    for &b in rest {
        h = (h ^ u64::from(b)).wrapping_mul(P);
    }
    // Rimescolamento di SplitMix64.
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

// ---- File -------------------------------------------------------------------

/// Snapshot completo: intestazione e `payload` (le sezioni).
pub fn encode_file(config_hash: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&config_hash.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    out.extend_from_slice(&hash64(payload).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Intestazione letta da [`decode_file`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub version: u32,
    pub config_hash: u64,
}

/// Legge l'intestazione e controlla magia, versione, lunghezza e somma di
/// controllo; restituisce il contenuto. La configurazione la controlla chi
/// chiama (confronto di `config_hash`).
pub fn decode_file(bytes: &[u8]) -> Result<(Header, &[u8])> {
    if bytes.len() < 8 || bytes[..8] != MAGIC {
        return Err(Error::BadMagic);
    }
    // La versione prima di tutto il resto: un formato diverso può avere
    // un'intestazione diversa.
    let mut r = Reader::new(&bytes[8..]);
    let version = r.u32()?;
    if version != FORMAT_VERSION {
        return Err(Error::Version { found: version, expected: FORMAT_VERSION });
    }
    let config_hash = r.u64()?;
    let len = r.u64()?;
    let sum = r.u64()?;
    if len != r.remaining() as u64 {
        return Err(Error::Truncated);
    }
    let payload = r.raw(len as usize)?;
    if hash64(payload) != sum {
        return Err(Error::Checksum);
    }
    Ok((Header { version, config_hash }, payload))
}

// ---- Dati grandi a blocchi --------------------------------------------------

/// Scrive `data` a blocchi da [`BLOCK`] byte: la lunghezza, poi per ogni
/// blocco non tutto a zero il suo indice, la codifica (0 = byte crudi, 1 =
/// [`lz`]) e il contenuto. I blocchi a zero non occupano nulla.
pub fn compress(w: &mut Writer, data: &[u8]) {
    w.len_of(data.len());
    let at = w.len();
    w.u64(0);
    let mut count = 0u64;
    let mut scratch = Vec::with_capacity(BLOCK + BLOCK / 8);
    let mut table = lz::Table::new();
    for (i, block) in data.chunks(BLOCK).enumerate() {
        if is_zero(block) {
            continue;
        }
        count += 1;
        w.u64(i as u64);
        scratch.clear();
        lz::compress(block, &mut scratch, &mut table);
        if scratch.len() < block.len() {
            w.u8(1);
            w.u32(scratch.len() as u32);
            w.raw(&scratch);
        } else {
            w.u8(0);
            w.raw(block);
        }
    }
    w.buf[at..at + 8].copy_from_slice(&count.to_le_bytes());
}

/// Legge dati scritti con [`compress`] in `out`, che deve avere la stessa
/// lunghezza; `out` va passato già a zero (i blocchi a zero non si
/// scrivono). `visit(indice)` si chiama per ogni blocco scritto.
pub fn decompress_into(r: &mut Reader<'_>, out: &mut [u8], mut visit: impl FnMut(usize)) -> Result<()> {
    let len = r.u64()?;
    if len != out.len() as u64 {
        return Err(Error::invalid(format!("dati di {len} byte, attesi {}", out.len())));
    }
    let count = r.u64()?;
    let blocks = out.len().div_ceil(BLOCK) as u64;
    if count > blocks {
        return Err(Error::invalid("più blocchi dei dati"));
    }
    let mut last: Option<u64> = None;
    for _ in 0..count {
        let i = r.u64()?;
        if i >= blocks || last.is_some_and(|l| i <= l) {
            return Err(Error::invalid(format!("blocco {i} fuori posto")));
        }
        last = Some(i);
        let start = i as usize * BLOCK;
        let dst = &mut out[start..(start + BLOCK).min(len as usize)];
        match r.u8()? {
            0 => dst.copy_from_slice(r.raw(dst.len())?),
            1 => {
                let n = r.u32()? as usize;
                lz::decompress(r.raw(n)?, dst)?;
            }
            v => return Err(Error::invalid(format!("codifica di blocco {v}"))),
        }
        visit(i as usize);
    }
    Ok(())
}

/// Come [`decompress_into`], in un vettore nuovo.
pub fn decompress(r: &mut Reader<'_>) -> Result<Vec<u8>> {
    let len = r.clone().u64()?;
    // Limite contro i file rovinati (la somma di controllo li ferma già).
    if len > 1 << 40 {
        return Err(Error::invalid(format!("dati di {len} byte")));
    }
    let mut out = vec![0u8; len as usize];
    decompress_into(r, &mut out, |_| {})?;
    Ok(out)
}

/// Vero se `b` è tutto a zero.
pub fn is_zero(b: &[u8]) -> bool {
    let (words, rest) = b.as_chunks::<8>();
    let acc = words.iter().fold(0u64, |a, c| a | u64::from_le_bytes(*c));
    acc == 0 && rest.iter().all(|&x| x == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interi_sezioni_e_opzioni() {
        let mut w = Writer::new();
        w.section(b"PROV", |w| {
            w.u8(7);
            w.u16(0x1234);
            w.u32(0xdead_beef);
            w.u64(u64::MAX);
            w.u128(1 << 100);
            w.bool(true);
            w.opt_u64(None);
            w.opt_u64(Some(5));
            w.str("città");
            w.seq([1u32, 2, 3], |w, x| w.u32(x));
        });
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        let mut s = r.section(b"PROV").unwrap();
        assert_eq!(s.u8().unwrap(), 7);
        assert_eq!(s.u16().unwrap(), 0x1234);
        assert_eq!(s.u32().unwrap(), 0xdead_beef);
        assert_eq!(s.u64().unwrap(), u64::MAX);
        assert_eq!(s.u128().unwrap(), 1 << 100);
        assert!(s.bool().unwrap());
        assert_eq!(s.opt_u64().unwrap(), None);
        assert_eq!(s.opt_u64().unwrap(), Some(5));
        assert_eq!(s.string().unwrap(), "città");
        assert_eq!(s.seq(4, |r| r.u32()).unwrap(), [1, 2, 3]);
        s.finish().unwrap();
        r.finish().unwrap();
        assert_eq!(
            Reader::new(&bytes).section(b"ALTR").unwrap_err(),
            Error::Section { expected: *b"ALTR", found: *b"PROV" }
        );
    }

    #[test]
    fn letture_oltre_la_fine_e_valori_invalidi() {
        let mut r = Reader::new(&[1, 2]);
        assert_eq!(r.u32(), Err(Error::Truncated));
        let mut r = Reader::new(&[2]);
        assert!(matches!(r.bool(), Err(Error::Invalid(_))));
        // Una lunghezza enorme non alloca: si rifiuta.
        let mut w = Writer::new();
        w.u64(u64::MAX / 2);
        let b = w.into_bytes();
        assert_eq!(Reader::new(&b).bytes(), Err(Error::Truncated));
        let mut w = Writer::new();
        w.section(b"ABCD", |w| w.u8(1));
        let b = w.into_bytes();
        let s = Reader::new(&b).section(b"ABCD").unwrap();
        assert_eq!(s.finish(), Err(Error::Trailing { section: *b"ABCD", bytes: 1 }));
    }

    #[test]
    fn file_con_versione_configurazione_e_somma() {
        let f = encode_file(42, b"contenuto");
        let (h, p) = decode_file(&f).unwrap();
        assert_eq!(h, Header { version: FORMAT_VERSION, config_hash: 42 });
        assert_eq!(p, b"contenuto");

        let mut other = f.clone();
        other[8..12].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        let e = decode_file(&other).unwrap_err();
        assert_eq!(e, Error::Version { found: FORMAT_VERSION + 1, expected: FORMAT_VERSION });
        assert!(e.to_string().contains("versione"), "{e}");

        let mut bad = f.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert_eq!(decode_file(&bad).unwrap_err(), Error::Checksum);
        assert_eq!(decode_file(&f[..f.len() - 1]).unwrap_err(), Error::Truncated);
        assert_eq!(decode_file(b"ELF\x7f....").unwrap_err(), Error::BadMagic);
    }

    const HASH_VETRO: u64 = 0x2e59_3998_1c8e_031d;

    #[test]
    fn hash_stabile() {
        // Valori fissati: l'hash entra nel formato e non deve cambiare.
        assert_eq!(hash64(b""), hash64(b""));
        assert_ne!(hash64(b"a"), hash64(b"b"));
        assert_ne!(hash64(&[0; 8]), hash64(&[0; 9]));
        assert_eq!(hash64(b"vetro-snapshot"), HASH_VETRO, "{:#x}", hash64(b"vetro-snapshot"));
    }

    #[test]
    fn blocchi_a_zero_omessi_e_compressione() {
        let mut data = vec![0u8; 10 * BLOCK + 100];
        data[BLOCK * 3..BLOCK * 3 + 11].copy_from_slice(b"hello world");
        for (i, b) in data[BLOCK * 7..BLOCK * 8].iter_mut().enumerate() {
            *b = ((i * 2_654_435_761usize) >> 7) as u8; // poco comprimibile
        }
        data[10 * BLOCK + 50] = 9; // ultimo blocco corto
        let mut w = Writer::new();
        compress(&mut w, &data);
        let b = w.into_bytes();
        assert!(b.len() < 2 * BLOCK, "{} byte", b.len());
        let mut seen = Vec::new();
        let mut out = vec![0u8; data.len()];
        decompress_into(&mut Reader::new(&b), &mut out, |i| seen.push(i)).unwrap();
        assert_eq!(out, data);
        assert_eq!(seen, [3, 7, 10]);
        assert_eq!(decompress(&mut Reader::new(&b)).unwrap(), data);
        // Lunghezza diversa: errore chiaro.
        let mut short = vec![0u8; 5];
        assert!(matches!(decompress_into(&mut Reader::new(&b), &mut short, |_| {}), Err(Error::Invalid(_))));
    }
}
