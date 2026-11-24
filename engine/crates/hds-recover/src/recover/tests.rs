use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use hds_io::out::{filetime_to_system_time, OnExists, Timestamps};
use hds_io::testing::{FaultySource, MemSource};
use hds_io::{BlockSource, ComponentLimit, Protected, RangedSource, SourceLocation, Verdict};
use hds_ntfs::fnv1a64;

use super::*;
use crate::plan::{PlanFile, PlanFolder};
use crate::source::{FileSource, Run};
use crate::testutil::{
    file_record, make_dir_link, make_file_link, names, pattern, tree, utf16, TempDir,
};

/// 2020-01-01T00:00:00Z as FILETIME, and one day.
const FT_2020: u64 = 132_223_104_000_000_000;
const FT_DAY: u64 = 864_000_000_000;

/// A complete 1x1 PNG.
const PNG: [u8; 67] = [
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x00, 0x00, 0x00, 0x00, 0x3A, 0x7E, 0x9B,
    0x55, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0x63, 0xA8, 0x07, 0x00, 0x00,
    0x81, 0x00, 0x80, 0x7E, 0x1C, 0x29, 0xC7, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

/// A disk image in memory with files placed on it.
struct Disk {
    data: Vec<u8>,
    next: u64,
}

impl Disk {
    fn new(len: usize) -> Disk {
        Disk {
            data: pattern(len),
            next: 64 * 1024,
        }
    }

    /// Places `bytes` at the next 4 KiB boundary: one run.
    fn put(&mut self, bytes: &[u8]) -> FileSource {
        let offset = self.next;
        self.data[offset as usize..offset as usize + bytes.len()].copy_from_slice(bytes);
        self.next = (offset + bytes.len() as u64 + 4096).next_multiple_of(4096);
        FileSource::Extents {
            runs: vec![Run::Data {
                offset,
                len: bytes.len() as u64,
            }],
            real_size: bytes.len() as u64,
            initialized_size: bytes.len() as u64,
        }
    }

    /// Places a `FILE` record holding `value` as resident data.
    fn record(&mut self, value: &[u8]) -> FileSource {
        let (record, value_offset) = file_record(1024, value, 0x5A5A);
        let offset = self.next;
        self.data[offset as usize..offset as usize + 1024].copy_from_slice(&record);
        self.next += 4096;
        FileSource::Resident {
            record_offset: offset,
            record_size: 1024,
            value_offset,
            value_len: value.len() as u32,
            hash: fnv1a64(&record),
        }
    }

    fn source(&self) -> RangedSource {
        let len = self.data.len() as u64;
        RangedSource::new(Arc::new(MemSource::new(self.data.clone())), 0..len).unwrap()
    }

    fn faulty(&self) -> (Arc<FaultySource>, RangedSource) {
        let len = self.data.len() as u64;
        let faulty = Arc::new(FaultySource::new(Arc::new(MemSource::new(
            self.data.clone(),
        ))));
        (faulty.clone(), RangedSource::new(faulty, 0..len).unwrap())
    }
}

fn times(base: u64) -> Timestamps {
    Timestamps {
        created: base,
        modified: base + FT_DAY,
        accessed: base + 2 * FT_DAY,
    }
}

fn file(node: u64, folder: Option<u64>, name: &str, source: FileSource) -> PlanFile {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    PlanFile {
        node,
        folder,
        name: utf16(name),
        ext,
        size: source.size(),
        first_data_offset: source.first_offset(),
        source,
        times: times(FT_2020 + node * FT_DAY),
        flags: FlagSet::new(),
        context: RatingContext::default(),
        in_use: true,
        record_number: Some(node),
        record_offset: Some(node * 1024),
        original_path: format!("orig/{name}"),
    }
}

fn folder(id: u64, parent: Option<u64>, name: &str) -> PlanFolder {
    PlanFolder {
        id,
        parent,
        name: utf16(name),
        times: times(FT_2020 - 100 * FT_DAY + id * FT_DAY),
        in_use: true,
        record_number: Some(id),
    }
}

#[derive(Default)]
struct Recorder {
    progress: Vec<(Instant, RecoverProgress)>,
    outcomes: Vec<FileOutcome>,
}

impl RecoverObserver for Recorder {
    fn progress(&mut self, progress: &RecoverProgress) {
        self.progress.push((Instant::now(), progress.clone()));
    }

    fn file_done(&mut self, outcome: &FileOutcome) {
        self.outcomes.push(outcome.clone());
    }
}

impl Recorder {
    fn outcome(&self, node: u64) -> &FileOutcome {
        self.outcomes
            .iter()
            .find(|o| o.node == node)
            .unwrap_or_else(|| panic!("no outcome for node {node}"))
    }

    fn status(&self, node: u64) -> String {
        self.outcome(node).status.to_string()
    }
}

fn options() -> RecoverOptions {
    RecoverOptions::new(Vec::new())
}

fn run_with(
    source: RangedSource,
    plan: Plan,
    dest: &Path,
    options: RecoverOptions,
    cancel: &AtomicBool,
) -> (RecoverSummary, Recorder) {
    let mut recorder = Recorder::default();
    let summary = recover(source, plan, dest, options, &mut recorder, cancel).unwrap();
    (summary, recorder)
}

fn run(
    source: RangedSource,
    plan: Plan,
    dest: &Path,
    options: RecoverOptions,
) -> (RecoverSummary, Recorder) {
    run_with(source, plan, dest, options, &AtomicBool::new(false))
}

/// A small RFC 4180 reader for the report.
fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    cell.push('"');
                    chars.next();
                } else {
                    quoted = false;
                }
            } else {
                cell.push(c);
            }
        } else {
            match c {
                '"' => quoted = true,
                ',' => row.push(std::mem::take(&mut cell)),
                '\r' => {
                    assert_eq!(chars.next(), Some('\n'), "lines end with CRLF");
                    row.push(std::mem::take(&mut cell));
                    rows.push(std::mem::take(&mut row));
                }
                '\n' => panic!("a bare LF outside quotes"),
                _ => cell.push(c),
            }
        }
    }
    assert!(
        cell.is_empty() && row.is_empty(),
        "the last line is complete"
    );
    rows
}

fn report_path(summary: &RecoverSummary) -> PathBuf {
    PathBuf::from(&summary.report_path)
}

/// The report's rows without the header, checking BOM and header.
fn report_rows(summary: &RecoverSummary) -> Vec<Vec<String>> {
    let bytes = std::fs::read(report_path(summary)).unwrap();
    assert!(bytes.starts_with(b"\xEF\xBB\xBF"), "BOM");
    let text = std::str::from_utf8(&bytes[3..]).unwrap();
    let mut rows = parse_csv(text);
    assert_eq!(rows.remove(0), crate::report::COLUMNS);
    for row in &rows {
        assert_eq!(row.len(), 11);
    }
    rows
}

fn sep(path: &str) -> String {
    path.replace('/', std::path::MAIN_SEPARATOR_STR)
}

fn part_files(dir: &Path) -> Vec<String> {
    tree(dir)
        .into_iter()
        .filter(|p| hds_io::out::is_part_name(p.rsplit('/').next().unwrap_or("")))
        .collect()
}

