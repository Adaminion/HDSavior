//! The engine protocol (PROTOCOL.md, version 1) as Rust types.
//!
//! * [`wire`] - envelopes: requests, responses, events, the error object
//!   and its codes;
//! * [`objects`] - the shared objects of section 2;
//! * [`commands`] - arguments and results of section 3;
//! * [`events`] - the events of sections 3 and 4.
//!
//! The objects `SourceInfo`, `Partition`, `Disk`, `Warning` and the
//! destination guard's result are the serde types of `hds-io`, re-exported
//! here under their protocol names. `check_destination` adds `lost_files`
//! and `lost_bytes` ([`CheckDestinationResult`]).
//!
//! Conventions kept by every type: member names exactly as documented;
//! `Option` members are written as `null`, never left out; members the
//! engine does not know are ignored on input; integers are `u64`/`i64` and
//! never exceed 2^63 - 1 on the wire ([`wire::parse_request`] refuses larger
//! ones on input, the server refuses to send them); names travel as UTF-8
//! with unpaired surrogates replaced by U+FFFD ([`wire_name`]).

pub mod commands;
pub mod events;
pub mod objects;
pub mod wire;

pub use commands::*;
pub use events::*;
pub use objects::*;
pub use wire::{
    parse_request, ErrorCode, EventData, EventLine, OutputLine, ProtocolError, Request, Response,
};

pub use hds_io::{
    DestinationCheck, DiskFlag, DiskInfo as Disk, FsKind, MediaKind, NtfsSummary,
    PartitionInfo as Partition, ProtectedReason, Purpose, SourceInfo, SourceKind, TableKind,
    Verdict, Warning,
};

/// Protocol version of this engine (`hello.protocol`).
pub const PROTOCOL_VERSION: u32 = 1;
