//! `JitCpu`: esecuzione in modalità utente che alterna blocchi tradotti e
//! passi dell'interprete (ADR 0012).
//!
//! # Contratto
//! [`JitCpu::run`] esegue al più `budget` passi e si ferma dopo il primo
//! passo che dà un'eccezione. Il risultato è identico a chiamare
//! `Cpu::step` lo stesso numero di volte: stessi registri, stessa memoria,
//! stessa eccezione (la produce sempre l'interprete: un blocco che fa fault
//! lascia lo stato di prima dell'istruzione e l'interprete la riprova).
//! Il chiamante (il kernel emulato) conta i passi come faceva con
//! l'interprete, così l'orologio del guest non cambia.
//!
//! # Cache e invalidazione
//! - I blocchi si cercano per (spazio d'indirizzamento, `pc`): lo spazio è
//!   [`UserMemory::space_id`], che cambia a ogni copia (fork).
//! - Un blocco sta in una pagina da 4 KiB, che si sorveglia con
//!   [`UserMemory::watch_code`]. Ogni cambiamento della pagina (store del
//!   guest, scrittura del kernel emulato, munmap, mprotect, mremap...) la
//!   mette tra le pagine sporche; prima di ogni blocco e dopo ogni passo
//!   dell'interprete le pagine sporche si raccolgono e i loro blocchi si
//!   scartano. Uno store di un blocco su una pagina sorvegliata fa uscire il
//!   blocco dopo l'istruzione (`STOP`).
//! - Il codice di pagine condivise (MAP_SHARED) non si traduce: può
//!   cambiare da un altro spazio.
//! - Lo stesso blocco (stesso `pc`, stesse parole) in spazi diversi riusa il
//!   modulo già compilato: dopo un fork o un exec dello stesso binario non si
//!   ricompila nulla.
//! - Un blocco si compila dopo `hot_threshold` esecuzioni del suo inizio
//!   con l'interprete (0 = subito, per i test di parità).

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::rc::Rc;

use vetro_cpu::{Cpu, Exception, Memory, UserMemory};

use crate::engine::{Engine, Host};
use crate::profile::Profile;
use crate::state::{self, JitState, off};
use crate::translate::{self, MAX_REGION};
use crate::wasm::MemoryImport;
use crate::{FAULT, NEXT, STOP, SVC};

#[derive(Clone, Copy, Debug)]
pub struct JitConfig {
    /// Esecuzioni con l'interprete prima di compilare un blocco.
    pub hot_threshold: u32,
    /// Come i moduli importano la memoria (condivisa nel browser con i
    /// thread).
    pub memory: MemoryImport,
    /// Indirizzo di `JitState` nella memoria del motore (allineato a 16).
    pub state_addr: u32,
    /// Conta per classe le istruzioni dell'interprete ([`Profile`]).
    pub profile: bool,
}

impl Default for JitConfig {
    fn default() -> Self {
        JitConfig {
            hot_threshold: 16,
            memory: MemoryImport { min: 1, shared_max: None },
            state_addr: 16,
            profile: false,
        }
    }
}

/// Contatori, per le misure.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JitStats {
    /// Passi eseguiti dai blocchi tradotti.
    pub jit_steps: u64,
    /// Passi eseguiti dall'interprete.
    pub interp_steps: u64,
    /// Corse di blocchi.
    pub block_runs: u64,
    /// Moduli compilati.
    pub compiled: u64,
    /// Blocchi riusati da un altro spazio senza ricompilare.
    pub reused: u64,
    /// Pagine invalidate.
    pub invalidated_pages: u64,
    /// Uscite per fault e per store su codice.
    pub faults: u64,
    pub stops: u64,
    /// Azzeramenti del motore (tutto il codice compilato scartato).
    pub resets: u64,
}

/// Esito della ricerca di un blocco.
enum Look<M> {
    Hot(Rc<Compiled<M>>),
    /// Da tradurre ora.
    Translate,
    /// All'interprete (freddo).
    Interp,
    /// All'interprete: la prima istruzione non si traduce (dopo di lei può
    /// cominciare una regione).
    One,
}

/// Hash per chiavi u64 (indirizzi, pagine): la ricerca del blocco si fa a
/// ogni corsa, SipHash costerebbe quanto il blocco.
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
        // Moltiplicazione di Fibonacci e ripiegamento: i bit bassi (quelli
        // che usa la tabella) dipendono da tutto l'indirizzo.
        let h = v.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 = h ^ (h >> 32);
    }
}

