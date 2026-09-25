//! Cattura di rete di `vetro boot` (M7, ADR 0016): `--pcap`, `--har` e
//! `--net-requests`. I frame vengono dal punto di cattura di virtio-net
//! (`Machine::net_tap`), con il tempo virtuale del guest; l'analisi è
//! quella di `vetro_analysis::net`.

use std::io;
use std::path::PathBuf;

use vetro_analysis::net::har::HarOptions;
use vetro_analysis::net::pcapng::{self, PcapngOptions};
use vetro_analysis::net::{Capture, Direction, NetworkAnalysis};
use vetro_machine::{FrameDir, Machine, TappedFrame};

/// Cosa esportare a fine esecuzione.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetCapture {
    pub pcap: Option<PathBuf>,
    pub har: Option<PathBuf>,
    /// Stampa su stderr la lista dell'ispettore (una riga per richiesta).
    pub requests: bool,
    capture: Capture,
}

impl NetCapture {
    /// Serve la cattura?
    pub fn wanted(&self) -> bool {
        self.pcap.is_some() || self.har.is_some() || self.requests
    }

    /// Aggiunge i frame catturati finora dalla macchina.
    pub fn collect(&mut self, m: &mut Machine) {
        for f in m.net_tap_take() {
            self.push(f);
        }
    }

    pub fn push(&mut self, f: TappedFrame) {
        let dir = match f.dir {
            FrameDir::FromGuest => Direction::FromGuest,
            FrameDir::ToGuest => Direction::ToGuest,
        };
        self.capture.push(f.at.0, dir, f.data);
    }

    pub fn capture(&self) -> &Capture {
        &self.capture
    }

    /// Scrive i file richiesti e stampa la lista; restituisce le righe di
    /// riepilogo per stderr.
    pub fn finish(&self) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        let frames = self.capture.frames();
        if let Some(p) = &self.pcap {
            std::fs::write(p, pcapng::write(frames, &PcapngOptions::default()))
                .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", p.display())))?;
            out.push(format!("pcapng: {} frame in {}", frames.len(), p.display()));
        }
        if self.har.is_some() || self.requests {
            let a = NetworkAnalysis::from_frames(frames);
            if self.requests {
                out.extend(a.requests().iter().map(|r| format!("http: {r}")));
            }
            if let Some(p) = &self.har {
                std::fs::write(p, a.to_har(&HarOptions::default()))
                    .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", p.display())))?;
                out.push(format!("har: {} richieste in {}", a.http.len(), p.display()));
            }
        }
        Ok(out)
    }
}

/// Riscrive `--pcap FILE` e `--har FILE` (valore separato) come
/// `--pcap=FILE`, la forma delle altre opzioni di `boot`.
pub fn join_values(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if matches!(a.as_str(), "--pcap" | "--har")
            && let Some(v) = it.next()
        {
            out.push(format!("{a}={v}"));
        } else {
            out.push(a.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valori_separati() {
        let a: Vec<String> =
            ["--pcap", "x.pcapng", "--har=y.har", "--net", "--har", "z"].map(String::from).into();
        assert_eq!(join_values(&a), ["--pcap=x.pcapng", "--har=y.har", "--net", "--har=z"]);
    }
}
