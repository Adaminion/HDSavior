//! The error-by-state table of PROTOCOL.md, cell by cell, through the
//! in-process server with the backend test double, plus the argument checks
//! and the epoch rules the engine owns.

mod common;

use common::{zero_image, Harness, TempDir};
use hds_engine::protocol::ErrorCode::{self, *};
use hds_engine::testing::{FakeJobs, PANIC_NODE};
use serde_json::{json, Value};

const IMAGE_SIZE: u64 = 4 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    N,
    O,
    S,
    C,
    B,
    J,
}

use State::*;

/// A harness in `state`, with its scratch directory and image.
struct Setup {
    h: Harness,
    dir: TempDir,
    image: String,
    epoch: u64,
}

fn setup(state: State) -> Setup {
    let dir = TempDir::new("states");
    let image = zero_image(&dir, "source.img", IMAGE_SIZE);
    let mut h = Harness::new(FakeJobs::UntilCancelled);
    let mut epoch = 1;
    if state != N {
        h.ok("open_source", json!({"kind": "image", "path": image}));
    }
    if matches!(state, S | C | B | J) {
        h.ok("scan_start", json!({}));
    }
    if matches!(state, C | B | J) {
        h.ok("job_cancel", json!({}));
        let done = h.wait_event("scan_done");
        assert_eq!(done["status"], "stopped");
        epoch = done["catalog"]["epoch"].as_u64().unwrap();
    }
    if state == B {
        h.ok("close_source", json!({}));
    }
    if state == J {
        h.ok(
            "recover_start",
            json!({"epoch": epoch, "include": [1], "dest": dir.join("busy-out")}),
        );
    }
    Setup {
        h,
        dir,
        image,
        epoch,
    }
}

/// Valid arguments of every command for a harness.
fn requests(s: &Setup) -> Vec<(&'static str, Value)> {
    let e = s.epoch;
    vec![
        ("hello", json!({})),
        ("list_disks", json!({})),
        ("job_cancel", json!({})),
        ("open_source", json!({"kind": "image", "path": s.image})),
        ("close_source", json!({})),
        ("session_load", json!({"path": s.dir.join("loaded.hds")})),
        (
            "session_attach_source",
            json!({"kind": "image", "path": s.image}),
        ),
        (
            "geometry_set",
            json!({"epoch": e, "instance": 0, "cluster_size": 4096, "volume_offset": 0}),
        ),
        (
            "instance_set",
            json!({"epoch": e, "instance": 2, "kind": "main"}),
        ),
        (
            "session_save",
            json!({"path": s.dir.join("saved.hds"), "allow_unknown_disk": true}),
        ),
        ("scan_start", json!({})),
        ("catalog_info", json!({})),
        ("list_children", json!({"epoch": e, "node": 0})),
        ("node_info", json!({"epoch": e, "node": 2})),
        ("search", json!({"epoch": e, "filter": {"query": "img"}})),
        ("selection_summary", json!({"epoch": e, "include": [1]})),
        (
            "check_destination",
            json!({"path": s.dir.join("dest"), "purpose": "recover"}),
        ),
        (
            "recover_start",
            json!({"epoch": e, "include": [1], "dest": s.dir.join("out")}),
        ),
        ("carve_start", json!({})),
        ("image_start", json!({"dest": s.dir.join("copy.img")})),
        ("read_file", json!({"epoch": e, "node": 2})),
        ("shutdown", json!({})),
    ]
}

