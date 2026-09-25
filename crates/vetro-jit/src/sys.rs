//! `SysJit`: il JIT della modalità sistema (ADR 0012, ADR 0013).
//!
//! # Contratto
//! [`SysJit::run`] esegue soltanto blocchi tradotti, al più `budget` passi,
//! e si ferma prima di qualunque cosa debba fare l'interprete: istruzioni
//! non tradotte, fault (compresi gli accessi fuori dalla RAM, cioè MMIO),
//! SVC. Ogni blocco eseguito dà lo stesso risultato che avrebbero dato i
//! suoi passi con `Cpu::step_system`, a patto che:
//! - il chiamante non gli chieda di girare quando l'interprete prenderebbe
//!   un interrupt (IRQ, FIQ o SError non mascherati), con PSTATE.IL o con
//!   un PC non allineato;
//! - `budget` non superi i passi fino al prossimo evento della piattaforma
//!   (scadenza del timer): dentro i blocchi nulla cambia le linee di
//!   interrupt, perché nessun blocco tocca l'MMIO o i registri di sistema.
//!
//! Così l'orologio (il numero di istruzioni) e i punti in cui arrivano gli
//! interrupt sono identici all'interprete.
//!
//! # Blocchi
//! - Un blocco si cerca per (`pc`, EL, TBI0, TBI1) e vale per l'indirizzo
//!   fisico da cui è stato letto: a ogni ingresso dall'host si ritraduce
//!   `pc` con la MMU (per un fetch, con i permessi di EL: la stessa
//!   traduzione dell'interprete, cache delle traduzioni recenti compresa) e
//!   si usa il blocco di quella pagina fisica.
//! - Ogni pagina fisica con blocchi è sorvegliata ([`SysPhys::watch_code`]):
//!   qualunque scrittura (CPU, DMA dei dispositivi, caricamento) la segna
//!   sporca, e i suoi blocchi si scartano prima della corsa successiva. Uno
//!   store di un blocco su una pagina sorvegliata lo fa uscire subito dopo
//!   (`STOP`).
//!
//! # Concatenamento
//! I blocchi stanno in una tabella di funzioni condivisa fra i moduli; il
//! dispatcher (un modulo generato, [`translate::dispatcher`]) passa da un
//! blocco al successivo senza tornare all'host, finché la cache dei salti
//! ([`area::JC`]) ha una voce per il nuovo `pc` con il contesto corrente. Le
//! voci le scrive solo l'host, dopo aver verificato la traduzione del
//! fetch; il contesto (`ctx`) è EL più un'epoca che cambia con i registri
//! di traduzione, con ogni TLBI e con ogni invalidazione di blocchi.
//!
//! # TLB software
//! I blocchi leggono e scrivono la RAM direttamente quando la pagina è
//! nella TLB software ([`area::tlb`]) del loro EL e l'accesso è allineato;
//! altrimenti chiamano `ld`/`st`. L'host riempie la TLB solo dopo un
//! accesso riuscito (stessi permessi, stessa pagina), solo per pagine di
//! RAM che il motore raggiunge ([`Engine::host_address`]) e, per le
//! scritture, solo per pagine senza blocchi. La svuota con i registri di
//! traduzione e con le TLBI, come il TLB della MMU.
//!
//! Il TLB della MMU vede meno accessi che con l'interprete (niente fetch
//! dentro i blocchi, niente accessi dal percorso veloce): il risultato
//! cambia solo per un guest che modifica le tabelle delle pagine senza
//! TLBI, che l'architettura lascia imprevedibile e Linux non fa.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::rc::Rc;

use vetro_cpu::sys::{AccessReq, SysBus, TranslationRegs, sctlr};
use vetro_cpu::{Access, Cpu, decode};
use vetro_mmu::{BusError, Mmu, MmuBus, PhysMemory, tcr};

use crate::engine::{Engine, Host, TABLE_SIZE};
use crate::state::{self, JitState, area, off};
use crate::translate::{self, Block, Kind, MAX_BLOCK, SysTarget, ZVA_BYTES};
use crate::wasm::MemoryImport;
use crate::{FAULT, NEXT, STOP, SVC};

