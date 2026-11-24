//! The protocol transcript of ARCHITECTURE.md section 5.6:
//! `docs/protocol-examples.jsonl`, a conversation recorded from the real
//! engine on the `basic` fixture, one JSON object per line in the order it
//! was exchanged (requests, responses, events). The engine's tests
//! regenerate and compare it; the Dart fake engine replays it.
//!
//! [`record_basic`] plays a fixed script against an engine; [`Recorder`]
//! keeps every line; [`portable`] removes the machine's scratch directory
//! from the lines (that is the form that is stored); [`comparable`] also
//! drops what differs from run to run (progress events, speeds, times,
//! host-dependent members of the destination check) so that two recordings
//! can be compared.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::backend::Backend;
use crate::protocol::{OutputLine, ProtocolError};
use crate::server::{response_result, InProcess};

/// How long the recorder waits for one line.
const LINE_TIMEOUT: Duration = Duration::from_secs(600);

/// Records a conversation with an in-process engine.
pub struct Recorder {
    engine: InProcess,
    lines: Vec<String>,
}

impl Recorder {
    /// A recorder over a fresh engine with `backend`.
    pub fn new(backend: Arc<dyn Backend>) -> Recorder {
        Recorder {
            engine: InProcess::start(backend),
            lines: Vec::new(),
        }
    }

    fn next(&mut self) -> Result<OutputLine, String> {
        let line = self
            .engine
            .next_line(LINE_TIMEOUT)
            .ok_or("the engine wrote nothing in time")?;
        let parsed =
            OutputLine::parse(&line).ok_or_else(|| format!("not a protocol line: {line}"))?;
        self.lines.push(line);
        Ok(parsed)
    }

    /// Sends a request and records it with everything up to its response.
    pub fn call<A: Serialize>(&mut self, cmd: &str, args: &A) -> Result<Value, String> {
        let id = self.engine.send(cmd, args);
        self.lines
            .push(json!({"id": id, "cmd": cmd, "args": args}).to_string());
        loop {
            if let OutputLine::Response(response) = self.next()? {
                if response.id != Some(id) {
                    return Err(format!(
                        "response {:?} where {id} was expected",
                        response.id
                    ));
                }
                return response_result(response).map_err(|e: ProtocolError| format!("{cmd}: {e}"));
            }
        }
    }

    /// Records lines until event `name` and returns its data.
    pub fn wait(&mut self, name: &str) -> Result<Value, String> {
        loop {
            match self.next()? {
                OutputLine::Event(event) if event.event == name => return Ok(event.data),
                OutputLine::Event(_) => {}
                OutputLine::Response(r) => return Err(format!("unexpected response {r:?}")),
            }
        }
    }

    /// The recorded lines.
    pub fn into_lines(self) -> Vec<String> {
        self.lines
    }
}

fn path_of(dir: &Path, name: &str) -> String {
    dir.join(name).to_string_lossy().into_owned()
}

/// The first file and first folder below `node`, depth first, at most
/// `budget` listings.
fn find_nodes(
    r: &mut Recorder,
    epoch: &Value,
    node: u64,
    budget: &mut u32,
) -> Result<(Option<u64>, Option<u64>), String> {
    if *budget == 0 {
        return Ok((None, None));
    }
    *budget -= 1;
    let page = r.call(
        "list_children",
        &json!({"epoch": epoch, "node": node, "limit": 50}),
    )?;
    let rows = page["rows"].as_array().cloned().unwrap_or_default();
    let mut file = rows
        .iter()
        .find(|row| row["kind"] == "file" && row["size"].as_u64().unwrap_or(0) > 0)
        .and_then(|row| row["id"].as_u64());
    let folder = rows
        .iter()
        .find(|row| row["kind"] == "dir")
        .and_then(|row| row["id"].as_u64());
    if file.is_none() {
        if let Some(folder) = folder {
            file = find_nodes(r, epoch, folder, budget)?.0;
        }
    }
    Ok((file, folder))
}

