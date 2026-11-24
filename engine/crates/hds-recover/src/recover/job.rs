//! Running a prepared recovery job (ALGORITHMS H5 to H12).

use std::cmp::Reverse;
use std::collections::HashSet;
use std::io::{self, Write};
use std::path::MAIN_SEPARATOR_STR;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use hds_io::out::{
    classify_error, CommitOutcome, OnExists, OutDir, OutErrorKind, PartFile, Timestamps,
    MAX_PATH_UNITS,
};
use hds_io::{fallback_component, sanitize_name, AlignedBuf, ComponentLimit, RangedSource};

use super::tree::{dir_key, Tree};
use super::{
    display_path, unsupported_reason, will_attempt, FailReason, FileOutcome, FileStatus, JobError,
    JobStatus, RecoverObserver, RecoverOptions, RecoverProgress, RecoverSummary, Recovery,
    SkipReason,
};
use crate::health::{rate, Flag, FlagSet, Rating};
use crate::naming::{self, claim_from, exact_key, folded_key, next_suffix, units, Assigned};
use crate::plan::{Plan, PlanFile};
use crate::progress::{eta_seconds, Meter};
use crate::reader::{signature_flag, ContentReader, ReadError, ReadReport, HEAD_LEN, READ_CHUNK};
use crate::report::{ReportFile, Row};

/// Directories kept open for reuse.
const DIR_CACHE: usize = 64;

/// How many ranges a report row lists.
const LISTED_RANGES: usize = 5;

/// The two signature flags, replaced by the check of the bytes written.
const SIGNATURE_FLAGS: FlagSet = FlagSet::of(&[Flag::SignatureOk, Flag::SignatureMismatch]);

/// What became of a plan folder.
#[derive(Debug, Clone)]
enum FolderState {
    /// Not handled yet.
    Pending,
    /// Exists under `name`; `len` is the length of its path in UTF-16 units.
    Ready {
        name: Box<str>,
        len: usize,
        created: bool,
    },
    /// Its path (or an ancestor's) would be longer than the limit.
    TooLong { len: usize },
    /// It (or an ancestor) could not be created.
    Failed { why: String },
}

/// Why the job ends before its last file (ALGORITHMS H12).
#[derive(Debug)]
enum End {
    Cancelled,
    SourceLost(String),
    NoSpace(String),
    Report(io::Error),
}

/// What a commit achieved.
enum Committed {
    Written {
        name: String,
        replaced: bool,
        fallback: bool,
    },
    Skipped,
    TooLong {
        len: usize,
    },
    TooLarge,
    Failed(String),
}

/// The result of one file, before it is reported.
struct Done {
    status: FileStatus,
    flags: FlagSet,
    output_path: String,
    bytes_written: u64,
    notes: Vec<String>,
}

impl Done {
    fn new(flags: FlagSet, output_path: String) -> Done {
        Done {
            status: FileStatus::Failed(FailReason::IoError),
            flags,
            output_path,
            bytes_written: 0,
            notes: Vec::new(),
        }
    }

    fn skipped(mut self, reason: SkipReason, note: impl Into<String>) -> Done {
        self.status = FileStatus::Skipped(reason);
        self.notes.push(note.into());
        self
    }

