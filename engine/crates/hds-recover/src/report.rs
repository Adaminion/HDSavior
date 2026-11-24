//! The recovery report (ALGORITHMS H10): `HDSavior-report-<UTC
//! yyyymmdd-hhmmss>.csv` in the destination, created when the job starts,
//! one row appended and flushed per file. UTF-8 with BOM, CRLF line ends,
//! RFC 4180 quoting, and the formula guard: a cell that starts with `=`,
//! `+`, `-`, `@`, TAB or CR gets an apostrophe in front, so that a
//! spreadsheet shows it as text instead of evaluating it.

use std::fmt::Write as _;
use std::io::{self, Write};
use std::time::SystemTime;

use hds_io::out::{OutDir, OutFile};

/// UTF-8 byte order mark: lets spreadsheet programs detect the encoding.
pub(crate) const BOM: &[u8] = b"\xEF\xBB\xBF";

/// The columns, in order (ALGORITHMS H10).
pub(crate) const COLUMNS: [&str; 11] = [
    "status",
    "flags",
    "size",
    "bytes_written",
    "output_path",
    "original_path",
    "mft_record",
    "record_offset",
    "first_data_offset",
    "modified_utc",
    "detail",
];

/// How many report names are tried when two jobs start in the same second.
const NAME_ATTEMPTS: u32 = 100;

/// One row of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row<'a> {
    pub status: &'a str,
    pub flags: &'a str,
    pub size: u64,
    pub bytes_written: u64,
    pub output_path: &'a str,
    pub original_path: &'a str,
    pub mft_record: Option<u64>,
    pub record_offset: Option<u64>,
    pub first_data_offset: Option<u64>,
    /// FILETIME of the modification time; shown when valid.
    pub modified: u64,
    pub detail: &'a str,
}

/// Appends one cell: formula guard, then RFC 4180 quoting when the cell
/// holds a comma, a quote, CR or LF.
pub(crate) fn push_cell(out: &mut String, cell: &str) {
    let guarded = matches!(
        cell.chars().next(),
        Some('=' | '+' | '-' | '@' | '\t' | '\r')
    );
    let quoted = cell.contains([',', '"', '\r', '\n']);
    if quoted {
        out.push('"');
    }
    if guarded {
        out.push('\'');
    }
    for c in cell.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    if quoted {
        out.push('"');
    }
}

/// One line of cells, ended by CRLF.
pub(crate) fn line(cells: &[&str]) -> String {
    let mut out = String::new();
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        push_cell(&mut out, cell);
    }
    out.push_str("\r\n");
    out
}

fn optional(value: Option<u64>) -> String {
    value.map(|v| v.to_string()).unwrap_or_default()
}

/// The line of one row.
pub(crate) fn row_line(row: &Row<'_>) -> String {
    let size = row.size.to_string();
    let written = row.bytes_written.to_string();
    let record = optional(row.mft_record);
    let record_offset = optional(row.record_offset);
    let first = optional(row.first_data_offset);
    let modified = format_filetime(row.modified).unwrap_or_default();
    line(&[
        row.status,
        row.flags,
        &size,
        &written,
        row.output_path,
        row.original_path,
        &record,
        &record_offset,
        &first,
        &modified,
        row.detail,
    ])
}

/// BOM and header line: the first bytes of every report.
pub(crate) fn header() -> Vec<u8> {
    let mut bytes = BOM.to_vec();
    bytes.extend_from_slice(line(&COLUMNS).as_bytes());
    bytes
}

/// Days since 1970-01-01 to (year, month, day) in the proleptic Gregorian
/// calendar (H. Hinnant's `civil_from_days`). `None` outside ±10^9 days,
/// far beyond any FILETIME.
#[allow(clippy::arithmetic_side_effects)] // bounded: |days| <= 10^9 keeps every term small
fn civil_from_days(days: i64) -> Option<(i64, u32, u32)> {
    if !(-1_000_000_000..=1_000_000_000).contains(&days) {
        return None;
    }
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = yoe + era * 400 + i64::from(month <= 2);
    Some((year, u32::try_from(month).ok()?, u32::try_from(day).ok()?))
}

/// Unix seconds to `(year, month, day, hour, minute, second)` in UTC.
fn civil(unix_seconds: i64) -> Option<(i64, u32, u32, u32, u32, u32)> {
    let days = unix_seconds.div_euclid(86_400);
    let secs = u32::try_from(unix_seconds.rem_euclid(86_400)).ok()?;
    let (y, m, d) = civil_from_days(days)?;
    Some((y, m, d, secs / 3600, secs % 3600 / 60, secs % 60))
}

/// A FILETIME as `YYYY-MM-DD hh:mm:ss` (UTC), `None` unless `0 < t < 2^63`.
pub(crate) fn format_filetime(filetime: u64) -> Option<String> {
    let (seconds, _) = hds_ntfs::filetime_to_unix_time(filetime)?;
    let (y, mo, d, h, mi, s) = civil(seconds)?;
    let mut out = String::with_capacity(19);
    write!(out, "{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}").ok()?;
    Some(out)
}

/// Seconds since the Unix epoch of `time` (negative before 1970).
fn unix_seconds(time: SystemTime) -> i64 {
    match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_secs())
            .map(|s| s.saturating_neg())
            .unwrap_or(i64::MIN),
    }
}

/// `HDSavior-report-<yyyymmdd-hhmmss>.csv` for the UTC time `now`, with
/// ` (n)` before the extension for `n >= 2`.
pub(crate) fn report_name(now: SystemTime, n: u32) -> String {
    let (y, mo, d, h, mi, s) = civil(unix_seconds(now)).unwrap_or((1970, 1, 1, 0, 0, 0));
    let mut name = format!("HDSavior-report-{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}");
    if n >= 2 {
        let _ = write!(name, " ({n})");
    }
    name.push_str(".csv");
    name
}

/// The report file of one job.
#[derive(Debug)]
pub(crate) struct ReportFile {
    file: OutFile,
    name: String,
}

impl ReportFile {
    /// Creates the report in `dir` (create-new: never an existing file) and
    /// writes the BOM and the header.
    pub(crate) fn create(dir: &OutDir, now: SystemTime) -> io::Result<ReportFile> {
        let mut last = io::Error::new(io::ErrorKind::AlreadyExists, "no free report name");
        for n in 1..=NAME_ATTEMPTS {
            let name = report_name(now, n);
            match dir.create_new(&name) {
                Ok(mut file) => {
                    file.write_all(&header())?;
                    file.flush()?;
                    return Ok(ReportFile { file, name });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = e,
                Err(e) => return Err(e),
            }
        }
        Err(last)
    }

    /// The file name of the report.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Appends one row and hands it to the system at once.
    pub(crate) fn append(&mut self, row: &Row<'_>) -> io::Result<()> {
        self.file.write_all(row_line(row).as_bytes())?;
        self.file.flush()
    }

    /// Flushes the report to the device.
    pub(crate) fn sync(&self) -> io::Result<()> {
        self.file.sync()
    }
}

#[cfg(test)]
mod tests;
