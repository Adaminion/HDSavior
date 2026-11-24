//! Output primitives: the only code of the engine that creates, writes,
//! renames or deletes files (safety invariant S6).
//!
//! Everything is done relative to an [`OutDir`], a directory that has been
//! verified to be a plain directory - not a symbolic link, junction, mounted
//! folder or any other reparse point - and is kept open for as long as the
//! value lives. An output tree starts at a root whose path is made canonical
//! (all links on the way to it resolved) and grows one verified level at a
//! time:
//!
//! * [`OutDir::create_root`] / [`OutDir::open_root`] - the canonical root;
//! * [`OutDir::subdir`] - one level down; an existing name is reused only if
//!   it is a plain directory;
//! * [`OutDir::create_part`] - a `~hds-<n>.part` file with create-new
//!   semantics; [`PartFile::finish`] sets its times on the writing handle
//!   and [`PartFile::commit`] renames it **without replacing**, applying the
//!   `on_exists` policy of ALGORITHMS H6;
//! * [`OutDir::create_new`], [`OutDir::open_existing`],
//!   [`OutDir::rename_noreplace`], [`OutDir::remove_file`] - for reports,
//!   images, session files and their side files;
//! * [`OutDir::remove_leftover_parts`] - cleans up after a killed run.
//!
//! Names are single path components that pass
//! [`is_valid_component`]; anything else is
//! refused before the file system is asked. There is no call in this module
//! that truncates an existing file on open, follows a link below the root or
//! replaces an existing name.
//!
//! How the guarantees are kept: on Unix every operation is relative to the
//! open directory descriptor (`openat` with `O_NOFOLLOW`, `mkdirat`,
//! `renameat2(RENAME_NOREPLACE)`, `unlinkat`), so no path is resolved twice.
//! On Windows paths are built from the canonical root with the `\\?\`
//! prefix; an existing name is trusted only after its directory entry, the
//! opened object and the object's resolved path all say that no link is
//! involved; a directory in use is held open in a way that denies renaming
//! and deleting it (where no other program's handle prevents that); files
//! are created with `CREATE_NEW` and renamed with `MoveFileExW` without the
//! replace flag.
//!
//! One file is out of reach altogether: an image that is open as the source
//! ([`ImageFile`](crate::ImageFile), safety invariant S4). Deleting it,
//! renaming it or opening it for writing fails - on Windows because the
//! source handle shares reading only, on Unix because this module compares
//! the identity of the file with the open sources first. So not even the
//! `overwrite` policy can remove the image when a recovered file happens to
//! carry its name.

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::sanitize::{is_valid_component, ComponentLimit};

#[cfg(unix)]
#[path = "out/unix.rs"]
mod sys;
#[cfg(windows)]
#[path = "out/windows.rs"]
mod sys;

/// Longest path, in UTF-16 units, that Windows accepts with the `\\?\` prefix.
pub const MAX_PATH_UNITS: usize = 32_767;

/// How many part-file names are tried before giving up.
const PART_NAME_ATTEMPTS: u32 = 10_000;

/// What kind of thing a name in an output directory is, without following links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A plain directory.
    Dir,
    /// A symbolic link, junction, mounted folder or any other reparse point.
    Link,
    /// Anything else (a device node, a pipe).
    Other,
}

/// Limits of the file system an output directory is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsLimits {
    /// Longest name of one file or directory.
    pub max_component: ComponentLimit,
    /// Largest file the file system can hold, if that is a practical limit
    /// (FAT: 4 GiB - 1); `None` otherwise.
    pub max_file_bytes: Option<u64>,
    /// Name of the file system as the system reports it (`"NTFS"`, `"FAT32"`,
    /// `"exFAT"`; on Linux a best-effort name), if known.
    pub fs_name: Option<String>,
}

