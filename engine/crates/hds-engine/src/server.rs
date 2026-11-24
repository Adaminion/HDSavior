//! `hdsavior-engine serve`: the JSON-lines transport of PROTOCOL.md section 1
//! over a byte stream pair (stdin / stdout), and an in-process client over
//! the same engine for the command-line interface and tests.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::backend::Backend;
use crate::engine::{Engine, Input, LineError, MAX_LINE_BYTES};
use crate::output::{log_warn, Output, WriteSink};
use crate::protocol::{EventLine, OutputLine, ProtocolError, Response};

/// Reads request lines from `input` and hands them to the engine: one
/// [`Input::Line`] per `\n`-terminated line (a trailing `\r` is removed; a
/// last line without terminator counts), lines over [`MAX_LINE_BYTES`] as
/// [`LineError::TooLong`] without buffering them, then [`Input::Eof`]. A
/// read error is treated as the end of input. Returns when the input ends
/// or the engine has gone.
pub fn read_requests(input: impl Read, engine: SyncSender<Input>) {
    let mut reader = BufReader::with_capacity(64 * 1024, input);
    let mut line: Vec<u8> = Vec::new();
    let mut too_long = false;
    loop {
        let (consumed, complete) = match reader.fill_buf() {
            Ok([]) => {
                // A last line without terminator still counts.
                let last = (!line.is_empty() || too_long)
                    .then(|| Input::Line(finish_line(&mut line, too_long)));
                if last.is_some_and(|last| engine.send(last).is_err()) {
                    return;
                }
                let _ = engine.send(Input::Eof);
                return;
            }
            Ok(buf) => {
                let (part, consumed, complete) = match buf.iter().position(|&b| b == b'\n') {
                    Some(i) => (&buf[..i], i + 1, true),
                    None => (buf, buf.len(), false),
                };
                if !too_long {
                    if line.len().saturating_add(part.len()) > MAX_LINE_BYTES {
                        too_long = true;
                        line = Vec::new();
                    } else {
                        line.extend_from_slice(part);
                    }
                }
                (consumed, complete)
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                log_warn(&format!("cannot read requests any more: {e}"));
                let _ = engine.send(Input::Eof);
                return;
            }
        };
        reader.consume(consumed);
        if complete {
            if engine
                .send(Input::Line(finish_line(&mut line, too_long)))
                .is_err()
            {
                return;
            }
            too_long = false;
        }
    }
}

fn finish_line(line: &mut Vec<u8>, too_long: bool) -> Result<String, LineError> {
    let mut bytes = std::mem::take(line);
    if too_long {
        return Err(LineError::TooLong);
    }
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    String::from_utf8(bytes).map_err(|_| LineError::NotUtf8)
}

/// Runs the protocol server: requests from `input`, responses and events to
/// `output` (nothing else is written there), until `shutdown` or the end of
/// input; then the running job is ended as PROTOCOL.md "End of input"
/// says, and the function returns.
pub fn serve(
    input: impl Read + Send + 'static,
    output: impl Write + Send + 'static,
    backend: Arc<dyn Backend>,
) {
    let engine = Engine::new(backend, Output::new(WriteSink(output)));
    let sender = engine.sender();
    // The reader may stay blocked in a read after the engine is done; the
    // process ends anyway when `main` returns.
    let spawned = thread::Builder::new()
        .name("hds-stdin".into())
        .spawn(move || read_requests(input, sender.clone()));
    if let Err(e) = spawned {
        log_warn(&format!("cannot start the request reader: {e}"));
        return;
    }
    engine.run();
}

/// An engine running on its own thread, driven by requests made in-process.
/// The command-line interface uses it, so that every subcommand goes through
/// the same dispatcher as `serve`.
pub struct InProcess {
    sender: SyncSender<Input>,
    lines: Receiver<String>,
    thread: Option<JoinHandle<()>>,
    next_id: i64,
}

/// What [`InProcess::call`] saw.
pub struct Called {
    /// The response's result or error.
    pub result: Result<Value, ProtocolError>,
    /// Events written before the response (for example `task_progress`).
    pub events: Vec<EventLine>,
}

impl InProcess {
    /// Starts an engine over `backend`.
    pub fn start(backend: Arc<dyn Backend>) -> InProcess {
        let (tx, rx) = mpsc::channel::<String>();
        let engine = Engine::new(backend, Output::new(tx));
        let sender = engine.sender();
        let thread = thread::Builder::new()
            .name("hds-engine".into())
            .spawn(move || engine.run())
            .ok();
        InProcess {
            sender,
            lines: rx,
            thread,
            next_id: 0,
        }
    }

