//! The command-line interface (ARCHITECTURE.md section 5.6).
//!
//! `serve` runs the protocol server on stdin / stdout. Every other
//! subcommand is a thin wrapper: it starts the same engine in-process
//! ([`InProcess`]), sends it protocol requests, and prints each result and
//! event as one JSON object per line on stdout (a result as its bare object,
//! an event as `{"event", "data"}`, an error as `{"error": {...}}`); logs go
//! to stderr. Exit code 0 = ok, 1 = a command or the job failed, 2 = usage
//! error, 130 = the job was stopped with Ctrl-C.
//!
//! Ctrl-C while a job runs ([`CtrlC`]) ends the job the clean way: a scan
//! gets `shutdown` (it stops reading and saves its session; no catalog is
//! built), the other jobs get `job_cancel` (a recovery removes its current
//! part file; `recover` and `carve` then save the session back as after
//! any job). A second Ctrl-C ends the process at once. For `recover` and
//! `carve` the first Ctrl-C is caught until the session is saved back: one
//! pressed during that save lets it finish and then exits with 130.
//!
//! The parser is hand-written: a dozen options do not justify a dependency.
//! Options take their value as the next argument or after `=`; `--` ends the
//! options.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::backend::Backend;
use crate::interrupt::CtrlC;
use crate::output::log_warn;
use crate::protocol::{OutputLine, ProtocolError, SourceKind};
use crate::server::{serve, InProcess};

/// Exit code: success.
pub const EXIT_OK: u8 = 0;
/// Exit code: a command or the job failed.
pub const EXIT_FAILED: u8 = 1;
/// Exit code: usage error.
pub const EXIT_USAGE: u8 = 2;
/// Exit code: the job was stopped with Ctrl-C (128 + `SIGINT`, as shells
/// report a process ended by it).
pub const EXIT_INTERRUPTED: u8 = 130;

/// The usage text.
pub const USAGE: &str = r"usage:
  hdsavior-engine serve
  hdsavior-engine disks
  hdsavior-engine scan    <source> --session <file> [--kind disk|image] [--partition K | --start N --end N]
                          [--no-carve-hits] [--resume | --force]
  hdsavior-engine info    --session <file> [--source <path>]
  hdsavior-engine ls      --session <file> [--node ID | --path P] [--deleted] [--limit N]
  hdsavior-engine find    --session <file> <query> [--ext a,b] [--deleted] [--limit N]
  hdsavior-engine recover --session <file> --dest <dir> [--source <path>] (--all | --include IDs) [--exclude IDs]
                          [--include-deleted] [--skip-damaged] [--allow-same-disk] [--allow-unknown-disk]
                          [--on-exists rename|skip|overwrite]
  hdsavior-engine carve   --session <file> [--source <path>] [--kinds a,b]
                          [--allow-same-disk] [--allow-unknown-disk]
  hdsavior-engine image   <source> --dest <file> [--partition K | --start N --end N] [--resume]
                          [--allow-same-disk] [--allow-unknown-disk]

A source is \\.\PhysicalDrive<N> (a disk) or the path of an image file.
scan refuses to replace an existing session file unless --force is given;
--resume continues it. recover and carve save the session back to --session.
info fails when --source is given and is not the session's source.
Results and events are printed as JSON lines on stdout, logs on stderr.
Ctrl-C stops a running job cleanly (a scan saves its session); a second
Ctrl-C ends the program at once.
Exit codes: 0 ok, 1 failed, 2 usage error, 130 stopped with Ctrl-C.";

/// A usage error: the message for stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError(pub String);

fn usage<T>(message: impl Into<String>) -> Result<T, UsageError> {
    Err(UsageError(message.into()))
}

/// Whether an option takes a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arity {
    Flag,
    Value,
}

/// Parsed arguments of one subcommand.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    /// Positional arguments.
    pub positional: Vec<String>,
    /// Options with a value.
    pub values: BTreeMap<String, String>,
    /// Flags given.
    pub flags: BTreeSet<String>,
}