/// Why an output operation failed, as far as the job has to tell the cases apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutErrorKind {
    /// The file system (or this module) rejects the name: too long or made of
    /// characters it cannot store. Recovery retries once with
    /// [`fallback_component`](crate::fallback_component).
    NameRejected,
    /// The destination is full (or the quota is used up): the job ends with `no_space`.
    NoSpace,
    /// The file is larger than the file system can hold.
    TooLarge,
    /// The name exists already.
    Exists,
    /// Any other error.
    Other,
}

/// Payload of the errors this module raises for names it refuses itself.
#[derive(Debug)]
struct NameError(String);

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NameError {}

fn name_error(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, NameError(message))
}

/// Sorts the error of an output operation into the classes a job reacts to.
pub fn classify_error(error: &io::Error) -> OutErrorKind {
    if error.get_ref().is_some_and(|inner| inner.is::<NameError>()) {
        return OutErrorKind::NameRejected;
    }
    if let Some(code) = error.raw_os_error() {
        if let Some(kind) = sys::classify_os_error(code) {
            return kind;
        }
    }
    match error.kind() {
        io::ErrorKind::AlreadyExists => OutErrorKind::Exists,
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => OutErrorKind::NoSpace,
        io::ErrorKind::FileTooLarge => OutErrorKind::TooLarge,
        io::ErrorKind::InvalidFilename => OutErrorKind::NameRejected,
        _ => OutErrorKind::Other,
    }
}

fn check_name(name: &str) -> io::Result<()> {
    let units: Vec<u16> = name.encode_utf16().collect();
    if is_valid_component(&units) {
        Ok(())
    } else {
        Err(name_error(format!(
            "{name:?} is not a valid name for an output file or directory"
        )))
    }
}

/// Creation, modification and access time as NTFS stores them: `FILETIME`
/// values, 100 ns units since 1601-01-01 UTC. Only values with
/// `0 < value < 2^63` are applied (ALGORITHMS H8); 0 therefore means "leave
/// this time alone".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Timestamps {
    /// Creation time.
    pub created: u64,
    /// Last modification time.
    pub modified: u64,
    /// Last access time.
    pub accessed: u64,
}

/// What happened to one of the three times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeOutcome {
    /// The time was set.
    Applied,
    /// The value is not a time that can be set (0, or 2^63 and above): left alone.
    Invalid,
    /// This system cannot set this time (creation time outside Windows).
    Unsupported,
    /// The file system refused (FAT cannot store dates before 1980, for example).
    Failed,
}

/// Result of setting times: one outcome per time. Never an error: a time
/// that cannot be set is a note in the report and changes no status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimesReport {
    /// Outcome for the creation time.
    pub created: TimeOutcome,
    /// Outcome for the modification time.
    pub modified: TimeOutcome,
    /// Outcome for the access time.
    pub accessed: TimeOutcome,
    /// Text of the first error the file system returned, if any.
    pub error: Option<String>,
}

impl TimesReport {
    /// A sentence for the report when something was not set; `None` when all
    /// three times were applied (or could not be on this system by design).
    pub fn note(&self) -> Option<String> {
        let mut parts = Vec::new();
        for (name, outcome) in [
            ("creation", self.created),
            ("modification", self.modified),
            ("access", self.accessed),
        ] {
            match outcome {
                TimeOutcome::Applied | TimeOutcome::Unsupported => {}
                TimeOutcome::Invalid => parts.push(format!("{name} time not set (no valid value)")),
                TimeOutcome::Failed => {
                    parts.push(format!("{name} time not set (refused by the destination)"))
                }
            }
        }
        if parts.is_empty() {
            return None;
        }
        let mut note = parts.join("; ");
        if let Some(error) = &self.error {
            note.push_str(": ");
            note.push_str(error);
        }
        Some(note)
    }
}

