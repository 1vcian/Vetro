//! Ciò che sta fuori dalla CPU: RAM, piattaforma e contatore del tempo.
//! Implementa la memoria fisica vista dalla MMU e l'ambiente della CPU
//! ([`CpuEnv`]): timer generico, interfaccia CPU del GIC, linea IRQ.

use core::cell::RefCell;

use vetro_cpu::sys::CpuEnv;
use vetro_cpu::sysreg::EnvReg;
use vetro_jit::SysPhys;
use vetro_mmu::{BusError, PhysMemory};
use vetro_platform::Virt;
use vetro_platform::map;
use vetro_platform::virtio::{GuestRam, RamError, VirtioBlk};

/// I byte della RAM in un blocco contiguo della memoria dell'host.
///
/// Di solito un `Vec<u8>`. Su wasm32 però nessuna allocazione di Rust (e
/// nessuna slice) può superare `isize::MAX` = 2 GiB - 1, mentre la memoria
/// lineare arriva a 4 GiB: una RAM più grande (Android vuole 2–3 GiB, ADR
/// 0028) è una regione presa direttamente con `memory.grow`, fuori
/// dall'allocatore, e si legge e si scrive solo a pezzi piccoli. Contigua in
/// tutti e due i casi: la TLB software del JIT punta dentro
/// ([`SysPhys::ram_region`]).
struct Store {
    ptr: *mut u8,
    len: usize,
    /// Il vettore che possiede i byte (assente per la regione di wasm32).
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    vec: Option<Vec<u8>>,
}

// SAFETY: `Store` possiede i suoi byte (il vettore, o la regione che nessun
// altro usa) come un `Vec<u8>`.
unsafe impl Send for Store {}

impl Store {
    fn new(size: u64) -> Self {
        let len = usize::try_from(size).expect("RAM più grande dello spazio d'indirizzi dell'host");
        if len <= isize::MAX as usize {
            let mut v = vec![0u8; len];
            return Store { ptr: v.as_mut_ptr(), len, vec: Some(v) };
        }
        Self::region(len)
    }

    /// Una regione a zero di `len` byte fuori dall'allocatore (solo wasm32:
    /// altrove `isize::MAX` basta sempre). Le pagine nuove di `memory.grow`
    /// sono a zero per la specifica; una regione liberata da una macchina di
    /// prima si riusa (la memoria lineare non si restituisce) dopo averla
    /// azzerata.
    #[cfg(target_arch = "wasm32")]
    fn region(len: usize) -> Self {
        let mut free = FREE_REGIONS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = free.iter().position(|&(_, l)| l >= len) {
            let (base, _) = free.swap_remove(i);
            let ptr = core::ptr::with_exposed_provenance_mut::<u8>(base);
            // A pezzi: niente scritture più lunghe di isize::MAX.
            let mut at = 0;
            while at < len {
                let n = (len - at).min(1 << 30);
                // SAFETY: la regione è nostra e lunga almeno `len` byte.
                unsafe { core::ptr::write_bytes(ptr.wrapping_add(at), 0, n) };
                at += n;
            }
            return Store { ptr, len, vec: None };
        }
        drop(free);
        let pages = len.div_ceil(65536);
        let old = core::arch::wasm32::memory_grow(0, pages);
        assert!(old != usize::MAX, "memoria lineare esaurita: RAM di {len} byte");
        Store { ptr: core::ptr::with_exposed_provenance_mut(old * 65536), len, vec: None }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn region(len: usize) -> Self {
        unreachable!("RAM di {len} byte oltre isize::MAX su un host a 64 bit")
    }

    /// `[o, o+n)` (già controllato da chi chiama, `n` piccolo).
    #[inline]
    fn get(&self, o: usize, n: usize) -> &[u8] {
        debug_assert!(o + n <= self.len);
        // SAFETY: dentro i byte posseduti; una slice lunga `n` <= isize::MAX.
        unsafe { core::slice::from_raw_parts(self.ptr.wrapping_add(o), n) }
    }

    #[inline]
    fn get_mut(&mut self, o: usize, n: usize) -> &mut [u8] {
        debug_assert!(o + n <= self.len);
        // SAFETY: come `get`, con `&mut self`.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.wrapping_add(o), n) }
    }
}

#[cfg(target_arch = "wasm32")]
impl Drop for Store {
    fn drop(&mut self) {
        if self.vec.is_none() {
            FREE_REGIONS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((self.ptr.expose_provenance(), self.len));
        }
    }
}

/// Regioni di RAM di macchine distrutte, da riusare (wasm32).
#[cfg(target_arch = "wasm32")]
static FREE_REGIONS: std::sync::Mutex<Vec<(usize, usize)>> = std::sync::Mutex::new(Vec::new());

/// Pezzo della RAM per le operazioni su tutta la RAM (hash, confronti).
pub const RAM_CHUNK: usize = 1 << 28;

/// La RAM del guest, da `map::RAM_BASE`.
///
/// Sorveglia le pagine da cui il JIT ha tradotto codice
/// ([`watch_code`](Self::watch_code)): ogni scrittura che passa da qui (CPU,
/// DMA dei dispositivi, caricamento delle immagini) le segna sporche. Per
/// questo i byte si scrivono solo con [`write`](Self::write).
pub struct Ram {
    bytes: Store,
    /// Un bit per pagina da 4 KiB: sorvegliata.
    code: Vec<u64>,
    /// Pagine sorvegliate.
    watched: usize,
    /// Pagine fisiche (`pa >> 12`) sorvegliate e poi scritte.
    dirty: Vec<u64>,
}

impl Ram {
    pub fn new(size: u64) -> Self {
        let pages = size.div_ceil(4096) as usize;
        Ram { bytes: Store::new(size), code: vec![0; pages.div_ceil(64)], watched: 0, dirty: Vec::new() }
    }