    fn failed(mut self, reason: FailReason, note: impl Into<String>) -> Done {
        self.status = FileStatus::Failed(reason);
        self.notes.push(note.into());
        self
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Counts {
    files_done: u64,
    ok: u64,
    warnings: u64,
    failed: u64,
    skipped: u64,
    bytes_written: u64,
    bytes_done: u64,
}

/// `ranges` as `a-b, c-d` (inclusive ends), at most [`LISTED_RANGES`].
fn list_ranges(ranges: impl Iterator<Item = std::ops::Range<u64>>) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut more = false;
    for (i, range) in ranges.enumerate() {
        if i >= LISTED_RANGES {
            more = true;
            break;
        }
        parts.push(format!("{}-{}", range.start, range.end.saturating_sub(1)));
    }
    let mut text = parts.join(", ");
    if more {
        text.push_str(", ...");
    }
    text
}

/// Report notes for what a read met.
fn read_notes(report: &ReadReport, notes: &mut Vec<String>) {
    if report.bad_bytes() > 0 {
        notes.push(format!(
            "{} unreadable bytes recovered as zeros (file bytes {})",
            report.bad_bytes(),
            list_ranges(report.bad_ranges())
        ));
    }
    if report.out_of_bounds_bytes() > 0 {
        notes.push(format!(
            "{} bytes lie outside the scanned area and were recovered as zeros (file bytes {})",
            report.out_of_bounds_bytes(),
            list_ranges(report.out_of_bounds_ranges())
        ));
    }
    if report.missing_bytes() > 0 {
        notes.push(format!(
            "{} bytes are not covered by the file's cluster list and were recovered as zeros (file bytes {})",
            report.missing_bytes(),
            list_ranges(report.missing_ranges())
        ));
    }
}

/// Files in ascending order of their first data byte (ALGORITHMS H5);
/// files that store no byte come last.
fn file_order(plan: &Plan) -> Vec<usize> {
    let mut order: Vec<usize> = (0..plan.files.len()).collect();
    order.sort_by_key(|&i| {
        plan.files.get(i).map_or((u64::MAX, i), |f| {
            (
                f.source
                    .first_offset()
                    .or(f.first_data_offset)
                    .unwrap_or(u64::MAX),
                i,
            )
        })
    });
    order
}

struct Runner<'a> {
    source: &'a RangedSource,
    plan: &'a Plan,
    tree: &'a Tree,
    options: &'a RecoverOptions,
    root: &'a OutDir,
    root_len: usize,
    max_file_bytes: Option<u64>,
    limit: ComponentLimit,
    report: &'a mut ReportFile,
    observer: &'a mut dyn RecoverObserver,
    cancel: &'a AtomicBool,
    /// Names claimed per directory (folded); grows when a name is retried.
    claimed: HashSet<u64>,
    /// Entries this run created, per directory (exact names).
    produced: HashSet<u64>,
    /// Per plan folder, and for the destination: entries this run created
    /// in it.
    outputs: Vec<u32>,
    root_outputs: u32,
    folders: Vec<FolderState>,
    cache: Vec<(usize, OutDir, u64)>,
    tick: u64,
    buffer: Vec<u8>,
    scratch: Option<AlignedBuf>,
    counts: Counts,
    meter: Meter,
    bytes_total: u64,
    files_total: u64,
}

