//! Network capture of `vetro boot` (M7, ADR 0016): `--pcap`, `--har` and
//! `--net-requests`. The frames come from virtio-net's capture point
//! (`Machine::net_tap`), with the guest's virtual time; the analysis is
//! that of `vetro_analysis::net`.

use std::io;
use std::path::PathBuf;

use vetro_analysis::net::har::HarOptions;
use vetro_analysis::net::pcapng::{self, PcapngOptions};
use vetro_analysis::net::{Capture, Direction, NetworkAnalysis, TlsConversation};
use vetro_machine::{FrameDir, Machine, TappedFrame};

/// What to export at the end of the run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetCapture {
    pub pcap: Option<PathBuf>,
    pub har: Option<PathBuf>,
    /// Prints the inspector's list to stderr (one line per request).
    pub requests: bool,
    capture: Capture,
    /// Decrypted TLS conversations from the hooks (M7): they end up in the HAR and
    /// in the list like the plaintext requests.
    tls: Vec<TlsConversation>,
}

impl NetCapture {
    /// Is the capture needed?
    pub fn wanted(&self) -> bool {
        self.pcap.is_some() || self.har.is_some() || self.requests
    }

    /// The decrypted TLS conversations to merge (from the M7 hooks).
    pub fn set_tls(&mut self, tls: Vec<TlsConversation>) {
        self.tls = tls;
    }

    /// Adds the frames captured so far by the machine.
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

    /// Writes the requested files and prints the list; returns the summary
    /// lines for stderr.
    pub fn finish(&self) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        let frames = self.capture.frames();
        if let Some(p) = &self.pcap {
            std::fs::write(p, pcapng::write(frames, &PcapngOptions::default()))
                .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", p.display())))?;
            out.push(format!("pcapng: {} frames in {}", frames.len(), p.display()));
        }
        if self.har.is_some() || self.requests {
            let mut a = NetworkAnalysis::from_frames(frames);
            if !self.tls.is_empty() {
                a.merge_tls(&self.tls);
            }
            if self.requests {
                out.extend(a.requests().iter().map(|r| format!("http: {r}")));
            }
            if let Some(p) = &self.har {
                std::fs::write(p, a.to_har(&HarOptions::default()))
                    .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", p.display())))?;
                out.push(format!("har: {} requests in {}", a.http.len(), p.display()));
            }
        }
        Ok(out)
    }
}

/// Rewrites `--pcap FILE` and `--har FILE` (separate value) as
/// `--pcap=FILE`, the form of the other `boot` options.
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
