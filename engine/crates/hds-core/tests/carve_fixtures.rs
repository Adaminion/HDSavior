//! Carving real NTFS volumes (`scripts/linux/make-fixtures.sh`): files
//! whose MFT record the accident destroyed but whose clusters survived are
//! found by their signature and come back bit-identical.
//!
//! `HDS_FIXTURES` names the directory of images and manifests. Unset: the
//! test says SKIPPED and passes; set but an image missing: it fails.
//!
//! The catalog is not part of this test: the protected files of I2 (a) are
//! taken from the manifest (the surviving in-use files of the old volume),
//! as the catalog would derive them from the records.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

#[path = "carve_common.rs"]
mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use common::*;
use hds_core::carve::{CarveGeometry, CarveStatus, KindSet};
use hds_io::{BlockSource, ImageFile, RangeSet};
use hds_recover::FileStatus;
use serde_json::Value;

const SCENARIOS: [&str; 4] = ["wiped", "basic", "big-clusters", "small-clusters"];

struct Entry {
    path: String,
    offset: u64,
    size: u64,
    sha256: String,
    content_type: String,
}

#[test]
fn files_whose_record_is_gone_are_carved_from_real_volumes() {
    let Some(dir) = std::env::var_os("HDS_FIXTURES").map(PathBuf::from) else {
        eprintln!("SKIPPED: HDS_FIXTURES is not set");
        return;
    };
    for name in SCENARIOS {
        let image = dir.join(format!("{name}.img"));
        let manifest: Value = serde_json::from_slice(
            &std::fs::read(dir.join(format!("{name}.manifest.json"))).unwrap(),
        )
        .unwrap();
        let old = &manifest["old_volume"];
        let cluster = old["cluster_size"].as_u64().unwrap();
        let volume = old["offset"].as_u64().unwrap();
        let g = CarveGeometry::new(cluster, volume).unwrap();
        let scan_range = &manifest["scan_range"];
        let start = scan_range["offset"].as_u64().unwrap();
        let range = start..start + scan_range["length"].as_u64().unwrap();

        // the old volume's surviving in-use files are protected; files whose
        // record is gone but whose single run survived are the targets
        let mut protected = RangeSet::new();
        let mut targets = Vec::new();
        for e in manifest["entries"].as_array().unwrap() {
            if e["type"] != "file" || e["file_system"] != "main" {
                continue;
            }
            let runs: Vec<(Option<u64>, u64)> = e["ondisk"]["runs"]
                .as_array()
                .map(|r| {
                    r.iter()
                        .map(|run| (run[0].as_u64(), run[1].as_u64().unwrap()))
                        .collect()
                })
                .unwrap_or_default();
            let intact = e["after"]["record_intact"] == true;
            if intact && e["deleted"] == false {
                for &(lcn, len) in &runs {
                    if let Some(lcn) = lcn {
                        protected.insert(volume + lcn * cluster..volume + (lcn + len) * cluster);
                    }
                }
            }
            if !intact
                && e["after"]["content_intact"] == true
                && runs.len() == 1
                && runs[0].0.is_some()
                && e["size"].as_u64().unwrap_or(0) > 0
            {
                targets.push(Entry {
                    path: e["path"].as_str().unwrap().to_string(),
                    offset: volume + runs[0].0.unwrap() * cluster,
                    size: e["size"].as_u64().unwrap(),
                    sha256: e["sha256"].as_str().unwrap().to_string(),
                    content_type: e["content_type"].as_str().unwrap_or("").to_string(),
                });
            }
        }

        let source: Arc<dyn BlockSource> =
            Arc::new(ImageFile::open(&image, None).unwrap_or_else(|e| panic!("{name}: {e}")));
        let session = scan_source(&source, range.clone());
        let (run, summary) = carve(
            &source,
            &session,
            input(
                &session,
                KindSet::all(),
                g,
                protected.clone(),
                RangeSet::new(),
                RangeSet::new(),
            ),
        );
        assert_eq!(summary.status, CarveStatus::Completed, "{name}");
        for f in &run.files {
            assert!(
                !protected.intersects(f.range()),
                "{name}: {f:?} inside a protected file"
            );
        }
        let carved: BTreeMap<u64, _> = run.files.iter().map(|f| (f.offset(), *f)).collect();

        // every target whose start the scanner sniffed is carved; with a
        // verified end, exactly
        let mut sniffed = 0;
        let mut exact = Vec::new();
        for t in &targets {
            let mut head = vec![0u8; 512];
            source.read_at(t.offset, &mut head).unwrap();
            if hds_carve::sniff(&head).is_none() {
                continue;
            }
            sniffed += 1;
            let f = carved
                .get(&t.offset)
                .unwrap_or_else(|| panic!("{name}: {} ({}) not carved", t.path, t.content_type));
            if f.end_verified() {
                assert_eq!(f.len(), t.size, "{name}: {}", t.path);
            }
            if f.len() == t.size {
                exact.push((t, *f));
            } else {
                println!(
                    "{name}: {} carved as {} bytes of {} ({:?})",
                    t.path,
                    f.len(),
                    t.size,
                    f.confidence()
                );
            }
        }
        assert!(
            sniffed >= 20,
            "{name}: only {sniffed} targets with a signature"
        );

        // recovered through hds-recover, bit-identical
        let files: Vec<_> = exact.iter().map(|(_, f)| *f).collect();
        let (rsum, recovered) = recover_files(&source, range.clone(), &files, &|_| {
            hds_recover::FlagSet::EMPTY
        });
        assert_eq!(rsum.files_total, files.len() as u64);
        let by_offset: BTreeMap<u64, &Entry> = exact.iter().map(|(t, _)| (t.offset, *t)).collect();
        let mut good = 0;
        for r in &recovered {
            let t = by_offset[&r.file.offset()];
            let content = r.content.as_ref().unwrap();
            assert_eq!(sha256_hex(content), t.sha256, "{name}: {}", t.path);
            if matches!(r.outcome.status, FileStatus::Ok | FileStatus::OkUnverified) {
                good += 1;
            }
        }
        println!(
            "{name}: {} candidates, {} carved, {sniffed} targets with a signature, {} bit-identical ({good} good)",
            summary.candidates_total,
            run.files.len(),
            recovered.len()
        );
    }
}