/// The expected outcome per state, in the order N, O, S, C, B, J
/// (`None` = answered with a result).
fn expected(cmd: &str) -> [Option<ErrorCode>; 6] {
    let ok = None;
    match cmd {
        "hello" | "list_disks" | "job_cancel" | "shutdown" => [ok; 6],
        "open_source" | "close_source" | "session_load" => [ok, ok, Some(Busy), ok, ok, Some(Busy)],
        "session_attach_source" | "session_save" => [
            Some(NoSession),
            Some(NoSession),
            Some(Busy),
            ok,
            ok,
            Some(Busy),
        ],
        "geometry_set" | "instance_set" => [
            Some(NoCatalog),
            Some(NoCatalog),
            Some(Busy),
            ok,
            ok,
            Some(Busy),
        ],
        "scan_start" => [
            Some(NoSource),
            ok,
            Some(Busy),
            ok,
            Some(NoSource),
            Some(Busy),
        ],
        "catalog_info" | "list_children" | "node_info" | "search" | "selection_summary" => [
            Some(NoCatalog),
            Some(NoCatalog),
            Some(NoCatalog),
            ok,
            ok,
            ok,
        ],
        "check_destination" => [Some(NoSource), ok, ok, ok, Some(NoSource), ok],
        "recover_start" | "carve_start" => [
            Some(NoCatalog),
            Some(NoCatalog),
            Some(NoCatalog),
            ok,
            Some(NoSource),
            Some(Busy),
        ],
        "image_start" => [
            Some(NoSource),
            ok,
            Some(Busy),
            ok,
            Some(NoSource),
            Some(Busy),
        ],
        "read_file" => [
            Some(NoCatalog),
            Some(NoCatalog),
            Some(NoCatalog),
            ok,
            Some(NoSource),
            ok,
        ],
        other => panic!("no expectation for {other}"),
    }
}

#[test]
fn error_by_state_table_cell_by_cell() {
    let states = [N, O, S, C, B, J];
    let commands: Vec<&str> = requests(&setup(N)).into_iter().map(|(c, _)| c).collect();
    assert_eq!(commands.len(), 22, "every command of PROTOCOL.md");
    let mut cells = 0;
    for (column, state) in states.into_iter().enumerate() {
        for cmd in &commands {
            let mut s = setup(state);
            let args = requests(&s)
                .into_iter()
                .find(|(c, _)| c == cmd)
                .map(|(_, a)| a)
                .unwrap();
            let got = s.h.call(cmd, args.clone()).err().map(|e| e.code);
            assert_eq!(
                got,
                expected(cmd)[column],
                "{cmd} {args} in state {state:?}"
            );
            cells += 1;
        }
    }
    assert_eq!(cells, 22 * 6);
}

#[test]
fn session_without_catalog() {
    // A scan that fails with source_lost leaves a resumable session without catalog.
    let mut s = setup(O);
    let lost = s.dir.join("lost.hds");
    s.h.ok("scan_start", json!({"session_path": lost}));
    // The fake's jobs wait for a cancel before they fail.
    s.h.ok("job_cancel", json!({}));
    let done = s.h.wait_event("scan_done");
    assert_eq!(done["status"], "failed");
    assert_eq!(done["error"]["code"], "source_lost");
    assert_eq!(done["catalog"], Value::Null);
    assert_eq!(s.h.code("catalog_info", json!({})), NoCatalog);
    assert_eq!(
        s.h.code("list_children", json!({"epoch": 1, "node": 0})),
        NoCatalog
    );
    assert_eq!(s.h.code("carve_start", json!({})), NoCatalog);
    assert_eq!(
        s.h.code(
            "geometry_set",
            json!({"epoch": 1, "instance": 0, "cluster_size": 4096, "volume_offset": 0})
        ),
        NoCatalog
    );
    // It is a session: it can be saved and resumed (with the session path as default).
    s.h.ok("session_save", json!({"path": s.dir.join("copy.hds")}));
    s.h.ok("scan_start", json!({"resume": true}));
    let resumed = s.h.fake.calls();
    assert_eq!(resumed.iter().filter(|c| *c == "scan_start").count(), 2);
    s.h.ok("job_cancel", json!({}));
    let done = s.h.wait_event("scan_done");
    assert_eq!(done["status"], "stopped");
    assert_eq!(
        done["session_path"],
        s.dir.join("copy.hds"),
        "the last saved file is the default"
    );
}

