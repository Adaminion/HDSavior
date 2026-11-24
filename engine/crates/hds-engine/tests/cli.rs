//! The command-line interface as a child process: exit codes, usage errors
//! on stderr, results and events as JSON lines on stdout.

mod common;

use std::process::{Command, Output};

use common::{zero_image, TempDir};
use serde_json::Value;

fn run(args: &[&str], backend: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hdsavior-engine"));
    command.args(args);
    match backend {
        Some(name) => command.env("HDS_ENGINE_TEST_BACKEND", name),
        None => command.env_remove("HDS_ENGINE_TEST_BACKEND"),
    };
    command.output().expect("run the engine")
}

/// Every stdout line as JSON.
fn json_lines(output: &Output) -> Vec<Value> {
    String::from_utf8(output.stdout.clone())
        .expect("UTF-8 stdout")
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
        .collect()
}

#[test]
fn usage_errors_exit_2_with_a_message_on_stderr() {
    for args in [
        &[][..],
        &["frobnicate"],
        &["disks", "extra"],
        &["scan"],
        &["scan", "img"],
        &[
            "scan",
            "img",
            "--session",
            "s",
            "--partition",
            "1",
            "--start",
            "0",
        ],
        &["recover", "--session", "s", "--dest", "d"],
        &[
            "recover",
            "--session",
            "s",
            "--dest",
            "d",
            "--all",
            "--on-exists",
            "replace",
        ],
        &["ls", "--session", "s", "--limit", "many"],
        &["image", "img"],
        &["info"],
        &["serve", "--port", "1"],
    ] {
        let out = run(args, None);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("usage:"), "{args:?}: {stderr}");
        assert!(out.stdout.is_empty(), "{args:?}: nothing on stdout");
    }
}

#[test]
fn help_and_disks() {
    let help = run(&["--help"], None);
    assert_eq!(help.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&help.stdout).contains("hdsavior-engine scan"));

    let disks = run(&["disks"], None);
    assert_eq!(
        disks.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&disks.stderr)
    );
    let lines = json_lines(&disks);
    assert_eq!(lines.len(), 1);
    let list = lines[0]["disks"].as_array().expect("a disk list");
    if !cfg!(windows) {
        assert!(list.is_empty());
    }
    for disk in list {
        // The disk list is made without reading a sector.
        for partition in disk["partitions"].as_array().unwrap() {
            assert_eq!(partition["type"], "");
            assert_eq!(partition["type_id"], "");
            assert_eq!(partition["ntfs"], Value::Null);
        }
    }
}

#[test]
fn the_build_backend_scans_and_images() {
    let dir = TempDir::new("cli");
    let image = zero_image(&dir, "src.img", 1 << 20);
    let session = dir.join("s.hds");
    let out = run(&["scan", &image, "--session", &session], None);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lines = json_lines(&out);
    assert!(
        lines[0]["source"].is_object(),
        "the open_source result comes first"
    );
    let done = lines.last().unwrap();
    assert_eq!(done["event"], "scan_done");
    assert_eq!(done["data"]["status"], "completed");
    assert!(done["data"]["catalog"].is_object());
    let out = run(&["info", "--session", &session], None);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // A source without a single record cannot prove its identity (B4:
    // fewer than 8 samples need `force`): the session is browsed alone.
    let loaded = json_lines(&out).last().unwrap().clone();
    assert_eq!(loaded["source_attached"], false);
    assert_eq!(loaded["source_error"]["code"], "source_mismatch");
    assert_eq!(loaded["catalog"]["scan_complete"], true);
    // Imaging copies the source; a second run is refused (`exists`).
    let out = run(&["image", &image, "--dest", &dir.join("x.img")], None);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let done = json_lines(&out).last().unwrap().clone();
    assert_eq!(done["event"], "image_done");
    assert_eq!(done["data"]["bytes_written"], 1 << 20);
    assert_eq!(
        std::fs::read(dir.join("x.img")).unwrap(),
        std::fs::read(&image).unwrap()
    );
    let out = run(&["image", &image, "--dest", &dir.join("x.img")], None);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(json_lines(&out).last().unwrap()["error"]["code"], "exists");
    let out = run(
        &[
            "image",
            &image,
            "--dest",
            &dir.join("x.img"),
            "--partition",
            "3",
        ],
        None,
    );
    assert_eq!(out.status.code(), Some(1), "no partition 3");
    assert!(String::from_utf8_lossy(&out.stderr).contains("no partition 3"));
}

