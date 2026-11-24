//! The server as a real child process: `hdsavior-engine serve` with requests
//! on its stdin and protocol lines (and nothing else) on its stdout.
//!
//! Debug builds of the binary serve the backend test double when
//! `HDS_ENGINE_TEST_BACKEND` is `fake` or `fake-wait`; without it they serve
//! the backend of this build, which has no scanner yet.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use common::{zero_image, TempDir, LINE_TIMEOUT};
use hds_engine::protocol::{OutputLine, Response};
use serde_json::{json, Value};

struct Engine {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    stderr: Receiver<String>,
}

impl Engine {
    fn start(backend: Option<&str>) -> Engine {
        let mut command = Command::new(env!("CARGO_BIN_EXE_hdsavior-engine"));
        command
            .arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match backend {
            Some(name) => command.env("HDS_ENGINE_TEST_BACKEND", name),
            None => command.env_remove("HDS_ENGINE_TEST_BACKEND"),
        };
        let mut child = command.spawn().expect("start the engine");
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let (etx, stderr_rx) = mpsc::channel();
        thread::spawn(move || {
            let mut text = String::new();
            let _ = BufReader::new(stderr).read_to_string(&mut text);
            let _ = etx.send(text);
        });
        let stdin = child.stdin.take();
        Engine {
            child,
            stdin,
            lines,
            stderr: stderr_rx,
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin.write_all(bytes).expect("write to the engine");
        stdin.flush().expect("flush");
    }

    fn send(&mut self, id: i64, cmd: &str, args: Value) {
        let line = json!({"id": id, "cmd": cmd, "args": args}).to_string();
        self.write(format!("{line}\n").as_bytes());
    }

    /// The next stdout line; every one must be a protocol line.
    fn next(&self) -> OutputLine {
        let line = self
            .lines
            .recv_timeout(LINE_TIMEOUT)
            .expect("a line from the engine");
        OutputLine::parse(&line)
            .unwrap_or_else(|| panic!("stdout carried a non-protocol line: {line}"))
    }

    fn response(&self) -> Response {
        match self.next() {
            OutputLine::Response(r) => r,
            OutputLine::Event(e) => panic!("expected a response, got event {}", e.event),
        }
    }

    fn call(&mut self, id: i64, cmd: &str, args: Value) -> Response {
        self.send(id, cmd, args);
        let r = self.response();
        assert_eq!(r.id, Some(id));
        r
    }

    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// Waits for the process to exit; returns its status, the remaining
    /// stdout lines and stderr.
    fn wait(mut self) -> (ExitStatus, Vec<String>, String) {
        self.close_stdin();
        let deadline = Instant::now() + LINE_TIMEOUT;
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("wait") {
                break status;
            }
            assert!(Instant::now() < deadline, "the engine did not exit");
            thread::sleep(Duration::from_millis(10));
        };
        let mut rest = Vec::new();
        while let Ok(line) = self.lines.recv_timeout(Duration::from_secs(5)) {
            rest.push(line);
        }
        let stderr = self
            .stderr
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_default();
        (status, rest, stderr)
    }
}