#[test]
fn resume_rules() {
    let mut s = setup(O);
    assert_eq!(s.h.code("scan_start", json!({"resume": true})), NoSession);
    s.h.ok(
        "scan_start",
        json!({"start": 512, "end": 1048576, "collect_carve_hits": false}),
    );
    s.h.ok("job_cancel", json!({}));
    s.h.wait_event("scan_done");
    for args in [
        json!({"resume": true, "start": 0}),
        json!({"resume": true, "end": 2097152}),
        json!({"resume": true, "collect_carve_hits": true}),
    ] {
        assert_eq!(s.h.code("scan_start", args.clone()), BadRequest, "{args}");
    }
    s.h.ok(
        "scan_start",
        json!({"resume": true, "start": 512, "end": 1048576, "collect_carve_hits": false}),
    );
}

#[test]
fn scan_ranges_are_validated() {
    let mut s = setup(O);
    for args in [
        json!({"start": 1}),
        json!({"end": 1000}),
        json!({"end": IMAGE_SIZE + 512}),
        json!({"start": 4096, "end": 4096}),
        json!({"start": 8192, "end": 4096}),
        json!({"start": -512}),
        json!({"start": 9223372036854775808u64}),
        json!({"start": 1.5}),
        json!({"resume": "yes"}),
    ] {
        assert_eq!(s.h.code("scan_start", args.clone()), BadRequest, "{args}");
    }
    assert!(
        s.h.fake.calls().iter().all(|c| c != "scan_start"),
        "nothing reached the backend"
    );
    let started = s.h.ok("scan_start", json!({"start": 512, "end": 4096}));
    assert_eq!(started, json!({"job": "scan", "bytes_total": 3584}));
}

#[test]
fn node_commands_check_epoch_and_arguments() {
    let mut s = setup(C);
    let e = s.epoch;
    let stale = e + 1;
    for (cmd, args) in [
        ("list_children", json!({"epoch": stale, "node": 0})),
        ("node_info", json!({"epoch": stale, "node": 0})),
        ("search", json!({"epoch": stale})),
        ("selection_summary", json!({"epoch": stale, "include": [1]})),
        (
            "recover_start",
            json!({"epoch": stale, "include": [1], "dest": s.dir.join("o")}),
        ),
        ("read_file", json!({"epoch": stale, "node": 2})),
        (
            "geometry_set",
            json!({"epoch": stale, "instance": 0, "cluster_size": 4096, "volume_offset": 0}),
        ),
        (
            "instance_set",
            json!({"epoch": stale, "instance": 2, "kind": "main"}),
        ),
    ] {
        assert_eq!(s.h.code(cmd, args.clone()), StaleCatalog, "{cmd} {args}");
    }
    for (cmd, args) in [
        ("list_children", json!({"node": 0})),
        (
            "list_children",
            json!({"epoch": e, "node": 0, "limit": 1001}),
        ),
        ("list_children", json!({"epoch": e, "node": 0, "exts": []})),
        (
            "list_children",
            json!({"epoch": e, "node": 0, "ratings": []}),
        ),
        (
            "list_children",
            json!({"epoch": e, "node": 0, "ratings": ["excellent"]}),
        ),
        (
            "list_children",
            json!({"epoch": e, "node": 0, "sort": "colour"}),
        ),
        ("search", json!({"epoch": e, "limit": 5000})),
        ("search", json!({"epoch": e, "filter": {"exts": []}})),
        ("search", json!({"epoch": e, "filter": {"exts": [".jpg"]}})),
        (
            "search",
            json!({"epoch": e, "filter": {"min_size": 5, "max_size": 1}}),
        ),
        (
            "selection_summary",
            json!({"epoch": e, "include": [1], "exclude": [1]}),
        ),
        (
            "selection_summary",
            json!({"epoch": e, "include": [1], "filter": {"ratings": []}}),
        ),
        (
            "recover_start",
            json!({"epoch": e, "include": [], "dest": s.dir.join("o")}),
        ),
        (
            "recover_start",
            json!({"epoch": e, "include": [1], "dest": ""}),
        ),
        (
            "recover_start",
            json!({"epoch": e, "include": [1], "dest": s.dir.join("o"), "on_exists": "replace"}),
        ),
        (
            "read_file",
            json!({"epoch": e, "node": 2, "length": 8388609}),
        ),
        ("carve_start", json!({"kinds": []})),
        ("carve_start", json!({"kinds": ["jpeg", "doc"]})),
        (
            "geometry_set",
            json!({"epoch": e, "instance": 0, "cluster_size": 3000, "volume_offset": 0}),
        ),
        (
            "geometry_set",
            json!({"epoch": e, "instance": 0, "cluster_size": 4096, "volume_offset": 100}),
        ),
        (
            "geometry_set",
            json!({"epoch": e, "instance": 0, "cluster_size": 4096, "volume_offset": IMAGE_SIZE}),
        ),
        (
            "instance_set",
            json!({"epoch": e, "instance": 1, "kind": "main"}),
        ),
        (
            "instance_set",
            json!({"epoch": e, "instance": 0, "kind": "older"}),
        ),
    ] {
        assert_eq!(s.h.code(cmd, args.clone()), BadRequest, "{cmd} {args}");
    }
    // Things only the backend knows.
    assert_eq!(
        s.h.code("list_children", json!({"epoch": e, "node": 77})),
        NotFound
    );
    assert_eq!(
        s.h.code("node_info", json!({"epoch": e, "node": 77})),
        NotFound
    );
    assert_eq!(
        s.h.code("read_file", json!({"epoch": e, "node": 1})),
        BadRequest,
        "a folder"
    );
    assert_eq!(
        s.h.code("read_file", json!({"epoch": e, "node": 3})),
        Unsupported
    );
    assert_eq!(
        s.h.code(
            "geometry_set",
            json!({"epoch": e, "instance": 9, "cluster_size": 4096, "volume_offset": 0})
        ),
        NotFound
    );
    assert_eq!(
        s.h.code(
            "instance_set",
            json!({"epoch": e, "instance": 9, "kind": "main"})
        ),
        NotFound
    );
    // Upper-case extensions are folded rather than refused.
    s.h.ok("search", json!({"epoch": e, "filter": {"exts": ["JPG"]}}));
    // `null` means "any", for the whole filter too (review m2).
    s.h.ok("search", json!({"epoch": e, "filter": null}));
}

