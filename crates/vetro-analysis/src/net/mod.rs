//! Analisi di rete (M7, ADR 0016): dai frame Ethernet catturati al confine
//! di virtio-net fino alle richieste HTTP decodificate.
//!
//! - [`capture`]: frame con marca temporale in tempo virtuale del guest;
//! - [`pcapng`]: esportazione pcapng (e rilettura, per i test);
//! - [`packet`]: Ethernet, IPv4, TCP, UDP;
//! - [`flow`]: flussi TCP ricostruiti (sequenze, ritrasmissioni, fuori
//!   ordine) e flussi UDP;
//! - [`dns`]: messaggi DNS (le domande al sinkhole e le sue risposte);
//! - [`http`]: HTTP/1.1, richieste e risposte, `chunked`, `gzip`/`deflate`
//!   (con [`inflate`]);
//! - [`body`]: decodificatori del corpo (JSON, form, multipart, protobuf
//!   senza schema);
//! - [`inspector`]: il modello dell'ispettore di rete (richieste con
//!   timing) e l'esportazione [`har`] 1.2.
//! - [`view`]: lista e dettaglio dell'ispettore in JSON per l'app web.
//!
//! Tutto è deterministico (niente orologio dell'host, tabelle ordinate) e
//! senza dipendenze: compila in `wasm32-unknown-unknown`.

pub mod body;
pub mod capture;
pub mod dns;
pub mod flow;
pub mod har;
pub mod http;
pub mod inflate;
pub mod inspector;
pub mod json;
pub mod packet;
pub mod pcapng;
pub mod view;

pub use capture::{Capture, Direction, Frame};
pub use inspector::{HttpExchange, NetworkAnalysis, RequestRow, Timings};
