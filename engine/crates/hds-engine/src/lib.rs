//! The HDSavior engine: protocol types, the JSON-lines server, the job
//! manager and the command-line interface of the binary `hdsavior-engine`.
//!
//! * [`protocol`] - every object, argument, result and event of
//!   `docs/PROTOCOL.md` as serde types.
//! * [`engine`] - the state machine (states N, O, S, C, B, J), the
//!   error-by-state table, argument validation, epochs, the destination
//!   guard's pre-check and the job manager.
//! * [`backend`] - the [`Backend`](backend::Backend) trait: everything that
//!   needs `hds-core` or `hds-recover`, at the protocol level.
//! * [`real_backend`] - its implementation on `hds-core` and `hds-recover`.
//! * [`recorded`] - where a session's source is while it is not attached.
//! * [`server`] - `serve` over stdin / stdout, and [`InProcess`](server::InProcess)
//!   for the command-line interface.
//! * [`cli`] - the subcommands of ARCHITECTURE.md section 5.6.
//! * [`testing`] - a test double of the backend.
//! * [`transcript`] - the recorded conversation `docs/protocol-examples.jsonl`.
//!
//! Safety invariants (ARCHITECTURE.md section 2) this crate keeps: it opens
//! sources only through `hds_io::open_source` (S1, S4) and never writes a
//! file (S6: the backend writes through `hds_io::out`); every destination
//! is checked with `hds_io::destination_check` against the protected ranges
//! (session range or whole source first, then the catalog's lost volumes,
//! plus the imaged range for `image_start`) before a backend sees it, with
//! no override for an overlap (S2, S3).

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

// The engine catches a panic per request handler and per job
// (`catch_unwind` in `engine`) and answers `internal`; with `panic = "abort"`
// one bug in a parser would end the process and every running job with it.
// The Cargo profiles keep the default `panic = "unwind"`
// (tests/panic_strategy.rs checks the manifests).
#[cfg(panic = "abort")]
compile_error!("hds-engine needs panic = \"unwind\": it catches panics per request and per job");

pub mod backend;
pub mod base64;
pub mod cli;
pub mod engine;
pub mod output;
pub mod platform;
pub mod protocol;
pub mod real_backend;
pub mod recorded;
pub mod server;
pub mod testing;
pub mod transcript;
mod validate;
