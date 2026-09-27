//! Analysis from the outside in `vetro boot` (M7/M8, ADR 0027): `--binder-log`
//! (decoded Binder calls and privacy inspector) and `--tls` (plaintext
//! from the TLS hooks, which ends up in the HAR like the plaintext
//! requests). It needs the kernel profile: `--kernel-profile=boot.img` (or
//! `Image`, with `--system-map`/`--kernel-btf` for the test kernel);
//! without it, `--boot-img`/`--kernel` is used if present.

use std::path::PathBuf;

use vetro_analysis::net::TlsConversation;
use vetro_machine::Machine;
use vetro_machine::analysis::{BinderTracer, Tracers, kernel_profile};
use vetro_machine::tls::TlsTracer;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnalysisOptions {
    pub profile: Option<String>,
    pub system_map: Option<String>,
    pub btf: Option<String>,
    /// File of the Binder calls: JSON if it ends in `.json`, lines of
    /// text otherwise.
    pub binder_log: Option<PathBuf>,
    /// TLS hooks: the decrypted HTTPS requests in the HAR and in the inspector.
    pub tls: bool,
}

impl AnalysisOptions {
    pub fn wanted(&self) -> bool {
        self.binder_log.is_some() || self.tls
    }

    /// Takes the options it knows; false if `a` is not one of its own.
    pub fn parse(&mut self, a: &str) -> bool {
        if a == "--tls" {
            self.tls = true;
            return true;
        }
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

    /// Puts the tracers into the machine. `fallback` is the kernel image
    /// or the boot's `boot.img`, if any.
    pub fn install(&self, m: &mut Machine, fallback: Option<&str>) -> Result<(), String> {
        if !self.wanted() {
            return Ok(());
        }
        let path = self
            .profile
            .as_deref()
            .or(fallback)
            .ok_or("--binder-log requires --kernel-profile=boot.img (or --boot-img/--kernel)")?;
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
            t.0.push(Box::new(BinderTracer::new(kernel.clone())));
        }
        if self.tls {
            t.0.push(Box::new(TlsTracer::new(kernel)));
        }
        m.set_tracer(Some(Box::new(t)));
        m.trace_syscalls(true);
        Ok(())
    }

    /// Updates the TLS hooks (new processes and returns); to be called between
    /// two quanta when `--tls` is enabled.
    pub fn tls_service(&self, m: &mut Machine) {
        if self.tls {
            vetro_machine::tls::tls_service(m);
        }
    }

    /// The TLS conversations captured so far.
    pub fn tls_conversations(&self, m: &mut Machine) -> Vec<TlsConversation> {
        m.tracer_mut::<Tracers>()
            .and_then(|t| t.get::<TlsTracer>())
            .map(|t| t.conversations.clone())
            .unwrap_or_default()
    }

    /// Writes the files; returns the summary lines for stderr.
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
            out.push(format!("binder: {} calls in {}", b.log.calls.len(), p.display()));
            for c in b.log.sensitive() {
                out.push(format!("privacy: {}", c.line()));
            }
        }
        Ok(out)
    }
}