/// La memoria fisica vista dal JIT della modalità sistema.
pub trait SysPhys: PhysMemory {
    /// Lettura dalla sola RAM, senza effetti: falso se `[pa, pa+len)` non è
    /// tutto RAM.
    fn ram_read(&mut self, pa: u64, buf: &mut [u8]) -> bool;
    /// Scrittura nella sola RAM: `None` se non è RAM, altrimenti
    /// `Some(true)` se ha toccato una pagina sorvegliata (ora sporca).
    fn ram_write(&mut self, pa: u64, data: &[u8]) -> Option<bool>;
    /// Sorveglia la pagina fisica `page` (`pa >> 12`): falso se non è RAM.
    fn watch_code(&mut self, page: u64) -> bool;
    /// Vero se la pagina è sorvegliata.
    fn is_watched(&self, page: u64) -> bool;
    /// Aggiunge a `out` le pagine sorvegliate scritte dall'ultima chiamata
    /// (che non sono più sorvegliate).
    fn take_code_dirty(&mut self, out: &mut Vec<u64>);
    /// La RAM come blocco contiguo dell'host: indirizzo fisico d'inizio,
    /// puntatore e lunghezza. Serve alla TLB software dei blocchi.
    fn ram_region(&mut self) -> Option<(u64, *mut u8, usize)> {
        None
    }
}

/// Memoria fisica ridotta alla RAM (per i walk e gli accessi dell'host):
/// tutto il resto è un decode error, che per il JIT vuol dire "lo fa
/// l'interprete".
struct RamOnly<'a>(&'a mut dyn SysPhys);

impl PhysMemory for RamOnly<'_> {
    fn read(&mut self, pa: u64, buf: &mut [u8]) -> Result<(), BusError> {
        if self.0.ram_read(pa, buf) { Ok(()) } else { Err(BusError::Decode) }
    }
    fn write(&mut self, pa: u64, data: &[u8]) -> Result<(), BusError> {
        self.0.ram_write(pa, data).map(|_| ()).ok_or(BusError::Decode)
    }
}

/// Registri di traduzione della CPU (come `Cpu::translation_regs`).
pub fn translation_regs(cpu: &Cpu) -> TranslationRegs {
    let s = &cpu.sys;
    TranslationRegs {
        sctlr: s.sctlr_el1,
        tcr: s.tcr_el1,
        ttbr0: s.ttbr0_el1,
        ttbr1: s.ttbr1_el1,
        mair: s.mair_el1,
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SysJitConfig {
    /// Ingressi (con l'interprete) all'inizio di un blocco prima di
    /// tradurlo.
    pub hot_threshold: u32,
    /// Blocchi da raccogliere prima di compilarli in un solo modulo: fino
    /// ad allora i blocchi caldi restano all'interprete. 1 = subito. Il
    /// modulo si compila prima se i blocchi in attesa vengono richiesti
    /// `batch` volte in tutto (un ciclo caldo non aspetta gli altri).
    pub batch: usize,
    /// Come i moduli importano la memoria.
    pub memory: MemoryImport,
    /// Indirizzo di `JitState` (e dell'area che lo segue, [`area::SIZE`]
    /// byte) nella memoria del motore, allineato a 16.
    pub state_addr: u32,
}

impl Default for SysJitConfig {
    fn default() -> Self {
        SysJitConfig {
            hot_threshold: 16,
            batch: 16,
            memory: MemoryImport { min: 1, shared_max: None },
            state_addr: 16,
        }
    }
}

/// Contatori, per le misure.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SysJitStats {
    /// Passi eseguiti dai blocchi.
    pub jit_steps: u64,
    /// Corse del dispatcher (ingressi dall'host).
    pub runs: u64,
    /// Voci della cache dei salti chieste dal dispatcher all'host.
    pub resolves: u64,
    /// Chiamate a [`SysJit::run`].
    pub calls: u64,
    /// Blocchi e moduli compilati.
    pub blocks: u64,
    pub modules: u64,
    /// Blocchi riusati (stesse istruzioni allo stesso indirizzo) senza
    /// ricompilare.
    pub reused: u64,
    /// Pagine con blocchi invalidate.
    pub invalidated_pages: u64,
    /// Uscite per fault (compresi gli accessi MMIO) e per SVC.
    pub faults: u64,
    pub svcs: u64,
    /// Uscite per store su codice.
    pub stops: u64,
    /// Nuove epoche del concatenamento e svuotamenti della TLB software.
    pub epochs: u64,
    pub tlb_flushes: u64,
    /// Azzeramenti del motore.
    pub resets: u64,
}

/// Esito di [`SysJit::run`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SysRun {
    /// Passi eseguiti dai blocchi.
    pub steps: u64,
    /// Che cosa fare dopo.
    pub next: Next,
}