    /// Byte di RAM.
    pub fn size(&self) -> u64 {
        self.bytes.len as u64
    }

    /// I byte della RAM in una slice sola (in sola lettura). Sugli host a 64
    /// bit sempre; su wasm32 solo fino a 2 GiB - 1 (oltre, [`chunks`](Self::chunks)).
    pub fn bytes(&self) -> &[u8] {
        assert!(self.bytes.len <= isize::MAX as usize, "RAM oltre isize::MAX: usare Ram::chunks");
        self.bytes.get(0, self.bytes.len)
    }

    /// La RAM a pezzi di [`RAM_CHUNK`] byte (l'ultimo più corto), in ordine:
    /// oltre 2 GiB su wasm32 non esiste una slice di tutta la RAM.
    pub fn chunks(&self) -> impl Iterator<Item = &[u8]> {
        let len = self.bytes.len;
        (0..len.div_ceil(RAM_CHUNK))
            .map(move |i| self.bytes.get(i * RAM_CHUNK, (len - i * RAM_CHUNK).min(RAM_CHUNK)))
    }

    /// [`vetro_snapshot::hash64`] di tutta la RAM, senza una slice di tutta
    /// la RAM (stesso valore: i pezzi sono multipli di 8 byte).
    pub fn hash(&self) -> u64 {
        const P: u64 = 0x0000_0100_0000_01b3;
        let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ self.bytes.len as u64;
        for c in self.chunks() {
            let (words, rest) = c.as_chunks::<8>();
            for w in words {
                h = (h ^ u64::from_le_bytes(*w)).wrapping_mul(P).rotate_left(23);
            }
            for &b in rest {
                h = (h ^ u64::from(b)).wrapping_mul(P);
            }
        }
        h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        h ^ (h >> 31)
    }