/// Converts a `FILETIME` to a `SystemTime` with checked arithmetic. `None`
/// for values that must not be set: 0 ("leave unchanged" for `SetFileTime`),
/// 2^63 and above (negative for the kernel; `u64::MAX` means "stop updating
/// this time"), and anything the platform's clock type cannot hold.
pub fn filetime_to_system_time(filetime: u64) -> Option<SystemTime> {
    if filetime == 0 || filetime > i64::MAX as u64 {
        return None;
    }
    const UNIX_EPOCH_AS_FILETIME_SECS: u64 = 11_644_473_600;
    let epoch_1601 =
        SystemTime::UNIX_EPOCH.checked_sub(Duration::from_secs(UNIX_EPOCH_AS_FILETIME_SECS))?;
    epoch_1601.checked_add(Duration::new(
        filetime / 10_000_000,
        ((filetime % 10_000_000) * 100) as u32,
    ))
}

/// Sets the given times on an open handle: all valid ones together, and one
/// by one if the file system refuses the set.
fn apply_times(file: &File, times: &Timestamps) -> TimesReport {
    let convert = |value: u64| filetime_to_system_time(value);
    let outcome = |value: Option<SystemTime>| {
        if value.is_some() {
            TimeOutcome::Failed
        } else {
            TimeOutcome::Invalid
        }
    };
    let (created, modified, accessed) = (
        convert(times.created),
        convert(times.modified),
        convert(times.accessed),
    );
    let mut report = TimesReport {
        created: outcome(created),
        modified: outcome(modified),
        accessed: outcome(accessed),
        error: None,
    };
    let created = if sys::CAN_SET_CREATION_TIME {
        created
    } else {
        if created.is_some() {
            report.created = TimeOutcome::Unsupported;
        }
        None
    };
    if created.is_none() && modified.is_none() && accessed.is_none() {
        return report;
    }
    let mark = |report: &mut TimesReport, c: bool, m: bool, a: bool| {
        if c {
            report.created = TimeOutcome::Applied;
        }
        if m {
            report.modified = TimeOutcome::Applied;
        }
        if a {
            report.accessed = TimeOutcome::Applied;
        }
    };
    match file.set_times(sys::file_times(created, modified, accessed)) {
        Ok(()) => mark(
            &mut report,
            created.is_some(),
            modified.is_some(),
            accessed.is_some(),
        ),
        Err(first) => {
            report.error = Some(first.to_string());
            for (c, m, a) in [
                (created, None, None),
                (None, modified, None),
                (None, None, accessed),
            ] {
                if (c.is_some() || m.is_some() || a.is_some())
                    && file.set_times(sys::file_times(c, m, a)).is_ok()
                {
                    mark(&mut report, c.is_some(), m.is_some(), a.is_some());
                }
            }
        }
    }
    report
}

/// An open, verified directory of an output tree.
struct Dir {
    /// Unix: the descriptor every operation is relative to. Windows: a handle
    /// that keeps the directory from being renamed or deleted, and through
    /// which it was verified and its times are set.
    handle: File,
    /// Canonical path (on Windows with the `\\?\` prefix).
    path: PathBuf,
    /// Longest name the file system of the tree accepts; names beyond it are
    /// refused here, with the same error on every system.
    name_limit: ComponentLimit,
}

/// A plain directory in which output may be created. Cheap to clone.
///
/// See the [module documentation](self) for the guarantees.
#[derive(Clone)]
pub struct OutDir {
    dir: Arc<Dir>,
}

impl fmt::Debug for OutDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutDir")
            .field("path", &self.dir.path)
            .finish()
    }
}

/// What [`OutDir::remove_leftover_parts`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Leftovers {
    /// Part files deleted.
    pub removed: u64,
    /// Part files that could not be deleted and directories that could not
    /// be looked into.
    pub failed: u64,
}

/// True for the names [`OutDir::create_part`] generates: `~hds-<digits>.part`.
pub fn is_part_name(name: &str) -> bool {
    name.strip_prefix("~hds-")
        .and_then(|rest| rest.strip_suffix(".part"))
        .is_some_and(|digits| {
            !digits.is_empty() && digits.len() <= 20 && digits.bytes().all(|b| b.is_ascii_digit())
        })
}