type FastMap<V> = HashMap<u64, V, BuildHasherDefault<U64Hasher>>;

/// Un ingresso di una regione compilata.
struct Compiled<M> {
    module: Rc<M>,
    index: u32,
    /// Passi massimi del blocco base d'ingresso.
    max_steps: u64,
    /// Indice del blocco base d'ingresso (`JitState::entry`).
    bb: u32,
}

/// Gli ingressi di una regione compilata; il primo è il suo inizio.
type Entries<M> = Rc<[(u64, Rc<Compiled<M>>)]>;

enum Entry<M> {
    /// Visto `n` volte, non ancora compilato.
    Cold(u32),
    Hot(Rc<Compiled<M>>),
    /// La prima istruzione non si traduce: la esegue l'interprete.
    NoBlock,
}

struct Space<M> {
    blocks: FastMap<Entry<M>>,
    /// Pagina → inizi dei blocchi compilati che vi stanno.
    pages: FastMap<Vec<u64>>,
    last_use: u64,
}

impl<M> Space<M> {
    fn new() -> Self {
        Space { blocks: FastMap::default(), pages: FastMap::default(), last_use: 0 }
    }

    fn invalidate(&mut self, page: u64) -> bool {
        match self.pages.remove(&page) {
            Some(pcs) => {
                for pc in pcs {
                    self.blocks.remove(&pc);
                }
                true
            }
            None => false,
        }
    }
}

/// Identità di un blocco compilato: indirizzo e parole delle istruzioni.
type BlockKey = (u64, Vec<u32>);

/// Spazi d'indirizzamento tenuti in cache (gli altri si scartano, il meno
/// usato di recente per primo).
const MAX_SPACES: usize = 64;

/// Esecutore JIT della modalità utente.
pub struct JitCpu<E: Engine> {
    engine: E,
    cfg: JitConfig,
    spaces: FastMap<Space<E::Module>>,
    /// Moduli già compilati, per (pc, parole del blocco).
    compiled: HashMap<BlockKey, Entries<E::Module>>,
    tick: u64,
    pub stats: JitStats,
    /// Istruzioni dell'interprete per classe, se `cfg.profile`.
    pub profile: Option<Profile>,
}

/// `Host` sopra la memoria utente.
struct MemHost<'a> {
    mem: &'a mut UserMemory,
    /// I registri SIMD/FP della `Cpu` (per `vsync`).
    v: &'a [u128; 32],
}

impl Host for MemHost<'_> {
    #[inline]
    fn ld(&mut self, _mem: &mut [u8], va: u64, size: u32) -> Result<u64, ()> {
        let mut b = [0u8; 8];
        self.mem.read(va, &mut b[..size as usize]).map_err(|_| ())?;
        Ok(u64::from_le_bytes(b))
    }

    #[inline]
    fn st(&mut self, _mem: &mut [u8], va: u64, size: u32, value: u64) -> Result<bool, ()> {
        self.mem.write(va, &value.to_le_bytes()[..size as usize]).map_err(|_| ())?;
        Ok(self.mem.code_dirty())
    }

    fn vsync(&mut self, mem: &mut [u8], state: u32) {
        state::vsync_in(mem, state as usize, self.v);
    }
}

impl<E: Engine> JitCpu<E> {
    pub fn new(mut engine: E, cfg: JitConfig) -> Self {
        assert!(cfg.state_addr.is_multiple_of(16), "JitState va allineato a 16 byte");
        engine.runtime(&translate::runtime(cfg.memory)).expect("runtime del JIT rifiutato dal motore");
        JitCpu {
            engine,
            cfg,
            spaces: FastMap::default(),
            compiled: HashMap::new(),
            tick: 0,
            stats: JitStats::default(),
            profile: cfg.profile.then(|| {
                crate::helper::profile(true);
                Profile::default()
            }),
        }
    }

    pub fn engine(&mut self) -> &mut E {
        &mut self.engine
    }

    /// Scarta i blocchi delle pagine cambiate.
    fn drain(&mut self, space: u64, mem: &mut UserMemory) {
        if !mem.code_dirty() {
            return;
        }
        let dirty = mem.take_code_dirty();
        if let Some(s) = self.spaces.get_mut(&space) {
            for p in dirty {
                if s.invalidate(p) {
                    self.stats.invalidated_pages += 1;
                }
            }
        }
    }