#[test]
fn hello_and_disks_with_the_build_backend() {
    let dir = TempDir::new("process");
    let image = zero_image(&dir, "src.img", 1 << 20);
    let mut e = Engine::start(None);
    let hello = e.call(1, "hello", json!({}));
    let result = hello.result.unwrap();
    assert_eq!(result["protocol"], 1);
    assert_eq!(result["os"], std::env::consts::OS);
    assert_eq!(result["raw_disk_support"], cfg!(windows));
    assert_eq!(
        result["carve_kinds"].as_array().unwrap().len(),
        hds_carve::FileKind::all().len()
    );
    assert_eq!(result["engine_version"], env!("CARGO_PKG_VERSION"));
    let disks = e.call(2, "list_disks", json!({})).result.unwrap();
    assert!(disks["disks"].is_array());
    if !cfg!(windows) {
        assert_eq!(disks["disks"], json!([]));
    }
    let opened = e.call(3, "open_source", json!({"kind": "image", "path": image}));
    assert!(opened.ok, "{opened:?}");
    let check = e.call(
        4,
        "check_destination",
        json!({"path": image, "purpose": "image"}),
    );
    assert_eq!(check.result.unwrap()["verdict"], "overlaps_source");
    // The build backend scans; imaging is not connected yet: an honest
    // refusal, not a crash.
    let image_job = e.call(
        5,
        "image_start",
        json!({"dest": dir.join("copy.img"), "allow_unknown_disk": true}),
    );
    assert_eq!(image_job.error.unwrap().code.as_str(), "unsupported");
    let scan = e.call(6, "scan_start", json!({}));
    assert!(scan.ok, "{scan:?}");
    let done = loop {
        match e.next() {
            OutputLine::Event(ev) if ev.event == "scan_done" => break ev,
            OutputLine::Event(_) => {}
            OutputLine::Response(r) => panic!("unexpected response {r:?}"),
        }
    };
    assert_eq!(done.data["status"], "completed");
    let (status, rest, stderr) = e.wait();
    assert!(status.success(), "{status:?}");
    assert!(rest.is_empty(), "{rest:?}");
    assert!(
        stderr.contains("hdsavior-engine"),
        "logs go to stderr: {stderr}"
    );
}

#[test]
fn malformed_and_long_lines() {
    let mut e = Engine::start(Some("fake"));
    let mut input: Vec<u8> = Vec::new();
    input.extend_from_slice(b"\n");
    input.extend_from_slice(b"not json\n");
    input.extend_from_slice(b"[1, 2]\n");
    input.extend_from_slice(b"{\"id\": \"7\", \"cmd\": \"hello\"}\n");
    input.extend_from_slice(b"{\"id\": 1.5, \"cmd\": \"hello\"}\n");
    input.extend_from_slice(b"\xff\xfe{\"id\": 2}\n");
    input.extend_from_slice(b"{\"id\": 3}\n");
    input.extend_from_slice(b"{\"id\": 4, \"cmd\": \"frobnicate\"}\n");
    input.extend_from_slice(
        b"{\"id\": 5, \"cmd\": \"hello\", \"args\": {\"unknown\": [1]}, \"extra\": {\"x\": 1}}\r\n",
    );
    input.extend_from_slice(b"{\"id\": 6, \"cmd\": \"hello\", \"args\": 3}\n");
    // A line of 17 MiB: refused without being buffered whole.
    input.extend_from_slice(b"{\"id\": 8, \"cmd\": \"hello\", \"args\": {\"pad\": \"");
    input.extend(std::iter::repeat_n(b'a', 17 << 20));
    input.extend_from_slice(b"\"}}\n");
    input.extend_from_slice(b"{\"id\": 9, \"cmd\": \"hello\"}\n");
    // Write from another thread: the engine answers while we read.
    let mut stdin = e.stdin.take().unwrap();
    let writer = thread::spawn(move || {
        stdin.write_all(&input).unwrap();
        stdin
    });
    let expected: [(Option<i64>, Option<&str>); 12] = [
        (None, Some("bad_request")),
        (None, Some("bad_request")),
        (None, Some("bad_request")),
        (None, Some("bad_request")),
        (None, Some("bad_request")),
        (None, Some("bad_request")),
        (Some(3), Some("bad_request")),
        (Some(4), Some("unknown_command")),
        (Some(5), None),
        (Some(6), Some("bad_request")),
        (None, Some("bad_request")),
        (Some(9), None),
    ];
    for (i, (id, code)) in expected.into_iter().enumerate() {
        let r = e.response();
        assert_eq!(r.id, id, "response {i}");
        assert_eq!(r.ok, code.is_none(), "response {i}: {r:?}");
        assert_eq!(
            r.error.map(|e| e.code.as_str().to_string()).as_deref(),
            code,
            "response {i}"
        );
    }
    e.stdin = Some(writer.join().unwrap());
    let (status, rest, _) = e.wait();
    assert!(status.success());
    assert!(rest.is_empty(), "{rest:?}");
}