#[test]
fn subcommands_drive_the_same_dispatcher() {
    let dir = TempDir::new("cli");
    let image = zero_image(&dir, "src.img", 1 << 20);
    let session = dir.join("s.hds");
    let fake = Some("fake");

    let out = run(
        &[
            "scan",
            &image,
            "--session",
            &session,
            "--start",
            "512",
            "--end",
            "4096",
        ],
        fake,
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lines = json_lines(&out);
    assert_eq!(
        lines[1],
        serde_json::json!({"job": "scan", "bytes_total": 3584})
    );
    assert!(lines.iter().any(|l| l["event"] == "scan_progress"));
    let done = lines.last().unwrap();
    assert_eq!(done["event"], "scan_done");
    assert_eq!(done["data"]["status"], "completed");

    // A session path inside the source image is refused before anything is read.
    let out = run(&["scan", &image, "--session", &image], fake);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        json_lines(&out).last().unwrap()["error"]["code"],
        "destination_refused"
    );

    let out = run(&["ls", "--session", &session, "--path", "photos"], fake);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows = json_lines(&out).last().unwrap()["rows"].clone();
    assert_eq!(
        rows[0]["name"], "IMG_0001.JPG",
        "the path was resolved case-insensitively"
    );
    let out = run(
        &["ls", "--session", &session, "--path", "nothing/here"],
        fake,
    );
    assert_eq!(out.status.code(), Some(1));

    let out = run(
        &["find", "--session", &session, "img", "--ext", "jpg"],
        fake,
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(json_lines(&out).last().unwrap()["total"], 1);

    let out = run(
        &[
            "recover",
            "--session",
            &session,
            "--source",
            &image,
            "--dest",
            &dir.join("out"),
            "--all",
        ],
        fake,
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(json_lines(&out).last().unwrap()["event"], "recover_done");

    let out = run(
        &[
            "recover",
            "--session",
            &session,
            "--dest",
            &dir.join("out"),
            "--all",
        ],
        fake,
    );
    assert_eq!(out.status.code(), Some(1), "no source attached");
    assert_eq!(
        json_lines(&out).last().unwrap()["error"]["code"],
        "no_source"
    );

    let out = run(
        &[
            "carve",
            "--session",
            &session,
            "--source",
            &image,
            "--kinds",
            "jpeg,png",
        ],
        fake,
    );
    assert_eq!(out.status.code(), Some(0));
    let out = run(
        &[
            "carve",
            "--session",
            &session,
            "--source",
            &image,
            "--kinds",
            "doc",
        ],
        fake,
    );
    assert_eq!(out.status.code(), Some(1));

    let out = run(&["image", &image, "--dest", &dir.join("copy.img")], fake);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        json_lines(&out).last().unwrap()["data"]["status"],
        "completed"
    );
}

/// Ctrl-C (`SIGINT`) while a job runs: the job ends the clean way and the
/// exit code is 130 (ARCHITECTURE 5.6).
#[cfg(unix)]
mod ctrl_c {
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::{run, zero_image, TempDir};
    use serde_json::Value;

    /// Starts the engine with `args`, waits for a stdout line containing
    /// `ready`, sends `SIGINT` and returns the exit code and stdout lines.
    fn interrupt(args: &[&str], backend: Option<&str>, ready: &str) -> (Option<i32>, Vec<Value>) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_hdsavior-engine"));
        command
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        match backend {
            Some(name) => command.env("HDS_ENGINE_TEST_BACKEND", name),
            None => command.env_remove("HDS_ENGINE_TEST_BACKEND"),
        };
        let mut child: Child = command.spawn().expect("start the engine");
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut lines = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let line = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("the job started in time");
            let found = line.contains(ready);
            lines.push(line);
            if found {
                break;
            }
        }
        let killed = Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .expect("run kill");
        assert!(killed.success());
        let started = Instant::now();
        let code = loop {
            if let Some(status) = child.try_wait().expect("wait for the engine") {
                break status.code();
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the engine did not end after Ctrl-C"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        lines.extend(rx.iter());
        let values = lines
            .iter()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
            .collect();
        (code, values)
    }

    #[test]
    fn ctrl_c_cancels_a_running_image_job() {
        let dir = TempDir::new("ctrlc-image");
        let image = zero_image(&dir, "src.img", 1 << 20);
        let (code, lines) = interrupt(
            &["image", &image, "--dest", &dir.join("copy.img")],
            Some("fake-wait"),
            "image_progress",
        );
        assert_eq!(code, Some(130));
        let done = lines.last().unwrap();
        assert_eq!(done["event"], "image_done");
        assert_eq!(done["data"]["status"], "cancelled");
    }

    /// Ctrl-C right after `recover_done`, while the session is saved back
    /// (the fake's save of a `slow-save` path takes 2 s): the save is not
    /// cut off by the signal, and the exit code is 130.
    #[test]
    fn ctrl_c_during_the_save_back_lets_the_save_finish() {
        let dir = TempDir::new("ctrlc-saveback");
        let image = zero_image(&dir, "src.img", 1 << 20);
        let session = dir.join("slow-save.hds");
        let started = std::time::Instant::now();
        let (code, lines) = interrupt(
            &[
                "recover",
                "--session",
                &session,
                "--source",
                &image,
                "--dest",
                &dir.join("out"),
                "--all",
            ],
            Some("fake"),
            "recover_done",
        );
        assert_eq!(code, Some(130), "not ended by the signal");
        // the save ran to its end (2 s) before the exit
        assert!(started.elapsed() >= Duration::from_secs(2));
        let done = lines.last().unwrap();
        assert_eq!(done["event"], "recover_done");
        assert_eq!(done["data"]["status"], "completed");
    }

    #[test]
    fn ctrl_c_stops_a_scan_and_keeps_its_session() {
        let dir = TempDir::new("ctrlc-scan");
        // Large enough that the scan is still reading when Ctrl-C comes;
        // sparse, so it takes no disk space.
        let image = zero_image(&dir, "big.img", 1 << 40);
        let session = dir.join("s.hds");
        let (code, lines) = interrupt(
            &["scan", &image, "--session", &session, "--no-carve-hits"],
            None,
            "scan_progress",
        );
        assert_eq!(code, Some(130));
        let done = lines.last().unwrap();
        assert_eq!(done["event"], "scan_done");
        assert_eq!(done["data"]["status"], "stopped");
        // Stopped like at the end of input: the session is saved, no
        // catalog is built.
        assert_eq!(done["data"]["catalog"], Value::Null);
        assert!(std::path::Path::new(&session).is_file());
        // The session file holds the stopped scan.
        let info = run(&["info", "--session", &session], None);
        assert_eq!(
            info.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&info.stderr)
        );
        let lines = super::json_lines(&info);
        let loaded = lines.iter().find(|l| l["catalog"].is_object()).unwrap();
        assert_eq!(loaded["catalog"]["scan_complete"], false);
        assert!(loaded["catalog"]["scanned_bytes"].as_u64().unwrap() > 0);
    }
}
