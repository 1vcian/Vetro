//! Network analysis (M7, ADR 0016): from the Ethernet frames captured at the
//! virtio-net boundary up to the decoded HTTP requests.
//!
//! - [`capture`]: frames timestamped in guest virtual time;
//! - [`pcapng`]: pcapng export (and reading back, for the tests);
//! - [`packet`]: Ethernet, IPv4, TCP, UDP;
//! - [`flow`]: reassembled TCP streams (sequences, retransmissions, out of
//!   order) and UDP flows;
//! - [`dns`]: DNS messages (the queries to the sinkhole and its answers);
//! - [`http`]: HTTP/1.1, requests and responses, `chunked`, `gzip`/`deflate`
//!   (with [`inflate`]);
//! - [`body`]: body decoders (JSON, form, multipart, schema-less
//!   protobuf);
//! - [`inspector`]: the network inspector model (requests with
//!   timing) and the [`har`] 1.2 export.
//! - [`view`]: inspector list and detail as JSON for the web app.
//!
//! Everything is deterministic (no host clock, sorted tables) and
//! dependency-free: it compiles for `wasm32-unknown-unknown`.

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
pub mod tls;
pub mod view;

pub use capture::{Capture, Direction, Frame};
pub use inspector::{Attribution, HttpExchange, NetworkAnalysis, RequestRow, Timings};
pub use tls::{TlsConversation, TlsMessage};