    fn space(&mut self, id: u64) -> &mut Space<E::Module> {
        self.tick += 1;
        if !self.spaces.contains_key(&id) && self.spaces.len() >= MAX_SPACES {
            let oldest = self.spaces.iter().min_by_key(|(_, s)| s.last_use).map(|(&k, _)| k);
            if let Some(k) = oldest {
                self.spaces.remove(&k);
            }
        }
        let tick = self.tick;
        let s = self.spaces.entry(id).or_insert_with(Space::new);
        s.last_use = tick;
        s
    }

    /// Esegue al più `budget` passi (almeno uno se `budget > 0`); si ferma
    /// dopo il primo passo che dà un'eccezione. Restituisce i passi eseguiti
    /// (compreso quello dell'eccezione) e l'esito dell'ultimo.
    pub fn run(&mut self, cpu: &mut Cpu, mem: &mut UserMemory, budget: u64) -> (u64, Result<(), Exception>) {
        let id = mem.space_id();
        self.drain(id, mem);
        self.space(id);
        let at = self.cfg.state_addr as usize;
        let mut done = 0u64;
        // Vero se lo stato aggiornato sta in JitState e non nella Cpu.
        let mut in_jit = false;
        // `pc` di JitState quando `in_jit` (letto insieme a `steps`).
        let mut jit_pc = 0;
        while done < budget {
            let pc = if in_jit { jit_pc } else { cpu.pc };
            let mut one = false;
            let hot = match self.lookup(id, pc) {
                Look::Hot(c) => Some(c),
                Look::Interp => None,
                Look::One => {
                    one = true;
                    None
                }
                Look::Translate => {
                    // La compilazione può azzerare il motore (e la sua
                    // memoria): prima lo stato torna nella Cpu.
                    if in_jit {
                        JitState::load(self.engine.memory(), at).to_cpu(cpu);
                        in_jit = false;
                    }
                    let c = self.install(id, pc, mem);
                    one = c.is_none();
                    c
                }
            };
            if let Some(c) = &hot
                && c.max_steps <= budget - done
            {
                let m = self.engine.memory();
                if !in_jit {
                    let mut s = JitState::from_cpu(cpu);
                    s.limit = budget - done;
                    s.store(m, at);
                    in_jit = true;
                } else {
                    state::write_u64(m, at, off::STEPS, 0);
                    state::write_u64(m, at, off::LIMIT, budget - done);
                    state::write_u32(m, at, off::EXIT_DETAIL, 0);
                }
                state::write_u32(m, at, off::ENTRY, c.bb);
                let code = {
                    let mut host = MemHost { mem, v: &cpu.v };
                    self.engine.run(&c.module, c.index, self.cfg.state_addr, &mut host)
                };
                let m = self.engine.memory();
                let steps = state::read_u64(m, at, off::STEPS);
                jit_pc = state::read_u64(m, at, off::PC);
                debug_assert!(steps <= budget - done);
                done += steps;
                self.stats.jit_steps += steps;
                self.stats.block_runs += 1;
                match code {
                    NEXT => {}
                    STOP => {
                        self.stats.stops += 1;
                        self.drain(id, mem);
                    }
                    FAULT | SVC => {
                        if code == FAULT {
                            self.stats.faults += 1;
                        }
                        JitState::load(self.engine.memory(), at).to_cpu(cpu);
                        in_jit = false;
                        self.drain(id, mem);
                        if done < budget {
                            let r = self.interp_step(id, cpu, mem);
                            done += 1;
                            if r.is_err() {
                                return (done, r);
                            }
                        }
                    }
                    other => panic!("codice d'uscita del blocco sconosciuto: {other}"),
                }
                continue;
            }
            if in_jit {
                JitState::load(self.engine.memory(), at).to_cpu(cpu);
                in_jit = false;
            }
            // Interprete fino alla fine del blocco (salto, pagina, budget).
            loop {
                let old = cpu.pc;
                let r = self.interp_step(id, cpu, mem);
                done += 1;
                if r.is_err() {
                    return (done, r);
                }
                if done >= budget || cpu.pc != old.wrapping_add(4) || cpu.pc >> 12 != old >> 12 {
                    break;
                }
                if hot.is_some() || one {
                    // Blocco compilato ma più lungo del budget rimasto, o
                    // istruzione non traducibile: un passo, poi si cerca
                    // di nuovo (una regione può cominciare subito dopo).
                    break;
                }
            }
        }
        if in_jit {
            JitState::load(self.engine.memory(), at).to_cpu(cpu);
        }
        (done, Ok(()))
    }