/// Splits the absolute path of an output *file* (an image, a session file)
/// into its directory and its name, and checks that the name is one the
/// output primitives accept.
pub fn split_file_path(path: &Path) -> io::Result<(PathBuf, String)> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            name_error(format!(
                "{} does not end in a usable file name",
                path.display()
            ))
        })?;
    check_name(name)?;
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => {
            Ok((parent.to_path_buf(), name.to_string()))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the path names no directory",
        )),
    }
}

impl OutDir {
    /// Opens an existing directory as the root of an output tree. Links on
    /// the way *to* the root are resolved (that is where the destination
    /// guard looked, too); the result is the canonical directory.
    pub fn open_root(path: &Path) -> io::Result<OutDir> {
        Ok(OutDir {
            dir: Arc::new(sys::open_root(path)?),
        })
    }

    /// Like [`open_root`](Self::open_root), but creates the directory and
    /// its missing ancestors first, one level at a time. `path` must be
    /// absolute. Run the destination guard on `path` before calling this
    /// (this function writes) and again on [`path`](Self::path) afterwards.
    pub fn create_root(path: &Path) -> io::Result<OutDir> {
        Ok(OutDir {
            dir: Arc::new(sys::create_root(path)?),
        })
    }

    /// The canonical path of the directory. On Windows it carries the `\\?\`
    /// prefix, which [`destination_check`](crate::destination_check) accepts.
    pub fn path(&self) -> &Path {
        &self.dir.path
    }

    /// Length of [`path`](Self::path) in UTF-16 units; a new entry adds one
    /// separator and its own length. Compare with [`MAX_PATH_UNITS`].
    pub fn path_len_units(&self) -> usize {
        sys::path_units(&self.dir.path)
    }

    /// Limits of the file system the directory is on.
    pub fn limits(&self) -> io::Result<FsLimits> {
        sys::limits(&self.dir)
    }

    /// Bytes available to the caller on the directory's file system.
    pub fn free_space(&self) -> io::Result<u64> {
        sys::free_space(&self.dir)
    }

    fn check_entry_name(&self, name: &str) -> io::Result<()> {
        check_name(name)?;
        let units: Vec<u16> = name.encode_utf16().collect();
        if !self.dir.name_limit.allows(&units) {
            return Err(name_error(format!(
                "the name {name:?} is longer than the destination allows"
            )));
        }
        if cfg!(windows) && self.path_len_units() + 1 + units.len() > MAX_PATH_UNITS {
            return Err(name_error(format!(
                "the path of {name:?} would exceed {MAX_PATH_UNITS} characters"
            )));
        }
        Ok(())
    }

    /// Creates the directory `name` in this directory, or reuses it if it
    /// exists already **and** is a plain directory. Returns the directory
    /// and whether it was created now.
    ///
    /// Fails with `AlreadyExists` when the name is taken by anything else -
    /// a file, a symbolic link, a junction, a mounted folder: the caller
    /// picks another name instead of entering it.
    pub fn subdir(&self, name: &str) -> io::Result<(OutDir, bool)> {
        self.check_entry_name(name)?;
        let (dir, created) = sys::make_subdir(&self.dir, name)?;
        Ok((OutDir { dir: Arc::new(dir) }, created))
    }

    /// Opens the existing plain directory `name`; `Ok(None)` when there is no
    /// such name. Fails with `AlreadyExists` when the name is something else.
    pub fn open_subdir(&self, name: &str) -> io::Result<Option<OutDir>> {
        self.check_entry_name(name)?;
        Ok(sys::open_subdir(&self.dir, name)?.map(|dir| OutDir { dir: Arc::new(dir) }))
    }

    /// Creates the file `name` for reading and writing. Fails with
    /// `AlreadyExists` if the name exists in any form, a dangling link
    /// included; nothing is ever truncated or followed.
    pub fn create_new(&self, name: &str) -> io::Result<OutFile> {
        self.check_entry_name(name)?;
        Ok(OutFile {
            file: sys::create_new(&self.dir, name)?,
        })
    }