#[test]
fn files_and_folders_are_written_with_a_report() {
    let tmp = TempDir::new("basic");
    let mut disk = Disk::new(24 << 20);
    let png = disk.put(&PNG);
    let big_content = pattern(9 << 20);
    let big = disk.put(&big_content);
    let resident = disk.record(b"resident text, crossing nothing");
    let empty = FileSource::Extents {
        runs: Vec::new(),
        real_size: 0,
        initialized_size: 0,
    };
    let plan = Plan {
        folders: vec![
            folder(2, Some(1), "2019"),
            folder(1, None, "Photos"),
            folder(3, None, "Empty"),
            folder(4, Some(2), "Deep"),
        ],
        files: vec![
            file(10, Some(2), "IMG.png", png),
            file(11, None, "big.bin", big),
            file(12, Some(1), "note.txt", resident),
            file(13, None, "empty.png", empty),
        ],
    };
    let dest = tmp.join("Rescued");
    let (summary, rec) = run(disk.source(), plan, &dest, options());
    assert_eq!(
        summary.status,
        JobStatus::Completed,
        "{:?}",
        summary.message
    );
    assert_eq!(summary.error, None);
    assert_eq!(
        (
            summary.files_total,
            summary.ok,
            summary.warnings,
            summary.failed,
            summary.skipped
        ),
        (4, 4, 0, 0, 0)
    );
    assert_eq!(summary.bytes_written, 67 + (9 << 20) + 31);
    assert_eq!(summary.dest, dest.display().to_string());
    let report_name = report_path(&summary)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(report_name.starts_with("HDSavior-report-") && report_name.ends_with(".csv"));
    let mut expected = vec![
        "Empty/",
        "Photos/",
        "Photos/2019/",
        "Photos/2019/Deep/",
        "Photos/2019/IMG.png",
        "Photos/note.txt",
        report_name.as_str(),
        "big.bin",
        "empty.png",
    ];
    expected.sort();
    assert_eq!(tree(&dest), expected);
    assert_eq!(
        std::fs::read(dest.join("Photos/2019/IMG.png")).unwrap(),
        PNG
    );
    assert_eq!(std::fs::read(dest.join("big.bin")).unwrap(), big_content);
    assert_eq!(
        std::fs::read(dest.join("Photos/note.txt")).unwrap(),
        b"resident text, crossing nothing"
    );
    assert!(std::fs::read(dest.join("empty.png")).unwrap().is_empty());
    assert_eq!(rec.status(10), "ok");
    assert_eq!(rec.status(11), "ok_unverified");
    assert_eq!(rec.status(12), "ok_unverified");
    assert_eq!(
        rec.status(13),
        "ok_unverified",
        "an empty file has no opinion"
    );
    assert_eq!(rec.outcome(10).flags, Flag::SignatureOk.into());
    assert_eq!(rec.outcome(10).output_path, sep("Photos/2019/IMG.png"));
    let rows = report_rows(&summary);
    assert_eq!(rows.len(), 4);
    let png_row = rows.iter().find(|r| r[5] == "orig/IMG.png").unwrap();
    assert_eq!(png_row[0], "ok");
    assert_eq!(png_row[1], "signature_ok");
    assert_eq!(png_row[2], "67");
    assert_eq!(png_row[3], "67");
    assert_eq!(png_row[4], sep("Photos/2019/IMG.png"));
    assert_eq!(png_row[6], "10");
    assert_eq!(png_row[7], "10240");
    assert_eq!(png_row[8], (64 * 1024).to_string());
    assert_eq!(png_row[9], "2020-01-12 00:00:00");
    assert!(part_files(&dest).is_empty());
    assert!(!rec.progress.is_empty());
}

#[test]
fn files_are_processed_in_order_of_their_first_data_byte() {
    let tmp = TempDir::new("order");
    let mut disk = Disk::new(1 << 20);
    let a = disk.put(b"first on disk");
    let b = disk.record(b"second on disk");
    let c = disk.put(b"third on disk");
    let holes = FileSource::Extents {
        runs: vec![Run::Hole { len: 10 }],
        real_size: 10,
        initialized_size: 10,
    };
    let plan = Plan {
        folders: Vec::new(),
        files: vec![
            file(1, None, "c.txt", c),
            file(2, None, "holes.txt", holes),
            file(3, None, "a.txt", a),
            file(4, None, "b.txt", b),
        ],
    };
    let (summary, rec) = run(disk.source(), plan, &tmp.join("out"), options());
    let order: Vec<u64> = rec.outcomes.iter().map(|o| o.node).collect();
    assert_eq!(order, [3, 4, 1, 2]);
    let rows: Vec<String> = report_rows(&summary)
        .into_iter()
        .map(|r| r[4].clone())
        .collect();
    assert_eq!(rows, ["a.txt", "b.txt", "c.txt", "holes.txt"]);
}

#[test]
fn names_are_made_safe_and_unique_in_h4_order() {
    let tmp = TempDir::new("names");
    let mut disk = Disk::new(1 << 20);
    let mut plan = Plan::default();
    let raw: Vec<(Vec<u16>, bool, u64)> = vec![
        (utf16("a:b?.txt "), true, 30),
        (utf16("CON"), true, 31),
        (utf16("Readme.txt"), false, 20),
        (utf16("README.TXT"), true, 40),
        (utf16("readme.txt"), true, 35),
        (vec![u16::from(b'x'), 0xDC00, u16::from(b'y')], true, 41),
        (utf16("HDSavior-report-x.csv"), true, 42),
        (utf16("~hds-7.part"), true, 43),
        (utf16("..."), true, 44),
        (utf16("=cmd|' /C calc'!A0.csv"), true, 45),
    ];
    for (i, (name, in_use, record)) in raw.into_iter().enumerate() {
        let mut f = file(
            i as u64 + 1,
            None,
            "x",
            disk.put(format!("file {i}").as_bytes()),
        );
        f.name = name;
        f.in_use = in_use;
        f.record_number = Some(record);
        f.ext = String::new();
        if !in_use {
            f.flags = Flag::Deleted.into();
        }
        plan.files.push(f);
    }
    let dest = tmp.join("out");
    let (summary, rec) = run(disk.source(), plan, &dest, options());
    assert_eq!(summary.status, JobStatus::Completed);
    let mut on_disk = names(&dest);
    on_disk.retain(|n| !n.starts_with("HDSavior-report-"));
    assert_eq!(
        on_disk,
        [
            "=cmd_' _C calc'!A0.csv",
            "README (2).TXT",
            "Readme (3).txt",
            "_",
            "_CON",
            "_HDSavior-report-x.csv",
            "_~hds-7.part",
            "a_b_.txt",
            "readme.txt",
            "x\u{FFFD}y",
        ]
    );
    assert_eq!(rec.outcome(3).output_path, "Readme (3).txt");
    assert_eq!(rec.status(3), "warning:deleted");
    // Each name holds the content of its own entry.
    for o in &rec.outcomes {
        let content = std::fs::read(dest.join(&o.output_path)).unwrap();
        assert_eq!(content, format!("file {}", o.node - 1).into_bytes());
    }
    // The formula guard protects the report.
    let text = String::from_utf8(std::fs::read(report_path(&summary)).unwrap()).unwrap();
    assert!(text.contains(",'=cmd_' _C calc'!A0.csv,"), "{text}");
}

