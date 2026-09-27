//! Vetro's analysis engine.
//!
//! M2: decoding of Linux arm64 syscalls for the tracer (`vetro run
//! --strace`). M7: network analysis ([`net`]: capture, pcapng, flows,
//! HTTP, body decoders, inspector, HAR; ADR 0016) and the input→effects
//! [`timeline`] (ADR 0023). TLS, Binder and ART hooks arrive
//! with M7–M9.

pub mod introspect;
pub mod net;
pub mod syscall;
pub mod timeline;
