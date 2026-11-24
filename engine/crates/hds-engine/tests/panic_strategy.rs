//! The engine catches panics per request handler and per job, so every
//! Cargo profile of the workspace must keep `panic = "unwind"` (the
//! default). `src/lib.rs` refuses to compile with `panic = "abort"`; this
//! test also reads the manifests, so that a profile that would only take
//! effect in release builds is caught by a debug test run.

use std::path::{Path, PathBuf};

/// The workspace manifest and the manifest of every crate.
fn manifests() -> Vec<PathBuf> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut out = vec![workspace.join("Cargo.toml")];
    for entry in std::fs::read_dir(workspace.join("crates")).unwrap() {
        let manifest = entry.unwrap().path().join("Cargo.toml");
        if manifest.is_file() {
            out.push(manifest);
        }
    }
    out
}

#[test]
fn no_cargo_profile_sets_panic_abort() {
    let manifests = manifests();
    assert!(manifests.len() > 2, "{manifests:?}");
    for manifest in manifests {
        let text = std::fs::read_to_string(&manifest).unwrap();
        let mut section = String::new();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.starts_with('[') {
                section = line.to_string();
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if key.trim() == "panic" {
                assert_eq!(
                    value.trim().trim_matches('"'),
                    "unwind",
                    "{}: {section} sets panic = {}; the engine needs unwinding",
                    manifest.display(),
                    value.trim()
                );
            }
        }
    }
}

#[test]
fn this_build_unwinds() {
    let caught = std::panic::catch_unwind(|| panic!("caught"));
    assert!(caught.is_err());
}