/// Dopo una corsa del JIT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// Passi esauriti: si può richiamare il JIT.
    Jit,
    /// La prossima istruzione va all'interprete (fault, accesso MMIO, SVC,
    /// istruzione non tradotta, blocco più lungo dei passi rimasti); dopo
    /// si può richiamare il JIT.
    One,
    /// Codice freddo (o in attesa di compilazione): conviene interpretare
    /// fino al prossimo salto prima di richiamare il JIT.
    Cold,
}

/// Hash per chiavi intere (indirizzi, pagine): la ricerca si fa a ogni
/// ingresso dall'host.
#[derive(Default)]
struct U64Hasher(u64);

impl Hasher for U64Hasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(8) ^ b as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    fn write_u64(&mut self, v: u64) {
        let h = (self.0 ^ v).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 = h ^ (h >> 32);
    }
}

type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<U64Hasher>>;

/// Chiave di un blocco: `pc` e i parametri di [`SysTarget`] in un byte.
type Key = (u64, u8);

fn flags(sys: SysTarget) -> u8 {
    sys.el | (sys.tbi0 as u8) << 1 | (sys.tbi1 as u8) << 2 | (sys.spsel as u8) << 3
}

/// I parametri di traduzione per lo stato corrente della CPU.
pub fn target(cpu: &Cpu) -> SysTarget {
    let t = cpu.sys.tcr_el1;
    SysTarget { el: cpu.sys.el, tbi0: t & tcr::TBI0 != 0, tbi1: t & tcr::TBI1 != 0, spsel: cpu.sys.spsel }
}

struct Compiled<M> {
    _module: Rc<M>,
    /// Voce della tabella.
    slot: u32,
    max_steps: u8,
}

/// Un blocco per una pagina fisica: compilato, o `None` se la prima
/// istruzione non si traduce.
struct Variant<M> {
    pa: u64,
    block: Option<Rc<Compiled<M>>>,
}

struct Entry<M> {
    /// Ingressi visti con l'interprete.
    seen: u32,
    variants: Vec<Variant<M>>,
}

impl<M> Default for Entry<M> {
    fn default() -> Self {
        Entry { seen: 0, variants: Vec::new() }
    }
}

/// Un blocco caldo in attesa di compilazione.
struct Pending {
    key: Key,
    pa: u64,
    words: Vec<u32>,
    block: Block,
}

enum Look<M> {
    Hot(Rc<Compiled<M>>),
    Translate(u64),
    One,
    Cold,
}

/// I blocchi conosciuti, separati dal motore: l'host li consulta
/// (`resolve`) mentre il motore esegue il dispatcher.
struct Cache<M> {
    blocks: FastMap<Key, Entry<M>>,
    /// Pagina fisica → chiavi con varianti in quella pagina.
    pages: FastMap<u64, Vec<Key>>,
    /// Moduli compilati per (chiave, istruzioni): lo stesso codice allo
    /// stesso indirizzo virtuale (un'altra pagina fisica, un altro
    /// processo) non si ricompila.
    compiled: HashMap<(Key, Vec<u32>), Rc<Compiled<M>>>,
    pending: Vec<Pending>,
    /// Richieste di blocchi in attesa dall'ultima compilazione.
    pending_hits: usize,
    next_slot: u32,
    epoch: u32,
    hot_threshold: u32,
    /// Indirizzo di `JitState` nella memoria del motore.
    at: usize,
    stats: SysJitStats,
}

impl<M> Cache<M> {
    /// Contesto delle voci della cache dei salti valide adesso per `el`.
    fn ctx(&self, el: u8) -> u32 {
        self.epoch << 1 | el as u32
    }