#[test]
fn overlong_paths_are_skipped_and_their_folders_not_created() {
    let tmp = TempDir::new("deep");
    let mut disk = Disk::new(1 << 20);
    // 140 levels of 240-character names: about 33,700 units below the root.
    let mut plan = Plan::default();
    for level in 0..140u64 {
        let name = format!("{level:03}{}", "d".repeat(237));
        plan.folders.push(folder(
            level + 1,
            level.checked_sub(1).map(|p| p + 1),
            &name,
        ));
    }
    plan.files
        .push(file(1000, Some(10), "shallow.txt", disk.put(b"fits")));
    plan.files
        .push(file(1001, Some(140), "deep.txt", disk.put(b"does not fit")));
    let dest = tmp.join("out");
    let (summary, rec) = run(disk.source(), plan, &dest, options());
    assert_eq!(summary.status, JobStatus::Completed);
    assert_eq!(rec.status(1000), "ok_unverified");
    if crate::testutil::under_wine() {
        // Wine hands the path to the host, whose own limit (4096 bytes) comes first.
        let status = rec.status(1001);
        assert!(
            status == "skipped:path_too_long" || status == "failed:write_error",
            "{status}"
        );
    } else {
        assert_eq!(rec.status(1001), "skipped:path_too_long");
        assert!(rec.outcome(1001).detail.contains("characters long"));
    }
    // Count the levels that exist, one relative open at a time (the absolute
    // paths are longer than the system's PATH_MAX).
    let mut depth = 0;
    let mut dir = hds_io::out::OutDir::open_root(&dest).unwrap();
    for level in 0..140u64 {
        let name = format!("{level:03}{}", "d".repeat(237));
        match dir.open_subdir(&name) {
            Ok(Some(next)) => {
                dir = next;
                depth += 1;
            }
            _ => break,
        }
    }
    let root_len = std::fs::canonicalize(&dest)
        .unwrap()
        .to_string_lossy()
        .encode_utf16()
        .count();
    let expected = (0..140)
        .take_while(|level| root_len + (level + 1) * 241 <= 32_767)
        .count();
    if crate::testutil::under_wine() {
        assert!(depth >= 10 && depth <= expected, "{depth}");
    } else {
        assert_eq!(depth, expected);
    }
    assert!(depth < 140);
}

#[test]
fn every_status_follows_flags_and_read_results() {
    // pre-read flags x rating context x skip_damaged x read outcome.
    let tmp = TempDir::new("statuses");
    let mut disk = Disk::new(8 << 20);
    let png = disk.put(&PNG);
    let text = disk.put(b"plain text");
    let bad = disk.put(&vec![b't'; 4096]);
    let bad_range = match &bad {
        FileSource::Extents { runs, .. } => match runs[0] {
            Run::Data { offset, .. } => offset + 1024..offset + 1536,
            _ => unreachable!(),
        },
        _ => unreachable!(),
    };
    let disk_len = disk.data.len() as u64;
    let outside = FileSource::Extents {
        runs: vec![Run::Data {
            offset: disk_len - 100,
            len: 300,
        }],
        real_size: 300,
        initialized_size: 300,
    };
    let short = FileSource::Extents {
        runs: vec![Run::Data {
            offset: 64 * 1024,
            len: 100,
        }],
        real_size: 300,
        initialized_size: 300,
    };
    let resident = disk.record(b"resident");
    let mut changed = disk.record(b"changed");
    if let FileSource::Resident { hash, .. } = &mut changed {
        *hash ^= 1;
    }
    let empty = FileSource::Extents {
        runs: Vec::new(),
        real_size: 0,
        initialized_size: 0,
    };
    // (name, ext, source, flags the read adds, signature result)
    let reads: Vec<(&str, &str, FileSource, FlagSet, Option<Flag>)> = vec![
        (
            "png-ok",
            "png",
            png.clone(),
            FlagSet::new(),
            Some(Flag::SignatureOk),
        ),
        (
            "png-as-jpg",
            "jpg",
            png,
            FlagSet::new(),
            Some(Flag::SignatureMismatch),
        ),
        ("text", "txt", text, FlagSet::new(), None),
        ("bad", "txt", bad, Flag::BadSectors.into(), None),
        ("outside", "txt", outside, Flag::OutOfBounds.into(), None),
        ("short", "txt", short, Flag::MissingFragments.into(), None),
        ("resident", "txt", resident, FlagSet::new(), None),
        ("changed", "txt", changed, FlagSet::new(), None),
        ("empty", "png", empty, FlagSet::new(), None),
    ];
    let pre_sets = [
        FlagSet::new(),
        Flag::Deleted.into(),
        Flag::Overwritten.into(),
        Flag::Compressed.into(),
        FlagSet::of(&[Flag::Encrypted, Flag::Deleted]),
        Flag::SignatureMismatch.into(),
        Flag::SignatureOk.into(),
        FlagSet::of(&[Flag::Sparse, Flag::SizeEstimated]),
    ];
    let contexts = [
        RatingContext::default(),
        RatingContext {
            geometry_not_validated: true,
            carved_end_unverified: false,
        },
    ];
    for skip_damaged in [false, true] {
        let mut plan = Plan::default();
        let mut expected: Vec<(u64, String)> = Vec::new();
        let mut node = 0u64;
        for (read_name, ext, source, read_flags, signature) in &reads {
            for pre in pre_sets {
                for context in contexts {
                    node += 1;
                    let name = format!("{read_name}-{node}.{ext}");
                    let mut f = file(node, None, &name, source.clone());
                    f.flags = pre;
                    f.context = context;
                    plan.files.push(f);
                    // The expectation, from the rules of H11.
                    let status = if pre.contains(Flag::Compressed) {
                        "skipped:compressed".to_string()
                    } else if pre.contains(Flag::Encrypted) {
                        "skipped:encrypted".to_string()
                    } else if skip_damaged && crate::rate(pre, context) == Rating::Damaged {
                        "skipped:damaged".to_string()
                    } else if *read_name == "changed" {
                        "failed:record_changed".to_string()
                    } else {
                        let mut flags = pre
                            .without(Flag::SignatureOk)
                            .without(Flag::SignatureMismatch)
                            | *read_flags;
                        flags.extend(*signature);
                        match crate::rate(flags, context) {
                            Rating::Good if flags.contains(Flag::SignatureOk) => "ok".to_string(),
                            Rating::Good => "ok_unverified".to_string(),
                            _ => format!(
                                "warning:{}",
                                crate::rating_reasons(flags, context).join(";")
                            ),
                        }
                    };
                    expected.push((node, status));
                }
            }
        }
        let (faulty, source) = disk.faulty();
        faulty.bad_sectors(bad_range.clone());
        let mut opts = options();
        opts.skip_damaged = skip_damaged;
        let dest = tmp.join(&format!("out-{skip_damaged}"));
        let (summary, rec) = run(source, plan, &dest, opts);
        assert_eq!(summary.status, JobStatus::Completed);
        for (node, status) in &expected {
            assert_eq!(
                &rec.status(*node),
                status,
                "node {node} skip_damaged {skip_damaged}"
            );
        }
        assert_eq!(
            summary.ok + summary.warnings + summary.failed + summary.skipped,
            summary.files_total
        );
        // Written files are exactly those with a written status; failed and skipped leave nothing.
        for outcome in &rec.outcomes {
            let path = dest.join(&outcome.output_path);
            assert_eq!(path.exists(), outcome.status.is_written(), "{outcome:?}");
            if outcome.status.is_written() {
                assert_eq!(
                    std::fs::metadata(&path).unwrap().len(),
                    outcome.bytes_written
                );
            } else {
                assert_eq!(outcome.bytes_written, 0);
            }
        }
        // The bad file reads zeros exactly where the sectors are bad.
        let bad_out = rec
            .outcomes
            .iter()
            .find(|o| o.output_path.starts_with("bad-") && o.status.is_written())
            .unwrap();
        let content = std::fs::read(dest.join(&bad_out.output_path)).unwrap();
        assert!(content[1024..1536].iter().all(|&b| b == 0));
        assert!(content[..1024]
            .iter()
            .chain(&content[1536..])
            .all(|&b| b == b't'));
        assert!(
            bad_out.detail.contains("512 unreadable bytes"),
            "{}",
            bad_out.detail
        );
        assert!(part_files(&dest).is_empty());
        // bytes_total counts what is attempted.
        let attempted: u64 = rec
            .outcomes
            .iter()
            .filter(|o| {
                !matches!(
                    o.status,
                    FileStatus::Skipped(
                        SkipReason::Compressed | SkipReason::Encrypted | SkipReason::Damaged
                    )
                )
            })
            .map(|o| plan_size_of(&reads, &o.output_path))
            .sum();
        let last = rec.progress.last().map(|(_, p)| p.bytes_total);
        if let Some(total) = last {
            assert_eq!(total, attempted);
        }
    }
}