#[test]
fn read_file_encodes_and_sniffs() {
    let mut s = setup(C);
    let e = s.epoch;
    let content = hds_engine::testing::file_content();
    let head =
        s.h.ok("read_file", json!({"epoch": e, "node": 2, "length": 100}));
    assert_eq!(head["size"], 1000);
    assert_eq!(head["eof"], false);
    assert_eq!(head["kind"], "jpeg");
    let data = hds_engine::base64::decode(head["data_b64"].as_str().unwrap()).unwrap();
    assert_eq!(data, content[..100]);
    let tail =
        s.h.ok("read_file", json!({"epoch": e, "node": 2, "offset": 900}));
    assert_eq!(tail["eof"], true);
    assert_eq!(tail["kind"], Value::Null, "kind only at offset 0");
    let data = hds_engine::base64::decode(tail["data_b64"].as_str().unwrap()).unwrap();
    assert_eq!(data, content[900..]);
}

#[test]
fn epochs() {
    let mut s = setup(C);
    let e1 = s.epoch;
    assert_ne!(e1, 999, "the backend's epoch is overwritten");
    assert_eq!(s.h.epoch(), e1);
    let geo = s.h.ok(
        "geometry_set",
        json!({"epoch": e1, "instance": 0, "cluster_size": 8192, "volume_offset": 1024}),
    );
    assert_eq!(geo["epoch"], e1, "geometry_set keeps the epoch");
    assert!(s
        .h
        .events
        .iter()
        .any(|e| e.event == "task_progress" && e.data["task"] == "geometry_set"));
    s.h.ok("carve_start", json!({"kinds": ["jpeg"]}));
    s.h.ok("job_cancel", json!({}));
    let carved = s.h.wait_event("carve_done");
    assert_eq!(carved["catalog"]["epoch"], e1, "carving keeps the epoch");
    let e2 = s.h.ok(
        "instance_set",
        json!({"epoch": e1, "instance": 2, "kind": "main"}),
    )["epoch"]
        .as_u64()
        .unwrap();
    assert!(e2 > e1, "instance_set starts a new epoch");
    assert_eq!(
        s.h.code("node_info", json!({"epoch": e1, "node": 2})),
        StaleCatalog
    );
    s.h.ok("node_info", json!({"epoch": e2, "node": 2}));
    let e3 = s.h.ok(
        "session_attach_source",
        json!({"kind": "image", "path": s.image}),
    )["catalog"]["epoch"]
        .as_u64()
        .unwrap();
    assert!(e3 > e2);
    let e4 = s.h.ok(
        "session_load",
        json!({"path": s.dir.join("x.hds"), "source": {"kind": "image", "path": s.image}}),
    )["catalog"]["epoch"]
        .as_u64()
        .unwrap();
    assert!(e4 > e3);
    // Any scan_start ends the epoch at once.
    s.h.ok("scan_start", json!({}));
    assert_eq!(
        s.h.code("node_info", json!({"epoch": e4, "node": 2})),
        NoCatalog
    );
    s.h.ok("job_cancel", json!({}));
    let done = s.h.wait_event("scan_done");
    let e5 = done["catalog"]["epoch"].as_u64().unwrap();
    assert!(e5 > e4, "every scan_done starts a new epoch");
    assert_eq!(
        s.h.code("node_info", json!({"epoch": e4, "node": 2})),
        StaleCatalog
    );
}