    /// Il contenuto della sezione `RAM ` di uno snapshot (formato di
    /// [`vetro_snapshot::compress`]) a pezzi di al più 1 MiB, in ordine:
    /// nessun buffer grande quanto lo snapshot (ADR 0028). Due chiamate danno
    /// gli stessi byte.
    pub fn save_chunks(&self, emit: &mut dyn FnMut(&[u8])) {
        const PAGE: usize = vetro_snapshot::BLOCK;
        const FLUSH: usize = 1 << 20;
        let len = self.bytes.len;
        let page = |i: usize| self.bytes.get(i * PAGE, (len - i * PAGE).min(PAGE));
        let present: Vec<u32> = (0..len.div_ceil(PAGE))
            .filter(|&i| !vetro_snapshot::is_zero(page(i)))
            .map(|i| i as u32)
            .collect();
        let mut out = Vec::with_capacity(FLUSH + 2 * PAGE);
        out.extend_from_slice(&(len as u64).to_le_bytes());
        out.extend_from_slice(&(present.len() as u64).to_le_bytes());
        let mut scratch = Vec::with_capacity(PAGE + PAGE / 8);
        let mut table = vetro_snapshot::lz::Table::new();
        for i in present {
            let block = page(i as usize);
            out.extend_from_slice(&u64::from(i).to_le_bytes());
            scratch.clear();
            vetro_snapshot::lz::compress(block, &mut scratch, &mut table);
            if scratch.len() < block.len() {
                out.push(1);
                out.extend_from_slice(&(scratch.len() as u32).to_le_bytes());
                out.extend_from_slice(&scratch);
            } else {
                out.push(0);
                out.extend_from_slice(block);
            }
            if out.len() >= FLUSH {
                emit(&out);
                out.clear();
            }
        }
        if !out.is_empty() {
            emit(&out);
        }
    }

    /// Vero se le due RAM hanno gli stessi byte.
    pub fn same_bytes(&self, other: &Ram) -> bool {
        self.size() == other.size() && self.chunks().zip(other.chunks()).all(|(a, b)| a == b)
    }

    /// Sorveglia la pagina fisica `page` (`pa >> 12`); falso se non è RAM.
    pub fn watch_code(&mut self, page: u64) -> bool {
        let Some(i) = (page << 12).checked_sub(map::RAM_BASE).map(|o| (o >> 12) as usize) else {
            return false;
        };
        if (i as u64) << 12 >= self.size() {
            return false;
        }
        let (w, b) = (i / 64, 1u64 << (i % 64));
        if self.code[w] & b == 0 {
            self.code[w] |= b;
            self.watched += 1;
        }
        true
    }

    /// Vero se la pagina fisica `page` è sorvegliata.
    pub fn is_watched(&self, page: u64) -> bool {
        match (page << 12).checked_sub(map::RAM_BASE) {
            Some(o) if o < self.size() => {
                let i = (o >> 12) as usize;
                self.code[i / 64] & 1 << (i % 64) != 0
            }
            _ => false,
        }
    }

    /// Aggiunge a `out` le pagine sorvegliate scritte da allora.
    pub fn take_code_dirty(&mut self, out: &mut Vec<u64>) {
        out.append(&mut self.dirty);
    }

    /// Segna sporche (e non più sorvegliate) le pagine di `[o, o+len)`
    /// (offset nella RAM); vero se ce n'era almeno una.
    #[inline]
    fn touch(&mut self, o: usize, len: usize) -> bool {
        if self.watched == 0 || len == 0 {
            return false;
        }
        let mut hit = false;
        for i in o >> 12..=(o + len - 1) >> 12 {
            let (w, b) = (i / 64, 1u64 << (i % 64));
            if self.code[w] & b != 0 {
                self.code[w] &= !b;
                self.watched -= 1;
                self.dirty.push((map::RAM_BASE >> 12) + i as u64);
                hit = true;
            }
        }
        hit
    }

    /// Scrittura che dice anche se ha toccato codice sorvegliato: `None`
    /// fuori dalla RAM.
    pub fn write_watched(&mut self, pa: u64, data: &[u8]) -> Option<bool> {
        let o = self.range(pa, data.len())?;
        self.bytes.get_mut(o, data.len()).copy_from_slice(data);
        Some(self.touch(o, data.len()))
    }
    /// Offset in `bytes` di `[pa, pa+len)`, se tutto dentro la RAM.
    #[inline]
    fn range(&self, pa: u64, len: usize) -> Option<usize> {
        let off = pa.checked_sub(map::RAM_BASE)?;
        (off.checked_add(len as u64)? <= self.bytes.len as u64).then_some(off as usize)
    }

