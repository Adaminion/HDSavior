//! The envelopes of PROTOCOL.md section 1: requests, responses, events and
//! the error object.

use std::fmt;
use std::io;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The error codes of the protocol. Wire form: snake case (`"bad_request"`, ...).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Malformed request or argument out of range.
    BadRequest,
    /// The `cmd` is not a command of this protocol.
    UnknownCommand,
    /// A job is running and the command cannot be handled meanwhile.
    Busy,
    /// The command needs an open (or attached) source.
    NoSource,
    /// The command needs a session.
    NoSession,
    /// The command needs a catalog.
    NoCatalog,
    /// The `epoch` of the request is not the current catalog epoch.
    StaleCatalog,
    /// A node, instance or file does not exist.
    NotFound,
    /// An input/output error.
    IoError,
    /// The operating system refused access (opening a disk without elevation).
    AccessDenied,
    /// The destination guard refused the location; `data` is the check.
    DestinationRefused,
    /// The source does not match the session; `data` names the failed checks.
    SourceMismatch,
    /// The source stopped answering (safety invariant S7).
    SourceLost,
    /// The destination is full.
    NoSpace,
    /// The output file exists already.
    Exists,
    /// The request is valid but cannot be served for this data.
    Unsupported,
    /// The job was cancelled.
    Cancelled,
    /// A bug in the engine.
    Internal,
}

impl ErrorCode {
    /// The wire name, e.g. `"bad_request"`.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::BadRequest => "bad_request",
            ErrorCode::UnknownCommand => "unknown_command",
            ErrorCode::Busy => "busy",
            ErrorCode::NoSource => "no_source",
            ErrorCode::NoSession => "no_session",
            ErrorCode::NoCatalog => "no_catalog",
            ErrorCode::StaleCatalog => "stale_catalog",
            ErrorCode::NotFound => "not_found",
            ErrorCode::IoError => "io_error",
            ErrorCode::AccessDenied => "access_denied",
            ErrorCode::DestinationRefused => "destination_refused",
            ErrorCode::SourceMismatch => "source_mismatch",
            ErrorCode::SourceLost => "source_lost",
            ErrorCode::NoSpace => "no_space",
            ErrorCode::Exists => "exists",
            ErrorCode::Unsupported => "unsupported",
            ErrorCode::Cancelled => "cancelled",
            ErrorCode::Internal => "internal",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error object of the protocol, `{"code", "message", "data"}`: the
/// `error` member of a failed response and of the `*_done` event of a
/// failed job.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError {
    /// Machine-readable code.
    pub code: ErrorCode,
    /// Plain-language explanation for the user.
    #[serde(default)]
    pub message: String,
    /// Optional details: the `check_destination` result for
    /// `destination_refused`, the failed checks for `source_mismatch`.
    #[serde(default)]
    pub data: Option<Value>,
}

impl ProtocolError {
    /// An error without `data`.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> ProtocolError {
        ProtocolError {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Adds `data`.
    pub fn with_data(mut self, data: Value) -> ProtocolError {
        self.data = Some(data);
        self
    }

    /// `bad_request` with the given explanation.
    pub fn bad_request(message: impl Into<String>) -> ProtocolError {
        ProtocolError::new(ErrorCode::BadRequest, message)
    }

    /// `internal` with the given explanation.
    pub fn internal(message: impl Into<String>) -> ProtocolError {
        ProtocolError::new(ErrorCode::Internal, message)
    }

    /// Maps an I/O error by its kind: `NotFound` to `not_found`,
    /// `PermissionDenied` to `access_denied`, `InvalidInput` to
    /// `bad_request`, `Unsupported` to `unsupported`, `StorageFull` to
    /// `no_space`, `AlreadyExists` to `exists`, anything else to `io_error`.
    /// `what` names the operation for the message.
    pub fn from_io(what: &str, error: &io::Error) -> ProtocolError {
        let code = match error.kind() {
            io::ErrorKind::NotFound => ErrorCode::NotFound,
            io::ErrorKind::PermissionDenied => ErrorCode::AccessDenied,
            io::ErrorKind::InvalidInput => ErrorCode::BadRequest,
            io::ErrorKind::Unsupported => ErrorCode::Unsupported,
            io::ErrorKind::StorageFull => ErrorCode::NoSpace,
            io::ErrorKind::AlreadyExists => ErrorCode::Exists,
            _ => ErrorCode::IoError,
        };
        ProtocolError::new(code, format!("{what}: {error}"))
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.message.is_empty() {
            f.write_str(self.code.as_str())
        } else {
            write!(f, "{}: {}", self.code, self.message)
        }
    }
}

impl std::error::Error for ProtocolError {}

/// A request line that passed the envelope checks.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// The client's id, echoed in the response.
    pub id: i64,
    /// The command name.
    pub cmd: String,
    /// The arguments: always an object (`{}` when the request had none or `null`).
    pub args: Map<String, Value>,
}