#[test]
fn session_load_and_attach() {
    let mut s = setup(N);
    assert_eq!(
        s.h.code("session_load", json!({"path": s.dir.join("missing.hds")})),
        NotFound
    );
    assert_eq!(s.h.code("session_load", json!({"path": ""})), BadRequest);
    // Without a source: browse only (B).
    let loaded = s.h.ok("session_load", json!({"path": s.dir.join("a.hds")}));
    assert_eq!(loaded["source_attached"], false);
    assert_eq!(loaded["catalog"]["source_attached"], false);
    assert_eq!(loaded["source_error"]["code"], "no_source");
    assert!(s
        .h
        .events
        .iter()
        .any(|e| e.event == "task_progress" && e.data["task"] == "session_load"));
    let e = loaded["catalog"]["epoch"].as_u64().unwrap();
    s.h.ok("list_children", json!({"epoch": e, "node": 0}));
    assert_eq!(
        s.h.code("read_file", json!({"epoch": e, "node": 2})),
        NoSource
    );
    assert_eq!(
        s.h.code(
            "check_destination",
            json!({"path": s.dir.join("d"), "purpose": "recover"})
        ),
        NoSource
    );
    // Saving without a source: the guard cannot tell where the source is.
    let refused =
        s.h.err("session_save", json!({"path": s.dir.join("b.hds")}));
    assert_eq!(refused.code, DestinationRefused);
    assert_eq!(refused.data.as_ref().unwrap()["verdict"], "unknown_disk");
    // A source that does not match.
    let mismatch = zero_image(&s.dir, "mismatch.img", IMAGE_SIZE);
    let err = s.h.err(
        "session_attach_source",
        json!({"kind": "image", "path": mismatch}),
    );
    assert_eq!(err.code, SourceMismatch);
    assert_eq!(err.data.unwrap()["checks"][0], "record_hashes");
    assert_eq!(
        s.h.code("read_file", json!({"epoch": e, "node": 2})),
        NoSource,
        "still browse only"
    );
    assert_eq!(
        s.h.code(
            "session_attach_source",
            json!({"kind": "image", "path": s.dir.join("nope.img")})
        ),
        NotFound
    );
    assert_eq!(
        s.h.code(
            "session_attach_source",
            json!({"kind": "disk", "path": "/dev/sda"})
        ),
        BadRequest
    );
    // The right one.
    let attached = s.h.ok(
        "session_attach_source",
        json!({"kind": "image", "path": s.image}),
    );
    assert_eq!(attached["catalog"]["source_attached"], true);
    let e = attached["catalog"]["epoch"].as_u64().unwrap();
    s.h.ok("read_file", json!({"epoch": e, "node": 2}));
    // A session without signature hits cannot be carved.
    s.h.ok(
        "session_load",
        json!({"path": s.dir.join("nohits.hds"), "source": {"kind": "image", "path": s.image}}),
    );
    assert_eq!(s.h.code("carve_start", json!({})), Unsupported);
}