    /// Opens the existing regular file `name` for reading and writing
    /// without truncating it (to append to a session file, to resume an
    /// image). A link or anything else that is not a regular file is refused.
    pub fn open_existing(&self, name: &str) -> io::Result<OutFile> {
        self.check_entry_name(name)?;
        Ok(OutFile {
            file: sys::open_existing(&self.dir, name)?,
        })
    }

    /// Creates a part file `~hds-<n>.part` with the next free number.
    pub fn create_part(&self) -> io::Result<PartFile> {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        for _ in 0..PART_NAME_ATTEMPTS {
            let name = format!("~hds-{}.part", COUNTER.fetch_add(1, Ordering::Relaxed));
            match sys::create_new(&self.dir, &name) {
                Ok(file) => {
                    return Ok(PartFile {
                        dir: self.clone(),
                        name,
                        file: Some(file),
                        done: false,
                    })
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no free name for a part file",
        ))
    }

    /// Renames `from` to `to` inside this directory. Fails with
    /// `AlreadyExists`, and changes nothing, when `to` exists in any form;
    /// the file system decides what "exists" means (case, short names).
    pub fn rename_noreplace(&self, from: &str, to: &str) -> io::Result<()> {
        self.check_entry_name(from)?;
        self.check_entry_name(to)?;
        sys::rename_noreplace(&self.dir, from, to)
    }

    /// Deletes the regular file `name`. Anything else under that name - a
    /// directory, a link - is refused with `InvalidInput`: the engine only
    /// ever deletes files, and never reaches through a link to do it.
    pub fn remove_file(&self, name: &str) -> io::Result<()> {
        self.check_entry_name(name)?;
        match sys::entry(&self.dir, name)? {
            Some((EntryKind::File, _)) => sys::remove_file(&self.dir, name),
            Some(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the name is not a regular file",
            )),
            None => Err(io::Error::new(io::ErrorKind::NotFound, "no such file")),
        }
    }

    /// What `name` is, without following a link; `None` if it does not exist.
    pub fn entry(&self, name: &str) -> io::Result<Option<EntryKind>> {
        self.check_entry_name(name)?;
        Ok(sys::entry(&self.dir, name)?.map(|(kind, _)| kind))
    }

    /// Sets the times of this directory itself. Call it after the last
    /// change inside the directory: every file created there moves its
    /// modification time again.
    pub fn set_times(&self, times: &Timestamps) -> TimesReport {
        apply_times(&self.dir.handle, times)
    }

    /// Deletes the part files (`~hds-<digits>.part`, regular files only) a
    /// killed run left in this directory and, if `recursive`, in the plain
    /// directories below it. Links are never entered. Run it when a job
    /// starts in a tree, so that every file under a final name is complete.
    ///
    /// An error is returned only when this directory itself cannot be read;
    /// problems below it are counted in [`Leftovers::failed`].
    pub fn remove_leftover_parts(&self, recursive: bool) -> io::Result<Leftovers> {
        struct Frame {
            dir: OutDir,
            subdirs: Vec<String>,
        }
        fn scan(dir: &OutDir, recursive: bool, report: &mut Leftovers) -> io::Result<Vec<String>> {
            let mut subdirs = Vec::new();
            let mut parts = Vec::new();
            sys::for_each_entry(&dir.dir, &mut |name, kind| match kind {
                EntryKind::File if is_part_name(name) => parts.push(name.to_string()),
                EntryKind::Dir if recursive => subdirs.push(name.to_string()),
                _ => {}
            })?;
            for part in parts {
                // The listing is only a hint; the removal looks again.
                match sys::entry(&dir.dir, &part) {
                    Ok(Some((EntryKind::File, _))) => match sys::remove_file(&dir.dir, &part) {
                        Ok(()) => report.removed += 1,
                        Err(_) => report.failed += 1,
                    },
                    Ok(_) => {}
                    Err(_) => report.failed += 1,
                }
            }
            Ok(subdirs)
        }

        let mut report = Leftovers::default();
        let subdirs = scan(self, recursive, &mut report)?;
        // Depth-first with one open directory per level, however wide the tree is.
        let mut stack = vec![Frame {
            dir: self.clone(),
            subdirs,
        }];
        while let Some(frame) = stack.last_mut() {
            let Some(name) = frame.subdirs.pop() else {
                stack.pop();
                continue;
            };
            // A name that is not valid for this module was not created by it.
            if check_name(&name).is_err() {
                continue;
            }
            let child = match sys::open_subdir(&frame.dir.dir, &name) {
                Ok(Some(dir)) => OutDir { dir: Arc::new(dir) },
                Ok(None) => continue,
                Err(e) => {
                    // Something that is not a plain directory (a link) is skipped, not an error.
                    if e.kind() != io::ErrorKind::AlreadyExists {
                        report.failed += 1;
                    }
                    continue;
                }
            };
            match scan(&child, recursive, &mut report) {
                Ok(subdirs) => stack.push(Frame {
                    dir: child,
                    subdirs,
                }),
                Err(_) => report.failed += 1,
            }
        }
        Ok(report)
    }
}

