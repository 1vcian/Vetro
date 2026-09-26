//! Motore di analisi di Vetro.
//!
//! M2: decodifica delle syscall Linux arm64 per il tracer (`vetro run
//! --strace`). M7: analisi di rete ([`net`]: cattura, pcapng, flussi,
//! HTTP, decodificatori del corpo, ispettore, HAR; ADR 0016) e la
//! [`timeline`] input→effetti (ADR 0023). Hook TLS, Binder e ART arrivano
//! con M7–M9.

pub mod introspect;
pub mod net;
pub mod syscall;
pub mod timeline;
