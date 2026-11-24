//! The ordering rules of PROTOCOL.md section 1, in-process: responses in
//! request order, the `X_start` response before any event of its job,
//! `X_done` last and only once the slot is free and the state is in place.

mod common;

use common::{zero_image, Harness, TempDir};
use hds_engine::protocol::OutputLine;
use hds_engine::testing::FakeJobs;
use serde_json::json;

/// Reads lines until `done`, asserting the job's lines come in the order
/// response, progress..., done.
fn job_lines(h: &mut Harness, id: i64, progress: &str, done: &str) -> serde_json::Value {
    match h.next() {
        OutputLine::Response(r) => assert_eq!(r.id, Some(id), "the start response comes first"),
        OutputLine::Event(e) => panic!("event {} before the response to its start", e.event),
    }
    let mut seen_progress = 0;
    loop {
        match h.next() {
            OutputLine::Event(e) if e.event == progress => seen_progress += 1,
            OutputLine::Event(e) if e.event == done => {
                assert_eq!(seen_progress, 3, "all progress events precede {done}");
                return e.data;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[test]
fn start_response_precedes_events_and_done_is_last() {
    let dir = TempDir::new("ordering");
    let image = zero_image(&dir, "src.img", 1 << 20);
    let mut h = Harness::new(FakeJobs::Finish);
    h.ok("open_source", json!({"kind": "image", "path": image}));
    for round in 0..100 {
        let id = h.engine.send("scan_start", &json!({}));
        let done = job_lines(&mut h, id, "scan_progress", "scan_done");
        assert_eq!(done["status"], "completed");
        let epoch = done["catalog"]["epoch"].as_u64().unwrap();
        // The state is in place the moment scan_done is read: no busy, no stale epoch.
        assert_eq!(h.epoch(), epoch, "round {round}");
        let id = h.engine.send(
            "recover_start",
            &json!({"epoch": epoch, "include": [1], "dest": dir.join("out")}),
        );
        let done = job_lines(&mut h, id, "recover_progress", "recover_done");
        assert_eq!(done["status"], "completed");
        // The slot is free when recover_done is read.
        let id = h
            .engine
            .send("image_start", &json!({"dest": dir.join("copy.img")}));
        let done = job_lines(&mut h, id, "image_progress", "image_done");
        assert_eq!(done["status"], "completed");
        let id = h.engine.send("carve_start", &json!({}));
        let done = job_lines(&mut h, id, "carve_progress", "carve_done");
        assert_eq!(done["catalog"]["epoch"], epoch);
    }
}

#[test]
fn responses_keep_request_order_while_a_job_runs() {
    let dir = TempDir::new("ordering");
    let image = zero_image(&dir, "src.img", 1 << 20);
    let mut h = Harness::new(FakeJobs::UntilCancelled);
    h.ok("open_source", json!({"kind": "image", "path": image}));
    h.ok("scan_start", json!({}));
    h.ok("job_cancel", json!({}));
    let epoch = h.wait_event("scan_done")["catalog"]["epoch"]
        .as_u64()
        .unwrap();
    h.ok(
        "recover_start",
        json!({"epoch": epoch, "include": [1], "dest": dir.join("o")}),
    );
    // Many queries at once while the job runs: answered in order.
    let ids: Vec<i64> = (0..200)
        .map(|i| {
            if i % 2 == 0 {
                h.engine
                    .send("list_children", &json!({"epoch": epoch, "node": 0}))
            } else {
                h.engine.send(
                    "check_destination",
                    &json!({"path": dir.join("d"), "purpose": "recover"}),
                )
            }
        })
        .collect();
    let mut answered = Vec::new();
    while answered.len() < ids.len() {
        if let OutputLine::Response(r) = h.next() {
            assert!(r.ok, "{r:?}");
            answered.push(r.id.unwrap());
        }
    }
    assert_eq!(answered, ids);
    let cancel = h.engine.send("job_cancel", &json!({}));
    match h.next() {
        OutputLine::Response(r) => assert_eq!(r.id, Some(cancel), "job_cancel answers at once"),
        OutputLine::Event(e) => panic!("{} before the job_cancel response", e.event),
    }
    let done = h.wait_event("recover_done");
    assert_eq!(done["status"], "cancelled");
}

/// The last line an engine wrote after the end of its input, as an event.
fn last_event_after_eof(h: Harness) -> hds_engine::protocol::EventLine {
    let rest = h.engine.finish();
    let last = rest.last().expect("lines after end of input");
    match OutputLine::parse(last) {
        Some(OutputLine::Event(event)) => event,
        _ => panic!("the last line is not an event: {last}"),
    }
}

#[test]
fn end_of_input_ends_the_running_job_with_its_done_event() {
    let dir = TempDir::new("ordering");
    let image = zero_image(&dir, "src.img", 1 << 20);

    // A scan stops, saves its session and builds no catalog.
    let mut h = Harness::new(FakeJobs::UntilCancelled);
    h.ok("open_source", json!({"kind": "image", "path": image}));
    h.ok("scan_start", json!({"session_path": dir.join("s.hds")}));
    let done = last_event_after_eof(h);
    assert_eq!(done.event, "scan_done");
    assert_eq!(done.data["status"], "stopped");
    assert_eq!(done.data["catalog"], serde_json::Value::Null);
    assert_eq!(done.data["session_path"], dir.join("s.hds"));

    // Other jobs are cancelled.
    let mut h = Harness::new(FakeJobs::UntilCancelled);
    h.ok(
        "session_load",
        json!({"path": dir.join("x.hds"), "source": {"kind": "image", "path": image}}),
    );
    let epoch = h.epoch();
    h.ok(
        "recover_start",
        json!({"epoch": epoch, "include": [1], "dest": dir.join("o")}),
    );
    let done = last_event_after_eof(h);
    assert_eq!(done.event, "recover_done");
    assert_eq!(done.data["status"], "cancelled");
}

#[test]
fn shutdown_answers_then_ends_the_job_and_ignores_later_requests() {
    let dir = TempDir::new("ordering");
    let image = zero_image(&dir, "src.img", 1 << 20);
    let mut h = Harness::new(FakeJobs::UntilCancelled);
    h.ok("open_source", json!({"kind": "image", "path": image}));
    h.ok("image_start", json!({"dest": dir.join("copy.img")}));
    let id = h.engine.send("shutdown", &json!({}));
    h.engine.send("hello", &json!({}));
    let mut lines = Vec::new();
    while let Some(line) = h.engine.next_line(common::LINE_TIMEOUT) {
        lines.push(OutputLine::parse(&line).unwrap());
        if matches!(lines.last(), Some(OutputLine::Event(e)) if e.event == "image_done") {
            break;
        }
    }
    let responses: Vec<_> = lines
        .iter()
        .filter_map(|l| match l {
            OutputLine::Response(r) => Some(r.id),
            _ => None,
        })
        .collect();
    assert_eq!(
        responses,
        vec![Some(id)],
        "shutdown is answered, the later hello is not"
    );
    let Some(OutputLine::Event(done)) = lines.last() else {
        panic!()
    };
    assert_eq!(done.data["status"], "cancelled");
    assert!(h
        .engine
        .next_line(std::time::Duration::from_millis(200))
        .is_none());
}
