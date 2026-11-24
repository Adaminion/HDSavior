//! The protocol transcript (ARCHITECTURE.md 5.6): the recorder plays its
//! script against the backend test double here; against the real engine on
//! the `basic` fixture it regenerates `docs/protocol-examples.jsonl` and
//! compares it.

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{zero_image, TempDir};
use hds_engine::backend::build_backend;
use hds_engine::protocol::{OutputLine, Request};
use hds_engine::testing::{FakeBackend, FakeConfig};
use hds_engine::transcript::{comparable, portable, record_basic};
use serde_json::Value;

#[test]
fn the_recorder_keeps_the_exchange_in_order() {
    let dir = TempDir::new("transcript");
    let image = zero_image(&dir, "basic.img", 4 << 20);
    let scratch = TempDir::new("transcript-scratch");
    let lines = record_basic(
        Arc::new(FakeBackend::new(FakeConfig::default())),
        Path::new(&image),
        scratch.path(),
    )
    .expect("the script runs against the fake");
    let lines = portable(&lines, scratch.path(), dir.path());
    // Requests and responses alternate (events in between), ids match.
    let mut pending: Option<i64> = None;
    let mut commands = Vec::new();
    for line in &lines {
        assert!(!line.contains(&*scratch.path().to_string_lossy()), "{line}");
        if let Ok(request) = hds_engine::protocol::parse_request(line) {
            assert!(
                pending.is_none(),
                "a request before the previous response: {line}"
            );
            let Request { id, cmd, .. } = request;
            pending = Some(id);
            commands.push(cmd);
            continue;
        }
        match OutputLine::parse(line).expect("a protocol line") {
            OutputLine::Response(r) => {
                assert_eq!(r.id, pending.take(), "{line}");
                assert!(r.ok, "{line}");
            }
            OutputLine::Event(_) => {}
        }
    }
    assert!(pending.is_none());
    for cmd in [
        "hello",
        "open_source",
        "scan_start",
        "list_children",
        "read_file",
        "recover_start",
        "carve_start",
        "image_start",
        "session_load",
        "shutdown",
    ] {
        assert!(commands.iter().any(|c| c == cmd), "the script uses {cmd}");
    }
    assert!(lines
        .iter()
        .any(|l| l.contains("\"<tmp>/basic.hds\"") || l.contains(r#""<tmp>\\basic.hds""#)));
    // Two recordings compare equal once volatile parts are masked.
    let again = record_basic(
        Arc::new(FakeBackend::new(FakeConfig::default())),
        Path::new(&image),
        scratch.path(),
    )
    .unwrap();
    assert_eq!(
        comparable(&lines),
        comparable(&portable(&again, scratch.path(), dir.path()))
    );
}

/// Regenerates the transcript from the engine this build serves and compares
/// it with `docs/protocol-examples.jsonl`; `HDS_TRANSCRIPT=write` stores it.
#[test]
fn transcript_matches_the_engine() {
    let Some(image) = common::fixture("basic.img") else {
        return;
    };
    let stored = common::docs_dir().join("protocol-examples.jsonl");
    let write = std::env::var("HDS_TRANSCRIPT").as_deref() == Ok("write");
    let scratch = TempDir::new("transcript-real");
    let recorded = match record_basic(build_backend(), &image, scratch.path()) {
        Ok(lines) => lines,
        Err(error) if !stored.exists() && !write => {
            eprintln!("SKIPPED: this build cannot record the transcript yet ({error})");
            return;
        }
        Err(error) => panic!("the transcript script failed: {error}"),
    };
    let fixtures = image.parent().expect("fixture directory");
    let recorded = portable(&recorded, scratch.path(), fixtures);
    if write || !stored.exists() {
        let mut text = recorded.join("\n");
        text.push('\n');
        std::fs::write(&stored, text).expect("write the transcript");
        eprintln!("wrote {}", stored.display());
        return;
    }
    let text = std::fs::read_to_string(&stored).expect("read the transcript");
    let stored_lines: Vec<String> = text.lines().map(str::to_string).collect();
    let (a, b): (Vec<Value>, Vec<Value>) = (comparable(&stored_lines), comparable(&recorded));
    // The fixture is built by mkntfs and ntfs-3g, whose versions differ from
    // host to host, so record and file counts can differ by a few. By default
    // the transcript is compared by its shape (member names, JSON types and
    // enumeration values); the counts themselves are checked against each
    // fixture's manifest by the other tests. `HDS_TRANSCRIPT=exact` compares
    // the values too, which holds on the host that recorded the transcript.
    let exact = std::env::var("HDS_TRANSCRIPT").as_deref() == Ok("exact");
    for (i, (x, y)) in a.iter().zip(&b).enumerate() {
        if exact {
            assert_eq!(
                x, y,
                "line {i} of the comparable transcript differs; HDS_TRANSCRIPT=write regenerates it"
            );
        } else {
            assert_eq!(
                shape(x, ""),
                shape(y, ""),
                "line {i} of the transcript changed shape; HDS_TRANSCRIPT=write regenerates it\n{x}\n{y}"
            );
        }
    }
    assert_eq!(
        a.len(),
        b.len(),
        "the transcript's length changed; HDS_TRANSCRIPT=write regenerates it"
    );
}

/// Members whose string values are protocol enumerations or names fixed by
/// the script; every other string is compared by type only.
const ENUM_MEMBERS: [&str; 16] = [
    "event",
    "cmd",
    "code",
    "status",
    "phase",
    "kind",
    "job",
    "rating",
    "flags",
    "table",
    "cluster_source",
    "offset_source",
    "on_exists",
    "reason",
    "warnings",
    "verdict",
];

/// The shape of a comparable line: numbers, booleans and free strings become
/// their type, enumeration members keep their value, and an array becomes the
/// sorted set of its elements' shapes (fixture builds differ in how many
/// carved files or rows there are, not in what they look like).
fn shape(value: &Value, key: &str) -> Value {
    match value {
        Value::Number(_) => Value::String("<number>".into()),
        Value::Bool(_) => Value::String("<bool>".into()),
        Value::String(s) if ENUM_MEMBERS.contains(&key) || s.starts_with('<') => {
            Value::String(s.clone())
        }
        Value::String(_) => Value::String("<string>".into()),
        Value::Array(items) => {
            let mut shapes: Vec<Value> = items.iter().map(|v| shape(v, key)).collect();
            shapes.sort_by_key(|v| v.to_string());
            shapes.dedup();
            Value::Array(shapes)
        }
        Value::Object(members) => Value::Object(
            members
                .iter()
                .map(|(k, v)| (k.clone(), shape(v, k)))
                .collect(),
        ),
        Value::Null => Value::Null,
    }
}

/// The stored transcript (no fixture needed): every job runs to its
/// `*_done`, and no path of the machine that recorded it is left.
#[test]
fn the_stored_transcript_shows_every_job_and_no_machine_path() {
    let stored = common::docs_dir().join("protocol-examples.jsonl");
    let Ok(text) = std::fs::read_to_string(&stored) else {
        eprintln!("SKIPPED: {} does not exist yet", stored.display());
        return;
    };
    let mut done = Vec::new();
    for line in text.lines() {
        let value: Value = serde_json::from_str(line).expect("one JSON object per line");
        for prefix in ["/tmp/", "/home/", "/root/", "C:\\"] {
            assert!(!line.contains(prefix), "a machine path in {line}");
        }
        if let Some(event) = value["event"].as_str() {
            if event.ends_with("_done") {
                assert_eq!(value["data"]["status"], "completed", "{line}");
                done.push(event.to_string());
            }
        }
        if value.get("ok").is_some() {
            assert_eq!(value["ok"], true, "{line}");
        }
    }
    assert_eq!(
        done,
        ["scan_done", "recover_done", "carve_done", "image_done"],
        "the script's jobs, in order"
    );
}