    /// Cerca il blocco di `pc` per la pagina fisica da cui la CPU lo
    /// leggerebbe adesso (stessa traduzione del fetch dell'interprete) e,
    /// se `count`, conta gli ingressi di quelli non tradotti.
    #[allow(clippy::too_many_arguments)]
    fn lookup(
        &mut self,
        pc: u64,
        fl: u8,
        regs: &TranslationRegs,
        el: u8,
        mmu: &mut Mmu,
        phys: &mut dyn SysPhys,
        count: bool,
    ) -> Look<M> {
        if !pc.is_multiple_of(4) {
            return Look::One;
        }
        let pa = {
            let mut ram = RamOnly(phys);
            let mut bus = MmuBus::new(mmu, &mut ram);
            match bus.translate(regs, pc, AccessReq { access: Access::Fetch, el, aligned: true }) {
                Ok(pa) => pa,
                Err(_) => return Look::One,
            }
        };
        let threshold = self.hot_threshold;
        let e = match self.blocks.get_mut(&(pc, fl)) {
            Some(e) => e,
            None if count => self.blocks.entry((pc, fl)).or_default(),
            None => return Look::Cold,
        };
        if let Some(v) = e.variants.iter().find(|v| v.pa == pa) {
            return match &v.block {
                Some(c) => Look::Hot(c.clone()),
                None => Look::One,
            };
        }
        if count {
            e.seen = e.seen.saturating_add(1);
        }
        if e.seen < threshold { Look::Cold } else { Look::Translate(pa) }
    }

    /// Voce della cache dei salti: `pc` → blocco, valida per `ctx`.
    fn install_jc(&self, mem: &mut [u8], pc: u64, ctx: u32, c: &Compiled<M>) {
        let e = self.at + area::JC as usize + ((pc >> 2) & (area::JC_ENTRIES as u64 - 1)) as usize * 16;
        mem[e..e + 8].copy_from_slice(&pc.to_le_bytes());
        mem[e + 8..e + 12].copy_from_slice(&ctx.to_le_bytes());
        mem[e + 12..e + 16].copy_from_slice(&(c.slot << 8 | c.max_steps as u32).to_le_bytes());
    }

    fn record(&mut self, key: Key, pa: u64, block: Option<Rc<Compiled<M>>>) {
        let e = self.blocks.entry(key).or_default();
        e.variants.retain(|v| v.pa != pa);
        e.variants.push(Variant { pa, block });
        self.pages.entry(pa >> 12).or_default().push(key);
    }
}

/// Il JIT della modalità sistema.
pub struct SysJit<E: Engine> {
    engine: E,
    cfg: SysJitConfig,
    dispatcher: Option<E::Module>,
    cache: Cache<E::Module>,
    regs: Option<TranslationRegs>,
    flushes: u64,
    /// La RAM vista dal motore: (pa d'inizio, indirizzo in `env.mem`, byte).
    ram: Option<(u64, u32, u64)>,
    ram_key: Option<(u64, usize, usize)>,
    dirty: Vec<u64>,
}

/// L'host dei blocchi: accessi attraverso la MMU con i permessi di EL e le
/// regole della modalità sistema; tutto ciò che non è un accesso semplice
/// alla RAM è un fault, e lo rifà l'interprete. Risolve anche le voci
/// mancanti della cache dei salti per il dispatcher.
struct SysHost<'a, M> {
    cache: &'a mut Cache<M>,
    mmu: &'a mut Mmu,
    phys: &'a mut dyn SysPhys,
    regs: TranslationRegs,
    el: u8,
    fl: u8,
    ram: Option<(u64, u32, u64)>,
}