#[test]
fn open_source_errors() {
    let mut s = setup(N);
    assert_eq!(
        s.h.code(
            "open_source",
            json!({"kind": "image", "path": s.dir.join("absent.img")})
        ),
        NotFound
    );
    assert_eq!(
        s.h.code("open_source", json!({"kind": "image", "path": ""})),
        BadRequest
    );
    assert_eq!(
        s.h.code("open_source", json!({"kind": "tape", "path": s.image})),
        BadRequest
    );
    assert_eq!(
        s.h.code(
            "open_source",
            json!({"kind": "image", "path": s.image, "sector_size": 1000})
        ),
        BadRequest
    );
    assert_eq!(
        s.h.code(
            "open_source",
            json!({"kind": "disk", "path": "C:\\not-a-disk"})
        ),
        BadRequest
    );
    if !cfg!(windows) {
        assert_eq!(
            s.h.code(
                "open_source",
                json!({"kind": "disk", "path": r"\\.\PhysicalDrive0"})
            ),
            Unsupported
        );
    }
    let opened = s.h.ok(
        "open_source",
        json!({"kind": "image", "path": s.image, "sector_size": 4096}),
    );
    assert_eq!(opened["source"]["sector_size"], 4096);
    assert_eq!(opened["source"]["kind"], "image");
    assert_eq!(opened["source"]["model"], Value::Null);
    assert_eq!(opened["table"], "none");
    assert_eq!(opened["warnings"], json!([]));
}

#[test]
fn opening_another_source_drops_the_session() {
    let mut s = setup(C);
    let other = zero_image(&s.dir, "other.img", IMAGE_SIZE);
    s.h.ok("open_source", json!({"kind": "image", "path": other}));
    assert!(s.h.fake.calls().contains(&"close_session".to_string()));
    assert_eq!(s.h.code("catalog_info", json!({})), NoCatalog);
    assert_eq!(
        s.h.code("session_save", json!({"path": s.dir.join("x.hds")})),
        NoSession
    );
}

#[test]
fn guard_refusals_never_reach_the_backend() {
    let mut s = setup(C);
    let e = s.epoch;
    // The source image itself is always refused.
    for (cmd, args) in [
        ("image_start", json!({"dest": s.image})),
        (
            "session_save",
            json!({"path": s.image, "allow_same_disk": true, "allow_unknown_disk": true}),
        ),
        (
            "scan_start",
            json!({"session_path": s.image, "allow_same_disk": true, "allow_unknown_disk": true}),
        ),
    ] {
        let err = s.h.err(cmd, args);
        assert_eq!(err.code, DestinationRefused, "{cmd}");
        let data = err.data.expect("the check as data");
        assert_eq!(data["verdict"], "overlaps_source");
        assert_eq!(data["reason"], "source_image");
        assert!(data.get("lost_files").is_some() && data.get("lost_bytes").is_some());
        assert!(!err.message.is_empty());
    }
    for (cmd, args) in [
        (
            "recover_start",
            json!({"epoch": e, "include": [1], "dest": "relative/dir"}),
        ),
        ("image_start", json!({"dest": "relative.img"})),
    ] {
        let err = s.h.err(cmd, args);
        assert_eq!(err.code, DestinationRefused, "{cmd}");
        assert_eq!(err.data.unwrap()["verdict"], "invalid_path");
    }
    let calls = s.h.fake.calls();
    for refused in ["image_start", "session_save", "recover_start"] {
        assert!(
            !calls.contains(&refused.to_string()),
            "{refused} reached the backend"
        );
    }
    assert_eq!(
        calls.iter().filter(|c| *c == "scan_start").count(),
        1,
        "only the setup's scan"
    );
    // Errors the backend owns pass through.
    assert_eq!(
        s.h.code("image_start", json!({"dest": s.dir.join("exists.img")})),
        Exists
    );
}

