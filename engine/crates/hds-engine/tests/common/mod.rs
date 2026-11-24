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
    /// An in-process engine over the backend this build serves.
    pub fn real() -> Harness {
        Harness {
            engine: InProcess::start(hds_engine::backend::build_backend()),
            fake: FakeBackend::new(FakeConfig::default()),
            events: Vec::new(),
        }
    }

    /// Like [`Harness::wait_event`] with a deadline of `timeout`.
    pub fn wait_event_for(&mut self, name: &str, timeout: Duration) -> Value {
        if let Some(i) = self.events.iter().position(|e| e.event == name) {
            return self.events.remove(i).data;
        }
        let deadline = Instant::now() + timeout;
        loop {
            assert!(Instant::now() < deadline, "no {name} in time");
            match self.engine.next_line(timeout) {
                Some(line) => match OutputLine::parse(&line) {
                    Some(OutputLine::Event(event)) if event.event == name => return event.data,
                    Some(OutputLine::Event(event)) => self.events.push(event),
                    other => panic!("unexpected line {other:?}"),
                },
                None => panic!("no {name} in time"),
            }
        }
    }

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

/// SHA-256 of `data` as lower-case hex (the fixture manifests' hashes).
pub fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());
    for block in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[4 * i],
                block[4 * i + 1],
                block[4 * i + 2],
                block[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}