impl<M> SysHost<'_, M> {
    /// Traduzione di un accesso ai dati di `size` byte (1, 2, 4, 8, o
    /// [`ZVA_BYTES`] per DC ZVA).
    fn translate(&mut self, va: u64, size: u32, access: Access) -> Result<u64, ()> {
        let zva = size == ZVA_BYTES;
        let aligned = !zva && va & (size as u64 - 1) == 0;
        // Come `SysMem::access`: big-endian non implementato, allineamento
        // con SCTLR_EL1.A (non per DC ZVA). A cavallo di pagina:
        // all'interprete.
        let big = if self.el == 0 { sctlr::E0E } else { sctlr::EE };
        if self.regs.sctlr & big != 0
            || (!aligned && !zva && self.regs.sctlr & sctlr::A != 0)
            || (va & 0xfff) + size as u64 > 0x1000
        {
            return Err(());
        }
        let mut ram = RamOnly(&mut *self.phys);
        let mut bus = MmuBus::new(&mut *self.mmu, &mut ram);
        bus.translate(&self.regs, va, AccessReq { access, el: self.el, aligned }).map_err(|_| ())
    }

    /// Voce della TLB software per la pagina di `va` (tradotta in `pa`).
    fn fill(&mut self, mem: &mut [u8], va: u64, pa: u64, write: bool) {
        let Some((base, addr, len)) = self.ram else { return };
        let page = pa & !0xfff;
        if page < base || page + 0x1000 > base + len || (write && self.phys.is_watched(pa >> 12)) {
            return;
        }
        let host = addr as u64 + (page - base);
        let vpage = va & !0xfff;
        let e = self.cache.at
            + area::tlb(self.el, write) as usize
            + ((va >> 12) & (area::TLB_ENTRIES as u64 - 1)) as usize * 16;
        mem[e..e + 8].copy_from_slice(&vpage.to_le_bytes());
        mem[e + 8..e + 16].copy_from_slice(&host.wrapping_sub(vpage).to_le_bytes());
    }
}

impl<M> Host for SysHost<'_, M> {
    fn ld(&mut self, mem: &mut [u8], va: u64, size: u32) -> Result<u64, ()> {
        let pa = self.translate(va, size, Access::Read)?;
        let mut b = [0u8; 8];
        if !self.phys.ram_read(pa, &mut b[..size as usize]) {
            return Err(());
        }
        self.fill(mem, va, pa, false);
        Ok(u64::from_le_bytes(b))
    }

    fn st(&mut self, mem: &mut [u8], va: u64, size: u32, value: u64) -> Result<bool, ()> {
        let pa = self.translate(va, size, Access::Write)?;
        if size == ZVA_BYTES {
            return self.phys.ram_write(pa, &[0u8; ZVA_BYTES as usize]).ok_or(());
        }
        match self.phys.ram_write(pa, &value.to_le_bytes()[..size as usize]) {
            None => Err(()),
            Some(true) => Ok(true),
            Some(false) => {
                self.fill(mem, va, pa, true);
                Ok(false)
            }
        }
    }

    /// Voce mancante della cache dei salti per `pc` di `JitState`: se il
    /// blocco c'è già (compilato, per la pagina fisica di adesso), la scrive
    /// col contesto corrente.
    fn resolve(&mut self, mem: &mut [u8]) -> bool {
        let at = self.cache.at;
        let pc = state::read_u64(mem, at, off::PC);
        let ctx = state::read_u32(mem, at, off::CTX);
        self.cache.stats.resolves += 1;
        match self.cache.lookup(pc, self.fl, &self.regs, self.el, self.mmu, self.phys, false) {
            Look::Hot(c) => {
                self.cache.install_jc(mem, pc, ctx, &c);
                true
            }
            _ => false,
        }
    }
}

impl<E: Engine> SysJit<E> {
    pub fn new(mut engine: E, cfg: SysJitConfig) -> Self {
        assert!(cfg.state_addr.is_multiple_of(16), "JitState va allineato a 16 byte");
        assert!(cfg.batch >= 1);
        engine.reserve(cfg.state_addr as usize + area::SIZE as usize);
        let mut j = SysJit {
            engine,
            cfg,
            dispatcher: None,
            cache: Cache {
                blocks: FastMap::default(),
                pages: FastMap::default(),
                compiled: HashMap::new(),
                pending: Vec::new(),
                pending_hits: 0,
                next_slot: 0,
                epoch: 1,
                hot_threshold: cfg.hot_threshold,
                at: cfg.state_addr as usize,
                stats: SysJitStats::default(),
            },
            regs: None,
            flushes: 0,
            ram: None,
            ram_key: None,
            dirty: Vec::new(),
        };
        j.init_area();
        j
    }

    pub fn engine(&mut self) -> &mut E {
        &mut self.engine
    }

    pub fn stats(&self) -> SysJitStats {
        self.cache.stats
    }

    /// Cache dei salti vuota e TLB software vuota.
    fn init_area(&mut self) {
        let at = self.cache.at;
        let m = self.engine.memory();
        m[at..at + area::SIZE as usize].fill(0);
        self.flush_tlb(false);
    }