    /// Sends a raw line (tests use it for malformed input).
    pub fn send_line(&self, line: &str) {
        let _ = self.sender.send(Input::Line(Ok(line.to_string())));
    }

    /// Sends a request and returns its id.
    pub fn send<A: Serialize>(&mut self, cmd: &str, args: &A) -> i64 {
        self.next_id += 1;
        let line = serde_json::json!({"id": self.next_id, "cmd": cmd, "args": args}).to_string();
        self.send_line(&line);
        self.next_id
    }

    /// The next output line, waiting at most `timeout`.
    pub fn next_line(&self, timeout: Duration) -> Option<String> {
        self.lines.recv_timeout(timeout).ok()
    }

    /// The next output line, parsed; `None` when the engine has ended (or
    /// wrote something that is not a protocol line, which it never does).
    pub fn next(&self) -> Option<OutputLine> {
        let line = self.lines.recv().ok()?;
        OutputLine::parse(&line)
    }

    /// Sends a request and waits for its response, collecting the events
    /// written before it.
    pub fn call<A: Serialize>(&mut self, cmd: &str, args: &A) -> Called {
        let id = self.send(cmd, args);
        let mut events = Vec::new();
        loop {
            match self.next() {
                Some(OutputLine::Event(event)) => events.push(event),
                Some(OutputLine::Response(response)) if response.id == Some(id) => {
                    return Called {
                        result: response_result(response),
                        events,
                    }
                }
                Some(OutputLine::Response(_)) => {}
                None => {
                    return Called {
                        result: Err(ProtocolError::internal("the engine ended")),
                        events,
                    }
                }
            }
        }
    }

    /// Ends the input and waits for the engine to finish (it ends a running
    /// job first). Returns the lines written meanwhile.
    pub fn finish(mut self) -> Vec<String> {
        let _ = self.sender.send(Input::Eof);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.lines.try_iter().collect()
    }
}

impl Drop for InProcess {
    fn drop(&mut self) {
        let _ = self.sender.send(Input::Eof);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The result or error of a response.
pub fn response_result(response: Response) -> Result<Value, ProtocolError> {
    if response.ok {
        Ok(response.result.unwrap_or(Value::Null))
    } else {
        Err(response
            .error
            .unwrap_or_else(|| ProtocolError::internal("a failed response without error")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(input: &[u8]) -> Vec<Result<String, LineError>> {
        let (tx, rx) = mpsc::sync_channel(1024);
        read_requests(input, tx);
        let mut out = Vec::new();
        for input in rx.try_iter() {
            match input {
                Input::Line(line) => out.push(line),
                Input::Eof => out.push(Ok("<eof>".into())),
                Input::JobFinished(_) => unreachable!(),
            }
        }
        out
    }

    #[test]
    fn line_splitting() {
        assert_eq!(
            read_all(b"a\nb\r\n\nlast"),
            vec![
                Ok("a".to_string()),
                Ok("b".to_string()),
                Ok(String::new()),
                Ok("last".to_string()),
                Ok("<eof>".to_string())
            ]
        );
        assert_eq!(read_all(b""), vec![Ok("<eof>".to_string())]);
        assert_eq!(
            read_all(b"\xff\xfe\nok\n"),
            vec![Err(LineError::NotUtf8), Ok("ok".into()), Ok("<eof>".into())]
        );
    }

    #[test]
    fn long_lines_are_refused_without_buffering() {
        let mut input = vec![b'x'; MAX_LINE_BYTES + 10];
        input.push(b'\n');
        input.extend_from_slice(b"next\n");
        input.extend_from_slice(&vec![b'y'; MAX_LINE_BYTES + 1]);
        assert_eq!(
            read_all(&input),
            vec![
                Err(LineError::TooLong),
                Ok("next".into()),
                Err(LineError::TooLong),
                Ok("<eof>".into())
            ]
        );
        // Exactly the limit is fine.
        let mut exact = vec![b'z'; MAX_LINE_BYTES];
        exact.push(b'\n');
        let lines = read_all(&exact);
        assert_eq!(lines[0].as_ref().map(|s| s.len()), Ok(MAX_LINE_BYTES));
    }
}