    pub fn read(&self, pa: u64, buf: &mut [u8]) -> bool {
        match self.range(pa, buf.len()) {
            Some(o) => {
                buf.copy_from_slice(self.bytes.get(o, buf.len()));
                true
            }
            None => false,
        }
    }

    pub fn write(&mut self, pa: u64, data: &[u8]) -> bool {
        self.write_watched(pa, data).is_some()
    }
}

// ---- Snapshot (M6, ADR 0015) -------------------------------------------------

/// I byte della RAM a pagine da 4 KiB: le pagine a zero non occupano nulla,
/// le altre sono compresse (`vetro_snapshot::compress`). La sorveglianza
/// delle pagine di codice non è stato del guest: al ripristino ogni pagina
/// sorvegliata risulta scritta, così il JIT scarta i blocchi tradotti dalla
/// RAM di prima.
impl vetro_snapshot::Snapshot for Ram {
    /// Lo stesso formato di [`vetro_snapshot::compress`] sulla RAM intera,
    /// scritto pagina per pagina ([`Ram::save_chunks`]).
    fn save(&self, w: &mut vetro_snapshot::Writer) {
        self.save_chunks(&mut |c| w.raw(c));
    }

    /// Come [`vetro_snapshot::decompress_into`] sulla RAM intera. Le pagine
    /// assenti devono essere a zero: si scrivono solo quelle che non lo sono
    /// già, così una RAM appena allocata resta non toccata (nel browser le
    /// pagine mai scritte non occupano memoria).
    fn restore(&mut self, r: &mut vetro_snapshot::Reader<'_>) -> vetro_snapshot::Result<()> {
        use vetro_snapshot::Error;
        const PAGE: usize = vetro_snapshot::BLOCK;
        let len = self.bytes.len;
        let found = r.u64()?;
        if found != len as u64 {
            return Err(Error::invalid(format!("dati di {found} byte, attesi {len}")));
        }
        let pages = len.div_ceil(PAGE);
        let count = r.u64()?;
        if count > pages as u64 {
            return Err(Error::invalid("più blocchi dei dati"));
        }
        let mut present = vec![0u64; pages.div_ceil(64)];
        let mut last: Option<u64> = None;
        for _ in 0..count {
            let i = r.u64()?;
            if i >= pages as u64 || last.is_some_and(|l| i <= l) {
                return Err(Error::invalid(format!("blocco {i} fuori posto")));
            }
            last = Some(i);
            let i = i as usize;
            let dst = self.bytes.get_mut(i * PAGE, (len - i * PAGE).min(PAGE));
            match r.u8()? {
                0 => dst.copy_from_slice(r.raw(dst.len())?),
                1 => {
                    let n = r.u32()? as usize;
                    vetro_snapshot::lz::decompress(r.raw(n)?, dst)?;
                }
                v => return Err(Error::invalid(format!("codifica di blocco {v}"))),
            }
            present[i / 64] |= 1 << (i % 64);
        }
        for i in 0..pages {
            if present[i / 64] & 1 << (i % 64) == 0 {
                let p = self.bytes.get_mut(i * PAGE, (len - i * PAGE).min(PAGE));
                if !vetro_snapshot::is_zero(p) {
                    p.fill(0);
                }
            }
        }
        self.touch(0, len);
        Ok(())
    }
}

impl GuestRam for Ram {
    fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), RamError> {
        if Ram::read(self, addr, buf) { Ok(()) } else { Err(RamError { addr, len: buf.len() }) }
    }
    fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), RamError> {
        if Ram::write(self, addr, data) { Ok(()) } else { Err(RamError { addr, len: data.len() }) }
    }
}