/// Plays the transcript's script: `image` is the `basic` fixture, `scratch`
/// an empty directory for the session, the recovered files and the image
/// copy. Every request must succeed; the first that does not ends the
/// recording with its error.
pub fn record_basic(
    backend: Arc<dyn Backend>,
    image: &Path,
    scratch: &Path,
) -> Result<Vec<String>, String> {
    let mut r = Recorder::new(backend);
    let image = image.to_string_lossy().into_owned();
    let session = path_of(scratch, "basic.hds");
    r.call("hello", &json!({}))?;
    r.call("list_disks", &json!({}))?;
    r.call("open_source", &json!({"kind": "image", "path": image}))?;
    r.call(
        "check_destination",
        &json!({"path": path_of(scratch, "out"), "purpose": "recover", "need_bytes": 1_048_576}),
    )?;
    r.call("scan_start", &json!({"session_path": session}))?;
    let done = r.wait("scan_done")?;
    if done["status"] != "completed" {
        return Err(format!("the scan ended {}", done["status"]));
    }
    let catalog = r.call("catalog_info", &json!({}))?;
    let epoch = catalog["epoch"].clone();
    let root = catalog["instances"][0]["root"]
        .as_u64()
        .ok_or("no main root")?;
    let mut budget = 8;
    let (file, folder) = find_nodes(&mut r, &epoch, root, &mut budget)?;
    let file = file.ok_or("no file with content in the main tree")?;
    r.call("node_info", &json!({"epoch": epoch, "node": file}))?;
    r.call(
        "read_file",
        &json!({"epoch": epoch, "node": file, "offset": 0, "length": 64}),
    )?;
    r.call(
        "search",
        &json!({"epoch": epoch, "filter": {"query": "a"}, "limit": 5}),
    )?;
    let include = vec![folder.unwrap_or(root)];
    r.call(
        "selection_summary",
        &json!({"epoch": epoch, "include": include}),
    )?;
    r.call(
        "check_destination",
        &json!({"path": path_of(scratch, "out"), "purpose": "recover"}),
    )?;
    r.call(
        "recover_start",
        &json!({"epoch": epoch, "include": include, "dest": path_of(scratch, "out")}),
    )?;
    r.wait("recover_done")?;
    r.call("carve_start", &json!({"kinds": null}))?;
    r.wait("carve_done")?;
    r.call(
        "session_save",
        &json!({"path": path_of(scratch, "copy.hds")}),
    )?;
    r.call(
        "image_start",
        &json!({"start": 0, "end": 1_048_576, "dest": path_of(scratch, "head.img")}),
    )?;
    r.wait("image_done")?;
    r.call("job_cancel", &json!({}))?;
    r.call("close_source", &json!({}))?;
    r.call(
        "session_load",
        &json!({"path": session, "source": {"kind": "image", "path": image}}),
    )?;
    r.call("shutdown", &json!({}))?;
    Ok(r.into_lines())
}

