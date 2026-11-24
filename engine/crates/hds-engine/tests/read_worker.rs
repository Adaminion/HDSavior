//! `read_file` reads on a worker thread (PROTOCOL.md section 1 "Ordering",
//! `read_file`): responses stay in request order, while `shutdown`, the end
//! of input and `job_cancel` of a running job stop a slow read at once.
//! The backend test double's node `SLOW_NODE` reads until it is stopped,
//! or for the real backend's time limit.

mod common;

use std::time::{Duration, Instant};

use common::{zero_image, Harness, TempDir};
use hds_engine::protocol::{ErrorCode, OutputLine};
use hds_engine::server::response_result;
use hds_engine::testing::{FakeJobs, SLOW_NODE};
use serde_json::{json, Value};

/// A harness with a catalog (state C); returns it with its epoch.
fn with_catalog(dir: &TempDir) -> (Harness, u64) {
    let image = zero_image(dir, "source.img", 4 << 20);
    let mut h = Harness::new(FakeJobs::UntilCancelled);
    h.ok("open_source", json!({"kind": "image", "path": image}));
    h.ok("scan_start", json!({}));
    h.ok("job_cancel", json!({}));
    let done = h.wait_event("scan_done");
    let epoch = done["catalog"]["epoch"].as_u64().unwrap();
    (h, epoch)
}

/// What the engine wrote, in order, until `until` holds or `timeout`.
enum Seen {
    Response(Option<i64>, Result<Value, ErrorCode>),
    Event(String),
}

fn collect(h: &Harness, timeout: Duration, until: impl Fn(&[Seen]) -> bool) -> Vec<Seen> {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    while !until(&seen) {
        let left = deadline.saturating_duration_since(Instant::now());
        let Some(line) = h.engine.next_line(left) else {
            panic!("the engine wrote nothing more in time");
        };
        match OutputLine::parse(&line) {
            Some(OutputLine::Response(r)) => {
                let id = r.id;
                seen.push(Seen::Response(id, response_result(r).map_err(|e| e.code)));
            }
            Some(OutputLine::Event(e)) => seen.push(Seen::Event(e.event)),
            None => panic!("not a protocol line: {line}"),
        }
    }
    seen
}

fn responses(seen: &[Seen]) -> Vec<(Option<i64>, Result<Value, ErrorCode>)> {
    seen.iter()
        .filter_map(|s| match s {
            Seen::Response(id, r) => Some((*id, r.clone())),
            Seen::Event(_) => None,
        })
        .collect()
}

#[test]
fn job_cancel_stops_a_slow_read_and_responses_keep_their_order() {
    let dir = TempDir::new("read-cancel");
    let (mut h, epoch) = with_catalog(&dir);
    h.ok(
        "recover_start",
        json!({"epoch": epoch, "include": [1], "dest": dir.join("out")}),
    );
    let started = Instant::now();
    let read = h
        .engine
        .send("read_file", &json!({"epoch": epoch, "node": SLOW_NODE}));
    let info = h.engine.send("catalog_info", &json!({}));
    let cancel = h.engine.send("job_cancel", &json!({}));
    let seen = collect(&h, Duration::from_secs(10), |seen| {
        responses(seen).len() == 3
            && seen
                .iter()
                .any(|s| matches!(s, Seen::Event(e) if e == "recover_done"))
    });
    let took = started.elapsed();
    let answered = responses(&seen);
    assert_eq!(
        answered.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![Some(read), Some(info), Some(cancel)],
        "responses come in request order"
    );
    assert_eq!(answered[0].1, Err(ErrorCode::IoError));
    assert!(answered[1].1.is_ok());
    assert_eq!(answered[2].1, Ok(json!({})));
    // The job's end follows the response to job_cancel.
    let cancel_at = seen
        .iter()
        .position(|s| matches!(s, Seen::Response(id, _) if *id == Some(cancel)))
        .unwrap();
    let done_at = seen
        .iter()
        .position(|s| matches!(s, Seen::Event(e) if e == "recover_done"))
        .unwrap();
    assert!(done_at > cancel_at);
    // Far below the read's own limit of about 5 s.
    eprintln!(
        "read_file, catalog_info, job_cancel during a slow read: all answered after {took:?}"
    );
    assert!(took < Duration::from_secs(2), "took {took:?}");
}

#[test]
fn shutdown_and_end_of_input_stop_a_slow_read() {
    let dir = TempDir::new("read-shutdown");
    let (mut h, epoch) = with_catalog(&dir);
    let started = Instant::now();
    let read = h
        .engine
        .send("read_file", &json!({"epoch": epoch, "node": SLOW_NODE}));
    let shutdown = h.engine.send("shutdown", &json!({}));
    let seen = collect(&h, Duration::from_secs(10), |seen| {
        responses(seen).len() == 2
    });
    let answered = responses(&seen);
    assert_eq!(answered[0].0, Some(read));
    assert_eq!(answered[0].1, Err(ErrorCode::IoError));
    assert_eq!(answered[1], (Some(shutdown), Ok(json!({}))));
    assert!(started.elapsed() < Duration::from_secs(2));

    // The end of input, with a recovery running.
    let dir = TempDir::new("read-eof");
    let (mut h, epoch) = with_catalog(&dir);
    h.ok(
        "recover_start",
        json!({"epoch": epoch, "include": [1], "dest": dir.join("out")}),
    );
    let read = h
        .engine
        .send("read_file", &json!({"epoch": epoch, "node": SLOW_NODE}));
    let started = Instant::now();
    let lines = h.engine.finish();
    assert!(started.elapsed() < Duration::from_secs(2));
    let read_line = lines
        .iter()
        .position(|l| l.contains(&format!("\"id\":{read}")))
        .expect("the read was answered");
    let done_line = lines
        .iter()
        .position(|l| l.contains("recover_done"))
        .expect("the job ended");
    assert!(lines[read_line].contains("io_error"));
    assert!(done_line > read_line);
    assert!(lines[done_line].contains("cancelled"));
}

#[test]
fn queries_wait_for_a_read_that_nothing_stops() {
    // A query and a job_cancel without a job do not stop the read: they
    // are answered after it, in order (the read ends by its time limit).
    let dir = TempDir::new("read-wait");
    let (mut h, epoch) = with_catalog(&dir);
    let started = Instant::now();
    let read = h
        .engine
        .send("read_file", &json!({"epoch": epoch, "node": SLOW_NODE}));
    let cancel = h.engine.send("job_cancel", &json!({}));
    let file = h
        .engine
        .send("read_file", &json!({"epoch": epoch, "node": 2}));
    let seen = collect(&h, Duration::from_secs(15), |seen| {
        responses(seen).len() == 3
    });
    let answered = responses(&seen);
    assert_eq!(answered[0].0, Some(read));
    assert_eq!(answered[0].1, Err(ErrorCode::IoError));
    assert_eq!(answered[1], (Some(cancel), Ok(json!({}))));
    assert_eq!(answered[2].0, Some(file));
    assert_eq!(answered[2].1.as_ref().unwrap()["kind"], "jpeg");
    assert!(started.elapsed() >= Duration::from_secs(4));
}