fn plan_size_of(reads: &[(&str, &str, FileSource, FlagSet, Option<Flag>)], output: &str) -> u64 {
    reads
        .iter()
        .find(|(name, ..)| output.starts_with(&format!("{name}-")))
        .map(|(_, _, source, ..)| source.size())
        .unwrap()
}

#[test]
fn on_exists_rename_keeps_whatever_was_there() {
    let tmp = TempDir::new("rename");
    let outside = TempDir::new("rename-outside");
    let dest = tmp.join("out");
    let links = prepare_existing(&dest, &outside);
    let (disk, plan) = existing_plan();
    let (summary, rec) = run(disk.source(), plan, &dest, options());
    assert_eq!(summary.status, JobStatus::Completed);
    assert_eq!(rec.outcome(1).output_path, "a (2).txt");
    assert_eq!(rec.outcome(2).output_path, "d (2).txt");
    assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"old");
    assert!(dest.join("d.txt").is_dir());
    assert_eq!(std::fs::read(dest.join("a (2).txt")).unwrap(), b"new a");
    assert_eq!(rec.outcome(4).output_path, sep("f (2)/y.txt"));
    if links {
        assert_eq!(rec.outcome(3).output_path, "l (2).txt");
        assert_eq!(rec.outcome(5).output_path, sep("dl (2)/x.txt"));
    }
    check_outside_untouched(&outside, links);
}

#[test]
fn on_exists_skip_keeps_whatever_was_there() {
    let tmp = TempDir::new("skip");
    let outside = TempDir::new("skip-outside");
    let dest = tmp.join("out");
    let links = prepare_existing(&dest, &outside);
    let (disk, plan) = existing_plan();
    let mut opts = options();
    opts.on_exists = OnExists::Skip;
    let (summary, rec) = run(disk.source(), plan, &dest, opts);
    assert_eq!(summary.status, JobStatus::Completed);
    assert_eq!(rec.status(1), "skipped:exists");
    assert_eq!(rec.status(2), "skipped:exists");
    assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"old");
    assert!(dest.join("d.txt").is_dir());
    if links {
        assert_eq!(rec.status(3), "skipped:exists");
        assert_eq!(
            rec.status(5),
            "ok_unverified",
            "the folder took another name"
        );
    }
    assert!(!dest.join("a (2).txt").exists());
    check_outside_untouched(&outside, links);
}

#[test]
fn on_exists_overwrite_replaces_only_regular_files() {
    let tmp = TempDir::new("overwrite");
    let outside = TempDir::new("overwrite-outside");
    let dest = tmp.join("out");
    let links = prepare_existing(&dest, &outside);
    let (disk, plan) = existing_plan();
    let mut opts = options();
    opts.on_exists = OnExists::Overwrite;
    let (summary, rec) = run(disk.source(), plan, &dest, opts);
    assert_eq!(summary.status, JobStatus::Completed);
    assert_eq!(rec.outcome(1).output_path, "a.txt");
    assert!(
        rec.outcome(1).detail.contains("replaced"),
        "{}",
        rec.outcome(1).detail
    );
    assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"new a");
    // A directory is never deleted or entered.
    assert_eq!(rec.outcome(2).output_path, "d (2).txt");
    assert!(dest.join("d.txt").is_dir());
    assert!(names(&dest.join("d.txt")).is_empty());
    if links {
        // Nor is a link, whatever it points to.
        assert_eq!(rec.outcome(3).output_path, "l (2).txt");
        let meta = std::fs::symlink_metadata(dest.join("l.txt")).unwrap();
        // (Wine does not mark file symbolic links as such; the link is still there.)
        assert!(meta.file_type().is_symlink() || crate::testutil::under_wine());
    }
    check_outside_untouched(&outside, links);
}

/// The destination before the run: a file `a.txt`, a directory `d.txt`, a
/// file `f` where a folder goes, and (if links can be made) a file link
/// `l.txt` and a directory link `dl` that lead out of the destination.
fn prepare_existing(dest: &Path, outside: &TempDir) -> bool {
    std::fs::create_dir_all(dest).unwrap();
    std::fs::write(dest.join("a.txt"), b"old").unwrap();
    std::fs::create_dir(dest.join("d.txt")).unwrap();
    std::fs::write(dest.join("f"), b"in the way").unwrap();
    std::fs::write(outside.join("target.txt"), b"target").unwrap();
    std::fs::create_dir(outside.join("target-dir")).unwrap();
    make_file_link(&outside.join("target.txt"), &dest.join("l.txt"))
        && make_dir_link(&outside.join("target-dir"), &dest.join("dl"))
}

fn existing_plan() -> (Disk, Plan) {
    let mut disk = Disk::new(1 << 20);
    let plan = Plan {
        folders: vec![folder(100, None, "f"), folder(101, None, "dl")],
        files: vec![
            file(1, None, "a.txt", disk.put(b"new a")),
            file(2, None, "d.txt", disk.put(b"new d")),
            file(3, None, "l.txt", disk.put(b"new l")),
            file(4, Some(100), "y.txt", disk.put(b"new y")),
            file(5, Some(101), "x.txt", disk.put(b"new x")),
        ],
    };
    (disk, plan)
}

fn check_outside_untouched(outside: &TempDir, links: bool) {
    assert_eq!(
        std::fs::read(outside.join("target.txt")).unwrap(),
        b"target"
    );
    assert!(
        names(&outside.join("target-dir")).is_empty(),
        "nothing written through a link"
    );
    if !links {
        eprintln!("links cannot be made here: the link cases were not checked");
    }
}

#[test]
fn skip_decides_without_reading_when_the_folder_holds_nothing_of_this_run() {
    let tmp = TempDir::new("skip-early");
    let dest = tmp.join("out");
    std::fs::create_dir_all(dest.join("sub")).unwrap();
    std::fs::write(dest.join("sub/a.txt"), b"old").unwrap();
    let mut disk = Disk::new(1 << 20);
    let plan = Plan {
        folders: vec![folder(1, None, "sub")],
        files: vec![file(10, Some(1), "a.txt", disk.put(b"new"))],
    };
    let (faulty, source) = disk.faulty();
    let mut opts = options();
    opts.on_exists = OnExists::Skip;
    let (summary, rec) = run(source, plan, &dest, opts);
    assert_eq!(summary.skipped, 1);
    assert_eq!(rec.status(10), "skipped:exists");
    assert_eq!(
        faulty.read_count(),
        0,
        "the existing file made reading unnecessary"
    );
    assert_eq!(std::fs::read(dest.join("sub/a.txt")).unwrap(), b"old");
}

#[test]
fn leftover_part_files_are_removed_when_a_job_starts() {
    let tmp = TempDir::new("leftovers");
    let dest = tmp.join("out");
    std::fs::create_dir_all(dest.join("old/deeper")).unwrap();
    std::fs::write(dest.join("~hds-17.part"), b"partial").unwrap();
    std::fs::write(dest.join("old/deeper/~hds-3.part"), b"partial").unwrap();
    std::fs::write(dest.join("old/~hds-x.part"), b"not ours").unwrap();
    std::fs::write(dest.join("old/keep.txt"), b"keep").unwrap();
    let mut disk = Disk::new(1 << 20);
    let plan = Plan {
        folders: Vec::new(),
        files: vec![file(1, None, "new.txt", disk.put(b"new"))],
    };
    let (summary, _) = run(disk.source(), plan, &dest, options());
    assert_eq!(summary.status, JobStatus::Completed);
    assert!(part_files(&dest).is_empty());
    assert!(dest.join("old/~hds-x.part").exists());
    assert!(dest.join("old/keep.txt").exists());
}