/// RAM, piattaforma e tempo.
pub struct Board {
    pub ram: Ram,
    pub virt: Virt,
    /// Valore corrente di CNTPCT_EL0.
    pub cntpct: u64,
    /// Qualcosa può aver cambiato il livello di una linea di interrupt
    /// (accesso MMIO, registro del timer): va chiamato `update_irqs`.
    pub(crate) irq_dirty: bool,
    /// Un accesso a uno slot virtio-mmio: il dispositivo va servito.
    pub(crate) virtio_dirty: bool,
    /// Livello della linea IRQ del GIC, se già calcolato: la CPU lo legge
    /// prima di ogni istruzione con PSTATE.I = 0, e `Gic::irq_line` scorre
    /// tutti gli interrupt. Si azzera a ogni operazione che può cambiare lo
    /// stato del GIC (MMIO, ICC_*, `update_irqs`, virtio).
    pub(crate) irq_cache: Option<bool>,
    /// Dopo l'ultimo servizio virtio una richiesta di virtio-blk aspetta
    /// dati dall'host (`BlockError::NotReady`): la macchina non esegue
    /// istruzioni finché non arrivano (`Stop::Blocked`).
    pub(crate) host_wait: bool,
}

impl Board {
    pub fn new(ram_size: u64, now_secs: u64) -> Self {
        Board {
            ram: Ram::new(ram_size),
            virt: Virt::new(now_secs),
            cntpct: 0,
            irq_dirty: true,
            virtio_dirty: false,
            irq_cache: None,
            host_wait: false,
        }
    }

    /// Porta al GIC i livelli di tutte le linee.
    pub fn update_irqs(&mut self) {
        self.virt.update_irqs(self.cntpct);
        self.irq_cache = None;
        self.irq_dirty = false;
    }

    /// Fa lavorare i dispositivi virtio sopra la RAM.
    pub fn service_virtio(&mut self) {
        let Board { ram, virt, .. } = self;
        virt.service_virtio(ram);
        self.host_wait = (0..map::VIRTIO_SLOTS as u32).any(|k| {
            virt.virtio(k).and_then(|t| t.device_as::<VirtioBlk>()).is_some_and(VirtioBlk::has_pending)
        });
        self.irq_cache = None;
        self.virtio_dirty = false;
        self.irq_dirty = true;
    }

    /// Pilota la linea d'ingresso `line` del GPIO PL061: la 3
    /// (`vetro_platform::pl061::POWER_KEY_LINE`) è il tasto di spegnimento
    /// (`gpio-keys`, KEY_POWER). L'interrupt arriva al guest prima della
    /// prossima istruzione. È un ingresso dell'host: l'host passa da
    /// `Machine::gpio_input` (o `Machine::input`), che lo registra per il
    /// replay (M10, ADR 0019); chiamato qui direttamente sfugge al log.
    pub fn gpio_input(&mut self, line: u32, level: bool) {
        self.virt.gpio_mut().set_input(line, level);
        self.irq_cache = None;
        self.irq_dirty = true;
    }

    fn mmio_touched(&mut self, pa: u64) {
        self.irq_cache = None;
        self.irq_dirty = true;
        let virtio_end = map::VIRTIO_BASE + map::VIRTIO_SLOTS * map::VIRTIO_SLOT_SIZE;
        if (map::VIRTIO_BASE..virtio_end).contains(&pa) {
            self.virtio_dirty = true;
        }
    }
}

fn mmio_size(len: usize) -> Option<u8> {
    matches!(len, 1 | 2 | 4 | 8).then_some(len as u8)
}

/// La memoria fisica: RAM, altrimenti il bus MMIO (un accesso da 1, 2, 4 o
/// 8 byte; ciò che non risponde è un decode error).
pub(crate) struct Phys<'a>(pub &'a RefCell<Board>);

impl PhysMemory for Phys<'_> {
    fn read(&mut self, pa: u64, buf: &mut [u8]) -> Result<(), BusError> {
        let mut b = self.0.borrow_mut();
        if b.ram.read(pa, buf) {
            return Ok(());
        }
        let size = mmio_size(buf.len()).ok_or(BusError::Slave)?;
        let v = b.virt.bus.read(pa, size).ok_or(BusError::Decode)?;
        buf.copy_from_slice(&v.to_le_bytes()[..buf.len()]);
        b.mmio_touched(pa);
        Ok(())
    }

    fn write(&mut self, pa: u64, data: &[u8]) -> Result<(), BusError> {
        let mut b = self.0.borrow_mut();
        if b.ram.write(pa, data) {
            return Ok(());
        }
        let size = mmio_size(data.len()).ok_or(BusError::Slave)?;
        let mut v = [0u8; 8];
        v[..data.len()].copy_from_slice(data);
        if !b.virt.bus.write(pa, size, u64::from_le_bytes(v)) {
            return Err(BusError::Decode);
        }
        b.mmio_touched(pa);
        Ok(())
    }
}