/// Why a request line was refused before its command was looked at.
#[derive(Debug, Clone, PartialEq)]
pub struct EnvelopeError {
    /// The id to answer with: `None` (`"id": null`) when the line is not a
    /// JSON object with an integer `id`.
    pub id: Option<i64>,
    /// Always `bad_request`.
    pub error: ProtocolError,
}

/// Largest integer the protocol carries (PROTOCOL.md, "Conventions").
pub const MAX_WIRE_INT: u64 = i64::MAX as u64;

/// Parses one request line (without its line terminator).
///
/// The line must be a JSON object with an integer `id` in the range of a
/// signed 64-bit integer; otherwise the answer goes to `"id": null`. `cmd`
/// must be a string; `args` may be omitted or `null`, otherwise it must be
/// an object. Members the protocol does not know are ignored. A number in
/// `args` that is not an integer up to 2^63 - 1 in magnitude is refused here
/// already, so no handler sees one.
pub fn parse_request(line: &str) -> Result<Request, EnvelopeError> {
    let no_id = |message: &str| EnvelopeError {
        id: None,
        error: ProtocolError::bad_request(message),
    };
    let value: Value = serde_json::from_str(line)
        .map_err(|e| no_id(&format!("the line is not valid JSON: {e}")))?;
    let Value::Object(mut object) = value else {
        return Err(no_id("the line is not a JSON object"));
    };
    let id = match object.get("id") {
        Some(Value::Number(n)) => n.as_i64(),
        _ => None,
    };
    let Some(id) = id else {
        return Err(no_id(
            "the request has no integer \"id\" between -2^63 and 2^63 - 1",
        ));
    };
    let with_id = |message: String| EnvelopeError {
        id: Some(id),
        error: ProtocolError::bad_request(message),
    };
    let cmd = match object.remove("cmd") {
        Some(Value::String(cmd)) => cmd,
        Some(_) => return Err(with_id("\"cmd\" must be a string".into())),
        None => return Err(with_id("the request has no \"cmd\"".into())),
    };
    let args = match object.remove("args") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(args)) => args,
        Some(_) => return Err(with_id("\"args\" must be an object".into())),
    };
    if let Some(path) = args
        .iter()
        .find_map(|(k, v)| find_out_of_range_number(v, &format!("args.{k}")))
    {
        return Err(with_id(format!(
            "{path} is not an integer of at most 2^63 - 1 in magnitude"
        )));
    }
    Ok(Request { id, cmd, args })
}

/// Finds a number that the protocol cannot carry: a float with a fractional
/// part or an integer outside the signed 64-bit range. Returns a description
/// of where it is. Nesting is bounded by `serde_json`'s recursion limit.
pub fn find_out_of_range_number(value: &Value, path: &str) -> Option<String> {
    match value {
        // A float is left to the typed parse, which refuses it for every
        // integer member (no argument of the protocol is a fraction).
        Value::Number(n) => (n.is_u64() && !n.is_i64()).then(|| path.to_string()),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(i, v)| find_out_of_range_number(v, &format!("{path}[{i}]"))),
        Value::Object(members) => members
            .iter()
            .find_map(|(k, v)| find_out_of_range_number(v, &format!("{path}.{k}"))),
        Value::Null | Value::Bool(_) | Value::String(_) => None,
    }
}

/// True when an outgoing value holds an integer above 2^63 - 1, which the
/// client cannot represent.
pub fn exceeds_wire_range(value: &Value) -> bool {
    match value {
        Value::Number(n) => n.as_u64().is_some_and(|u| u > MAX_WIRE_INT),
        Value::Array(items) => items.iter().any(exceeds_wire_range),
        Value::Object(members) => members.values().any(exceeds_wire_range),
        Value::Null | Value::Bool(_) | Value::String(_) => false,
    }
}

/// A response line as it is read back (by the command-line interface and by
/// tests): `{"id", "ok", "result"}` or `{"id", "ok", "error"}`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Response {
    /// The request's id; `None` for a line that could not be read as a request.
    pub id: Option<i64>,
    /// True for success.
    pub ok: bool,
    /// The result when `ok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// The error when not `ok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ProtocolError>,
}

