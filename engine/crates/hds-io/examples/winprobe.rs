//! Probe for the parts of `hds-io` that only a real Windows machine can
//! prove: run by the Windows CI job against the virtual disk it attaches,
//! and by hand when a machine behaves oddly. Everything it prints is JSON or
//! plain lines on stdout; the exit code is 0 when all went well, 1 when a
//! step failed or an expectation was not met, 2 for a usage error.
//!
//! ```text
//! winprobe disks
//!     list_disks() as JSON.
//! winprobe read <N | \\.\PhysicalDriveN | image file> [sectors]
//!     Opens the source read-only, prints its SourceInfo, partition table and
//!     warnings as JSON and a hex dump of the first sectors (default 2).
//! winprobe table <image file> [sector size]
//!     The partition table of an image file as JSON.
//! winprobe dest <path> <disk number> <start> <end> [purpose] [--expect <verdict>]
//!     destination_check() for <path> against bytes [start, end) of the disk,
//!     as JSON. Purpose: recover (default), image, session, app_data.
//! winprobe out <directory> [--keep]
//!     Exercises the output primitives in a new folder below <directory>:
//!     part files, commit policies, times, name rules, links, cleanup.
//! ```

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;

use hds_io::out::{CommitOutcome, EntryKind, OnExists, OutDir, TimeOutcome, Timestamps};
use hds_io::testing::MemSource;
use hds_io::{
    annotate_partitions, destination_check, list_disks, open_source, parse_partition_table,
    read_resilient, sanitize_component, source_warnings, AlignedBuf, BlockSource, ComponentLimit,
    Protected, Purpose, ReadOutcome, SourceKind, SourceLocation, Verdict,
};

const USAGE: &str = "usage:
  winprobe disks
  winprobe read <N | \\\\.\\PhysicalDriveN | image file> [sectors]
  winprobe table <image file> [sector size]
  winprobe dest <path> <disk number> <start> <end> [recover|image|session|app_data] [--expect <verdict>]
  winprobe out <directory> [--keep]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
    let result = match (args.first().map(String::as_str), rest.as_slice()) {
        (Some("disks"), []) => disks(),
        (Some("read"), [source]) => read(source, 2),
        (Some("read"), [source, sectors]) => match sectors.parse() {
            Ok(sectors) => read(source, sectors),
            Err(_) => return usage(),
        },
        (Some("table"), [image]) => table(image, None),
        (Some("table"), [image, sector_size]) => match sector_size.parse() {
            Ok(size) => table(image, Some(size)),
            Err(_) => return usage(),
        },
        (Some("dest"), [path, disk, start, end, options @ ..]) => {
            match (
                disk.parse(),
                start.parse(),
                end.parse(),
                parse_dest_options(options),
            ) {
                (Ok(disk), Ok(start), Ok(end), Some((purpose, expect))) => {
                    dest(path, disk, start..end, purpose, expect)
                }
                _ => return usage(),
            }
        }
        (Some("out"), [directory]) => out(directory, false),
        (Some("out"), [directory, "--keep"]) => out(directory, true),
        _ => return usage(),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(message) => {
            println!("FAILED: {message}");
            ExitCode::from(1)
        }
    }
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(2)
}

fn json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|e| format!("\"<not serializable: {e}>\""))
}

fn disks() -> Result<bool, String> {
    println!("{}", json(&list_disks()));
    Ok(true)
}

/// A disk number, a `\\.\PhysicalDriveN` path, or the path of an image file.
fn open(source: &str) -> Result<std::sync::Arc<dyn BlockSource>, String> {
    let (kind, path) = match source.parse::<u32>() {
        Ok(number) => (SourceKind::Disk, format!(r"\\.\PhysicalDrive{number}")),
        Err(_)
            if source
                .to_ascii_lowercase()
                .starts_with(r"\\.\physicaldrive") =>
        {
            (SourceKind::Disk, source.to_string())
        }
        Err(_) => (SourceKind::Image, source.to_string()),
    };
    open_source(kind, Path::new(&path), None).map_err(|e| {
        format!(
            "cannot open {path}: {e} (kind {:?}, os error {:?})",
            e.kind(),
            e.raw_os_error()
        )
    })
}

