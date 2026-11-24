//! Every scenario, decoded by the independent inspector
//! (`scripts/linux/inspect-ntfs.py`), must be exactly what its manifest
//! says: the same boot sectors, `FILE` records and `INDX` blocks with the
//! same fields, files that rebuild to the manifest's SHA-256 through the
//! decoder's runlists, bitmaps that mark what the manifest lists as live.
//! `tests/crosscheck.py` explains the comparison.
//!
//! Needs `python3` and the inspector script; without them the tests print
//! a notice and pass (set `HDS_TESTKIT_REQUIRE_TOOLS=1` to fail instead).

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use common::{crosscheck, inspector, python, skip, write_built, TempDir};
use hds_testkit::scenarios::{self, scale};

#[test]
fn every_scenario_decodes_to_its_manifest() {
    let (Some(python), Some(inspector)) = (python(), inspector()) else {
        return skip(
            "the inspector cross-check",
            "python3 or scripts/linux/inspect-ntfs.py is missing",
        );
    };
    let catalog = scenarios::catalog();
    let next = AtomicUsize::new(0);
    let failures: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let summaries: Mutex<Vec<String>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for worker in 0..4 {
            let (catalog, next, failures, summaries, inspector) =
                (&catalog, &next, &failures, &summaries, &inspector);
            scope.spawn(move || {
                let dir = TempDir::new(&format!("crosscheck-{worker}"));
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(scenario) = catalog.get(i) else {
                        break;
                    };
                    let built = (scenario.build)(0x5EED);
                    assert_eq!(
                        built.manifest.scenario, scenario.name,
                        "catalog name and manifest name differ"
                    );
                    let (image, manifest) = write_built(&dir, scenario.name, &built);
                    let (ok, output) = crosscheck(python, inspector, &image, &manifest);
                    if ok {
                        summaries.lock().unwrap().push(output.trim().to_string());
                    } else {
                        failures
                            .lock()
                            .unwrap()
                            .push(format!("{}:\n{output}", scenario.name));
                    }
                    let _ = std::fs::remove_file(image);
                    let _ = std::fs::remove_file(manifest);
                }
            });
        }
    });
    let mut summaries = summaries.into_inner().unwrap();
    summaries.sort();
    for line in &summaries {
        println!("{line}");
    }
    let failures = failures.into_inner().unwrap();
    assert!(
        failures.is_empty(),
        "{} scenario(s) disagree with their manifest:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert_eq!(summaries.len(), catalog.len());
}

#[test]
fn the_scale_generator_decodes_to_its_manifest() {
    let (Some(python), Some(inspector)) = (python(), inspector()) else {
        return skip(
            "the inspector cross-check of the scale generator",
            "python3 or the inspector is missing",
        );
    };
    // the code path of the million-record volume, at 8000 records
    let built = scale::scale_in_memory(&scale::ScaleSpec::small(11));
    assert!(built.manifest.records.len() > 7000);
    let dir = TempDir::new("crosscheck-scale");
    let (image, manifest) = write_built(&dir, "scale", &built);
    let (ok, output) = crosscheck(python, &inspector, &image, &manifest);
    assert!(ok, "{output}");
    println!("{}", output.trim());
}

/// The comparison must be able to fail: a wrong manifest and a wrong image
/// are both noticed, in every category it checks.
#[test]
fn the_cross_check_is_not_vacuous() {
    let (Some(python), Some(inspector)) = (python(), inspector()) else {
        return skip(
            "the negative controls of the cross-check",
            "python3 or the inspector is missing",
        );
    };
    let built = scenarios::accidents::post_format_activity(7);
    let dir = TempDir::new("crosscheck-negative");
    let (image, manifest) = write_built(&dir, "good", &built);
    let (ok, output) = crosscheck(python, &inspector, &image, &manifest);
    assert!(ok, "the unmodified scenario must pass:\n{output}");

    let check = |name: &str, built: &hds_testkit::disk::Built, expect: &str| {
        let (image, manifest) = write_built(&dir, name, built);
        let (ok, output) = crosscheck(python, &inspector, &image, &manifest);
        assert!(!ok, "{name}: the cross-check did not notice");
        assert!(
            output.contains(expect),
            "{name}: expected a complaint about {expect:?}, got:\n{output}"
        );
    };
    let m = &built.manifest;
    let file = m
        .entries
        .iter()
        .find(|e| e.must_recover_exactly() && !e.resident && e.size > 10_000)
        .expect("an intact non-resident file");
    let record_at = file.record_offset.unwrap();
    let position = m
        .records
        .iter()
        .position(|r| r.offset == record_at)
        .unwrap();

    // manifest lies: sequence number, a run, a name, a missing record, an index entry, a boot field
    let mut wrong = built.clone();
    wrong.manifest.records[position].sequence += 1;
    check("wrong-sequence", &wrong, "sequence is");
    let mut wrong = built.clone();
    wrong.manifest.records[position].data[0]
        .non_resident
        .as_mut()
        .unwrap()
        .runs[0]
        .lcn = Some(1);
    check("wrong-run", &wrong, "runs is");
    let mut wrong = built.clone();
    wrong.manifest.records[position].names[0].name_utf16[0] ^= 1;
    check("wrong-name", &wrong, "name is");
    let mut wrong = built.clone();
    wrong.manifest.records.remove(position);
    check(
        "missing-record",
        &wrong,
        "is in the image but not in the manifest",
    );
    let mut wrong = built.clone();
    let block = wrong
        .manifest
        .index_blocks
        .iter_mut()
        .find(|b| !b.entries.is_empty())
        .unwrap();
    block.entries[0].child.seq += 1;
    check("wrong-index-entry", &wrong, "child is");
    let mut wrong = built.clone();
    wrong.manifest.boot_sectors[0].mft_lcn += 1;
    check("wrong-boot", &wrong, "mft lcn is");
    let mut wrong = built.clone();
    let live = wrong
        .manifest
        .instances
        .iter_mut()
        .find(|i| !i.live_allocated.is_empty())
        .unwrap();
    live.live_allocated.pop();
    check("wrong-bitmap", &wrong, "$Bitmap marks");
    let mut wrong = built.clone();
    let entry = wrong
        .manifest
        .entries
        .iter_mut()
        .find(|e| e.id == file.id)
        .unwrap();
    entry.truth.content_intact = Some(false);
    check("wrong-truth", &wrong, "reads back intact");

    // image lies: one byte of a file's content, one byte of a record, a record gone
    let mut wrong = built.clone();
    let at = file.extents[0].offset.unwrap() + 4000;
    let byte = wrong.image.read_vec(at, 1)[0];
    wrong.image.write_at(at, &[byte ^ 0x40]);
    check(
        "flipped-content-byte",
        &wrong,
        "content rebuilt from the decoded records",
    );
    let mut wrong = built.clone();
    let byte = wrong.image.read_vec(record_at + 0x10, 1)[0];
    wrong.image.write_at(record_at + 0x10, &[byte ^ 1]);
    check("flipped-record-byte", &wrong, "sequence is");
    let mut wrong = built.clone();
    wrong.image.write_at(record_at, &[0; 1024]);
    check(
        "erased-record",
        &wrong,
        "is in the manifest but not in the image",
    );
}