fn replace_strings(value: &mut Value, from: &str, to: &str) {
    match value {
        Value::String(s) => {
            if s.contains(from) {
                *s = s.replace(from, to);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| replace_strings(v, from, to)),
        Value::Object(members) => members
            .values_mut()
            .for_each(|v| replace_strings(v, from, to)),
        _ => {}
    }
}

/// The stored form: every occurrence of the scratch directory and of the
/// fixture directory replaced by `<tmp>` and `<fixtures>`.
pub fn portable(lines: &[String], scratch: &Path, fixtures: &Path) -> Vec<String> {
    let scratch = scratch.to_string_lossy();
    let fixtures = fixtures.to_string_lossy();
    lines
        .iter()
        .map(|line| match serde_json::from_str::<Value>(line) {
            Ok(mut value) => {
                replace_strings(&mut value, &scratch, "<tmp>");
                replace_strings(&mut value, &fixtures, "<fixtures>");
                value.to_string()
            }
            Err(_) => line.clone(),
        })
        .collect()
}

/// Members whose values differ from run to run or from host to host.
const VOLATILE: [&str; 13] = [
    "speed_bps",
    "eta_seconds",
    "elapsed_seconds",
    "snapshot_time",
    "elevated",
    "os",
    "raw_disk_support",
    "free_bytes",
    "enough_space",
    "fs",
    "max_file_bytes",
    "disk_numbers",
    "disks",
];

fn mask(value: &mut Value) {
    match value {
        // A path below the scratch or fixture directory: the separator is
        // the host's.
        Value::String(s) if s.starts_with("<tmp>") || s.starts_with("<fixtures>") => {
            *s = s.replace('\\', "/");
        }
        Value::Object(members) => {
            for (key, member) in members.iter_mut() {
                if VOLATILE.contains(&key.as_str()) {
                    *member = Value::String("<volatile>".into());
                } else if key == "report_path" {
                    if let Value::String(s) = member {
                        *s = s
                            .chars()
                            .map(|c| match c {
                                '0'..='9' => '0',
                                '\\' => '/',
                                c => c,
                            })
                            .collect();
                    }
                } else {
                    mask(member);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(mask),
        _ => {}
    }
}

/// The form two recordings are compared in: portable lines with volatile
/// members masked and the size of saved session files left out.
///
/// How many progress events a job sends depends on its speed (at most 4 a
/// second), so they are compared by what does not: a run of read-phase
/// `scan_progress` events (`mft_probe`, `sweep`) becomes its last event,
/// which carries the final counters; a run of `recover_progress`,
/// `carve_progress` or `image_progress` events becomes one entry with its
/// members' names. `analyzing` and `task_progress` events are kept. Every
/// event of a run must have the members of the run's first event. `log`
/// events are left out.
pub fn comparable(lines: &[String]) -> Vec<Value> {
    let mut out = Vec::new();
    let mut save_ids = Vec::new();
    // The pending run of timing-dependent progress events: name, its
    // members' names, the last event, and whether every event matched.
    let mut run: Option<(String, Vec<String>, Value, bool)> = None;
    let keys = |data: &Value| -> Vec<String> {
        data.as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default()
    };
    let flush = |run: &mut Option<(String, Vec<String>, Value, bool)>, out: &mut Vec<Value>| {
        if let Some((name, keys, last, same_shape)) = run.take() {
            let mut entry = if name == "scan_progress" {
                let mut last = last;
                mask(&mut last);
                last
            } else {
                serde_json::json!({"event": name, "members": keys})
            };
            if !same_shape {
                entry["members_differ"] = Value::Bool(true);
            }
            out.push(entry);
        }
    };
    for line in lines {
        let Ok(mut value) = serde_json::from_str::<Value>(line) else {
            flush(&mut run, &mut out);
            out.push(Value::String(line.clone()));
            continue;
        };
        if let Some(event) = value
            .get("event")
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            if event == "log" {
                continue;
            }
            let phase = value["data"]["phase"].as_str().unwrap_or("");
            let timed = matches!(
                event.as_str(),
                "recover_progress" | "carve_progress" | "image_progress"
            ) || (event == "scan_progress" && phase != "analyzing");
            if timed {
                let members = keys(&value["data"]);
                match &mut run {
                    Some((name, first, last, same)) if *name == event => {
                        *same &= *first == members;
                        *last = value;
                    }
                    _ => {
                        flush(&mut run, &mut out);
                        run = Some((event, members, value, true));
                    }
                }
                continue;
            }
        }
        flush(&mut run, &mut out);
        if value.get("cmd").and_then(Value::as_str) == Some("session_save") {
            save_ids.push(value["id"].clone());
        }
        if value.get("ok").is_some() && save_ids.contains(&value["id"]) {
            if let Some(result) = value.get_mut("result").and_then(Value::as_object_mut) {
                result.remove("bytes");
            }
        }
        mask(&mut value);
        out.push(value);
    }
    flush(&mut run, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masking() {
        let lines = vec![
            r#"{"id":1,"cmd":"session_save","args":{"path":"/s/x"}}"#.to_string(),
            r#"{"id":1,"ok":true,"result":{"path":"/s/x","bytes":77}}"#.to_string(),
            r#"{"event":"scan_progress","data":{"speed_bps":5}}"#.to_string(),
            r#"{"event":"recover_done","data":{"report_path":"/s/HDSavior-report-20261124-031500.csv","eta_seconds":3}}"#.to_string(),
        ];
        let portable = portable(&lines, Path::new("/s"), Path::new("/f"));
        assert!(portable[0].contains("<tmp>/x"));
        let c = comparable(&portable);
        assert_eq!(c.len(), 4);
        assert_eq!(c[1]["result"], json!({"path": "<tmp>/x"}));
        assert_eq!(c[2]["data"]["speed_bps"], "<volatile>");
        assert_eq!(
            c[3]["data"]["report_path"],
            "<tmp>/HDSavior-report-00000000-000000.csv"
        );
        assert_eq!(c[3]["data"]["eta_seconds"], "<volatile>");
        // The host's path separator does not matter below <tmp>.
        let windows = vec![r#"{"id":2,"ok":true,"result":{"path":"<tmp>\\x\\y"}}"#.to_string()];
        assert_eq!(comparable(&windows)[0]["result"]["path"], "<tmp>/x/y");
    }

    #[test]
    fn progress_runs_are_compared_by_their_last_event_and_shape() {
        let read = |done: u64| {
            format!(r#"{{"event":"scan_progress","data":{{"phase":"sweep","bytes_done":{done}}}}}"#)
        };
        let fast = vec![read(4), read(8)];
        let slow = vec![read(1), read(2), read(4), read(8)];
        assert_eq!(comparable(&fast), comparable(&slow));
        assert_eq!(comparable(&fast).len(), 1);
        assert_ne!(comparable(&fast), comparable(&[read(4), read(7)]));
        // A member missing in one event of a run is seen.
        let odd = vec![
            r#"{"event":"recover_progress","data":{"files_done":1,"current":"a"}}"#.to_string(),
            r#"{"event":"recover_progress","data":{"files_done":2}}"#.to_string(),
        ];
        assert_eq!(comparable(&odd)[0]["members_differ"], true);
        // Analysis steps and task progress are kept one by one.
        let analyzing = r#"{"event":"scan_progress","data":{"phase":"analyzing","bytes_done":1}}"#;
        assert_eq!(
            comparable(&[analyzing.to_string(), analyzing.to_string()]).len(),
            2
        );
    }
}