#[test]
fn times_are_set_on_files_and_bottom_up_on_created_folders() {
    let tmp = TempDir::new("times");
    let dest = tmp.join("out");
    std::fs::create_dir_all(dest.join("existing")).unwrap();
    let before = std::fs::metadata(dest.join("existing"))
        .unwrap()
        .modified()
        .unwrap();
    let mut disk = Disk::new(1 << 20);
    let mut no_times = file(3, Some(2), "none.txt", disk.put(b"no times"));
    no_times.times = Timestamps {
        created: 0,
        modified: 1 << 63,
        accessed: 0,
    };
    let plan = Plan {
        folders: vec![
            folder(1, None, "top"),
            folder(2, Some(1), "middle"),
            folder(4, None, "existing"),
        ],
        files: vec![
            file(1, Some(2), "a.txt", disk.put(b"a")),
            file(2, Some(1), "b.txt", disk.put(b"b")),
            no_times,
            file(5, Some(4), "c.txt", disk.put(b"c")),
        ],
    };
    let (summary, rec) = run(disk.source(), plan.clone(), &dest, options());
    assert_eq!(summary.status, JobStatus::Completed);
    let check = |path: PathBuf, t: Timestamps| {
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(
            meta.modified().unwrap(),
            filetime_to_system_time(t.modified).unwrap(),
            "{}",
            path.display()
        );
        if !crate::testutil::under_wine() {
            assert_eq!(
                meta.accessed().unwrap(),
                filetime_to_system_time(t.accessed).unwrap(),
                "{}",
                path.display()
            );
        }
    };
    // Folders first: reading files does not change them, listing would.
    check(dest.join("top"), plan.folders[0].times);
    check(dest.join("top/middle"), plan.folders[1].times);
    check(dest.join("top/middle/a.txt"), plan.files[0].times);
    check(dest.join("top/b.txt"), plan.files[1].times);
    // A folder that existed is not given times.
    let after = std::fs::metadata(dest.join("existing"))
        .unwrap()
        .modified()
        .unwrap();
    assert!(after >= before);
    assert_ne!(
        after,
        filetime_to_system_time(plan.folders[2].times.modified).unwrap()
    );
    // Times that cannot be set are a note, not a different status.
    assert_eq!(rec.status(3), "ok_unverified");
    assert!(
        rec.outcome(3).detail.contains("modification time not set"),
        "{}",
        rec.outcome(3).detail
    );
}

#[test]
fn a_lost_source_ends_the_job_and_leaves_nothing_of_the_current_file() {
    let tmp = TempDir::new("lost");
    let mut disk = Disk::new(40 << 20);
    let first = disk.put(b"first file");
    let big = disk.put(&pattern(20 << 20));
    let last = disk.put(b"never reached");
    let plan = Plan {
        folders: Vec::new(),
        files: vec![
            file(1, None, "first.txt", first),
            file(2, None, "big.bin", big),
            file(3, None, "last.txt", last),
        ],
    };
    let (faulty, source) = disk.faulty();
    faulty.lose_after_reads(2); // the first file and the first chunk of the big one
    let dest = tmp.join("out");
    let (summary, rec) = run(source, plan, &dest, options());
    assert_eq!(summary.status, JobStatus::Failed);
    assert_eq!(summary.error, Some(JobError::SourceLost));
    assert_eq!(summary.ok, 1);
    assert_eq!(rec.outcomes.len(), 1);
    let report_name = report_path(&summary)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(tree(&dest), [report_name.as_str(), "first.txt"]);
    assert_eq!(report_rows(&summary).len(), 1);
    let json = serde_json::to_value(&summary).unwrap();
    assert_eq!(json["status"], "failed");
    assert_eq!(json["error"], "source_lost");
}

#[test]
fn a_cancel_removes_the_current_part_file() {
    let tmp = TempDir::new("cancel");
    let mut disk = Disk::new(40 << 20);
    let first = disk.put(b"first file");
    let big = disk.put(&pattern(24 << 20));
    let plan = Plan {
        folders: Vec::new(),
        files: vec![
            file(1, None, "first.txt", first),
            file(2, None, "big.bin", big),
        ],
    };
    let (faulty, source) = disk.faulty();
    let cancel = Arc::new(AtomicBool::new(false));
    faulty.cancel_after_reads(3, cancel.clone());
    let dest = tmp.join("out");
    let (summary, rec) = run_with(source, plan, &dest, options(), &cancel);
    assert_eq!(summary.status, JobStatus::Cancelled);
    assert_eq!(summary.error, None);
    assert_eq!(
        rec.outcomes.len(),
        1,
        "the file being written is not listed"
    );
    let report_name = report_path(&summary)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(tree(&dest), [report_name.as_str(), "first.txt"]);
    assert_eq!(
        serde_json::to_value(&summary).unwrap()["status"],
        "cancelled"
    );
}