#[test]
fn ordering_and_end_of_input_during_a_job() {
    let dir = TempDir::new("process");
    let image = zero_image(&dir, "src.img", 1 << 20);
    let mut e = Engine::start(Some("fake-wait"));
    assert!(
        e.call(1, "open_source", json!({"kind": "image", "path": image}))
            .ok
    );
    e.send(2, "scan_start", json!({"session_path": dir.join("s.hds")}));
    let r = e.response();
    assert_eq!(
        r.id,
        Some(2),
        "the start response precedes every event of the job"
    );
    for _ in 0..3 {
        match e.next() {
            OutputLine::Event(ev) => assert_eq!(ev.event, "scan_progress"),
            other => panic!("{other:?}"),
        }
    }
    // Queries during the scan.
    assert_eq!(
        e.call(3, "catalog_info", json!({}))
            .error
            .unwrap()
            .code
            .as_str(),
        "no_catalog"
    );
    // End of input: the scan stops and its scan_done is the last line.
    let (status, rest, stderr) = e.wait();
    assert!(status.success(), "{status:?} {stderr}");
    assert_eq!(rest.len(), 1, "{rest:?}");
    let OutputLine::Event(done) = OutputLine::parse(&rest[0]).unwrap() else {
        panic!()
    };
    assert_eq!(done.event, "scan_done");
    assert_eq!(done.data["status"], "stopped");
    assert_eq!(done.data["catalog"], Value::Null);
}

#[test]
fn shutdown_during_a_job() {
    let dir = TempDir::new("process");
    let image = zero_image(&dir, "src.img", 1 << 20);
    let mut e = Engine::start(Some("fake-wait"));
    assert!(
        e.call(1, "open_source", json!({"kind": "image", "path": image}))
            .ok
    );
    e.send(2, "image_start", json!({"dest": dir.join("copy.img")}));
    assert_eq!(e.response().id, Some(2));
    e.send(3, "shutdown", json!({}));
    e.send(4, "hello", json!({}));
    let mut ids = Vec::new();
    let done = loop {
        match e.next() {
            OutputLine::Response(r) => ids.push(r.id),
            OutputLine::Event(ev) if ev.event == "image_done" => break ev,
            OutputLine::Event(_) => {}
        }
    };
    assert_eq!(ids, vec![Some(3)], "shutdown is answered, nothing after it");
    assert_eq!(done.data["status"], "cancelled");
    let (status, rest, _) = e.wait();
    assert!(status.success());
    assert!(rest.is_empty(), "{rest:?}");
}

#[test]
fn a_panicking_handler_does_not_kill_the_engine() {
    let dir = TempDir::new("process");
    let image = zero_image(&dir, "src.img", 1 << 20);
    let mut e = Engine::start(Some("fake"));
    assert!(
        e.call(1, "open_source", json!({"kind": "image", "path": image}))
            .ok
    );
    e.send(2, "scan_start", json!({}));
    assert_eq!(e.response().id, Some(2));
    let epoch = loop {
        if let OutputLine::Event(ev) = e.next() {
            if ev.event == "scan_done" {
                break ev.data["catalog"]["epoch"].clone();
            }
        }
    };
    let r = e.call(3, "list_children", json!({"epoch": epoch, "node": 666}));
    assert_eq!(r.error.unwrap().code.as_str(), "internal");
    e.send(
        4,
        "recover_start",
        json!({"epoch": epoch, "include": [666], "dest": dir.join("o")}),
    );
    assert_eq!(e.response().id, Some(4));
    let done = loop {
        if let OutputLine::Event(ev) = e.next() {
            if ev.event == "recover_done" {
                break ev.data;
            }
        }
    };
    assert_eq!(done["status"], "failed");
    assert_eq!(done["error"]["code"], "internal");
    assert!(e.call(5, "hello", json!({})).ok);
    let (status, _, stderr) = e.wait();
    assert!(status.success());
    assert!(stderr.contains("panicked"), "the panic is logged: {stderr}");
}