    /// Svuota la TLB software (solo le tabelle di scrittura se `writes_only`).
    fn flush_tlb(&mut self, writes_only: bool) {
        let at = self.cache.at;
        let m = self.engine.memory();
        for el in 0..2u8 {
            for write in [false, true] {
                if writes_only && !write {
                    continue;
                }
                let t = at + area::tlb(el, write) as usize;
                for i in 0..area::TLB_ENTRIES as usize {
                    m[t + i * 16..t + i * 16 + 8].copy_from_slice(&area::TLB_INVALID.to_le_bytes());
                }
            }
        }
        self.cache.stats.tlb_flushes += 1;
    }

    /// Nuova epoca: le voci della cache dei salti non valgono più.
    fn new_epoch(&mut self) {
        self.cache.epoch += 1;
        self.cache.stats.epochs += 1;
        if self.cache.epoch >= 1 << 30 {
            let at = self.cache.at + area::JC as usize;
            self.engine.memory()[at..at + area::JC_ENTRIES as usize * 16].fill(0);
            self.cache.epoch = 1;
        }
    }

    /// Scarta i blocchi delle pagine scritte.
    fn drain(&mut self, phys: &mut dyn SysPhys) {
        phys.take_code_dirty(&mut self.dirty);
        if self.dirty.is_empty() {
            return;
        }
        let mut any = false;
        let c = &mut self.cache;
        for &page in &self.dirty {
            c.pending.retain(|p| p.pa >> 12 != page);
            if let Some(keys) = c.pages.remove(&page) {
                for k in keys {
                    if let Some(e) = c.blocks.get_mut(&k) {
                        e.variants.retain(|v| v.pa >> 12 != page);
                    }
                }
                c.stats.invalidated_pages += 1;
                any = true;
            }
        }
        self.dirty.clear();
        if any {
            self.new_epoch();
        }
    }

    /// Registri di traduzione cambiati o TLBI: nuova epoca e TLB software
    /// vuota.
    fn sync_regime(&mut self, cpu: &Cpu, mmu: &Mmu) {
        let r = translation_regs(cpu);
        let f = mmu.tlb().flushes();
        if self.regs != Some(r) || self.flushes != f {
            self.regs = Some(r);
            self.flushes = f;
            self.new_epoch();
            self.flush_tlb(false);
        }
    }

    /// La RAM vista dal motore (si ricalcola se cambia).
    fn sync_ram(&mut self, phys: &mut dyn SysPhys) {
        let Some((base, ptr, len)) = phys.ram_region() else {
            self.ram = None;
            return;
        };
        let key = (base, ptr as usize, len);
        if self.ram_key != Some(key) {
            self.ram_key = Some(key);
            self.ram = self.engine.host_address(ptr, len).map(|a| (base, a, len as u64));
        }
    }