    /// Scarta tutti i blocchi di tutti gli spazi e azzera il motore. Le
    /// pagine restano sorvegliate: al massimo qualche invalidazione a vuoto.
    fn reset(&mut self) {
        self.spaces.clear();
        self.compiled.clear();
        self.engine.reset();
        self.stats.resets += 1;
    }

    fn interp_step(&mut self, id: u64, cpu: &mut Cpu, mem: &mut UserMemory) -> Result<(), Exception> {
        if let Some(p) = &mut self.profile
            && let Ok(w) = mem.fetch(cpu.pc)
        {
            p.note(w, None);
        }
        let r = cpu.step(mem);
        self.stats.interp_steps += 1;
        self.drain(id, mem);
        r
    }

    /// Cerca il blocco di `pc` e conta le esecuzioni di quelli freddi.
    fn lookup(&mut self, id: u64, pc: u64) -> Look<E::Module> {
        let threshold = self.cfg.hot_threshold;
        let s = self.spaces.get_mut(&id).expect("spazio creato da run");
        match s.blocks.get_mut(&pc) {
            Some(Entry::Hot(c)) => Look::Hot(c.clone()),
            Some(Entry::NoBlock) => Look::One,
            Some(Entry::Cold(n)) => {
                *n += 1;
                if *n < threshold { Look::Interp } else { Look::Translate }
            }
            None => {
                if threshold > 1 {
                    s.blocks.insert(pc, Entry::Cold(1));
                    Look::Interp
                } else {
                    Look::Translate
                }
            }
        }
    }

    /// Traduce e installa il blocco di `pc` (o lo segna non traducibile).
    fn install(&mut self, id: u64, pc: u64, mem: &mut UserMemory) -> Option<Rc<Compiled<E::Module>>> {
        let es = self.translate(pc, mem);
        // Dopo un azzeramento del motore lo spazio va ricreato.
        let s = self.space(id);
        match es {
            Some(es) => {
                // L'inizio, e gli altri blocchi base dove non c'è già un
                // blocco compilato.
                for (i, (epc, c)) in es.iter().enumerate() {
                    if i > 0 && matches!(s.blocks.get(epc), Some(Entry::Hot(_))) {
                        continue;
                    }
                    s.blocks.insert(*epc, Entry::Hot(c.clone()));
                    s.pages.entry(pc >> 12).or_default().push(*epc);
                }
                mem.watch_code(pc >> 12);
                Some(es[0].1.clone())
            }
            None => {
                s.blocks.insert(pc, Entry::NoBlock);
                None
            }
        }
    }

    /// Legge, decodifica e compila il blocco che inizia a `pc`.
    fn translate(&mut self, pc: u64, mem: &mut UserMemory) -> Option<Entries<E::Module>> {
        let (block, words) = translate::discover(pc, None, MAX_REGION, |a| {
            if !mem.is_private(a) {
                return None;
            }
            mem.fetch(a).ok()
        })?;
        let key = (pc, words);
        if let Some(es) = self.compiled.get(&key) {
            self.stats.reused += 1;
            return Some(es.clone());
        }
        let wasm = translate::module(std::slice::from_ref(&block), self.cfg.memory);
        let module = match self.engine.compile(&wasm) {
            Ok(m) => m,
            Err(_) => {
                // Motore pieno (wasmtime: istanze per store): si scarta
                // tutto il codice compilato e si riprova una volta.
                self.reset();
                match self.engine.compile(&wasm) {
                    Ok(m) => m,
                    Err(e) => panic!("modulo del JIT rifiutato dal motore a pc={pc:#x}: {e}"),
                }
            }
        };
        self.stats.compiled += 1;
        let module = Rc::new(module);
        let first = block.entry_index();
        let mut es: Vec<(u64, Rc<Compiled<E::Module>>)> = block
            .entries()
            .into_iter()
            .map(|(epc, bb, max_steps)| {
                (epc, Rc::new(Compiled { module: module.clone(), index: 0, max_steps, bb }))
            })
            .collect();
        let at = es.iter().position(|e| e.1.bb == first).expect("ingresso della regione");
        es.swap(0, at);
        let es: Entries<E::Module> = es.into();
        self.compiled.insert(key, es.clone());
        Some(es)
    }
}
