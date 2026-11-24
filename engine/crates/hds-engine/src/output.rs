//! The one writer of protocol lines, and the log on stderr.
//!
//! Every response and event goes through one [`Output`]: it serializes the
//! value, refuses integers the client cannot represent, and writes the line
//! and a flush under one lock, so lines never interleave and appear in the
//! order in which the writers took the lock. Nothing else writes to stdout.

use std::io::{self, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use serde_json::Value;

use crate::protocol::wire::exceeds_wire_range;
use crate::protocol::{EventData, ProtocolError, Response};

/// Where protocol lines go.
pub trait LineSink: Send {
    /// Writes one line (without terminator) and makes it visible to the reader.
    fn write_line(&mut self, line: &str) -> io::Result<()>;
}

/// A sink over a byte stream (stdout): each line is written with `\n` and
/// flushed.
pub struct WriteSink<W: Write + Send>(pub W);

impl<W: Write + Send> LineSink for WriteSink<W> {
    fn write_line(&mut self, line: &str) -> io::Result<()> {
        let mut bytes = Vec::with_capacity(line.len() + 1);
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
        self.0.write_all(&bytes)?;
        self.0.flush()
    }
}

/// A sink that hands each line to a channel (the command-line interface
/// and in-process tests read the engine this way).
impl LineSink for mpsc::Sender<String> {
    fn write_line(&mut self, line: &str) -> io::Result<()> {
        self.send(line.to_string())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the reader has gone"))
    }
}

struct State {
    sink: Box<dyn LineSink>,
    broken: bool,
}

/// The single writer of protocol lines. Cheap to clone; all clones write to
/// the same sink under the same lock.
#[derive(Clone)]
pub struct Output {
    state: Arc<Mutex<State>>,
}

impl Output {
    /// An output over `sink`.
    pub fn new(sink: impl LineSink + 'static) -> Output {
        Output {
            state: Arc::new(Mutex::new(State {
                sink: Box::new(sink),
                broken: false,
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic while the lock was held cannot leave a half-written line
        // behind that matters more than going on: the state is just the sink.
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Writes the response to request `id`: `result` serialized, or the error.
    /// A result that cannot be sent (not serializable, or an integer above
    /// 2^63 - 1) is answered with `internal` instead.
    pub fn respond<T: Serialize>(&self, id: Option<i64>, result: Result<&T, &ProtocolError>) {
        let response = match result {
            Ok(value) => match to_wire_value(value) {
                Ok(value) => match id {
                    Some(id) => Response::success(id, value),
                    None => Response::failure(
                        None,
                        ProtocolError::internal("a result was produced for a request without id"),
                    ),
                },
                Err(error) => {
                    log_error(&format!("result of request {id:?} not sent: {error}"));
                    Response::failure(id, error)
                }
            },
            Err(error) => Response::failure(id, error.clone()),
        };
        match serde_json::to_value(&response) {
            Ok(value) => self.write_value(&value),
            Err(error) => log_error(&format!("response {id:?} not serializable: {error}")),
        }
    }

    /// Writes an event. An event that cannot be sent is logged and dropped.
    pub fn event<E: EventData>(&self, data: &E) {
        match to_wire_value(data) {
            Ok(data) => self.write_value(&serde_json::json!({ "event": E::NAME, "data": data })),
            Err(error) => log_error(&format!("event {} not sent: {error}", E::NAME)),
        }
    }

    fn write_value(&self, value: &Value) {
        let line = value.to_string();
        let mut state = self.lock();
        if state.broken {
            return;
        }
        if let Err(error) = state.sink.write_line(&line) {
            state.broken = true;
            log_warn(&format!(
                "cannot write to the client any more ({error}); further output is dropped"
            ));
        }
    }
}

/// Serializes `value` for the wire; `internal` when that fails or when it
/// holds an integer the client cannot represent.
fn to_wire_value<T: Serialize + ?Sized>(value: &T) -> Result<Value, ProtocolError> {
    let value = serde_json::to_value(value)
        .map_err(|e| ProtocolError::internal(format!("cannot serialize the result: {e}")))?;
    if exceeds_wire_range(&value) {
        return Err(ProtocolError::internal(
            "the result holds an integer above 2^63 - 1",
        ));
    }
    Ok(value)
}

/// Writes an informational line to stderr.
pub fn log_info(message: &str) {
    log_line("info", message);
}

/// Writes a warning to stderr.
pub fn log_warn(message: &str) {
    log_line("warn", message);
}

/// Writes an error to stderr.
pub fn log_error(message: &str) {
    log_line("error", message);
}

fn log_line(level: &str, message: &str) {
    // stderr is for humans; a failure to write it is not worth more than ignoring.
    let _ = writeln!(io::stderr().lock(), "hdsavior-engine [{level}] {message}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ErrorCode, LogEvent, LogLevel};

    #[test]
    fn lines_and_range_check() {
        let (tx, rx) = mpsc::channel();
        let out = Output::new(tx);
        out.respond(Some(1), Ok(&serde_json::json!({"a": 1})));
        out.respond(Some(2), Ok(&serde_json::json!({"a": u64::MAX})));
        out.respond::<Value>(None, Err(&ProtocolError::bad_request("x")));
        out.event(&LogEvent {
            level: LogLevel::Info,
            message: "m".into(),
        });
        let lines: Vec<String> = rx.try_iter().collect();
        assert_eq!(lines[0], r#"{"id":1,"ok":true,"result":{"a":1}}"#);
        let second: Response = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(second.error.unwrap().code, ErrorCode::Internal);
        let third: Response = serde_json::from_str(&lines[2]).unwrap();
        assert_eq!(third.id, None);
        assert_eq!(third.error.unwrap().code, ErrorCode::BadRequest);
        let fourth: Value = serde_json::from_str(&lines[3]).unwrap();
        assert_eq!(
            fourth,
            serde_json::json!({"event": "log", "data": {"level": "info", "message": "m"}})
        );
        for line in &lines {
            assert!(!line.contains('\n'));
        }
    }

    #[test]
    fn broken_sink_stops_writing() {
        let (tx, rx) = mpsc::channel();
        drop(rx);
        let out = Output::new(tx);
        out.respond(Some(1), Ok(&Value::Null));
        out.respond(Some(2), Ok(&Value::Null));
        assert!(out.lock().broken);
    }
}