    /// Esegue blocchi tradotti per al più `budget` passi (vedi il contratto
    /// del modulo).
    pub fn run(&mut self, cpu: &mut Cpu, mmu: &mut Mmu, phys: &mut dyn SysPhys, budget: u64) -> SysRun {
        self.cache.stats.calls += 1;
        self.drain(phys);
        self.sync_regime(cpu, mmu);
        self.sync_ram(phys);
        let regs = translation_regs(cpu);
        let sys = target(cpu);
        let (el, fl) = (sys.el, flags(sys));
        let at = self.cache.at;
        let mut done = 0u64;
        let mut in_jit = false;
        let mut pc = cpu.pc;
        let next = loop {
            if done >= budget {
                break Next::Jit;
            }
            let c = match self.cache.lookup(pc, fl, &regs, el, mmu, phys, true) {
                Look::Hot(c) => c,
                Look::One => break Next::One,
                Look::Cold => break Next::Cold,
                Look::Translate(pa) => {
                    // La compilazione può azzerare il motore e la sua
                    // memoria: prima lo stato torna nella Cpu.
                    if in_jit {
                        JitState::load(self.engine.memory(), at).to_cpu_sys(cpu);
                        in_jit = false;
                    }
                    match self.install(pc, fl, sys, pa, phys) {
                        Ok(c) => c,
                        Err(n) => break n,
                    }
                }
            };
            if c.max_steps as u64 > budget - done {
                break Next::One;
            }
            if self.dispatcher.is_none() {
                if in_jit {
                    JitState::load(self.engine.memory(), at).to_cpu_sys(cpu);
                    in_jit = false;
                }
                self.compile_dispatcher();
            }
            // L'epoca può essere cambiata (compilazione, azzeramento).
            let ctx = self.cache.ctx(el);
            let m = self.engine.memory();
            self.cache.install_jc(m, pc, ctx, &c);
            if !in_jit {
                let mut s = JitState::from_cpu_sys(cpu);
                s.ctx = ctx;
                s.limit = budget - done;
                s.store(m, at);
                in_jit = true;
            } else {
                state::write_u64(m, at, off::STEPS, 0);
                state::write_u64(m, at, off::LIMIT, budget - done);
                state::write_u32(m, at, off::CTX, ctx);
                state::write_u32(m, at, off::EXIT_DETAIL, 0);
            }
            let code = {
                let mut host = SysHost { cache: &mut self.cache, mmu, phys, regs, el, fl, ram: self.ram };
                let d = self.dispatcher.as_ref().expect("dispatcher compilato");
                self.engine.run(d, 0, self.cfg.state_addr, &mut host)
            };
            self.cache.stats.runs += 1;
            let m = self.engine.memory();
            let steps = state::read_u64(m, at, off::STEPS);
            pc = state::read_u64(m, at, off::PC);
            debug_assert!(steps <= budget - done);
            done += steps;
            self.cache.stats.jit_steps += steps;
            match code {
                NEXT => {}
                STOP => {
                    self.cache.stats.stops += 1;
                    self.drain(phys);
                }
                FAULT => {
                    self.cache.stats.faults += 1;
                    break Next::One;
                }
                SVC => {
                    self.cache.stats.svcs += 1;
                    break Next::One;
                }
                other => panic!("codice d'uscita del dispatcher sconosciuto: {other}"),
            }
        };
        if in_jit {
            JitState::load(self.engine.memory(), at).to_cpu_sys(cpu);
        }
        SysRun { steps: done, next }
    }

    /// Legge e traduce il blocco di `pc` alla pagina fisica di `pa`; lo
    /// compila subito o lo mette fra quelli in attesa (`Err(Cold)`).
    /// `Err(One)`: la prima istruzione non si traduce.
    fn install(
        &mut self,
        pc: u64,
        fl: u8,
        sys: SysTarget,
        pa: u64,
        phys: &mut dyn SysPhys,
    ) -> Result<Rc<Compiled<E::Module>>, Next> {
        let key = (pc, fl);
        if self.cache.pending.iter().any(|p| p.key == key && p.pa == pa) {
            self.cache.pending_hits += 1;
            if self.cache.pending_hits < self.cfg.batch {
                return Err(Next::Cold);
            }
            self.compile_pending();
            return self.compiled_block(key, pa);
        }
        let mut words = Vec::new();
        let mut insns = Vec::new();
        let mut a = pc;
        while insns.len() < MAX_BLOCK && a >> 12 == pc >> 12 {
            let mut w = [0u8; 4];
            if !phys.ram_read(pa + (a - pc), &mut w) {
                break;
            }
            let w = u32::from_le_bytes(w);
            let insn = decode(w);
            match translate::kind_in(&insn, Some(sys)) {
                Kind::Unsupported => break,
                Kind::Svc => {
                    if !insns.is_empty() {
                        words.push(w);
                        insns.push(insn);
                    }
                    break;
                }
                Kind::Branch => {
                    words.push(w);
                    insns.push(insn);
                    break;
                }
                Kind::Linear => {
                    words.push(w);
                    insns.push(insn);
                }
            }
            a = a.wrapping_add(4);
        }
        let page = pa >> 12;
        let was_watched = phys.is_watched(page);
        if insns.is_empty() || !phys.watch_code(page) {
            self.cache.record(key, pa, None);
            return Err(Next::One);
        }
        if !was_watched {
            // Niente scritture dirette su una pagina che ora ha blocchi.
            self.flush_tlb(true);
        }
        if let Some(c) = self.cache.compiled.get(&(key, words.clone())) {
            let c = c.clone();
            self.cache.stats.reused += 1;
            self.cache.record(key, pa, Some(c.clone()));
            return Ok(c);
        }
        let block = Block { pc, insns, sys: Some(sys) };
        self.cache.pending.push(Pending { key, pa, words, block });
        if self.cache.pending.len() < self.cfg.batch {
            // I blocchi in attesa restano sorvegliati: una scrittura li
            // toglie da `pending` (`drain`).
            self.cache.pages.entry(page).or_default().push(key);
            return Err(Next::Cold);
        }
        self.compile_pending();
        self.compiled_block(key, pa)
    }