impl Runner<'_> {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn progress(&mut self, current: &str) {
        let now = Instant::now();
        if !self.meter.due(now) {
            return;
        }
        let rates = self.meter.event(now, self.counts.bytes_done);
        let progress = RecoverProgress {
            files_done: self.counts.files_done,
            files_total: self.files_total,
            bytes_done: self.counts.bytes_done,
            bytes_total: self.bytes_total,
            current: current.to_string(),
            speed_bps: rates.speed_bps,
            eta_seconds: eta_seconds(
                self.bytes_total.saturating_sub(self.counts.bytes_done),
                rates.speed_bps,
            ),
            elapsed_seconds: rates.elapsed_seconds,
            ok: self.counts.ok,
            warnings: self.counts.warnings,
            failed: self.counts.failed,
            skipped: self.counts.skipped,
        };
        self.observer.progress(&progress);
    }

    // -- directories ------------------------------------------------------------------------

    fn cache_get(&mut self, folder: usize) -> Option<OutDir> {
        self.tick = self.tick.wrapping_add(1);
        let tick = self.tick;
        self.cache
            .iter_mut()
            .find(|(key, _, _)| *key == folder)
            .map(|(_, dir, used)| {
                *used = tick;
                dir.clone()
            })
    }

    fn cache_put(&mut self, folder: usize, dir: OutDir) {
        self.tick = self.tick.wrapping_add(1);
        if let Some(slot) = self.cache.iter_mut().find(|(key, _, _)| *key == folder) {
            *slot = (folder, dir, self.tick);
        } else if self.cache.len() < DIR_CACHE {
            self.cache.push((folder, dir, self.tick));
        } else if let Some(oldest) = self.cache.iter_mut().min_by_key(|(_, _, used)| *used) {
            *oldest = (folder, dir, self.tick);
        }
    }

    fn folder_name(&self, folder: usize) -> &str {
        match self.folders.get(folder) {
            Some(FolderState::Ready { name, .. }) => name,
            _ => self.tree.folder_names.get(folder).map_or("_", |a| &a.name),
        }
    }

    /// The open directory of `folder`, reopened level by level from the
    /// nearest open ancestor (never through a link: see `OutDir`).
    fn dir_for(&mut self, folder: Option<usize>) -> io::Result<OutDir> {
        let Some(folder) = folder else {
            return Ok(self.root.clone());
        };
        if let Some(dir) = self.cache_get(folder) {
            return Ok(dir);
        }
        let tree = self.tree;
        let mut chain = vec![folder];
        let mut up = tree.parent.get(folder).copied().flatten();
        let mut dir = loop {
            match up {
                None => break self.root.clone(),
                Some(p) => {
                    if let Some(dir) = self.cache_get(p) {
                        break dir;
                    }
                    chain.push(p);
                    up = tree.parent.get(p).copied().flatten();
                }
            }
        };
        while let Some(level) = chain.pop() {
            let name = match self.folders.get(level) {
                Some(FolderState::Ready { name, .. }) => name.clone(),
                _ => return Err(io::Error::other("the folder was not created")),
            };
            dir = dir.open_subdir(&name)?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "the folder has disappeared")
            })?;
            self.cache_put(level, dir.clone());
        }
        Ok(dir)
    }

    fn outputs_in(&self, folder: Option<usize>) -> u32 {
        match folder {
            None => self.root_outputs,
            Some(i) => self.outputs.get(i).copied().unwrap_or(0),
        }
    }

    fn note_output(&mut self, folder: Option<usize>, name: &str) {
        self.produced.insert(exact_key(dir_key(folder), name));
        match folder {
            None => self.root_outputs = self.root_outputs.saturating_add(1),
            Some(i) => {
                if let Some(count) = self.outputs.get_mut(i) {
                    *count = count.saturating_add(1);
                }
            }
        }
    }

    /// The next free variant of `base` after `suffix` in directory `dir`.
    fn alternative(&mut self, dir: u64, base: &[u16], suffix: u32) -> Option<Assigned> {
        claim_from(
            &mut self.claimed,
            dir,
            base,
            next_suffix(suffix)?,
            self.limit,
        )
    }

    /// The output path of an entry named `name` in `folder`, relative to
    /// the destination.
    fn output_path(&self, folder: Option<usize>, name: &str) -> String {
        let mut parts: Vec<&str> = vec![name];
        let mut up = folder;
        // The tree has no cycles; the bound only makes that obvious.
        for _ in 0..=self.tree.parent.len() {
            let Some(i) = up else { break };
            parts.push(self.folder_name(i));
            up = self.tree.parent.get(i).copied().flatten();
        }
        parts.reverse();
        parts.join(MAIN_SEPARATOR_STR)
    }

    // -- folders ----------------------------------------------------------------------------

    fn create_folders(&mut self) -> Result<(), End> {
        let tree = self.tree;
        for &folder in &tree.order {
            if self.cancelled() {
                return Err(End::Cancelled);
            }
            let state = self.create_folder(folder)?;
            if let Some(slot) = self.folders.get_mut(folder) {
                *slot = state;
            }
            self.progress("");
        }
        Ok(())
    }

    fn create_folder(&mut self, folder: usize) -> Result<FolderState, End> {
        let (plan, tree) = (self.plan, self.tree);
        let (Some(spec), Some(assigned)) =
            (plan.folders.get(folder), tree.folder_names.get(folder))
        else {
            return Ok(FolderState::Failed {
                why: "internal error: unknown folder".into(),
            });
        };
        let parent = tree.parent.get(folder).copied().flatten();
        let parent_len = match parent {
            None => self.root_len,
            Some(p) => match self.folders.get(p) {
                Some(FolderState::Ready { len, .. }) => *len,
                Some(FolderState::TooLong { len }) => {
                    return Ok(FolderState::TooLong {
                        len: len.saturating_add(1).saturating_add(units(&assigned.name)),
                    })
                }
                Some(FolderState::Failed { why }) => {
                    return Ok(FolderState::Failed { why: why.clone() })
                }
                Some(FolderState::Pending) | None => {
                    return Ok(FolderState::Failed {
                        why: "internal error: the parent folder was not handled first".into(),
                    })
                }
            },
        };
        let dir = match self.dir_for(parent) {
            Ok(dir) => dir,
            Err(e) => {
                return Ok(FolderState::Failed {
                    why: format!("the folder it lies in cannot be opened: {e}"),
                })
            }
        };
        let key = dir_key(parent);
        let mut name = assigned.name.to_string();
        let mut base = sanitize_name(&spec.name);
        let mut suffix = assigned.suffix;
        let mut fallback_used = false;
        for _ in 0..naming::MAX_NAME_ATTEMPTS {
            let len = parent_len.saturating_add(1).saturating_add(units(&name));
            if len > MAX_PATH_UNITS {
                return Ok(FolderState::TooLong { len });
            }
            match dir.subdir(&name) {
                Ok((out, created)) => {
                    if created {
                        self.note_output(parent, &name);
                    }
                    self.cache_put(folder, out);
                    return Ok(FolderState::Ready {
                        name: name.into_boxed_str(),
                        len,
                        created,
                    });
                }
                Err(e) => match classify_error(&e) {
                    // Taken by a file, a link or anything else that is not a
                    // plain directory: never entered, the next name is taken.
                    OutErrorKind::Exists => match self.alternative(key, &base, suffix) {
                        Some(next) => {
                            name = next.name.into();
                            suffix = next.suffix;
                        }
                        None => break,
                    },
                    OutErrorKind::NameRejected if !fallback_used => {
                        fallback_used = true;
                        base = fallback_component(
                            &base,
                            spec.record_number.unwrap_or(spec.id),
                            self.limit,
                        );
                        name = String::from_utf16_lossy(&base);
                        suffix = 1;
                        self.claimed.insert(folded_key(key, &name));
                    }
                    OutErrorKind::NoSpace => return Err(End::NoSpace(e.to_string())),
                    _ => return Ok(FolderState::Failed { why: e.to_string() }),
                },
            }
        }
        Ok(FolderState::Failed {
            why: "no free name was found for it".into(),
        })
    }

    /// Sets the times of the folders this run created, deepest first, after
    /// everything inside them is written (ALGORITHMS H8). Best effort: a
    /// time that cannot be set changes nothing else.
    fn set_folder_times(&mut self) {
        let (plan, tree) = (self.plan, self.tree);
        let mut created: Vec<usize> = self
            .folders
            .iter()
            .enumerate()
            .filter_map(|(i, state)| {
                matches!(state, FolderState::Ready { created: true, .. }).then_some(i)
            })
            .collect();
        created.sort_by_key(|&i| Reverse(tree.depth.get(i).copied().unwrap_or(0)));
        for folder in created {
            let Some(times) = plan.folders.get(folder).map(|f| f.times) else {
                continue;
            };
            if times == Timestamps::default() {
                continue;
            }
            if let Ok(dir) = self.dir_for(Some(folder)) {
                let _ = dir.set_times(&times);
            }
        }
    }

    // -- files ------------------------------------------------------------------------------

    fn process_files(&mut self) -> Result<(), End> {
        for index in file_order(self.plan) {
            if self.cancelled() {
                return Err(End::Cancelled);
            }
            let start = self.counts.bytes_done;
            let done = self.process_file(index)?;
            self.finish_file(index, start, done)?;
        }
        Ok(())
    }

    fn process_file(&mut self, index: usize) -> Result<Done, End> {
        let (plan, tree) = (self.plan, self.tree);
        let (Some(file), Some(assigned)) = (plan.files.get(index), tree.file_names.get(index))
        else {
            return Ok(Done::new(FlagSet::new(), String::new())
                .failed(FailReason::IoError, "internal error: unknown file"));
        };
        let folder = tree.file_folder.get(index).copied().flatten();
        let done = Done::new(file.flags, self.output_path(folder, &assigned.name));
        if let Some(reason) = unsupported_reason(file.flags) {
            return Ok(done.skipped(
                reason,
                "the content is not stored in a form HDSavior can recover",
            ));
        }
        if self.options.skip_damaged && rate(file.flags, file.context) == Rating::Damaged {
            return Ok(done.skipped(
                SkipReason::Damaged,
                "rated damaged before reading, and damaged files are skipped",
            ));
        }
        let parent_len = match folder {
            None => self.root_len,
            Some(f) => match self.folders.get(f) {
                Some(FolderState::Ready { len, .. }) => *len,
                Some(FolderState::TooLong { len }) => {
                    return Ok(done.skipped(
                        SkipReason::PathTooLong,
                        format!("the folder's path would be {len} characters long"),
                    ))
                }
                Some(FolderState::Failed { why }) => {
                    return Ok(done.failed(
                        FailReason::WriteError,
                        format!("the folder could not be created: {why}"),
                    ))
                }
                Some(FolderState::Pending) | None => {
                    return Ok(done.failed(FailReason::WriteError, "the folder was not created"))
                }
            },
        };
        let len = parent_len
            .saturating_add(1)
            .saturating_add(units(&assigned.name));
        if len > MAX_PATH_UNITS {
            return Ok(done.skipped(
                SkipReason::PathTooLong,
                format!("the output path would be {len} characters long"),
            ));
        }
        let content_size = file.source.size();
        if let Some(max) = self.max_file_bytes {
            if content_size > max {
                return Ok(done.skipped(
                    SkipReason::TooLarge,
                    format!("{content_size} bytes; the destination's file system holds at most {max} bytes per file"),
                ));
            }
        }
        let dir = match self.dir_for(folder) {
            Ok(dir) => dir,
            Err(e) => {
                return Ok(done.failed(
                    FailReason::WriteError,
                    format!("the folder cannot be opened: {e}"),
                ))
            }
        };
        // With `skip`, a name that existed before this run created anything in
        // the folder is decided without reading the file.
        if self.options.on_exists == OnExists::Skip
            && self.outputs_in(folder) == 0
            && matches!(dir.entry(&assigned.name), Ok(Some(_)))
        {
            return Ok(done.skipped(SkipReason::Exists, "an entry with this name exists already"));
        }
        let source = self.source;
        let cancel = self.cancel;
        let scratch = self.scratch.take().unwrap_or_else(|| AlignedBuf::new(0));
        let mut reader = ContentReader::with_buffer(source, &file.source, cancel, scratch);
        let result = self.write_file(&mut reader, file, &dir, folder, parent_len, assigned, done);
        self.scratch = Some(reader.into_buffer());
        result
    }

    /// Reading, writing, committing and judging one file.
    #[allow(clippy::too_many_arguments)]
    fn write_file(
        &mut self,
        reader: &mut ContentReader<'_>,
        file: &PlanFile,
        dir: &OutDir,
        folder: Option<usize>,
        parent_len: usize,
        assigned: &Assigned,
        mut done: Done,
    ) -> Result<Done, End> {
        let on_read_error =
            |e: ReadError, mut done: Done, report: &ReadReport| -> Result<Done, End> {
                done.flags |= report.flags();
                match e {
                    ReadError::SourceLost(e) => Err(End::SourceLost(e.to_string())),
                    ReadError::Cancelled => Err(End::Cancelled),
                    ReadError::RecordChanged => {
                        Ok(done.failed(FailReason::RecordChanged, e.to_string()))
                    }
                    other => Ok(done.failed(FailReason::IoError, other.to_string())),
                }
            };
        if let Err(e) = reader.verify_record() {
            return on_read_error(e, done, reader.report());
        }
        let mut part = match dir.create_part() {
            Ok(part) => part,
            Err(e) => {
                return match classify_error(&e) {
                    OutErrorKind::NoSpace => Err(End::NoSpace(e.to_string())),
                    _ => Ok(done.failed(
                        FailReason::WriteError,
                        format!("the part file cannot be created: {e}"),
                    )),
                }
            }
        };
        if self.buffer.len() < READ_CHUNK {
            self.buffer.resize(READ_CHUNK, 0);
        }
        let size = reader.size();
        let start = self.counts.bytes_done;
        let mut head: Vec<u8> = Vec::new();
        let mut pos = 0u64;
        while pos < size {
            if self.cancelled() {
                let _ = part.discard();
                return Err(End::Cancelled);
            }
            let n = match reader.read_at(pos, &mut self.buffer) {
                Ok(n) => n,
                Err(e) => {
                    let _ = part.discard();
                    return on_read_error(e, done, reader.report());
                }
            };
            let Some(chunk) = self.buffer.get(..n).filter(|c| !c.is_empty()) else {
                // Cannot happen (`read_at` returns data up to the size); a short
                // file must never be committed as complete.
                let _ = part.discard();
                return Ok(done.failed(
                    FailReason::IoError,
                    "internal error: the content ended before its size",
                ));
            };
            if pos == 0 {
                head = chunk.get(..HEAD_LEN.min(n)).unwrap_or(chunk).to_vec();
            }
            if let Err(e) = part.write_all(chunk) {
                let _ = part.discard();
                done.flags |= reader.flags();
                return match classify_error(&e) {
                    OutErrorKind::NoSpace => Err(End::NoSpace(e.to_string())),
                    OutErrorKind::TooLarge => Ok(done.skipped(
                        SkipReason::TooLarge,
                        format!("the destination refused a file of this size: {e}"),
                    )),
                    _ => Ok(done.failed(FailReason::WriteError, format!("write error: {e}"))),
                };
            }
            pos = pos.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
            self.counts.bytes_done = start.saturating_add(pos.min(file.size));
            self.progress(&file.original_path);
        }
        done.flags = done.flags.difference(SIGNATURE_FLAGS) | reader.flags();
        let times = match part.finish(&file.times) {
            Ok(times) => times,
            Err(e) => {
                let _ = part.discard();
                return Ok(done.failed(FailReason::WriteError, e.to_string()));
            }
        };
        match self.commit(&mut part, folder, parent_len, file, assigned)? {
            Committed::Written {
                name,
                replaced,
                fallback,
            } => {
                if let Some(flag) = signature_flag(&head, size, &file.ext) {
                    done.flags.insert(flag);
                }
                done.output_path = self.output_path(folder, &name);
                done.bytes_written = pos;
                done.status = match rate(done.flags, file.context) {
                    Rating::Good if done.flags.contains(Flag::SignatureOk) => FileStatus::Ok,
                    Rating::Good => FileStatus::OkUnverified,
                    _ => FileStatus::Warning {
                        flags: done.flags,
                        context: file.context,
                    },
                };
                read_notes(reader.report(), &mut done.notes);
                if done.flags.contains(Flag::SignatureMismatch) {
                    done.notes.push(format!(
                        "the content does not start the way a .{} file does",
                        file.ext
                    ));
                }
                if replaced {
                    done.notes
                        .push("an existing file of this name was replaced".into());
                }
                if fallback {
                    done.notes.push(
                        "the destination refused the name; the file was saved under a shorter one"
                            .into(),
                    );
                }
                if file.size != size {
                    done.notes.push(format!(
                        "the catalog size {} differs from the content size {size}",
                        file.size
                    ));
                }
                if let Some(note) = times.note() {
                    done.notes.push(note);
                }
                self.counts.bytes_written = self.counts.bytes_written.saturating_add(pos);
                Ok(done)
            }
            Committed::Skipped => {
                Ok(done.skipped(SkipReason::Exists, "an entry with this name exists already"))
            }
            Committed::TooLong { len } => Ok(done.skipped(
                SkipReason::PathTooLong,
                format!("the output path would be {len} characters long"),
            )),
            Committed::TooLarge => Ok(done.skipped(
                SkipReason::TooLarge,
                "the destination refused a file of this size",
            )),
            Committed::Failed(why) => Ok(done.failed(FailReason::WriteError, why)),
        }
    }

    /// Gives the finished part file its final name (ALGORITHMS H6): without
    /// replacing; a name of this run is never overwritten or skipped for;
    /// a name that existed before is handled by `on_exists`; a name the file
    /// system rejects is retried once with the fallback name.
    fn commit(
        &mut self,
        part: &mut PartFile,
        folder: Option<usize>,
        parent_len: usize,
        file: &PlanFile,
        assigned: &Assigned,
    ) -> Result<Committed, End> {
        let key = dir_key(folder);
        let mut name = assigned.name.to_string();
        let mut base = sanitize_name(&file.name);
        let mut suffix = assigned.suffix;
        let mut fallback_used = false;
        for _ in 0..naming::MAX_NAME_ATTEMPTS {
            let len = parent_len.saturating_add(1).saturating_add(units(&name));
            if len > MAX_PATH_UNITS {
                // The caller drops the part file, which deletes it.
                return Ok(Committed::TooLong { len });
            }
            let produced = &self.produced;
            let result = part.commit(&name, self.options.on_exists, &|existing| {
                produced.contains(&exact_key(key, existing))
            });
            match result {
                Ok(CommitOutcome::Committed | CommitOutcome::Overwritten) => {
                    let replaced = matches!(result, Ok(CommitOutcome::Overwritten));
                    self.note_output(folder, &name);
                    return Ok(Committed::Written {
                        name,
                        replaced,
                        fallback: fallback_used,
                    });
                }
                Ok(CommitOutcome::Skipped) => return Ok(Committed::Skipped),
                Ok(CommitOutcome::NameTaken) => match self.alternative(key, &base, suffix) {
                    Some(next) => {
                        name = next.name.into();
                        suffix = next.suffix;
                    }
                    None => break,
                },
                Err(e) => match classify_error(&e) {
                    OutErrorKind::Exists => match self.alternative(key, &base, suffix) {
                        Some(next) => {
                            name = next.name.into();
                            suffix = next.suffix;
                        }
                        None => break,
                    },
                    OutErrorKind::NameRejected if !fallback_used => {
                        fallback_used = true;
                        base = fallback_component(
                            &base,
                            file.record_number.unwrap_or(file.node),
                            self.limit,
                        );
                        name = String::from_utf16_lossy(&base);
                        suffix = 1;
                        self.claimed.insert(folded_key(key, &name));
                    }
                    OutErrorKind::NoSpace => return Err(End::NoSpace(e.to_string())),
                    OutErrorKind::TooLarge => return Ok(Committed::TooLarge),
                    _ => return Ok(Committed::Failed(format!("the file cannot be named: {e}"))),
                },
            }
        }
        Ok(Committed::Failed(
            "no free name was found for the file".into(),
        ))
    }

    /// Appends the report row, counts the file and tells the observer.
    fn finish_file(&mut self, index: usize, start: u64, done: Done) -> Result<(), End> {
        let Some(file) = self.plan.files.get(index) else {
            return Ok(());
        };
        let status = done.status.to_string();
        let flags = done.flags.to_string();
        let detail = done.notes.join("; ");
        let appended = self.report.append(&Row {
            status: &status,
            flags: &flags,
            size: file.size,
            bytes_written: done.bytes_written,
            output_path: &done.output_path,
            original_path: &file.original_path,
            mft_record: file.record_number,
            record_offset: file.record_offset,
            first_data_offset: file.first_data_offset,
            modified: file.times.modified,
            detail: &detail,
        });
        let counts = &mut self.counts;
        counts.files_done = counts.files_done.saturating_add(1);
        match done.status {
            FileStatus::Ok | FileStatus::OkUnverified => counts.ok = counts.ok.saturating_add(1),
            FileStatus::Warning { .. } => counts.warnings = counts.warnings.saturating_add(1),
            FileStatus::Skipped(_) => counts.skipped = counts.skipped.saturating_add(1),
            FileStatus::Failed(_) => counts.failed = counts.failed.saturating_add(1),
        }
        if will_attempt(file, self.options.skip_damaged) {
            counts.bytes_done = start.saturating_add(file.size);
        }
        self.observer.file_done(&FileOutcome {
            node: file.node,
            status: done.status,
            flags: done.flags,
            rating: rate(done.flags, file.context),
            output_path: done.output_path,
            bytes_written: done.bytes_written,
            detail,
        });
        self.progress(&file.original_path);
        appended.map_err(|e| match classify_error(&e) {
            OutErrorKind::NoSpace => End::NoSpace(e.to_string()),
            _ => End::Report(e),
        })
    }
}