/// A file opened through [`OutDir::create_new`] or [`OutDir::open_existing`].
/// Reads, writes and seeks like a `File`; writes are not buffered.
#[derive(Debug)]
pub struct OutFile {
    file: File,
}

impl OutFile {
    /// Current length of the file in bytes.
    pub fn len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    /// True when the file is empty.
    pub fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Cuts or extends the file to `len` bytes (resuming an image truncates
    /// it to the confirmed byte count).
    pub fn set_len(&self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }

    /// Flushes file data and metadata to the device (`FlushFileBuffers` / `fsync`).
    pub fn sync(&self) -> io::Result<()> {
        self.file.sync_all()
    }

    /// Sets the file's times. Do it after the last write, as the last call
    /// before the file is dropped.
    pub fn set_times(&self, times: &Timestamps) -> TimesReport {
        apply_times(&self.file, times)
    }
}

impl Read for OutFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Write for OutFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Seek for OutFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.file.seek(pos)
    }
}

/// What to do when the final name of a recovered file exists already and
/// was **not** produced by this run (`on_exists` of the protocol).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnExists {
    /// Keep the existing file; the caller takes the next ` (n)` name.
    Rename,
    /// Keep the existing file and drop the new one (`skipped:exists`).
    Skip,
    /// Replace the existing file - only a regular file that is not a link:
    /// it is deleted, then the part file is renamed.
    Overwrite,
}

/// Result of [`PartFile::commit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitOutcome {
    /// The part file now carries the final name.
    Committed,
    /// An existing regular file was deleted and the part file took its name.
    Overwritten,
    /// The name is taken and stays taken: by a file of this run, by policy
    /// [`OnExists::Rename`], or by something that may not be overwritten (a
    /// directory, a link). The part file is untouched; call `commit` again
    /// with the next ` (n)` name.
    NameTaken,
    /// Policy [`OnExists::Skip`]: the existing file stays and the part file
    /// has been deleted.
    Skipped,
}

/// A `~hds-<n>.part` file being written. Dropping it without a successful
/// [`commit`](Self::commit) deletes it, so a failed or cancelled file leaves
/// nothing behind.
#[derive(Debug)]
pub struct PartFile {
    dir: OutDir,
    name: String,
    file: Option<File>,
    done: bool,
}

impl PartFile {
    /// The temporary name, `~hds-<n>.part`.
    pub fn name(&self) -> &str {
        &self.name
    }

