//! Cross-check against real volumes.
//!
//! Runs when the environment variable `HDS_FIXTURES` names a directory with
//! disk images (the fixtures built by `scripts/linux/make-fixtures.sh`; any
//! `*.img`, `*.raw`, `*.dd` or `*.vhd` file is taken). For every image the
//! test walks every 512-byte offset, parses what it finds with this crate,
//! and compares the result field by field with what the independent
//! `scripts/linux/inspect-ntfs.py --json` says about the same bytes:
//!
//! * the same set of boot sectors, `FILE` records and `INDX` blocks;
//! * the same verdict on each (valid or not);
//! * per boot sector every decoded field;
//! * per record the header, every attribute header, every name with parent
//!   reference, namespace, times and sizes, every non-resident extent with
//!   its sizes and decoded runs, resident value locations, attribute lists,
//!   index roots with their entries, volume names and bitmaps;
//! * per index block the header and every entry.
//!
//! `HDS_INSPECT` overrides the path of the script, `HDS_PYTHON` the
//! interpreter (default `python3`). The comparison itself is in
//! `tests/common/xcheck.rs`; `tests/corpus.rs` runs it without Python on a
//! few committed structures.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::json::Json;
use common::xcheck::{cross_check, Counts};

fn inspector_script() -> PathBuf {
    match std::env::var_os("HDS_INSPECT") {
        Some(path) => PathBuf::from(path),
        None => {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../scripts/linux/inspect-ntfs.py")
        }
    }
}

fn run_inspector(image: &Path) -> Json {
    let python = std::env::var_os("HDS_PYTHON").unwrap_or_else(|| "python3".into());
    let script = inspector_script();
    let output = Command::new(&python)
        .arg(&script)
        .arg("--json")
        .arg(image)
        .output()
        .unwrap_or_else(|e| panic!("cannot run {python:?} {}: {e}", script.display()));
    assert!(
        output.status.success(),
        "{} failed on {}: {}",
        script.display(),
        image.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    Json::parse(&output.stdout)
        .unwrap_or_else(|e| panic!("inspector output for {} is not JSON: {e}", image.display()))
}

fn cross_check_image(path: &Path) -> (Counts, Vec<String>) {
    let image =
        std::fs::read(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let doc = run_inspector(path);
    let name = path.file_name().unwrap().to_string_lossy();
    cross_check(&name, &image, &doc)
}

#[test]
fn every_structure_of_every_fixture_matches_the_inspector() {
    let Some(dir) = common::fixtures_dir("every_structure_of_every_fixture_matches_the_inspector")
    else {
        return;
    };
    let images = common::fixture_images(&dir);
    assert!(
        inspector_script().is_file(),
        "{} not found (set HDS_INSPECT)",
        inspector_script().display()
    );

    // One image per thread, at most four at a time.
    let mut total = Counts::default();
    let mut failures = Vec::new();
    for batch in images.chunks(4) {
        let results: Vec<(Counts, Vec<String>)> = std::thread::scope(|scope| {
            let handles: Vec<_> = batch
                .iter()
                .map(|image| scope.spawn(move || cross_check_image(image)))
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("cross-check thread panicked"))
                .collect()
        });
        for (image, (counts, mut image_failures)) in batch.iter().zip(results) {
            eprintln!("{}: {counts:?}", image.display());
            total.add(&counts);
            failures.append(&mut image_failures);
        }
    }
    eprintln!("TOTAL: {total:?}");
    assert!(
        failures.is_empty(),
        "{} disagreement(s) with the inspector:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(total.records > 0 && total.index_blocks > 0 && total.boot_sectors > 0);
}