impl Parsed {
    fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }

    fn required(&self, name: &str) -> Result<&str, UsageError> {
        self.value(name)
            .map_or_else(|| usage(format!("{name} is required")), Ok)
    }

    fn number(&self, name: &str) -> Result<Option<u64>, UsageError> {
        self.value(name).map(|v| parse_number(name, v)).transpose()
    }
}

fn parse_number(name: &str, text: &str) -> Result<u64, UsageError> {
    match text.parse::<u64>() {
        Ok(n) if n <= i64::MAX as u64 => Ok(n),
        _ => usage(format!("{name}: \"{text}\" is not a number")),
    }
}

fn parse_ids(name: &str, text: &str) -> Result<Vec<u64>, UsageError> {
    let ids = text
        .split(',')
        .map(|part| parse_number(name, part.trim()))
        .collect::<Result<Vec<u64>, UsageError>>()?;
    if ids.is_empty() {
        return usage(format!("{name} names no node"));
    }
    Ok(ids)
}

fn parse_list(text: &str) -> Vec<String> {
    text.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Parses `args` against a spec of options; `positional` is the number of
/// positional arguments expected.
fn parse(args: &[String], spec: &[(&str, Arity)], positional: usize) -> Result<Parsed, UsageError> {
    let mut parsed = Parsed::default();
    let mut options_done = false;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        if options_done || !arg.starts_with("--") || arg == "-" {
            parsed.positional.push(arg.clone());
            continue;
        }
        if arg == "--" {
            options_done = true;
            continue;
        }
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        let Some(&(name, arity)) = spec.iter().find(|(n, _)| *n == name) else {
            return usage(format!("unknown option {name}"));
        };
        match arity {
            Arity::Flag => {
                if inline.is_some() {
                    return usage(format!("{name} takes no value"));
                }
                if !parsed.flags.insert(name.to_string()) {
                    return usage(format!("{name} is given twice"));
                }
            }
            Arity::Value => {
                let value = match inline {
                    Some(value) => value,
                    None => {
                        let Some(value) = args.get(i) else {
                            return usage(format!("{name} needs a value"));
                        };
                        i += 1;
                        value.clone()
                    }
                };
                if parsed.values.insert(name.to_string(), value).is_some() {
                    return usage(format!("{name} is given twice"));
                }
            }
        }
    }
    if parsed.positional.len() != positional {
        return usage(format!(
            "expected {positional} argument{} before or between the options, got {}",
            if positional == 1 { "" } else { "s" },
            parsed.positional.len()
        ));
    }
    Ok(parsed)
}

use Arity::{Flag, Value as Val};

const RANGE: [(&str, Arity); 3] = [("--partition", Val), ("--start", Val), ("--end", Val)];
const CONSENT: [(&str, Arity); 2] = [("--allow-same-disk", Flag), ("--allow-unknown-disk", Flag)];

/// A subcommand with its validated arguments.
#[derive(Debug, PartialEq, Eq)]
pub enum Subcommand {
    /// `serve`
    Serve,
    /// `disks`
    Disks,
    /// `scan`
    Scan(Parsed),
    /// `info`
    Info(Parsed),
    /// `ls`
    Ls(Parsed),
    /// `find`
    Find(Parsed),
    /// `recover`
    Recover(Parsed),
    /// `carve`
    Carve(Parsed),
    /// `image`
    Image(Parsed),
    /// `help`, `--help`, `-h`
    Help,
}

