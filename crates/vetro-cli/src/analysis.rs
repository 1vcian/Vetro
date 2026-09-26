//! Analisi dall'esterno in `vetro boot` (M8, ADR 0027): `--binder-log`
//! (chiamate Binder decodificate e ispettore privacy). Serve il profilo
//! del kernel: `--kernel-profile=boot.img` (o `Image`, con
//! `--system-map`/`--kernel-btf` per il kernel di prova); senza, si usa
//! `--boot-img`/`--kernel` se ci sono.

use std::path::PathBuf;

use vetro_machine::Machine;
use vetro_machine::analysis::{BinderTracer, Tracers, kernel_profile};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnalysisOptions {
    pub profile: Option<String>,
    pub system_map: Option<String>,
    pub btf: Option<String>,
    /// File delle chiamate Binder: JSON se finisce in `.json`, righe di
    /// testo altrimenti.
    pub binder_log: Option<PathBuf>,
}

impl AnalysisOptions {
    pub fn wanted(&self) -> bool {
        self.binder_log.is_some()
    }

    /// Prende le opzioni che conosce; falso se `a` non è sua.
    pub fn parse(&mut self, a: &str) -> bool {
        let Some((k, v)) = a.split_once('=') else { return false };
        match k {
            "--kernel-profile" => self.profile = Some(v.into()),
            "--system-map" => self.system_map = Some(v.into()),
            "--kernel-btf" => self.btf = Some(v.into()),
            "--binder-log" => self.binder_log = Some(v.into()),
            _ => return false,
        }
        true
    }

    /// Mette i tracciatori nella macchina. `fallback` è l'immagine del
    /// kernel o il `boot.img` dell'avvio, se c'è.
    pub fn install(&self, m: &mut Machine, fallback: Option<&str>) -> Result<(), String> {
        if !self.wanted() {
            return Ok(());
        }
        let path = self
            .profile
            .as_deref()
            .or(fallback)
            .ok_or("--binder-log richiede --kernel-profile=boot.img (o --boot-img/--kernel)")?;
        let file = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
        let map = match &self.system_map {
            Some(p) => Some(std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}"))?),
            None => None,
        };
        let btf = match &self.btf {
            Some(p) => Some(std::fs::read(p).map_err(|e| format!("{p}: {e}"))?),
            None => None,
        };
        let kernel =
            kernel_profile(&file, map.as_deref(), btf.as_deref()).map_err(|e| format!("{path}: {e}"))?;
        let mut t = Tracers::default();
        if self.binder_log.is_some() {
            t.0.push(Box::new(BinderTracer::new(kernel)));
        }
        m.set_tracer(Some(Box::new(t)));
        m.trace_syscalls(true);
        Ok(())
    }

    /// Scrive i file; restituisce le righe di riepilogo per stderr.
    pub fn finish(&self, m: &mut Machine) -> std::io::Result<Vec<String>> {
        let mut out = Vec::new();
        let Some(t) = m.tracer_mut::<Tracers>() else { return Ok(out) };
        if let (Some(p), Some(b)) = (&self.binder_log, t.get::<BinderTracer>()) {
            let text = if p.extension().is_some_and(|e| e == "json") {
                b.log.to_json()
            } else {
                b.log.calls.iter().map(|c| c.line() + "\n").collect()
            };
            std::fs::write(p, text)
                .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", p.display())))?;
            out.push(format!("binder: {} chiamate in {}", b.log.calls.len(), p.display()));
            for c in b.log.sensitive() {
                out.push(format!("privacy: {}", c.line()));
            }
        }
        Ok(out)
    }
}