impl Response {
    /// A successful response.
    pub fn success(id: i64, result: Value) -> Response {
        Response {
            id: Some(id),
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    /// A failed response.
    pub fn failure(id: Option<i64>, error: ProtocolError) -> Response {
        Response {
            id,
            ok: false,
            result: None,
            error: Some(error),
        }
    }
}

/// An event line: `{"event": name, "data": {...}}`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct EventLine {
    /// Event name, e.g. `"scan_progress"`.
    pub event: String,
    /// The event's object.
    pub data: Value,
}

/// Any line the engine writes to stdout, as read back.
#[derive(Debug, Clone, PartialEq)]
pub enum OutputLine {
    /// A response.
    Response(Response),
    /// An event.
    Event(EventLine),
}

impl OutputLine {
    /// Reads one output line; `None` when it is neither a response nor an event.
    pub fn parse(line: &str) -> Option<OutputLine> {
        let value: Value = serde_json::from_str(line).ok()?;
        let object = value.as_object()?;
        if object.contains_key("event") {
            serde_json::from_value(value).ok().map(OutputLine::Event)
        } else if object.contains_key("ok") {
            serde_json::from_value(value).ok().map(OutputLine::Response)
        } else {
            None
        }
    }
}

/// The object of an event, with its name.
pub trait EventData: Serialize {
    /// The event name on the wire.
    const NAME: &'static str;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_rules() {
        let ok = parse_request(r#"{"id": 7, "cmd": "hello", "extra": [1, 2]}"#).unwrap();
        assert_eq!(ok.id, 7);
        assert_eq!(ok.cmd, "hello");
        assert!(ok.args.is_empty());
        let null_args = parse_request(r#"{"id": -3, "cmd": "x", "args": null}"#).unwrap();
        assert_eq!(null_args.id, -3);

        for (line, id) in [
            ("", None),
            ("[1]", None),
            ("{\"cmd\": \"hello\"}", None),
            ("{\"id\": 1.5, \"cmd\": \"hello\"}", None),
            ("{\"id\": \"1\", \"cmd\": \"hello\"}", None),
            ("{\"id\": 9223372036854775808, \"cmd\": \"hello\"}", None),
            ("{\"id\": 1}", Some(1)),
            ("{\"id\": 1, \"cmd\": 5}", Some(1)),
            ("{\"id\": 1, \"cmd\": \"hello\", \"args\": []}", Some(1)),
            (
                "{\"id\": 2, \"cmd\": \"x\", \"args\": {\"a\": [18446744073709551615]}}",
                Some(2),
            ),
        ] {
            let err = parse_request(line).unwrap_err();
            assert_eq!(err.id, id, "{line}");
            assert_eq!(err.error.code, ErrorCode::BadRequest, "{line}");
        }
    }

    #[test]
    fn integer_limits() {
        assert!(
            parse_request(r#"{"id": 1, "cmd": "x", "args": {"n": 9223372036854775807}}"#).is_ok()
        );
        let err = parse_request(r#"{"id": 1, "cmd": "x", "args": {"n": 9223372036854775808}}"#)
            .unwrap_err();
        assert!(
            err.error.message.contains("args.n"),
            "{}",
            err.error.message
        );
        assert!(exceeds_wire_range(&serde_json::json!({"a": [u64::MAX]})));
        assert!(!exceeds_wire_range(
            &serde_json::json!({"a": [i64::MAX, -1]})
        ));
    }

    #[test]
    fn deep_nesting_is_refused_not_overflowing() {
        let line = format!(
            "{{\"id\": 1, \"cmd\": \"x\", \"args\": {{\"a\": {}{}}}}}",
            "[".repeat(100_000),
            "]".repeat(100_000)
        );
        let err = parse_request(&line).unwrap_err();
        assert_eq!(err.id, None);
    }

    #[test]
    fn io_error_mapping() {
        let cases = [
            (io::ErrorKind::NotFound, ErrorCode::NotFound),
            (io::ErrorKind::PermissionDenied, ErrorCode::AccessDenied),
            (io::ErrorKind::InvalidInput, ErrorCode::BadRequest),
            (io::ErrorKind::Unsupported, ErrorCode::Unsupported),
            (io::ErrorKind::StorageFull, ErrorCode::NoSpace),
            (io::ErrorKind::AlreadyExists, ErrorCode::Exists),
            (io::ErrorKind::UnexpectedEof, ErrorCode::IoError),
        ];
        for (kind, code) in cases {
            let e = ProtocolError::from_io("open", &io::Error::new(kind, "x"));
            assert_eq!(e.code, code);
            assert_eq!(
                serde_json::to_value(code).unwrap(),
                Value::String(code.as_str().into())
            );
        }
    }
}
