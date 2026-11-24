//! Helpers shared by the integration tests.

#![allow(dead_code)] // not every test binary uses every helper

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hds_engine::protocol::{ErrorCode, EventLine, OutputLine, ProtocolError};
use hds_engine::server::{response_result, InProcess};
use hds_engine::testing::{FakeBackend, FakeConfig, FakeJobs};
use serde_json::{json, Value};

/// How long a test waits for one line of the engine.
pub const LINE_TIMEOUT: Duration = Duration::from_secs(20);

/// A scratch directory, removed on drop. Below `HDS_ENGINE_TEST_DIR` when set.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let base = std::env::var_os("HDS_ENGINE_TEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = base.join(format!(
            "hds-engine-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create scratch directory");
        let path = std::path::absolute(&path).expect("absolute scratch path");
        TempDir(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn join(&self, name: &str) -> String {
        self.0.join(name).to_string_lossy().into_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Writes an image file of `size` zero bytes and returns its path.
pub fn zero_image(dir: &TempDir, name: &str, size: u64) -> String {
    let path = dir.join(name);
    let file = std::fs::File::create(&path).expect("create image");
    file.set_len(size).expect("size image");
    path
}

/// An in-process engine over the fake backend.
pub struct Harness {
    pub engine: InProcess,
    pub fake: FakeBackend,
    /// Events seen while waiting for responses, in order.
    pub events: Vec<EventLine>,
}

impl Harness {
    pub fn new(jobs: FakeJobs) -> Harness {
        let fake = FakeBackend::new(FakeConfig {
            jobs,
            ..FakeConfig::default()
        });
        Harness {
            engine: InProcess::start(Arc::new(fake.clone())),
            fake,
            events: Vec::new(),
        }
    }

    /// The next output line, parsed; panics after [`LINE_TIMEOUT`].
    pub fn next(&self) -> OutputLine {
        let line = self
            .engine
            .next_line(LINE_TIMEOUT)
            .expect("the engine wrote nothing in time");
        OutputLine::parse(&line).unwrap_or_else(|| panic!("not a protocol line: {line}"))
    }

    /// Sends a request and returns its response, keeping the events seen before it.
    pub fn call(&mut self, cmd: &str, args: Value) -> Result<Value, ProtocolError> {
        let id = self.engine.send(cmd, &args);
        loop {
            match self.next() {
                OutputLine::Event(event) => self.events.push(event),
                OutputLine::Response(response) => {
                    assert_eq!(response.id, Some(id), "responses come in request order");
                    return response_result(response);
                }
            }
        }
    }

    pub fn ok(&mut self, cmd: &str, args: Value) -> Value {
        self.call(cmd, args.clone())
            .unwrap_or_else(|e| panic!("{cmd} {args} failed: {e}"))
    }

    pub fn err(&mut self, cmd: &str, args: Value) -> ProtocolError {
        match self.call(cmd, args.clone()) {
            Ok(result) => panic!("{cmd} {args} succeeded: {result}"),
            Err(error) => error,
        }
    }

    pub fn code(&mut self, cmd: &str, args: Value) -> ErrorCode {
        self.err(cmd, args).code
    }

    /// Waits for event `name` (also among those already seen) and returns its data.
    pub fn wait_event(&mut self, name: &str) -> Value {
        if let Some(i) = self.events.iter().position(|e| e.event == name) {
            return self.events.remove(i).data;
        }
        let deadline = Instant::now() + LINE_TIMEOUT;
        loop {
            assert!(Instant::now() < deadline, "no {name} in time");
            match self.next() {
                OutputLine::Event(event) if event.event == name => return event.data,
                OutputLine::Event(event) => self.events.push(event),
                OutputLine::Response(r) => panic!("unexpected response {r:?}"),
            }
        }
    }

    /// The current epoch (`catalog_info`).
    pub fn epoch(&mut self) -> u64 {
        self.ok("catalog_info", json!({}))["epoch"]
            .as_u64()
            .expect("epoch")
    }
}

/// The fixture directory from `HDS_FIXTURES`: `None` (and a SKIPPED message)
/// when unset; panics when set but `name` is missing.
pub fn fixture(name: &str) -> Option<PathBuf> {
    let Some(dir) = std::env::var_os("HDS_FIXTURES") else {
        eprintln!("SKIPPED: HDS_FIXTURES is not set; fixture {name} not tested");
        return None;
    };
    let path = PathBuf::from(dir).join(name);
    assert!(
        path.is_file(),
        "HDS_FIXTURES is set but {} is missing",
        path.display()
    );
    Some(std::path::absolute(path).expect("absolute fixture path"))
}

/// The repository's `docs/` directory.
pub fn docs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../docs")
}