fn hex_dump(offset: u64, bytes: &[u8]) {
    for (i, line) in bytes.chunks(16).enumerate() {
        let hex: Vec<String> = line.iter().map(|b| format!("{b:02x}")).collect();
        let text: String = line
            .iter()
            .map(|&b| {
                if (0x20..0x7F).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        println!(
            "{:012x}  {:<47}  {text}",
            offset + 16 * i as u64,
            hex.join(" ")
        );
    }
}

fn read(source: &str, sectors: u32) -> Result<bool, String> {
    let source = open(source)?;
    println!(
        "{}",
        json(&serde_json::json!({ "source": source.info(), "alive": source.is_alive() }))
    );
    let mut table = parse_partition_table(&*source);
    annotate_partitions(&mut table, &*source);
    println!("{}", json(&table));
    println!(
        "{}",
        json(&serde_json::json!({ "warnings": source_warnings(&*source, 0..source.size()) }))
    );

    let sector = u64::from(source.sector_size());
    let len = (u64::from(sectors.clamp(1, 2048)) * sector).min(source.size());
    let mut buf = AlignedBuf::new(len as usize);
    match read_resilient(&*source, 0, &mut buf, &AtomicBool::new(false)) {
        ReadOutcome::Data { bad } => {
            println!(
                "{}",
                json(
                    &serde_json::json!({ "read_bytes": len, "bad_ranges": bad.iter().map(|r| [r.start, r.end]).collect::<Vec<_>>() })
                )
            );
            hex_dump(0, &buf);
            // The last sector of the source must be readable too: size() is exact.
            let mut last = AlignedBuf::new(sector as usize);
            let outcome = read_resilient(
                &*source,
                source.size() - sector,
                &mut last,
                &AtomicBool::new(false),
            );
            println!(
                "{}",
                json(&serde_json::json!({ "last_sector_readable": outcome.is_clean() }))
            );
            Ok(bad.is_empty() && outcome.is_clean())
        }
        other => Err(format!("reading the first {len} bytes: {other:?}")),
    }
}

fn table(image: &str, sector_size: Option<u32>) -> Result<bool, String> {
    let source = open_source(SourceKind::Image, Path::new(image), sector_size)
        .map_err(|e| format!("cannot open {image}: {e}"))?;
    println!(
        "{}",
        json(
            &serde_json::json!({ "source": source.info(), "table": parse_partition_table(&*source) })
        )
    );
    Ok(true)
}

fn parse_dest_options(options: &[&str]) -> Option<(Purpose, Option<Verdict>)> {
    let word = |text: &str| serde_json::Value::String(text.to_string());
    let (purpose, rest) = match options {
        [first, rest @ ..] if !first.starts_with("--") => {
            (serde_json::from_value(word(first)).ok()?, rest)
        }
        rest => (Purpose::Recover, rest),
    };
    match rest {
        [] => Some((purpose, None)),
        ["--expect", verdict] => Some((purpose, Some(serde_json::from_value(word(verdict)).ok()?))),
        _ => None,
    }
}

fn dest(
    path: &str,
    disk: u32,
    range: std::ops::Range<u64>,
    purpose: Purpose,
    expect: Option<Verdict>,
) -> Result<bool, String> {
    // Only the location of the source matters to the guard.
    let source = MemSource::new(vec![0u8; 512]).at_location(SourceLocation::Disk { number: disk });
    let check = destination_check(
        Path::new(path),
        purpose,
        &source,
        &[Protected::session(range)],
        Some(1),
    );
    println!("{}", json(&check));
    Ok(expect.is_none_or(|wanted| wanted == check.verdict))
}

/// Collects the results of the `out` steps.
struct Steps {
    failed: u32,
}

impl Steps {
    fn check(&mut self, what: &str, ok: bool, detail: String) {
        println!(
            "{} {what}{}{detail}",
            if ok { "ok    " } else { "FAILED" },
            if detail.is_empty() { "" } else { ": " }
        );
        if !ok {
            self.failed += 1;
        }
    }

    fn skipped(&mut self, what: &str, why: &str) {
        println!("skip   {what}: {why}");
    }
}

fn read_text(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| format!("<unreadable: {e}>"))
}

fn never(_: &str) -> bool {
    false
}

fn part(dir: &OutDir, content: &str, times: &Timestamps) -> Result<hds_io::out::PartFile, String> {
    let mut part = dir.create_part().map_err(|e| format!("create_part: {e}"))?;
    part.write_all(content.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    part.finish(times).map_err(|e| format!("finish: {e}"))?;
    Ok(part)
}

/// Makes `link` lead to the directory `target` with the system's own tools.
fn make_dir_link(target: &Path, link: &Path) -> bool {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output();
        if std::fs::symlink_metadata(link).is_err() {
            let _ = std::os::windows::fs::symlink_dir(target, link);
        }
    }
    #[cfg(unix)]
    {
        let _ = std::os::unix::fs::symlink(target, link);
    }
    std::fs::symlink_metadata(link).is_ok()
}

fn make_file_link(target: &Path, link: &Path) -> bool {
    #[cfg(windows)]
    {
        let _ = std::os::windows::fs::symlink_file(target, link);
    }
    #[cfg(unix)]
    {
        let _ = std::os::unix::fs::symlink(target, link);
    }
    std::fs::symlink_metadata(link).is_ok()
}

fn out(directory: &str, keep: bool) -> Result<bool, String> {
    let base = PathBuf::from(directory).join(format!("hds-winprobe-{}", std::process::id()));
    let mut steps = Steps { failed: 0 };
    let result = out_steps(&base, &mut steps);
    if !keep {
        // The probe's own scratch tree; the engine itself never removes directories.
        let _ = std::fs::remove_dir_all(&base);
    }
    result?;
    println!(
        "{}",
        json(&serde_json::json!({ "failed_steps": steps.failed }))
    );
    Ok(steps.failed == 0)
}

fn out_steps(base: &Path, steps: &mut Steps) -> Result<(), String> {
    // 2020-01-01, 2020-02-01 and 2020-03-01 as FILETIME.
    const FT_2020: u64 = 132_223_104_000_000_000;
    const DAY: u64 = 864_000_000_000;
    let times = Timestamps {
        created: FT_2020,
        modified: FT_2020 + 31 * DAY + 1_234_567,
        accessed: FT_2020 + 60 * DAY,
    };

    let outside = base.join("outside");
    std::fs::create_dir_all(&outside)
        .map_err(|e| format!("cannot create {}: {e}", outside.display()))?;
    std::fs::write(outside.join("victim.txt"), b"original").map_err(|e| e.to_string())?;

    // The root: created level by level, then canonical.
    let root = OutDir::create_root(&base.join("dest").join("level 1").join("level 2"))
        .map_err(|e| format!("create_root: {e}"))?;
    println!("root   {}", root.path().display());
    steps.check(
        "create_root builds missing levels",
        root.path().is_dir(),
        String::new(),
    );
    let limits = root.limits().map_err(|e| format!("limits: {e}"))?;
    println!("limits {limits:?}, free {:?}", root.free_space());

    // A file: part, content, times on the writing handle, rename without replace.
    let (photos, created) = root.subdir("Photos").map_err(|e| format!("subdir: {e}"))?;
    steps.check("subdir creates a directory", created, String::new());
    let mut first = part(&photos, "first", &times)?;
    let outcome = first
        .commit("Report.txt", OnExists::Rename, &never)
        .map_err(|e| format!("commit: {e}"))?;
    let final_path = photos.path().join("Report.txt");
    let meta = std::fs::metadata(&final_path).map_err(|e| e.to_string())?;
    steps.check(
        "commit renames the part file",
        outcome == CommitOutcome::Committed && read_text(&final_path) == "first",
        format!("{outcome:?}"),
    );
    let expected = |ft| hds_io::out::filetime_to_system_time(ft);
    steps.check(
        "modification time is applied",
        meta.modified().ok() == expected(times.modified),
        format!("{:?}", meta.modified()),
    );
    if cfg!(windows) {
        steps.check(
            "creation time is applied",
            meta.created().ok() == expected(times.created),
            format!("{:?}", meta.created()),
        );
    }

    // The same name in another case: the file system decides.
    let mut second = part(&photos, "second", &times)?;
    let stored = std::cell::RefCell::new(String::new());
    let produced = |name: &str| {
        *stored.borrow_mut() = name.to_string();
        true
    };
    let outcome = second
        .commit("REPORT.TXT", OnExists::Overwrite, &produced)
        .map_err(|e| format!("commit: {e}"))?;
    match outcome {
        CommitOutcome::NameTaken => steps.check(
            "a name that differs only in case is taken, and reported as stored",
            *stored.borrow() == "Report.txt" && read_text(&final_path) == "first",
            format!("stored as {:?}", stored.borrow()),
        ),
        CommitOutcome::Committed => {
            steps.skipped("case collision", "this file system is case-sensitive")
        }
        other => steps.check("case collision", false, format!("{other:?}")),
    }
    drop(second);

    // on_exists for a file that was there before the run.
    for (policy, wanted, content) in [
        (OnExists::Rename, CommitOutcome::NameTaken, "first"),
        (OnExists::Skip, CommitOutcome::Skipped, "first"),
        (OnExists::Overwrite, CommitOutcome::Overwritten, "newer"),
    ] {
        let mut newer = part(&photos, "newer", &times)?;
        let outcome = newer
            .commit("Report.txt", policy, &never)
            .map_err(|e| format!("commit: {e}"))?;
        steps.check(
            &format!("on_exists {policy:?}"),
            outcome == wanted && read_text(&final_path) == content,
            format!("{outcome:?}"),
        );
    }
    let leftovers: Vec<String> = std::fs::read_dir(photos.path())
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .filter(|name| hds_io::out::is_part_name(name))
        .collect();
    steps.check(
        "no part file is left behind",
        leftovers.is_empty(),
        format!("{leftovers:?}"),
    );

    // Links below the root are never entered or written through.
    if make_dir_link(&outside, &root.path().join("Linked")) {
        let refused = root.subdir("Linked").is_err();
        let kind = root.entry("Linked").ok().flatten();
        steps.check(
            "a linked directory is refused",
            refused && kind == Some(EntryKind::Link),
            format!("{kind:?}"),
        );
        let mut through = part(&root, "must not land outside", &times)?;
        let outcome = through.commit("Linked", OnExists::Overwrite, &never);
        steps.check(
            "overwrite does not replace a linked directory",
            matches!(outcome, Ok(CommitOutcome::NameTaken)),
            format!("{outcome:?}"),
        );
    } else {
        steps.skipped(
            "linked directory",
            "no junction or symbolic link could be created",
        );
    }
    if make_file_link(&outside.join("victim.txt"), &root.path().join("linked.txt")) {
        let mut over = part(&root, "clobbered", &times)?;
        let outcome = over.commit("linked.txt", OnExists::Overwrite, &never);
        let intact = read_text(&outside.join("victim.txt")) == "original";
        steps.check(
            "overwrite does not write through a linked file",
            matches!(outcome, Ok(CommitOutcome::NameTaken)) && intact,
            format!("{outcome:?}"),
        );
        steps.check(
            "a linked file is not opened for writing",
            root.open_existing("linked.txt").is_err(),
            String::new(),
        );
        steps.check(
            "create_new on a link is 'exists'",
            root.create_new("linked.txt").is_err(),
            String::new(),
        );
    } else {
        steps.skipped(
            "linked file",
            "no symbolic link could be created (needs privilege)",
        );
    }
    let outside_names: Vec<String> = std::fs::read_dir(&outside)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    steps.check(
        "nothing was written outside the root",
        outside_names == ["victim.txt"],
        format!("{outside_names:?}"),
    );

    // Names: what the sanitizer makes can be created; what it would have changed is refused.
    let (names, _) = root.subdir("names").map_err(|e| format!("subdir: {e}"))?;
    for raw in [
        "CON",
        "NUL.txt",
        "aux",
        "COM1.log",
        "trailing dot.",
        "trailing space ",
        "a:b",
        "q?*<>|\"x",
        "...",
    ] {
        let units: Vec<u16> = raw.encode_utf16().collect();
        let safe = String::from_utf16_lossy(&sanitize_component(&units, 0, limits.max_component));
        let created = names.create_new(&safe);
        steps.check(
            &format!("sanitized {raw:?} -> {safe:?} can be created"),
            created.is_ok(),
            format!("{:?}", created.err()),
        );
        if safe != raw {
            steps.check(
                &format!("raw {raw:?} is refused"),
                names.create_new(raw).is_err(),
                String::new(),
            );
        }
    }
    let longest = match limits.max_component {
        ComponentLimit::Utf16Units(n) | ComponentLimit::Utf8Bytes(n) => n as usize,
    };
    let long_ok = names.create_new(&("n".repeat(longest - 4) + ".bin"));
    steps.check(
        "a name of exactly the component limit can be created",
        long_ok.is_ok(),
        format!("{:?}", long_ok.err()),
    );
    steps.check(
        "a longer name is refused",
        names
            .create_new(&("n".repeat(longest - 3) + ".bin"))
            .is_err(),
        String::new(),
    );

    // A path far beyond 260 characters.
    let mut deep = root.clone();
    for level in 0..12 {
        deep = deep
            .subdir(&format!("{level:02}-{}", "d".repeat(30)))
            .map_err(|e| format!("deep subdir {level}: {e}"))?
            .0;
    }
    let mut deep_part = part(&deep, "deep", &times)?;
    let outcome = deep_part.commit("deep file.txt", OnExists::Rename, &never);
    steps.check(
        &format!("files in a path of {} characters", deep.path_len_units()),
        matches!(outcome, Ok(CommitOutcome::Committed)),
        format!("{outcome:?}"),
    );

    // Times: values that must not be applied, and directory times.
    let file = root
        .create_new("times.bin")
        .map_err(|e| format!("create_new: {e}"))?;
    for garbage in [0u64, 1 << 63, u64::MAX] {
        let report = file.set_times(&Timestamps {
            created: garbage,
            modified: garbage,
            accessed: garbage,
        });
        let untouched = [report.created, report.modified, report.accessed]
            .iter()
            .all(|&o| o == TimeOutcome::Invalid);
        steps.check(
            &format!("FILETIME {garbage:#x} is not applied"),
            untouched,
            format!("{report:?}"),
        );
    }
    let year_1601 = file.set_times(&Timestamps {
        created: 1,
        modified: 1,
        accessed: 1,
    });
    println!("note   FILETIME 1 (1601-01-01): {year_1601:?}");
    let far_future = file.set_times(&Timestamps {
        created: 0,
        modified: (1 << 63) - 1,
        accessed: 0,
    });
    println!("note   FILETIME 2^63-1 (year 30828): {far_future:?}");
    drop(file);
    let report = photos.set_times(&times);
    let dir_meta = std::fs::metadata(photos.path()).map_err(|e| e.to_string())?;
    let dir_entry_meta = std::fs::read_dir(root.path())
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .find(|e| e.file_name() == "Photos")
        .and_then(|e| e.metadata().ok());
    steps.check(
        "directory times are applied",
        report.modified == TimeOutcome::Applied
            && dir_meta.modified().ok() == expected(times.modified),
        format!(
            "{report:?}; expected {:?}, by handle {:?}, by parent listing {:?}",
            expected(times.modified),
            dir_meta.modified().ok(),
            dir_entry_meta.and_then(|m| m.modified().ok()),
        ),
    );

    // Leftovers of a killed run.
    std::fs::write(root.path().join("~hds-999999.part"), b"half").map_err(|e| e.to_string())?;
    std::fs::write(photos.path().join("~hds-999998.part"), b"half").map_err(|e| e.to_string())?;
    let cleaned = root
        .remove_leftover_parts(true)
        .map_err(|e| format!("cleanup: {e}"))?;
    steps.check(
        "leftover part files are removed",
        cleaned.removed == 2 && cleaned.failed == 0,
        format!("{cleaned:?}"),
    );
    Ok(())
}