/// La memoria fisica per il JIT: la sola RAM, con le pagine di codice
/// sorvegliate.
impl SysPhys for Phys<'_> {
    fn ram_read(&mut self, pa: u64, buf: &mut [u8]) -> bool {
        self.0.borrow().ram.read(pa, buf)
    }
    fn ram_write(&mut self, pa: u64, data: &[u8]) -> Option<bool> {
        self.0.borrow_mut().ram.write_watched(pa, data)
    }
    fn watch_code(&mut self, page: u64) -> bool {
        self.0.borrow_mut().ram.watch_code(page)
    }
    fn is_watched(&self, page: u64) -> bool {
        self.0.borrow().ram.is_watched(page)
    }
    fn take_code_dirty(&mut self, out: &mut Vec<u64>) {
        self.0.borrow_mut().ram.take_code_dirty(out)
    }
    fn ram_region(&mut self) -> Option<(u64, *mut u8, usize)> {
        let b = self.0.borrow();
        Some((map::RAM_BASE, b.ram.bytes.ptr, b.ram.bytes.len))
    }
}

/// L'ambiente della CPU: linea IRQ del GIC, timer generico, ICC_*.
pub(crate) struct Env<'a>(pub &'a RefCell<Board>);

/// INTID "nessun interrupt" dell'interfaccia CPU.
const SPURIOUS: u64 = 1023;