    fn compiled_block(&self, key: Key, pa: u64) -> Result<Rc<Compiled<E::Module>>, Next> {
        self.cache
            .blocks
            .get(&key)
            .and_then(|e| e.variants.iter().find(|v| v.pa == pa))
            .and_then(|v| v.block.clone())
            .ok_or(Next::Cold)
    }

    /// Compila in un modulo i blocchi in attesa.
    fn compile_pending(&mut self) {
        let n = self.cache.pending.len() as u32;
        self.cache.pending_hits = 0;
        if n == 0 {
            return;
        }
        if self.cache.next_slot + n > TABLE_SIZE {
            self.reset();
        }
        let blocks: Vec<Block> = self.cache.pending.iter().map(|p| p.block.clone()).collect();
        let wasm = translate::module(&blocks, self.cfg.memory);
        let module = match self.engine.compile(&wasm) {
            Ok(m) => m,
            Err(_) => {
                // Motore pieno (wasmtime: istanze per store): si scarta
                // tutto e si riprova una volta.
                self.reset();
                match self.engine.compile(&wasm) {
                    Ok(m) => m,
                    Err(e) => panic!("modulo del JIT rifiutato dal motore: {e}"),
                }
            }
        };
        let base = self.cache.next_slot;
        self.engine.place(&module, n, base);
        self.cache.next_slot += n;
        self.cache.stats.modules += 1;
        let module = Rc::new(module);
        let pending = std::mem::take(&mut self.cache.pending);
        for (i, p) in pending.into_iter().enumerate() {
            let c = Rc::new(Compiled {
                _module: module.clone(),
                slot: base + i as u32,
                max_steps: p.block.max_steps() as u8,
            });
            self.cache.stats.blocks += 1;
            self.cache.compiled.insert((p.key, p.words), c.clone());
            self.cache.record(p.key, p.pa, Some(c));
        }
    }

    fn compile_dispatcher(&mut self) {
        let wasm = translate::dispatcher(self.cfg.memory);
        let d = match self.engine.compile(&wasm) {
            Ok(d) => d,
            Err(_) => {
                self.reset();
                self.engine.compile(&wasm).expect("dispatcher del JIT rifiutato dal motore")
            }
        };
        self.dispatcher = Some(d);
    }

    /// Scarta tutto il codice compilato e azzera il motore (tabella piena o
    /// motore pieno). Le pagine restano sorvegliate: al più qualche
    /// invalidazione a vuoto.
    fn reset(&mut self) {
        let c = &mut self.cache;
        c.blocks.clear();
        c.pages.clear();
        c.compiled.clear();
        c.next_slot = 0;
        c.stats.resets += 1;
        self.dispatcher = None;
        self.engine.reset();
        self.engine.reserve(self.cfg.state_addr as usize + area::SIZE as usize);
        self.ram_key = None;
        self.init_area();
        self.new_epoch();
        // I blocchi in attesa tornano a contare nelle pagine.
        let c = &mut self.cache;
        for i in 0..c.pending.len() {
            let (k, pa) = (c.pending[i].key, c.pending[i].pa);
            c.pages.entry(pa >> 12).or_default().push(k);
        }
    }
}

/// Il JIT della modalità sistema come oggetto (per `vetro-machine`, che
/// non conosce il motore).
pub trait SysJitDyn {
    fn run(&mut self, cpu: &mut Cpu, mmu: &mut Mmu, phys: &mut dyn SysPhys, budget: u64) -> SysRun;
    fn stats(&self) -> SysJitStats;
}

impl<E: Engine> SysJitDyn for SysJit<E> {
    fn run(&mut self, cpu: &mut Cpu, mmu: &mut Mmu, phys: &mut dyn SysPhys, budget: u64) -> SysRun {
        SysJit::run(self, cpu, mmu, phys, budget)
    }
    fn stats(&self) -> SysJitStats {
        self.cache.stats
    }
}