#[test]
fn check_destination_results() {
    let mut s = setup(O);
    let ok = s.h.ok(
        "check_destination",
        json!({"path": s.dir.join("dest"), "purpose": "recover", "need_bytes": 1}),
    );
    assert_eq!(ok["verdict"], "ok");
    assert_eq!(ok["reason"], Value::Null);
    assert_eq!(ok["lost_files"], 0);
    assert_eq!(ok["lost_bytes"], 0);
    assert!(ok.get("source_extents").is_none());
    assert!(ok["enough_space"].is_boolean() || ok["enough_space"].is_null());
    let image = s.h.ok(
        "check_destination",
        json!({"path": s.image, "purpose": "image"}),
    );
    assert_eq!(image["verdict"], "overlaps_source");
    assert_eq!(image["reason"], "source_image");
    let invalid = s.h.ok(
        "check_destination",
        json!({"path": "not/absolute", "purpose": "session"}),
    );
    assert_eq!(invalid["verdict"], "invalid_path");
    assert_eq!(
        s.h.code(
            "check_destination",
            json!({"path": s.image, "purpose": "backup"})
        ),
        BadRequest
    );
    assert_eq!(
        s.h.code("check_destination", json!({"purpose": "recover"})),
        BadRequest
    );
}

#[test]
fn panicking_handler_answers_internal_and_engine_lives_on() {
    let mut s = setup(C);
    let e = s.epoch;
    for (cmd, args) in [
        ("list_children", json!({"epoch": e, "node": PANIC_NODE})),
        ("node_info", json!({"epoch": e, "node": PANIC_NODE})),
        ("read_file", json!({"epoch": e, "node": PANIC_NODE})),
    ] {
        let err = s.h.err(cmd, args);
        assert_eq!(err.code, Internal, "{cmd}");
        assert!(
            err.message.contains("panicked") || err.message.contains("failed"),
            "{}",
            err.message
        );
    }
    s.h.ok("hello", json!({}));
    s.h.ok("list_children", json!({"epoch": e, "node": 0}));
}

#[test]
fn panicking_job_ends_failed_internal_and_frees_the_slot() {
    let mut s = setup(C);
    let e = s.epoch;
    let dest = s.dir.join("panic-out");
    s.h.ok(
        "recover_start",
        json!({"epoch": e, "include": [PANIC_NODE], "dest": dest}),
    );
    let done = s.h.wait_event("recover_done");
    assert_eq!(done["status"], "failed");
    assert_eq!(done["error"]["code"], "internal");
    assert_eq!(done["dest"], dest);
    // The slot is free again.
    s.h.ok(
        "recover_start",
        json!({"epoch": e, "include": [1], "dest": s.dir.join("again")}),
    );
}

#[test]
fn job_cancel_without_job_and_cancel_statuses() {
    let mut s = setup(O);
    assert_eq!(s.h.ok("job_cancel", json!({})), json!({}));
    s.h.ok(
        "image_start",
        json!({"dest": s.dir.join("copy.img"), "start": 0, "end": 1048576}),
    );
    // An image job without a catalog: J for jobs, no catalog for queries.
    assert_eq!(
        s.h.code(
            "recover_start",
            json!({"epoch": 1, "include": [1], "dest": s.dir.join("o")})
        ),
        Busy
    );
    assert_eq!(s.h.code("catalog_info", json!({})), NoCatalog);
    assert_eq!(
        s.h.code("open_source", json!({"kind": "image", "path": s.image})),
        Busy
    );
    s.h.ok("job_cancel", json!({}));
    let done = s.h.wait_event("image_done");
    assert_eq!(done["status"], "cancelled");
    assert_eq!(done["path"], s.dir.join("copy.img"));
    s.h.ok("open_source", json!({"kind": "image", "path": s.image}));
}