    fn open_file(&mut self) -> io::Result<&mut File> {
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("the part file has been finished already"))
    }

    /// Cuts or extends the file to `len` bytes.
    pub fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.open_file()?.set_len(len)
    }

    /// Flushes data and metadata to the device.
    pub fn sync(&mut self) -> io::Result<()> {
        self.open_file()?.sync_all()
    }

    /// Sets the times on the writing handle, as the last operation on it,
    /// and closes it. Nothing can be written afterwards.
    pub fn finish(&mut self, times: &Timestamps) -> io::Result<TimesReport> {
        let file = self
            .file
            .take()
            .ok_or_else(|| io::Error::other("the part file has been finished already"))?;
        Ok(apply_times(&file, times))
    }

    /// Moves the part file to `final_name` without replacing anything
    /// (ALGORITHMS H6). Closes the file first if [`finish`](Self::finish)
    /// was not called.
    ///
    /// When the name exists, `produced_by_this_run` is asked whether the
    /// existing entry is one this run made. On Windows it is given the
    /// entry's name as the file system stores it, which can differ from
    /// `final_name` in case or be the long form of a short name; on Unix,
    /// where names are compared byte by byte, it is `final_name`. So a plain
    /// set of the names this run has committed in the directory, compared
    /// exactly, is the right thing to ask. If the answer is yes the outcome is
    /// [`CommitOutcome::NameTaken`] whatever the policy: a run never
    /// overwrites or skips in favour of its own output. Otherwise
    /// `on_exists` decides.
    ///
    /// After `Committed`, `Overwritten` or `Skipped` the part file is gone
    /// and further calls fail; after `NameTaken` or an error it still
    /// exists (and is deleted when the value is dropped).
    pub fn commit(
        &mut self,
        final_name: &str,
        on_exists: OnExists,
        produced_by_this_run: &dyn Fn(&str) -> bool,
    ) -> io::Result<CommitOutcome> {
        if self.done {
            return Err(io::Error::other(
                "the part file has been committed or discarded already",
            ));
        }
        self.dir.check_entry_name(final_name)?;
        // Close the writing handle: the rename and the times need it closed.
        self.file = None;
        let dir = &self.dir.dir;
        match sys::rename_noreplace(dir, &self.name, final_name) {
            Ok(()) => {
                self.done = true;
                return Ok(CommitOutcome::Committed);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let Some((kind, actual_name)) = sys::entry(dir, final_name)? else {
            // Gone again between the two calls: one more try, then the caller moves on.
            return match sys::rename_noreplace(dir, &self.name, final_name) {
                Ok(()) => {
                    self.done = true;
                    Ok(CommitOutcome::Committed)
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(CommitOutcome::NameTaken),
                Err(e) => Err(e),
            };
        };
        if produced_by_this_run(&actual_name) {
            return Ok(CommitOutcome::NameTaken);
        }
        match on_exists {
            OnExists::Rename => Ok(CommitOutcome::NameTaken),
            OnExists::Skip => {
                sys::remove_file(dir, &self.name)?;
                self.done = true;
                Ok(CommitOutcome::Skipped)
            }
            OnExists::Overwrite => {
                if kind != EntryKind::File {
                    return Ok(CommitOutcome::NameTaken);
                }
                sys::remove_file(dir, final_name)?;
                match sys::rename_noreplace(dir, &self.name, final_name) {
                    Ok(()) => {
                        self.done = true;
                        Ok(CommitOutcome::Overwritten)
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        Ok(CommitOutcome::NameTaken)
                    }
                    Err(e) => Err(e),
                }
            }
        }
    }

    /// Deletes the part file now (a cancelled or failed file).
    pub fn discard(mut self) -> io::Result<()> {
        if self.done {
            return Ok(());
        }
        self.file = None;
        self.done = true;
        sys::remove_file(&self.dir.dir, &self.name)
    }
}

impl Write for PartFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.open_file()?.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.open_file()?.flush()
    }
}

impl Drop for PartFile {
    fn drop(&mut self) {
        if !self.done {
            self.file = None;
            // Best effort: what stays behind is removed by the next job's cleanup.
            let _ = sys::remove_file(&self.dir.dir, &self.name);
        }
    }
}

#[cfg(test)]
mod tests;
