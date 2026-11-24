//! Runs `sniff` and `measure` on whole files and compares the measured
//! length with the file size.
//!
//! ```text
//! cargo run --release -p hds-carve --example probe -- FILE...
//! ```
//!
//! One line per file: size, what `sniff` says about the first bytes, what
//! `measure` says, how much it had to read, and a verdict: `ok` when the
//! measured length is the file size, `short` or `long` otherwise, `none`
//! when the file was recognised but not measured, `-` when it was not
//! recognised. Files are read in place, so multi-gigabyte videos are fine.
//!
//! Handy for trying the crate on a folder of real photos and videos:
//! every `short`, `long` or `none` on an intact file is worth a bug report.

use std::cell::Cell;
use std::fs::File;
use std::io;
use std::process::ExitCode;

use hds_carve::{measure, sniff, Confidence, ReadAt, SNIFF_LEN};

/// A file, read at arbitrary offsets, counting the traffic.
struct Disk {
    file: File,
    reads: Cell<u64>,
    bytes: Cell<u64>,
}

impl ReadAt for Disk {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        #[cfg(unix)]
        let n = std::os::unix::fs::FileExt::read_at(&self.file, buf, offset)?;
        #[cfg(windows)]
        let n = std::os::windows::fs::FileExt::seek_read(&self.file, buf, offset)?;
        #[cfg(not(any(unix, windows)))]
        let n: usize = return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("no positional read ({offset}, {})", buf.len()),
        ));
        self.reads.set(self.reads.get() + 1);
        self.bytes.set(self.bytes.get() + n as u64);
        Ok(n)
    }
}

fn probe(path: &str) -> io::Result<bool> {
    let file = File::open(path)?;
    let size = file.metadata()?.len();
    let disk = Disk {
        file,
        reads: Cell::new(0),
        bytes: Cell::new(0),
    };
    let mut head = [0u8; SNIFF_LEN];
    let mut got = 0;
    while got < head.len() {
        match disk.read_at(got as u64, &mut head[got..])? {
            0 => break,
            n => got += n,
        }
    }
    let Some(kind) = sniff(&head[..got]) else {
        println!("{size:>14}  {:<5} {:<30} {:<5} {path}", "-", "", "-");
        return Ok(true);
    };
    disk.reads.set(0);
    disk.bytes.set(0);
    let (what, verdict) = match measure(kind, &disk, size) {
        None => (String::new(), "none"),
        Some(m) => {
            let confidence = if m.confidence == Confidence::Exact {
                "exact"
            } else {
                "probable"
            };
            let verdict = match m.len.cmp(&size) {
                std::cmp::Ordering::Equal => "ok",
                std::cmp::Ordering::Less => "short",
                std::cmp::Ordering::Greater => "long",
            };
            let end = if m.end_verified { "end ok" } else { "end ?" };
            (format!("{} {confidence} {end} .{}", m.len, m.ext), verdict)
        }
    };
    println!(
        "{size:>14}  {:<5} {what:<30} {verdict:<5} {path}  [{} reads, {} bytes]",
        kind.name(),
        disk.reads.get(),
        disk.bytes.get()
    );
    Ok(verdict == "ok")
}

fn main() -> ExitCode {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        eprintln!("usage: probe FILE...");
        return ExitCode::from(2);
    }
    let mut disagreements = 0u32;
    for path in &paths {
        match probe(path) {
            Ok(true) => {}
            Ok(false) => disagreements += 1,
            Err(e) => println!("{path}: {e}"),
        }
    }
    if disagreements > 0 {
        eprintln!("{disagreements} file(s) recognised but not measured to their full size");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