impl CpuEnv for Env<'_> {
    fn irq_line(&mut self) -> bool {
        let mut b = self.0.borrow_mut();
        if let Some(l) = b.irq_cache {
            return l;
        }
        let l = b.virt.irq_line();
        b.irq_cache = Some(l);
        l
    }

    fn read_sysreg(&mut self, reg: EnvReg) -> u64 {
        use EnvReg::*;
        let mut b = self.0.borrow_mut();
        let c = b.cntpct;
        b.irq_cache = None;
        let v = &mut b.virt;
        match reg {
            CntfrqEl0 => u64::from(map::CNTFRQ_HZ),
            CntpctEl0 => c,
            CntvctEl0 => v.timer.cntvct(c),
            CntpTvalEl0 => v.timer.cntp_tval(c),
            CntpCtlEl0 => v.timer.cntp_ctl(c),
            CntpCvalEl0 => v.timer.cntp_cval(),
            CntvTvalEl0 => v.timer.cntv_tval(c),
            CntvCtlEl0 => v.timer.cntv_ctl(c),
            CntvCvalEl0 => v.timer.cntv_cval(),
            IccPmrEl1 => v.gic().read_pmr(),
            IccIar1El1 => v.gic_mut().read_iar1(),
            IccHppir1El1 => v.gic().read_hppir1(),
            IccBpr1El1 => v.gic().read_bpr1(),
            IccRprEl1 => v.gic().read_rpr(),
            IccCtlrEl1 => v.gic().read_ctlr(),
            IccSreEl1 => v.gic().read_sre(),
            IccIgrpen1El1 => v.gic().read_igrpen1(),
            IccAp1r0El1 => v.gic().read_ap1r0(),
            // Il GIC di Vetro ha solo il gruppo 1 (Linux non usa il gruppo 0).
            IccIar0El1 | IccHppir0El1 => SPURIOUS,
            IccBpr0El1 | IccAp0r0El1 | IccIgrpen0El1 => 0,
            // Registri di sola scrittura: la CPU non li legge mai.
            IccEoir0El1 | IccEoir1El1 | IccDirEl1 | IccSgi1rEl1 | IccAsgi1rEl1 | IccSgi0rEl1 => 0,
        }
    }

    fn write_sysreg(&mut self, reg: EnvReg, value: u64) {
        use EnvReg::*;
        let mut b = self.0.borrow_mut();
        let c = b.cntpct;
        b.irq_cache = None;
        b.irq_dirty = true;
        let v = &mut b.virt;
        match reg {
            CntpTvalEl0 => v.timer.set_cntp_tval(c, value),
            CntpCtlEl0 => v.timer.set_cntp_ctl(value),
            CntpCvalEl0 => v.timer.set_cntp_cval(value),
            CntvTvalEl0 => v.timer.set_cntv_tval(c, value),
            CntvCtlEl0 => v.timer.set_cntv_ctl(value),
            CntvCvalEl0 => v.timer.set_cntv_cval(value),
            IccPmrEl1 => v.gic_mut().write_pmr(value),
            IccEoir1El1 => v.gic_mut().write_eoir1(value),
            IccDirEl1 => v.gic_mut().write_dir(value),
            IccSgi1rEl1 | IccAsgi1rEl1 => v.gic_mut().write_sgi1r(value),
            IccBpr1El1 => v.gic_mut().write_bpr1(value),
            IccCtlrEl1 => v.gic_mut().write_ctlr(value),
            IccSreEl1 => v.gic_mut().write_sre(value),
            IccIgrpen1El1 => v.gic_mut().write_igrpen1(value),
            IccAp1r0El1 => v.gic_mut().write_ap1r0(value),
            // Gruppo 0 assente; CNTFRQ/CNTPCT/CNTVCT e le letture pure non
            // arrivano qui (la CPU rifiuta la scrittura).
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vetro_platform::gic::*;
    use vetro_platform::timer::CTL_ENABLE;

    /// La RAM a pezzi (hash, snapshot pagina per pagina) dà gli stessi byte
    /// delle funzioni di `vetro-snapshot` sulla RAM intera: gli snapshot e i
    /// log restano uguali fra l'host e wasm32 con più di 2 GiB.
    #[test]
    fn ram_a_pezzi_come_la_ram_intera() {
        use vetro_snapshot::{Reader, Snapshot, Writer};
        // Più di un pezzo da RAM_CHUNK non serve: la logica dei pezzi è la
        // stessa, e l'ultimo pezzo corto lo prova una lunghezza dispari.
        let size = 3 * 4096 * 64 + 4096 * 3 + 520;
        let mut ram = Ram::new(size as u64);
        for (i, pa) in [0u64, 4096 * 7 + 13, 4096 * 100, size as u64 - 9].into_iter().enumerate() {
            let data: Vec<u8> = (0..9).map(|k| (i * 31 + k * 7 + 1) as u8).collect();
            assert!(ram.write(map::RAM_BASE + pa, &data));
        }
        let incompressible: Vec<u8> =
            (0..4096u32).map(|k| (k.wrapping_mul(2654435761) >> 13) as u8).collect();
        assert!(ram.write(map::RAM_BASE + 4096 * 50, &incompressible));
        assert_eq!(ram.hash(), vetro_snapshot::hash64(ram.bytes()));
        let mut a = Writer::new();
        ram.save(&mut a);
        let mut b = Writer::new();
        vetro_snapshot::compress(&mut b, ram.bytes());
        assert_eq!(a.as_bytes(), b.as_bytes(), "stesso formato di compress");
        // Ripristino sopra una RAM sporca: le pagine assenti tornano a zero.
        let mut other = Ram::new(size as u64);
        assert!(other.write(map::RAM_BASE + 4096 * 9, &[0xaa; 100]));
        other.restore(&mut Reader::new(a.as_bytes())).unwrap();
        assert!(other.same_bytes(&ram));
        assert_eq!(other.hash(), ram.hash());
        // Lunghezza diversa rifiutata.
        let mut small = Ram::new(4096);
        assert!(small.restore(&mut Reader::new(a.as_bytes())).is_err());
    }

    fn board_with_vtimer_enabled() -> RefCell<Board> {
        let b = RefCell::new(Board::new(1 << 20, 0));
        {
            let mut bb = b.borrow_mut();
            let bus = &mut bb.virt.bus;
            bus.write(map::GICR_BASE + GICR_WAKER, 4, 0);
            bus.write(map::GICD_BASE + GICD_CTLR, 4, u64::from(GICD_CTLR_ENABLE_GRP1));
            bus.write(map::GICR_BASE + GICR_SGI_BASE + GICR_IGROUPR0, 4, 0xFFFF_FFFF);
            bus.write(map::GICR_BASE + GICR_SGI_BASE + GICR_ISENABLER0, 4, 1 << map::PPI_VTIMER);
        }
        let mut env = Env(&b);
        env.write_sysreg(EnvReg::IccPmrEl1, 0xF0);
        env.write_sysreg(EnvReg::IccIgrpen1El1, 1);
        b
    }

    /// La linea IRQ in cache segue ogni cambiamento del GIC: timer che scade
    /// (`update_irqs`), acknowledge (lettura di ICC_IAR1), EOI (scrittura) e
    /// accessi MMIO. Senza gli azzeramenti la CPU vedrebbe il livello vecchio.
    #[test]
    fn linea_irq_in_cache_segue_il_gic() {
        let b = board_with_vtimer_enabled();
        let mut env = Env(&b);
        b.borrow_mut().update_irqs();
        assert!(!env.irq_line());
        assert_eq!(b.borrow().irq_cache, Some(false), "il livello resta in cache");

        env.write_sysreg(EnvReg::CntvCvalEl0, 100);
        env.write_sysreg(EnvReg::CntvCtlEl0, CTL_ENABLE);
        b.borrow_mut().cntpct = 100;
        b.borrow_mut().update_irqs();
        assert!(env.irq_line(), "il timer scaduto alza la linea");

        assert_eq!(env.read_sysreg(EnvReg::IccIar1El1), u64::from(map::PPI_VTIMER));
        assert!(!env.irq_line(), "dopo l'acknowledge l'interrupt è attivo, non più in attesa");

        env.write_sysreg(EnvReg::CntvCtlEl0, 0);
        b.borrow_mut().update_irqs();
        env.write_sysreg(EnvReg::IccEoir1El1, u64::from(map::PPI_VTIMER));
        assert!(!env.irq_line());

        // Un accesso MMIO al GIC azzera la cache: un SGI di nuovo pendente.
        let mut phys = Phys(&b);
        phys.write(map::GICR_BASE + GICR_SGI_BASE + GICR_ISENABLER0, &1u32.to_le_bytes()).unwrap();
        phys.write(map::GICR_BASE + GICR_SGI_BASE + GICR_ISPENDR0, &1u32.to_le_bytes()).unwrap();
        assert!(env.irq_line(), "SGI 0 abilitato e reso pendente via MMIO");
    }

    /// Il tasto di spegnimento premuto dall'host: `gpio_input` segna le linee
    /// da aggiornare (il ciclo di `Machine::run` chiama `update_irqs` prima
    /// della prossima istruzione) e l'INTID 39 arriva alla CPU.
    #[test]
    fn tasto_di_spegnimento_dall_host() {
        use vetro_platform::pl061;
        let b = board_with_vtimer_enabled();
        let intid = map::SPI_BASE + map::GPIO_SPI;
        {
            let mut bb = b.borrow_mut();
            let bus = &mut bb.virt.bus;
            bus.write(map::GICD_BASE + GICD_IGROUPR + 4, 4, 0xFFFF_FFFF);
            bus.write(map::GICD_BASE + GICD_ISENABLER + 4, 4, 1 << (intid % 32));
            let m = 1u64 << pl061::POWER_KEY_LINE;
            bus.write(map::GPIO_BASE + pl061::IBE, 1, m);
            bus.write(map::GPIO_BASE + pl061::IE, 1, m);
            bb.update_irqs();
        }
        let mut env = Env(&b);
        assert!(!env.irq_line());
        b.borrow_mut().gpio_input(pl061::POWER_KEY_LINE, true);
        assert!(b.borrow().irq_dirty, "le linee vanno riportate al GIC");
        b.borrow_mut().update_irqs();
        assert!(env.irq_line());
        assert_eq!(env.read_sysreg(EnvReg::IccIar1El1), u64::from(intid));
    }
}