fn spec(parts: &[&[(&'static str, Arity)]]) -> Vec<(&'static str, Arity)> {
    parts.iter().flat_map(|p| p.iter().copied()).collect()
}

fn check_range_options(p: &Parsed) -> Result<(), UsageError> {
    if p.value("--partition").is_some()
        && (p.value("--start").is_some() || p.value("--end").is_some())
    {
        return usage("--partition cannot be combined with --start / --end");
    }
    p.number("--partition")?;
    p.number("--start")?;
    p.number("--end")?;
    Ok(())
}

/// Parses the command line (without the program name).
pub fn parse_command_line(args: &[String]) -> Result<Subcommand, UsageError> {
    let Some((name, rest)) = args.split_first() else {
        return usage("no subcommand given");
    };
    let session = ("--session", Val);
    let source = ("--source", Val);
    let limit = ("--limit", Val);
    let deleted = ("--deleted", Flag);
    match name.as_str() {
        "help" | "--help" | "-h" => Ok(Subcommand::Help),
        "serve" => parse(rest, &[], 0).map(|_| Subcommand::Serve),
        "disks" => parse(rest, &[], 0).map(|_| Subcommand::Disks),
        "scan" => {
            let p = parse(
                rest,
                &spec(&[
                    &[
                        session,
                        ("--kind", Val),
                        ("--no-carve-hits", Flag),
                        ("--resume", Flag),
                        ("--force", Flag),
                    ],
                    &RANGE,
                ]),
                1,
            )?;
            p.required("--session")?;
            if p.flag("--resume") && p.flag("--force") {
                return usage("--resume and --force cannot be combined");
            }
            if let Some(kind) = p.value("--kind") {
                if kind != "disk" && kind != "image" {
                    return usage("--kind must be disk or image");
                }
            }
            check_range_options(&p)?;
            Ok(Subcommand::Scan(p))
        }
        "info" => {
            let p = parse(rest, &[session, source], 0)?;
            p.required("--session")?;
            Ok(Subcommand::Info(p))
        }
        "ls" => {
            let p = parse(
                rest,
                &[session, ("--node", Val), ("--path", Val), deleted, limit],
                0,
            )?;
            p.required("--session")?;
            if p.value("--node").is_some() && p.value("--path").is_some() {
                return usage("--node and --path cannot be combined");
            }
            p.number("--node")?;
            p.number("--limit")?;
            Ok(Subcommand::Ls(p))
        }
        "find" => {
            let p = parse(rest, &[session, ("--ext", Val), deleted, limit], 1)?;
            p.required("--session")?;
            p.number("--limit")?;
            Ok(Subcommand::Find(p))
        }
        "recover" => {
            let p = parse(
                rest,
                &spec(&[
                    &[
                        session,
                        ("--dest", Val),
                        source,
                        ("--all", Flag),
                        ("--include", Val),
                        ("--exclude", Val),
                        ("--include-deleted", Flag),
                        ("--skip-damaged", Flag),
                        ("--on-exists", Val),
                    ],
                    &CONSENT,
                ]),
                0,
            )?;
            p.required("--session")?;
            p.required("--dest")?;
            match (p.flag("--all"), p.value("--include")) {
                (true, Some(_)) => return usage("--all and --include cannot be combined"),
                (false, None) => return usage("one of --all or --include is required"),
                (false, Some(ids)) => {
                    parse_ids("--include", ids)?;
                }
                (true, None) => {}
            }
            if let Some(ids) = p.value("--exclude") {
                parse_ids("--exclude", ids)?;
            }
            if let Some(mode) = p.value("--on-exists") {
                if !["rename", "skip", "overwrite"].contains(&mode) {
                    return usage("--on-exists must be rename, skip or overwrite");
                }
            }
            Ok(Subcommand::Recover(p))
        }
        "carve" => {
            let p = parse(
                rest,
                &spec(&[&[session, source, ("--kinds", Val)], &CONSENT]),
                0,
            )?;
            p.required("--session")?;
            if p.value("--kinds").is_some_and(|k| parse_list(k).is_empty()) {
                return usage("--kinds names no kind");
            }
            Ok(Subcommand::Carve(p))
        }
        "image" => {
            let p = parse(
                rest,
                &spec(&[&[("--dest", Val), ("--resume", Flag)], &RANGE, &CONSENT]),
                1,
            )?;
            p.required("--dest")?;
            check_range_options(&p)?;
            Ok(Subcommand::Image(p))
        }
        other => usage(format!("unknown subcommand \"{other}\"")),
    }
}

/// Runs the command line `args` (without the program name) with `backend`
/// and returns the exit code.
pub fn run(args: Vec<OsString>, backend: Arc<dyn Backend>) -> u8 {
    let args: Option<Vec<String>> = args.into_iter().map(|a| a.into_string().ok()).collect();
    let Some(args) = args else {
        eprintln!("hdsavior-engine: an argument is not valid Unicode\n\n{USAGE}");
        return EXIT_USAGE;
    };
    let command = match parse_command_line(&args) {
        Ok(command) => command,
        Err(UsageError(message)) => {
            eprintln!("hdsavior-engine: {message}\n\n{USAGE}");
            return EXIT_USAGE;
        }
    };
    match command {
        Subcommand::Help => {
            println!("{USAGE}");
            EXIT_OK
        }
        Subcommand::Serve => {
            serve(io::stdin(), io::stdout(), backend);
            EXIT_OK
        }
        other => {
            let mut client = Client {
                engine: InProcess::start(backend),
            };
            let code = match client.execute(other) {
                Ok(()) => EXIT_OK,
                Err(Failure::Usage(message)) => {
                    eprintln!("hdsavior-engine: {message}\n\n{USAGE}");
                    EXIT_USAGE
                }
                Err(Failure::Failed(message)) => {
                    eprintln!("hdsavior-engine: {message}");
                    EXIT_FAILED
                }
                Err(Failure::Interrupted(message)) => {
                    eprintln!("hdsavior-engine: {message}");
                    EXIT_INTERRUPTED
                }
            };
            drop(client);
            code
        }
    }
}

enum Failure {
    Usage(String),
    Failed(String),
    /// The job was stopped with Ctrl-C.
    Interrupted(String),
}

/// What Ctrl-C sends to the engine while a job runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnCtrlC {
    /// `job_cancel`: the job ends `cancelled` (or `stopped`).
    Cancel,
    /// `shutdown`: a scan saves its session without building a catalog,
    /// and the engine ends.
    Shutdown,
}

impl From<UsageError> for Failure {
    fn from(e: UsageError) -> Self {
        Failure::Usage(e.0)
    }
}

/// Prints one JSON line on stdout. A closed stdout is not an error worth more
/// than stopping to print.
fn print_line(value: &Value) {
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

/// Infers the kind of a source from its path.
fn source_kind(path: &str) -> SourceKind {
    if path.starts_with(r"\\.\") {
        SourceKind::Disk
    } else {
        SourceKind::Image
    }
}

/// A file-system path made absolute (the protocol takes absolute paths).
/// Disk names are left alone.
fn absolute(path: &str) -> String {
    if path.starts_with(r"\\.\") {
        return path.to_string();
    }
    std::path::absolute(Path::new(path))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

struct Client {
    engine: InProcess,
}

impl Client {
    /// Sends one request, prints its events and result, and returns the result.
    fn call<A: Serialize>(&mut self, cmd: &str, args: &A) -> Result<Value, Failure> {
        let called = self.engine.call(cmd, args);
        for event in &called.events {
            print_line(&json!({"event": event.event, "data": event.data}));
        }
        match called.result {
            Ok(result) => {
                print_line(&result);
                Ok(result)
            }
            Err(error) => {
                print_line(&json!({ "error": error }));
                Err(Failure::Failed(describe(cmd, &error)))
            }
        }
    }

    /// Prints events until `done` arrives; fails unless its status is
    /// `completed`. The first Ctrl-C meanwhile sends `on_ctrl_c`.
    fn wait_done(&mut self, done: &str, on_ctrl_c: OnCtrlC) -> Result<Value, Failure> {
        let ctrl_c = CtrlC::catch();
        self.wait_done_with(&ctrl_c, done, on_ctrl_c)
    }

    /// [`wait_done`](Self::wait_done) under a [`CtrlC`] the caller keeps
    /// alive for what follows the job.
    fn wait_done_with(
        &mut self,
        ctrl_c: &CtrlC,
        done: &str,
        on_ctrl_c: OnCtrlC,
    ) -> Result<Value, Failure> {
        let mut interrupted = false;
        loop {
            if !interrupted && ctrl_c.pressed() {
                interrupted = true;
                let cmd = match on_ctrl_c {
                    OnCtrlC::Cancel => "job_cancel",
                    OnCtrlC::Shutdown => "shutdown",
                };
                log_warn(&format!(
                    "Ctrl-C: stopping the job ({cmd}); press Ctrl-C again to end at once"
                ));
                self.engine.send(cmd, &json!({}));
            }
            let line = match self.engine.next_within(Duration::from_millis(100)) {
                Ok(Some(line)) => Some(line),
                Ok(None) => continue,
                Err(()) => None,
            };
            match line {
                Some(OutputLine::Event(event)) => {
                    print_line(&json!({"event": event.event, "data": event.data}));
                    if event.event == done {
                        let status = event
                            .data
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        return if status == "completed" {
                            Ok(event.data)
                        } else if interrupted {
                            Err(Failure::Interrupted(format!(
                                "{done}: stopped with Ctrl-C (status \"{status}\")"
                            )))
                        } else {
                            Err(Failure::Failed(format!(
                                "{done}: the job ended with status \"{status}\""
                            )))
                        };
                    }
                }
                Some(OutputLine::Response(_)) => {}
                None => return Err(Failure::Failed("the engine ended unexpectedly".into())),
            }
        }
    }

    fn open(&mut self, path: &str, kind: Option<&str>) -> Result<Value, Failure> {
        let kind = match kind {
            Some("disk") => SourceKind::Disk,
            Some(_) => SourceKind::Image,
            None => source_kind(path),
        };
        self.call(
            "open_source",
            &json!({"kind": kind, "path": absolute(path)}),
        )
    }

    /// The range options against the partitions of an `open_source` result.
    fn range(p: &Parsed, opened: &Value) -> Result<(Option<u64>, Option<u64>), Failure> {
        if let Some(index) = p.number("--partition")? {
            let partition = opened["partitions"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|part| part["index"].as_u64() == Some(index))
                .ok_or_else(|| Failure::Failed(format!("the source has no partition {index}")))?;
            let offset = partition["offset"].as_u64().unwrap_or(0);
            let size = partition["size"].as_u64().unwrap_or(0);
            return Ok((Some(offset), Some(offset.saturating_add(size))));
        }
        Ok((p.number("--start")?, p.number("--end")?))
    }

    fn load(&mut self, p: &Parsed) -> Result<Value, Failure> {
        let session = absolute(p.required("--session")?);
        let source = p
            .value("--source")
            .map(|path| json!({"kind": source_kind(path), "path": absolute(path)}));
        self.call("session_load", &json!({"path": session, "source": source}))
    }

    fn execute(&mut self, command: Subcommand) -> Result<(), Failure> {
        match command {
            Subcommand::Serve | Subcommand::Help => Ok(()),
            Subcommand::Disks => self.call("list_disks", &json!({})).map(|_| ()),
            Subcommand::Scan(p) => {
                let source = &p.positional[0];
                let opened = self.open(source, p.value("--kind"))?;
                let (start, end) = Self::range(&p, &opened)?;
                let session = absolute(p.required("--session")?);
                if p.flag("--resume") {
                    let kind = opened["source"]["kind"].clone();
                    let loaded = self.call(
                        "session_load",
                        &json!({"path": session, "source": {"kind": kind, "path": absolute(source)}}),
                    )?;
                    if loaded["source_attached"] != true {
                        // The user named the source. A session too young to
                        // be compared (fewer than 8 stored records) is
                        // attached with force; any other mismatch fails.
                        if !too_few_samples_only(&loaded["source_error"]) {
                            return Err(Failure::Failed(format!(
                                "{source} is not the source of the session {session}"
                            )));
                        }
                        log_warn(&format!(
                            "the session {session} holds too few records to recognise {source};                              attaching it because it was named on the command line"
                        ));
                        self.call(
                            "session_attach_source",
                            &json!({"kind": kind, "path": absolute(source), "force": true}),
                        )?;
                    }
                    self.call(
                        "scan_start",
                        &json!({"resume": true, "start": start, "end": end, "session_path": session}),
                    )?;
                } else {
                    if !p.flag("--force") && is_session_file(&session) {
                        return Err(Failure::Failed(format!(
                            "{session} exists: --resume continues that session, --force replaces it"
                        )));
                    }
                    self.call(
                        "scan_start",
                        &json!({
                            "start": start, "end": end, "session_path": session,
                            "collect_carve_hits": !p.flag("--no-carve-hits"),
                        }),
                    )?;
                }
                self.wait_done("scan_done", OnCtrlC::Shutdown).map(|_| ())
            }
            Subcommand::Info(p) => {
                let loaded = self.load(&p)?;
                if p.value("--source").is_some() && loaded["source_attached"] != true {
                    return Err(Failure::Failed(format!(
                        "{} is not the source of this session",
                        p.value("--source").unwrap_or_default()
                    )));
                }
                Ok(())
            }
            Subcommand::Ls(p) => {
                let loaded = self.load(&p)?;
                let epoch = loaded["catalog"]["epoch"].clone();
                let show_deleted = p.flag("--deleted");
                let node = match (p.number("--node")?, p.value("--path")) {
                    (Some(node), _) => node,
                    (None, Some(path)) => self.resolve(&loaded, path, show_deleted)?,
                    (None, None) => main_root(&loaded),
                };
                self.call(
                    "list_children",
                    &json!({
                        "epoch": epoch, "node": node, "show_deleted": show_deleted,
                        "limit": p.number("--limit")?.unwrap_or(200),
                    }),
                )
                .map(|_| ())
            }
            Subcommand::Find(p) => {
                let loaded = self.load(&p)?;
                let exts = p.value("--ext").map(parse_list);
                self.call(
                    "search",
                    &json!({
                        "epoch": loaded["catalog"]["epoch"],
                        "filter": {"query": p.positional[0], "exts": exts},
                        "show_deleted": p.flag("--deleted"),
                        "limit": p.number("--limit")?.unwrap_or(200),
                    }),
                )
                .map(|_| ())
            }
            Subcommand::Recover(p) => {
                let loaded = self.load(&p)?;
                let include = match p.value("--include") {
                    Some(ids) => parse_ids("--include", ids)?,
                    None => loaded["catalog"]["instances"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|i| i["root"].as_u64())
                        .collect(),
                };
                let exclude = match p.value("--exclude") {
                    Some(ids) => parse_ids("--exclude", ids)?,
                    None => Vec::new(),
                };
                self.call(
                    "recover_start",
                    &json!({
                        "epoch": loaded["catalog"]["epoch"],
                        "include": include, "exclude": exclude,
                        "dest": absolute(p.required("--dest")?),
                        "include_deleted": p.flag("--include-deleted"),
                        "skip_damaged": p.flag("--skip-damaged"),
                        "allow_same_disk": p.flag("--allow-same-disk"),
                        "allow_unknown_disk": p.flag("--allow-unknown-disk"),
                        "on_exists": p.value("--on-exists").unwrap_or("rename"),
                    }),
                )?;
                // What the reads found (G1) is part of the session.
                self.job_then_save_back(&p, "recover_done")
            }
            Subcommand::Carve(p) => {
                self.load(&p)?;
                let kinds = p.value("--kinds").map(parse_list);
                self.call("carve_start", &json!({ "kinds": kinds }))?;
                // The carved files are part of the session (I5).
                self.job_then_save_back(&p, "carve_done")
            }
            Subcommand::Image(p) => {
                let opened = self.open(&p.positional[0], None)?;
                let (start, end) = Self::range(&p, &opened)?;
                self.call(
                    "image_start",
                    &json!({
                        "start": start.unwrap_or(0), "end": end,
                        "dest": absolute(p.required("--dest")?),
                        "resume": p.flag("--resume"),
                        "allow_same_disk": p.flag("--allow-same-disk"),
                        "allow_unknown_disk": p.flag("--allow-unknown-disk"),
                    }),
                )?;
                self.wait_done("image_done", OnCtrlC::Cancel).map(|_| ())
            }
        }
    }

    /// Waits for the job's `done` event, then saves the session back. The
    /// first Ctrl-C stays caught until the save has ended: during the job
    /// it cancels the job, after it the save still completes (no partial
    /// temporary session file is left) and the exit code is 130.
    fn job_then_save_back(&mut self, p: &Parsed, done: &str) -> Result<(), Failure> {
        let ctrl_c = CtrlC::catch();
        let result = self.wait_done_with(&ctrl_c, done, OnCtrlC::Cancel);
        let pressed_during_job = ctrl_c.pressed();
        self.save_back(p)?;
        result?;
        if !pressed_during_job && ctrl_c.pressed() {
            return Err(Failure::Interrupted(format!(
                "Ctrl-C after {done}: the session was saved back first"
            )));
        }
        Ok(())
    }

    /// Saves the session back to `--session` (with the consent options):
    /// the loaded file is not always written to (it may need consents the
    /// load does not give), and what a job added must not be lost.
    /// Only an error is printed: the job's `*_done` event stays the last
    /// line of a successful run.
    fn save_back(&mut self, p: &Parsed) -> Result<(), Failure> {
        let called = self.engine.call(
            "session_save",
            &json!({
                "path": absolute(p.required("--session")?),
                "allow_same_disk": p.flag("--allow-same-disk"),
                "allow_unknown_disk": p.flag("--allow-unknown-disk"),
            }),
        );
        match called.result {
            Ok(_) => Ok(()),
            Err(error) => {
                print_line(&json!({ "error": error }));
                Err(Failure::Failed(describe("session_save", &error)))
            }
        }
    }

    /// Finds the node at display path `path` below the main root by listing
    /// folder after folder (names compared case-insensitively).
    fn resolve(&mut self, loaded: &Value, path: &str, show_deleted: bool) -> Result<u64, Failure> {
        let epoch = loaded["catalog"]["epoch"].clone();
        let mut node = main_root(loaded);
        for component in path.split(['/', '\\']).filter(|c| !c.is_empty()) {
            let wanted = component.to_lowercase();
            let mut offset = 0u64;
            let found = loop {
                let page = self.engine.call(
                    "list_children",
                    &json!({"epoch": epoch, "node": node, "offset": offset, "limit": 1000,
                            "show_deleted": show_deleted}),
                );
                let page = page
                    .result
                    .map_err(|e| Failure::Failed(describe("list_children", &e)))?;
                let rows = page["rows"].as_array().cloned().unwrap_or_default();
                if let Some(row) = rows.iter().find(|r| {
                    r["name"]
                        .as_str()
                        .is_some_and(|n| n.to_lowercase() == wanted)
                }) {
                    break row["id"].as_u64();
                }
                let total = page["total"].as_u64().unwrap_or(0);
                offset = offset.saturating_add(rows.len() as u64);
                if rows.is_empty() || offset >= total {
                    break None;
                }
            };
            node = found.ok_or_else(|| {
                Failure::Failed(format!("\"{path}\": no entry named \"{component}\""))
            })?;
        }
        Ok(node)
    }
}

/// Whether `path` holds an HDSavior session file (its magic bytes). Reads
/// four bytes; anything else at the path is the engine's to refuse.
fn is_session_file(path: &str) -> bool {
    use std::io::Read;
    let mut magic = [0_u8; 4];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok_and(|()| magic == hds_core::session::file::MAGIC)
}

/// Whether a `session_load` `source_error` says only that the session is
/// too young to be compared (`source_mismatch` with `failed` exactly
/// `["too_few_samples"]`).
fn too_few_samples_only(error: &Value) -> bool {
    error["code"] == "source_mismatch" && error["data"]["failed"] == json!(["too_few_samples"])
}

fn main_root(loaded: &Value) -> u64 {
    loaded["catalog"]["instances"][0]["root"]
        .as_u64()
        .unwrap_or(0)
}

fn describe(cmd: &str, error: &ProtocolError) -> String {
    format!("{cmd} failed: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn parses_every_subcommand() {
        for line in [
            "serve",
            "disks",
            "help",
            "scan img --session s",
            r"scan \\.\PhysicalDrive1 --session s --kind disk --partition 2 --no-carve-hits --resume",
            "scan img --session=s --start 0 --end 1048576",
            "scan img --session s --force",
            "carve --session s --allow-same-disk --allow-unknown-disk",
            "info --session s --source img",
            "ls --session s --node 4 --deleted --limit 10",
            "ls --session s --path Photos/2024",
            "find --session s img_ --ext jpg,png --deleted --limit 5",
            "recover --session s --dest d --all --exclude 3,4 --include-deleted --skip-damaged --allow-same-disk --allow-unknown-disk --on-exists skip",
            "recover --session s --dest d --source img --include 17,99",
            "carve --session s --source img --kinds jpeg,png",
            "image img --dest out.img --partition 1 --resume --allow-same-disk --allow-unknown-disk",
            "image img --dest out.img --start 512",
        ] {
            parse_command_line(&args(line)).unwrap_or_else(|e| panic!("{line}: {e:?}"));
        }
    }

    #[test]
    fn refuses_misuse() {
        for line in [
            "",
            "bogus",
            "serve extra",
            "disks --limit 3",
            "scan --session s",
            "scan img",
            "scan img --session s --kind tape",
            "scan img --session s --partition 1 --start 0",
            "scan img --session s --start -5",
            "scan img --session s --start 9223372036854775808",
            "scan img img2 --session s",
            "scan img --session s --session t",
            "scan img --session",
            "scan img --session s --resume=yes",
            "scan img --session s --resume --force",
            "ls --session s --node 1 --path a",
            "ls --session s --node x",
            "find --session s",
            "recover --session s --dest d",
            "recover --session s --dest d --all --include 1",
            "recover --session s --dest d --include 1,x",
            "recover --session s --dest d --all --on-exists replace",
            "recover --session s --all",
            "carve --session s --kinds ,",
            "image img",
            "image --dest x",
        ] {
            assert!(parse_command_line(&args(line)).is_err(), "{line}");
        }
    }

    #[test]
    fn values_and_positionals() {
        let Ok(Subcommand::Find(p)) =
            parse_command_line(&args("find --session=s --limit 3 -- --weird-name"))
        else {
            panic!("find did not parse");
        };
        assert_eq!(p.positional, vec!["--weird-name".to_string()]);
        assert_eq!(p.value("--session"), Some("s"));
        assert_eq!(p.number("--limit").unwrap(), Some(3));
        assert!(!p.flag("--deleted"));
        assert_eq!(parse_ids("--include", "1, 2,3").unwrap(), vec![1, 2, 3]);
        assert_eq!(
            parse_list("jpeg,,png "),
            vec!["jpeg".to_string(), "png".to_string()]
        );
    }
}
