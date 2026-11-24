//! Shared helpers of the integration tests. Not every test file uses every
//! helper.
#![allow(dead_code)]

pub mod build;
pub mod exercise;
pub mod json;
pub mod rng;
pub mod xcheck;

use std::path::PathBuf;

/// Directory named by `HDS_FIXTURES`, or `None` after printing why the
/// calling test does nothing.
pub fn fixtures_dir(test: &str) -> Option<PathBuf> {
    match std::env::var_os("HDS_FIXTURES") {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => {
            eprintln!(
                "SKIPPED {test}: set HDS_FIXTURES to the directory with the fixture images \
                 (built by scripts/linux/make-fixtures.sh) to run it"
            );
            None
        }
    }
}

/// The disk images in the fixtures directory, sorted by name.
pub fn fixture_images(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut images: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()))
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("img" | "raw" | "dd" | "vhd")
            )
        })
        .collect();
    images.sort();
    assert!(
        !images.is_empty(),
        "HDS_FIXTURES={} holds no *.img, *.raw, *.dd or *.vhd file",
        dir.display()
    );
    images
}

/// Directory with the committed sample structures.
pub fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}