#[test]
fn the_destination_guard_decides_before_anything_is_created() {
    let tmp = TempDir::new("guard");
    let disk = Disk::new(1 << 20);
    let plan = Plan {
        folders: vec![folder(1, None, "f")],
        files: Vec::new(),
    };
    // On this system the disk of a destination cannot be determined for a disk source.
    let len = disk.data.len() as u64;
    let on_disk = || {
        RangedSource::new(
            Arc::new(
                MemSource::new(disk.data.clone()).at_location(SourceLocation::Disk { number: 3 }),
            ),
            0..len,
        )
        .unwrap()
    };
    let dest = tmp.join("out");
    let mut opts = RecoverOptions::new(vec![Protected::session(0..len)]);
    let mut recorder = Recorder::default();
    let cancel = AtomicBool::new(false);
    if cfg!(windows) {
        eprintln!("SKIPPED part: on Windows the guard resolves real disks");
    } else {
        match recover(
            on_disk(),
            plan.clone(),
            &dest,
            opts.clone(),
            &mut recorder,
            &cancel,
        ) {
            Err(StartError::DestinationRefused(check)) => {
                assert_eq!(check.verdict, Verdict::UnknownDisk);
                assert!(!check.message.is_empty());
            }
            other => panic!("{other:?}"),
        }
        assert!(!dest.exists(), "nothing was created");
        opts.allow_unknown_disk = true;
        let summary =
            recover(on_disk(), plan.clone(), &dest, opts, &mut recorder, &cancel).unwrap();
        assert_eq!(summary.status, JobStatus::Completed);
        assert!(dest.join("f").is_dir());
    }
    // A relative path is never accepted.
    match recover(
        disk.source(),
        plan,
        Path::new("relative/out"),
        options(),
        &mut recorder,
        &cancel,
    ) {
        Err(StartError::DestinationRefused(check)) => {
            assert_eq!(check.verdict, Verdict::InvalidPath)
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn contradictory_plans_are_refused() {
    let tmp = TempDir::new("invalid");
    let disk = Disk::new(1 << 20);
    let src = FileSource::Extents {
        runs: Vec::new(),
        real_size: 0,
        initialized_size: 0,
    };
    let cases = [
        Plan {
            folders: vec![folder(1, None, "a"), folder(1, None, "b")],
            files: Vec::new(),
        },
        Plan {
            folders: vec![folder(1, Some(9), "a")],
            files: Vec::new(),
        },
        Plan {
            folders: vec![
                folder(1, Some(2), "a"),
                folder(2, Some(1), "b"),
                folder(3, None, "c"),
            ],
            files: Vec::new(),
        },
        Plan {
            folders: vec![folder(1, Some(1), "self")],
            files: Vec::new(),
        },
        Plan {
            folders: Vec::new(),
            files: vec![file(1, Some(5), "a.txt", src)],
        },
    ];
    let dest = tmp.join("out");
    for plan in cases {
        match Recovery::prepare(disk.source(), plan.clone(), &dest, options()) {
            Err(StartError::InvalidPlan(why)) => assert!(!why.is_empty()),
            other => panic!("{plan:?}: {other:?}"),
        }
        assert!(!dest.exists());
    }
}

#[test]
fn a_rejected_name_is_retried_once_with_the_fallback_name() {
    let tmp = TempDir::new("fallback");
    let mut disk = Disk::new(1 << 20);
    let long_file = "f".repeat(400) + ".jpeg";
    let long_dir = "d".repeat(400);
    let mut plan = Plan {
        folders: vec![folder(7, None, &long_dir)],
        files: vec![file(
            42,
            Some(7),
            &long_file,
            disk.put(b"\xFF\xD8\xFF\xE0 a JPEG start"),
        )],
    };
    plan.files[0].record_number = Some(4242);
    let dest = tmp.join("out");
    // Names are fitted to 1000 units, which this file system does not take.
    let job = Recovery::prepare_with(
        disk.source(),
        plan,
        &dest,
        options(),
        Overrides {
            component: Some(ComponentLimit::Utf16Units(1000)),
            max_file_bytes: None,
        },
    )
    .unwrap();
    let mut rec = Recorder::default();
    let summary = job.run(&mut rec, &AtomicBool::new(false));
    assert_eq!(
        summary.status,
        JobStatus::Completed,
        "{:?}",
        summary.message
    );
    let expected = sep(&format!(
        "{}~7/{}~4242.jpeg",
        "d".repeat(24),
        "f".repeat(24)
    ));
    assert_eq!(rec.outcome(42).output_path, expected);
    assert_eq!(rec.status(42), "ok", "the extension survived the fallback");
    assert!(rec.outcome(42).detail.contains("refused the name"));
    assert_eq!(
        std::fs::read(dest.join(&expected)).unwrap(),
        b"\xFF\xD8\xFF\xE0 a JPEG start"
    );
}

#[test]
fn files_beyond_the_destination_file_size_limit_are_skipped() {
    let tmp = TempDir::new("too-large");
    let mut disk = Disk::new(1 << 20);
    let huge = FileSource::Extents {
        runs: vec![Run::Hole { len: 5 << 30 }],
        real_size: 5 << 30,
        initialized_size: 0,
    };
    let plan = Plan {
        folders: Vec::new(),
        files: vec![
            file(1, None, "huge.vhd", huge),
            file(2, None, "small.txt", disk.put(b"ok")),
        ],
    };
    let (faulty, source) = disk.faulty();
    let job = Recovery::prepare_with(
        source,
        plan,
        &tmp.join("out"),
        options(),
        Overrides {
            component: None,
            max_file_bytes: Some(u64::from(u32::MAX)),
        },
    )
    .unwrap();
    let mut rec = Recorder::default();
    let summary = job.run(&mut rec, &AtomicBool::new(false));
    assert_eq!(summary.status, JobStatus::Completed);
    assert_eq!(rec.status(1), "skipped:too_large");
    assert!(rec.outcome(1).detail.contains("4294967295"));
    assert_eq!(rec.status(2), "ok_unverified");
    assert_eq!(faulty.read_count(), 1, "only the small file was read");
}

#[test]
fn progress_events_come_at_most_four_times_a_second() {
    let tmp = TempDir::new("progress");
    let mut disk = Disk::new(4 << 20);
    let mut plan = Plan::default();
    for i in 0..40u64 {
        plan.files
            .push(file(i, None, &format!("f{i}.txt"), disk.put(&[b'x'; 1000])));
    }
    let (faulty, source) = disk.faulty();
    faulty.set_delay(Duration::from_millis(40)); // about 1.6 s in all
    let (summary, rec) = run(source, plan, &tmp.join("out"), options());
    assert_eq!(summary.status, JobStatus::Completed);
    assert!(rec.progress.len() >= 3, "{}", rec.progress.len());
    for pair in rec.progress.windows(2) {
        assert!(pair[1].0.duration_since(pair[0].0) >= Duration::from_millis(249));
    }
    let last = &rec.progress.last().unwrap().1;
    assert_eq!(last.files_total, 40);
    assert_eq!(last.bytes_total, 40_000);
    assert!(last.files_done <= 40 && last.bytes_done <= 40_000);
    let json = serde_json::to_value(last).unwrap();
    for key in [
        "files_done",
        "files_total",
        "bytes_done",
        "bytes_total",
        "current",
        "speed_bps",
        "eta_seconds",
        "elapsed_seconds",
        "ok",
        "warnings",
        "failed",
        "skipped",
    ] {
        assert!(json.get(key).is_some(), "{key}");
    }
    assert_eq!(json.as_object().unwrap().len(), 12);
}

#[test]
fn the_summary_has_the_wire_form_of_recover_done() {
    let tmp = TempDir::new("summary");
    let mut disk = Disk::new(1 << 20);
    let plan = Plan {
        folders: Vec::new(),
        files: vec![file(1, None, "a.txt", disk.put(b"a"))],
    };
    let (summary, rec) = run(disk.source(), plan, &tmp.join("out"), options());
    let json = serde_json::to_value(&summary).unwrap();
    let keys: Vec<&String> = json.as_object().unwrap().keys().collect();
    let mut expected = [
        "status",
        "error",
        "files_total",
        "ok",
        "warnings",
        "failed",
        "skipped",
        "bytes_written",
        "dest",
        "report_path",
    ];
    expected.sort();
    let mut keys: Vec<&str> = keys.into_iter().map(String::as_str).collect();
    keys.sort();
    assert_eq!(keys, expected);
    assert_eq!(json["status"], "completed");
    assert!(json["error"].is_null());
    let outcome = serde_json::to_value(&rec.outcomes[0]).unwrap();
    assert_eq!(outcome["status"], "ok_unverified");
    assert_eq!(outcome["rating"], "good");
}

#[test]
fn a_full_destination_ends_the_job_once() {
    // Needs a small file system: HDS_RECOVER_NOSPACE_DIR names a directory on
    // one (for example a 1 MiB tmpfs: mount -t tmpfs -o size=1m tmpfs DIR).
    let Some(base) = std::env::var_os("HDS_RECOVER_NOSPACE_DIR") else {
        eprintln!("SKIPPED: set HDS_RECOVER_NOSPACE_DIR to a directory on a small file system (about 1 MiB)");
        return;
    };
    let tmp = TempDir::below(Path::new(&base), "nospace");
    let free = {
        let probe = hds_io::out::OutDir::open_root(tmp.path()).unwrap();
        match probe.free_space() {
            Ok(free) => free,
            Err(e) => {
                eprintln!("SKIPPED: the file system at HDS_RECOVER_NOSPACE_DIR does not report its free space here: {e}");
                return;
            }
        }
    };
    assert!(
        free < 64 << 20,
        "HDS_RECOVER_NOSPACE_DIR is not small: {free} bytes free"
    );
    let mut disk = Disk::new((3 * free as usize).max(4 << 20) + (1 << 20));
    let piece = (free / 3) as usize;
    let plan = Plan {
        folders: Vec::new(),
        files: (0..6u64)
            .map(|i| file(i, None, &format!("f{i}.bin"), disk.put(&pattern(piece))))
            .collect(),
    };
    let dest = tmp.join("out");
    let (summary, rec) = run(disk.source(), plan, &dest, options());
    assert_eq!(summary.status, JobStatus::Failed, "{summary:?}");
    assert_eq!(summary.error, Some(JobError::NoSpace));
    assert!(summary.ok >= 1 && summary.ok < 6, "{summary:?}");
    assert_eq!(
        summary.failed, 0,
        "the job ends once instead of failing every file"
    );
    assert!(
        part_files(&dest).is_empty(),
        "the current part file is removed"
    );
    for outcome in &rec.outcomes {
        assert_eq!(
            std::fs::metadata(dest.join(&outcome.output_path))
                .unwrap()
                .len(),
            piece as u64
        );
    }
    let written = tree(&dest)
        .into_iter()
        .filter(|p| p.ends_with(".bin"))
        .count();
    assert_eq!(written as u64, summary.ok);
    assert_eq!(report_rows(&summary).len() as u64, summary.ok);
}

#[test]
fn display_paths_drop_the_verbatim_prefix() {
    assert_eq!(display_path(Path::new(r"\\?\D:\Rescued")), r"D:\Rescued");
    assert_eq!(
        display_path(Path::new(r"\\?\UNC\srv\share\x")),
        r"\\srv\share\x"
    );
    assert_eq!(display_path(Path::new("/tmp/x")), "/tmp/x");
}

#[test]
fn status_texts() {
    assert_eq!(FileStatus::Ok.to_string(), "ok");
    assert_eq!(FileStatus::OkUnverified.to_string(), "ok_unverified");
    assert_eq!(
        FileStatus::Warning {
            flags: FlagSet::of(&[Flag::Sparse, Flag::Deleted, Flag::BadSectors]),
            context: RatingContext {
                geometry_not_validated: true,
                carved_end_unverified: false
            }
        }
        .to_string(),
        "warning:deleted;bad_sectors;geometry_not_validated"
    );
    for (reason, text) in [
        (SkipReason::Compressed, "skipped:compressed"),
        (SkipReason::Encrypted, "skipped:encrypted"),
        (SkipReason::Placeholder, "skipped:placeholder"),
        (SkipReason::Embedded, "skipped:embedded"),
        (SkipReason::Damaged, "skipped:damaged"),
        (SkipReason::Exists, "skipped:exists"),
        (SkipReason::TooLarge, "skipped:too_large"),
        (SkipReason::PathTooLong, "skipped:path_too_long"),
    ] {
        assert_eq!(FileStatus::Skipped(reason).to_string(), text);
        assert_eq!(
            serde_json::to_value(reason).unwrap(),
            text.strip_prefix("skipped:").unwrap()
        );
    }
    for (reason, text) in [
        (FailReason::RecordChanged, "failed:record_changed"),
        (FailReason::WriteError, "failed:write_error"),
        (FailReason::IoError, "failed:io_error"),
    ] {
        assert_eq!(FileStatus::Failed(reason).to_string(), text);
    }
    let _ = SystemTime::now();
    let _ = Ordering::Relaxed;
    let _: &dyn BlockSource = &MemSource::new(Vec::new());
}

fn random_name(rng: &mut hds_io::testing::XorShift) -> Vec<u16> {
    const PIECES: [&str; 12] = [
        "a",
        "Z",
        ".",
        " ",
        ":",
        "*",
        "CON",
        "nul",
        "~hds-1.part",
        "HDSavior-report-",
        "\u{E9}",
        "\u{1F600}",
    ];
    let len = if rng.below(20) == 0 {
        300
    } else {
        rng.below(12)
    };
    let mut name = Vec::new();
    for _ in 0..len {
        match rng.below(8) {
            0 => name.push(rng.below(0x20) as u16),
            1 => name.push(0xD800 + rng.below(0x800) as u16), // a surrogate, paired or not
            _ => name.extend(PIECES[rng.below(PIECES.len() as u64) as usize].encode_utf16()),
        }
    }
    name
}

fn random_source(rng: &mut hds_io::testing::XorShift, disk_len: u64) -> FileSource {
    if rng.below(4) == 0 {
        return FileSource::Resident {
            record_offset: rng.below(disk_len + 8192) / 512 * 512,
            record_size: [1024, 4096, 1000, 0, 65_536][rng.below(5) as usize],
            value_offset: rng.below(2048) as u32,
            value_len: rng.below(2048) as u32,
            hash: rng.next_u64(),
        };
    }
    let mut runs = Vec::new();
    for _ in 0..rng.below(5) {
        let len = rng.below(40_000);
        runs.push(match rng.below(4) {
            0 => Run::Hole { len },
            1 => Run::Data {
                offset: u64::MAX - rng.below(1000),
                len,
            },
            _ => Run::Data {
                offset: rng.below(disk_len + 100_000),
                len,
            },
        });
    }
    let covered: u64 = runs.iter().map(Run::len).sum();
    let real_size = rng.below(covered + 20_000);
    FileSource::Extents {
        runs,
        real_size,
        initialized_size: if rng.below(3) == 0 {
            rng.below(real_size + 1)
        } else {
            real_size
        },
    }
}

#[test]
fn random_plans_finish_and_account_for_every_file() {
    // HDS_RECOVER_RANDOM_ROUNDS=<n> runs a longer search.
    let rounds: usize = std::env::var("HDS_RECOVER_RANDOM_ROUNDS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(16);
    let seed = std::env::var("HDS_RECOVER_RANDOM_SEED")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0xBAD_C0DE);
    let mut rng = hds_io::testing::XorShift::new(seed);
    let all_flags: Vec<Flag> = Flag::ALL.to_vec();
    for round in 0..rounds {
        let tmp = TempDir::new("random");
        let disk = Disk::new(2 << 20);
        let disk_len = disk.data.len() as u64;
        let source = RangedSource::new(
            Arc::new(MemSource::new(disk.data.clone())),
            65_536..disk_len - 65_536,
        )
        .unwrap();
        let mut plan = Plan::default();
        let folders = rng.below(30);
        for i in 0..folders {
            let parent = (i > 0 && rng.below(4) != 0).then(|| 1 + rng.below(i));
            plan.folders.push(PlanFolder {
                id: i + 1,
                parent,
                name: random_name(&mut rng),
                times: Timestamps {
                    created: rng.next_u64(),
                    modified: [0, 1 << 63, u64::MAX, FT_2020][rng.below(4) as usize],
                    accessed: rng.next_u64() >> 1,
                },
                in_use: rng.below(4) != 0,
                record_number: (rng.below(3) != 0).then(|| rng.below(1000)),
            });
        }
        for node in 0..rng.below(60) {
            let source = random_source(&mut rng, disk_len);
            let mut flags = FlagSet::new();
            for _ in 0..rng.below(3) {
                flags.insert(all_flags[rng.below(all_flags.len() as u64) as usize]);
            }
            plan.files.push(PlanFile {
                node,
                folder: (folders > 0 && rng.below(5) != 0).then(|| 1 + rng.below(folders)),
                name: random_name(&mut rng),
                ext: ["jpg", "png", "", "txt", "pdf"][rng.below(5) as usize].into(),
                size: source.size(),
                first_data_offset: source.first_offset(),
                source,
                times: times(rng.next_u64() >> 2),
                flags,
                context: RatingContext {
                    geometry_not_validated: rng.below(4) == 0,
                    carved_end_unverified: rng.below(8) == 0,
                },
                in_use: rng.below(4) != 0,
                record_number: (rng.below(3) != 0).then(|| rng.below(1000)),
                record_offset: None,
                original_path: String::from_utf16_lossy(&random_name(&mut rng)),
            });
        }
        let mut opts = options();
        opts.on_exists = [OnExists::Rename, OnExists::Skip, OnExists::Overwrite][round % 3];
        opts.skip_damaged = round % 2 == 0;
        let files = plan.files.clone();
        let dest = tmp.join("out");
        let (summary, rec) = run(source, plan, &dest, opts);
        assert_eq!(
            summary.status,
            JobStatus::Completed,
            "round {round}: {:?}",
            summary.message
        );
        assert_eq!(rec.outcomes.len(), files.len());
        assert_eq!(
            summary.ok + summary.warnings + summary.failed + summary.skipped,
            summary.files_total
        );
        assert_eq!(report_rows(&summary).len(), files.len());
        assert!(part_files(&dest).is_empty(), "round {round}");
        let mut seen = std::collections::HashSet::new();
        for outcome in &rec.outcomes {
            let file = files.iter().find(|f| f.node == outcome.node).unwrap();
            if outcome.status.is_written() {
                assert!(
                    seen.insert(outcome.output_path.clone()),
                    "round {round}: {} twice",
                    outcome.output_path
                );
                let len = std::fs::metadata(dest.join(&outcome.output_path))
                    .unwrap()
                    .len();
                assert_eq!(len, outcome.bytes_written);
                assert_eq!(len, file.source.size());
            }
            if crate::rate(file.flags, file.context) == Rating::Unsupported {
                assert!(matches!(outcome.status, FileStatus::Skipped(_)));
            }
        }
    }
}

// ------------------------------------------------------------------------------------------
// Lazy plans (`Recovery::prepare_lazy`)
// ------------------------------------------------------------------------------------------

/// A [`PlanFiles`] over full entries that answers every question
/// separately, as a catalog does, and can fail one file.
struct Pieces {
    files: Vec<PlanFile>,
    fail_node: Option<u64>,
    described: Arc<std::sync::atomic::AtomicUsize>,
}

impl PlanFiles for Pieces {
    fn count(&self) -> usize {
        self.files.len()
    }
    fn facts(&self, index: usize) -> Option<crate::plan::FileFacts> {
        self.files.facts(index)
    }
    fn place(&mut self, index: usize) -> Result<crate::plan::FilePlace, String> {
        self.files.place(index)
    }
    fn name(&mut self, index: usize) -> Result<crate::plan::FileName, String> {
        self.files.name(index)
    }
    fn file(&mut self, index: usize) -> Result<PlanFile, String> {
        self.described.fetch_add(1, Ordering::SeqCst);
        let file = self.files.file(index)?;
        if Some(file.node) == self.fail_node {
            return Err(format!("node {} vanished", file.node));
        }
        Ok(file)
    }
}

fn lazy(plan: Plan, fail_node: Option<u64>) -> (LazyPlan, Arc<std::sync::atomic::AtomicUsize>) {
    let described = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    (
        LazyPlan {
            folders: plan.folders,
            files: Box::new(Pieces {
                files: plan.files,
                fail_node,
                described: described.clone(),
            }),
        },
        described,
    )
}

/// A plan with colliding names in several folders and files in an order
/// that differs from their disk order.
fn colliding_plan(disk: &mut Disk) -> Plan {
    let mut plan = Plan {
        folders: vec![
            folder(1, None, "Photos"),
            folder(2, Some(1), "photos"),
            folder(3, None, "PHOTOS"),
            folder(4, Some(1), "Empty"),
        ],
        files: Vec::new(),
    };
    let names = [
        "a.txt", "A.TXT", "a.txt", "photos", "b?.txt", "CON.txt", "c.txt",
    ];
    let mut sources: Vec<FileSource> = (0..21)
        .map(|i| disk.put(format!("content {i}").as_bytes()))
        .collect();
    sources.reverse();
    for (i, source) in sources.into_iter().enumerate() {
        let folder = [None, Some(1), Some(2), Some(3)][i % 4];
        let mut f = file(i as u64 + 1, folder, names[i % names.len()], source);
        f.in_use = i % 3 != 0;
        f.record_number = Some(100 - i as u64);
        plan.files.push(f);
    }
    plan
}

#[test]
fn a_lazy_plan_writes_what_the_full_plan_writes() {
    let tmp = TempDir::new("lazy-same");
    let mut disk = Disk::new(1 << 20);
    let plan = colliding_plan(&mut disk);
    let files = plan.files.len();
    let eager_dest = tmp.join("eager");
    let (eager, eager_rec) = run(disk.source(), plan.clone(), &eager_dest, options());
    let lazy_dest = tmp.join("lazy");
    let (lazy_plan, described) = lazy(plan, None);
    let job = Recovery::prepare_lazy(disk.source(), lazy_plan, &lazy_dest, options()).unwrap();
    // Preparing describes no file in full.
    assert_eq!(described.load(Ordering::SeqCst), 0);
    assert_eq!(job.files_total(), eager.files_total);
    let mut lazy_rec = Recorder::default();
    let summary = job.run(&mut lazy_rec, &AtomicBool::new(false));
    assert_eq!(summary.status, JobStatus::Completed);
    // Each file is described once, when it is written.
    assert_eq!(described.load(Ordering::SeqCst), files);
    assert_eq!(tree(&eager_dest).len(), tree(&lazy_dest).len());
    let strip = |mut rows: Vec<String>| {
        rows.retain(|p| !p.contains("HDSavior-report-"));
        rows
    };
    assert_eq!(strip(tree(&eager_dest)), strip(tree(&lazy_dest)));
    assert_eq!(report_rows(&eager), report_rows(&summary));
    let outcomes = |rec: &Recorder| {
        rec.outcomes
            .iter()
            .map(|o| (o.node, o.output_path.clone(), o.status.to_string()))
            .collect::<Vec<_>>()
    };
    assert_eq!(outcomes(&eager_rec), outcomes(&lazy_rec));
    // Collisions were resolved (H4 rule 5), and folders were created.
    assert!(eager_rec
        .outcomes
        .iter()
        .any(|o| o.output_path.ends_with("a (2).txt")));
    assert!(lazy_dest.join("Photos").join("Empty").is_dir());
}

#[test]
fn a_lazy_file_that_cannot_be_described_fails_alone() {
    let tmp = TempDir::new("lazy-fail");
    let mut disk = Disk::new(1 << 20);
    let plan = colliding_plan(&mut disk);
    let total = plan.files.len() as u64;
    let (lazy_plan, _) = lazy(plan, Some(5));
    let dest = tmp.join("out");
    let job = Recovery::prepare_lazy(disk.source(), lazy_plan, &dest, options()).unwrap();
    let mut rec = Recorder::default();
    let summary = job.run(&mut rec, &AtomicBool::new(false));
    assert_eq!(summary.status, JobStatus::Completed);
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.files_total, total);
    assert_eq!(rec.status(5), "failed:io_error");
    assert!(rec.outcome(5).detail.contains("vanished"));
    assert_eq!(report_rows(&summary).len() as u64, total);
    assert!(part_files(&dest).is_empty());
}

#[test]
fn a_contradictory_lazy_plan_fails_the_job_and_a_cancel_before_naming_stops_it() {
    let tmp = TempDir::new("lazy-invalid");
    let mut disk = Disk::new(1 << 20);
    let src = disk.put(b"x");
    let plan = Plan {
        folders: Vec::new(),
        files: vec![file(1, Some(5), "a.txt", src)],
    };
    let (lazy_plan, _) = lazy(plan.clone(), None);
    // Only the destination is checked when the job is prepared.
    let job = Recovery::prepare_lazy(disk.source(), lazy_plan, &tmp.join("a"), options()).unwrap();
    let summary = job.run(&mut (), &AtomicBool::new(false));
    assert_eq!(summary.status, JobStatus::Failed);
    assert_eq!(summary.error, Some(JobError::Internal));
    assert!(summary
        .message
        .clone()
        .unwrap_or_default()
        .contains("not in the plan"));
    assert!(report_rows(&summary).is_empty());
    // A cancel that comes before the names are assigned.
    let plan = colliding_plan(&mut disk);
    let (lazy_plan, described) = lazy(plan, None);
    let job = Recovery::prepare_lazy(disk.source(), lazy_plan, &tmp.join("b"), options()).unwrap();
    let summary = job.run(&mut (), &AtomicBool::new(true));
    assert_eq!(summary.status, JobStatus::Cancelled);
    assert_eq!(described.load(Ordering::SeqCst), 0);
    assert!(report_rows(&summary).is_empty());
}