/// Runs `job` (see [`Recovery::run`]).
pub(super) fn run(
    job: Recovery,
    observer: &mut dyn RecoverObserver,
    cancel: &AtomicBool,
) -> RecoverSummary {
    let Recovery {
        source,
        plan,
        options,
        dest_display,
        root,
        limits,
        limit,
        check: _,
        mut tree,
        mut report,
        bytes_total,
    } = job;
    let claimed = std::mem::take(&mut tree.claimed);
    let report_path = display_path(&root.path().join(report.name()));
    let folder_count = plan.folders.len();
    let mut runner = Runner {
        source: &source,
        plan: &plan,
        tree: &tree,
        options: &options,
        root: &root,
        root_len: root.path_len_units(),
        max_file_bytes: limits.max_file_bytes,
        limit,
        report: &mut report,
        observer,
        cancel,
        claimed,
        produced: HashSet::new(),
        outputs: vec![0; folder_count],
        root_outputs: 0,
        folders: vec![FolderState::Pending; folder_count],
        cache: Vec::with_capacity(DIR_CACHE),
        tick: 0,
        buffer: Vec::new(),
        scratch: None,
        counts: Counts::default(),
        meter: Meter::new(Instant::now()),
        bytes_total,
        files_total: u64::try_from(plan.files.len()).unwrap_or(u64::MAX),
    };
    let end = runner
        .create_folders()
        .and_then(|()| runner.process_files());
    runner.set_folder_times();
    let counts = runner.counts;
    let files_total = runner.files_total;
    drop(runner);
    let _ = report.sync();
    let (status, error, message) = match end {
        Ok(()) => (JobStatus::Completed, None, None),
        Err(End::Cancelled) => (JobStatus::Cancelled, None, None),
        Err(End::SourceLost(why)) => (JobStatus::Failed, Some(JobError::SourceLost), Some(why)),
        Err(End::NoSpace(why)) => (JobStatus::Failed, Some(JobError::NoSpace), Some(why)),
        Err(End::Report(e)) => (
            JobStatus::Failed,
            Some(JobError::IoError),
            Some(format!("the report cannot be written: {e}")),
        ),
    };
    RecoverSummary {
        status,
        error,
        files_total,
        ok: counts.ok,
        warnings: counts.warnings,
        failed: counts.failed,
        skipped: counts.skipped,
        bytes_written: counts.bytes_written,
        dest: dest_display,
        report_path,
        message,
    }
}

#[cfg(test)]
mod tests;
