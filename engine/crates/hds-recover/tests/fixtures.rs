//! Real NTFS volumes, really quick-formatted (scripts/linux/make-fixtures.sh):
//! plans built straight from each fixture's ground-truth manifest are
//! recovered and every output file is compared with the manifest's SHA-256.
//! Then the same plans with a wrong geometry: rated as the catalog rates an
//! unvalidated geometry, no file may be `ok`; rated as if the wrong geometry
//! had been validated, every file whose wrong content the signature check can
//! see must be caught, and the only `ok` with wrong content allowed is one
//! whose wrong location starts like another file of the same type (the limit
//! of a first-bytes check, which is why F6 must never validate such a
//! geometry).
//!
//! `HDS_FIXTURES` names the directory with `<scenario>.img` and
//! `<scenario>.manifest.json`; without it the tests are skipped. When it is
//! set, every scenario listed below must be there.

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use hds_io::out::{filetime_to_system_time, Timestamps};
use hds_io::testing::FaultySource;
use hds_io::{open_source, BlockSource, Protected, RangedSource, SourceKind};
use hds_recover::{
    recover, FileOutcome, FileSource, FileStatus, Flag, FlagSet, Plan, PlanFile, PlanFolder,
    RatingContext, RecoverObserver, RecoverOptions, RecoverProgress,
};
use serde_json::Value;

use common::{sha256_hex, TempDir};

const SCENARIOS: [&str; 11] = [
    "basic",
    "fragmented",
    "wiped",
    "cluster-change",
    "offset-mbr",
    "offset-mbr63",
    "offset-gpt",
    "post-write",
    "small-clusters",
    "big-clusters",
    "4k-records",
];

/// True when the tests run under Wine (`HDS_IO_WINE` is set).
fn under_wine() -> bool {
    std::env::var_os("HDS_IO_WINE").is_some()
}

fn fixtures_dir() -> Option<PathBuf> {
    match std::env::var_os("HDS_FIXTURES") {
        Some(dir) => Some(PathBuf::from(dir)),
        None => {
            eprintln!(
                "SKIPPED: set HDS_FIXTURES to a directory built by scripts/linux/make-fixtures.sh"
            );
            None
        }
    }
}

struct Fixture {
    name: String,
    manifest: Value,
    image: PathBuf,
    sector_size: u32,
    volume_offset: u64,
    volume_len: u64,
    cluster_size: u64,
    record_size: usize,
}

fn load(dir: &Path, name: &str) -> Fixture {
    let image = dir.join(format!("{name}.img"));
    let manifest_path = dir.join(format!("{name}.manifest.json"));
    assert!(
        image.is_file(),
        "HDS_FIXTURES is set but {} is missing",
        image.display()
    );
    let text = std::fs::read_to_string(&manifest_path)
        .unwrap_or_else(|e| panic!("HDS_FIXTURES is set but {}: {e}", manifest_path.display()));
    let manifest: Value = serde_json::from_str(&text).unwrap();
    let old = &manifest["old_volume"];
    Fixture {
        name: name.to_string(),
        sector_size: manifest["image"]["sector_size"].as_u64().unwrap() as u32,
        volume_offset: old["offset"].as_u64().unwrap(),
        volume_len: old["length"].as_u64().unwrap(),
        cluster_size: old["cluster_size"].as_u64().unwrap(),
        record_size: old["mft_record_size"].as_u64().unwrap() as usize,
        manifest,
        image,
    }
}

/// What the manifest says a recovered file must be.
struct Expect {
    path: String,
    sha256: String,
    deleted: bool,
    compressed: bool,
    times: Timestamps,
}

fn times_of(entry: &Value) -> Timestamps {
    Timestamps {
        created: entry["created"].as_u64().unwrap_or(0),
        modified: entry["modified"].as_u64().unwrap_or(0),
        accessed: entry["accessed"].as_u64().unwrap_or(0),
    }
}

/// Where the resident value of the record at `offset` is, read the way the
/// scanner reads it: the hash of the bytes as read, then hds-ntfs.
fn resident_source(image: &[u8], offset: u64, record_size: usize) -> FileSource {
    let record = &image[offset as usize..offset as usize + record_size];
    let hash = hds_ntfs::fnv1a64(record);
    let mut copy = record.to_vec();
    let parsed = hds_ntfs::parse_file_record(&mut copy).unwrap();
    let value = parsed
        .attributes()
        .filter(|a| a.type_code == hds_ntfs::attr_type::DATA && a.is_unnamed())
        .find_map(|a| a.resident())
        .unwrap_or_else(|| panic!("no resident $DATA in the record at {offset}"));
    FileSource::Resident {
        record_offset: offset,
        record_size: record_size as u32,
        value_offset: value.offset,
        value_len: value.len(),
        hash,
    }
}

/// A plan of every file whose record and content survived, with the old
/// volume read at (`cluster_size`, `volume_offset`).
fn build_plan(
    fx: &Fixture,
    image: &[u8],
    cluster_size: u64,
    volume_offset: u64,
    context: RatingContext,
) -> (Plan, HashMap<u64, Expect>) {
    let entries = fx.manifest["entries"].as_array().unwrap();
    let dirs: HashMap<&str, &Value> = entries
        .iter()
        .filter(|e| e["type"] == "dir")
        .map(|e| (e["path"].as_str().unwrap(), e))
        .collect();
    let mut plan = Plan::default();
    let mut folder_ids: HashMap<String, u64> = HashMap::new();
    let mut expect = HashMap::new();
    for (index, entry) in entries.iter().enumerate() {
        if entry["type"] != "file"
            || entry["after"]["record_intact"] != true
            || entry["after"]["content_intact"] != true
        {
            continue;
        }
        let path = entry["path"].as_str().unwrap();
        let mut components: Vec<&str> = path.split('/').collect();
        let name = components.pop().unwrap();
        // The folders on the way, created once each.
        let mut parent: Option<u64> = None;
        let mut so_far = String::new();
        for component in components {
            if !so_far.is_empty() {
                so_far.push('/');
            }
            so_far.push_str(component);
            let next_id = 1_000_000 + folder_ids.len() as u64;
            let id = *folder_ids.entry(so_far.clone()).or_insert_with(|| {
                let spec = dirs.get(so_far.as_str());
                plan.folders.push(PlanFolder {
                    id: next_id,
                    parent,
                    name: component.encode_utf16().collect(),
                    times: spec.map(|d| times_of(d)).unwrap_or_default(),
                    in_use: spec.is_none_or(|d| d["deleted"] != true),
                    record_number: spec.and_then(|d| d["mft_record"].as_u64()),
                });
                next_id
            });
            parent = Some(id);
        }
        let ondisk = &entry["ondisk"];
        let size = entry["size"].as_u64().unwrap();
        let data_flags: Vec<&str> = ondisk["data_flags"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let source = if ondisk["resident"] == true {
            resident_source(
                image,
                ondisk["record_offset"].as_u64().unwrap(),
                fx.record_size,
            )
        } else {
            let runs = ondisk["runs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|run| (run[0].as_u64(), run[1].as_u64().unwrap()));
            FileSource::from_clusters(
                runs,
                cluster_size,
                volume_offset,
                size,
                ondisk["initialized_size"].as_u64().unwrap(),
            )
        };
        let deleted = entry["deleted"] == true;
        let compressed = data_flags.contains(&"compressed");
        let mut flags = FlagSet::new();
        if deleted {
            flags.insert(Flag::Deleted);
        }
        if compressed {
            flags.insert(Flag::Compressed);
        }
        if data_flags.contains(&"sparse") {
            flags.insert(Flag::Sparse);
        }
        let node = index as u64;
        let ext = name
            .rsplit_once('.')
            .filter(|(stem, _)| !stem.is_empty())
            .map(|(_, e)| e.to_ascii_lowercase())
            .unwrap_or_default();
        plan.files.push(PlanFile {
            node,
            folder: parent,
            name: name.encode_utf16().collect(),
            ext,
            first_data_offset: source.first_offset(),
            size,
            source,
            times: times_of(entry),
            flags,
            context,
            in_use: !deleted,
            record_number: entry["mft_record"].as_u64(),
            record_offset: ondisk["record_offset"].as_u64(),
            original_path: path.to_string(),
        });
        expect.insert(
            node,
            Expect {
                path: path.to_string(),
                sha256: entry["sha256"].as_str().unwrap().to_string(),
                deleted,
                compressed,
                times: times_of(entry),
            },
        );
    }
    (plan, expect)
}

#[derive(Default)]
struct Outcomes(Vec<FileOutcome>);

impl RecoverObserver for Outcomes {
    fn progress(&mut self, _: &RecoverProgress) {}

    fn file_done(&mut self, outcome: &FileOutcome) {
        self.0.push(outcome.clone());
    }
}

/// The session range is the old volume's partition; everything read must
/// stay inside it.
fn session(fx: &Fixture) -> (Arc<FaultySource>, RangedSource) {
    let image = open_source(SourceKind::Image, &fx.image, Some(fx.sector_size)).unwrap();
    let watched = Arc::new(FaultySource::new(image));
    let range = fx.volume_offset..fx.volume_offset + fx.volume_len;
    let source = RangedSource::new(watched.clone() as Arc<dyn BlockSource>, range).unwrap();
    (watched, source)
}

fn run(fx: &Fixture, plan: Plan, dest: &Path) -> (hds_recover::RecoverSummary, Vec<FileOutcome>) {
    let (watched, source) = session(fx);
    let range = source.range();
    let options = RecoverOptions::new(vec![Protected::session(range.clone())]);
    let mut outcomes = Outcomes::default();
    let summary = recover(
        source,
        plan,
        dest,
        options,
        &mut outcomes,
        &AtomicBool::new(false),
    )
    .unwrap();
    for touched in watched.touched().iter() {
        assert!(
            touched.start >= range.start && touched.end <= range.end,
            "{}: read {touched:?} outside the session range {range:?}",
            fx.name
        );
    }
    (summary, outcomes.0)
}

#[test]
fn every_surviving_file_comes_back_bit_identical() {
    let Some(dir) = fixtures_dir() else { return };
    for name in SCENARIOS {
        let fx = load(&dir, name);
        let image = std::fs::read(&fx.image).unwrap();
        let (plan, expect) = build_plan(
            &fx,
            &image,
            fx.cluster_size,
            fx.volume_offset,
            RatingContext::default(),
        );
        drop(image);
        let tmp = TempDir::new(name);
        let dest = tmp.join("rescued");
        let total = plan.files.len();
        let (summary, outcomes) = run(&fx, plan, &dest);
        assert_eq!(
            summary.status,
            hds_recover::JobStatus::Completed,
            "{name}: {:?}",
            summary.message
        );
        assert_eq!(outcomes.len(), total);
        let (mut ok, mut verified, mut deleted, mut compressed, mut host_limit) = (0, 0, 0, 0, 0);
        for outcome in &outcomes {
            let want = &expect[&outcome.node];
            // Under Wine a name is stored on the host, which limits it to 255
            // bytes of UTF-8 (NTFS: 255 UTF-16 units) and reports a longer one
            // as "not found" rather than as a name it rejects.
            if under_wine()
                && want.path.split('/').any(|c| c.len() > 255)
                && outcome.status.to_string() == "failed:write_error"
            {
                host_limit += 1;
                continue;
            }
            if want.compressed {
                assert_eq!(
                    outcome.status.to_string(),
                    "skipped:compressed",
                    "{name}: {}",
                    want.path
                );
                compressed += 1;
                continue;
            }
            assert!(
                outcome.status.is_written(),
                "{name}: {} is {}: {}",
                want.path,
                outcome.status,
                outcome.detail
            );
            let path = dest.join(&outcome.output_path);
            // Times first: reading the file could move its access time.
            let meta = std::fs::metadata(&path).unwrap();
            if let Some(modified) = filetime_to_system_time(want.times.modified) {
                assert_eq!(meta.modified().unwrap(), modified, "{name}: {}", want.path);
            }
            let content = std::fs::read(&path).unwrap();
            assert_eq!(
                sha256_hex(&content),
                want.sha256,
                "{name}: {} differs",
                want.path
            );
            assert_eq!(outcome.bytes_written, content.len() as u64);
            // The status is exactly what the content and the flags call for.
            let ext = want
                .path
                .rsplit_once('.')
                .map(|(_, e)| e.to_ascii_lowercase())
                .unwrap_or_default();
            let opinion = if content.is_empty() {
                None
            } else {
                hds_carve::matches_extension(&content[..content.len().min(1024)], &ext)
            };
            let expected = match (want.deleted, opinion) {
                (false, Some(true)) => "ok".to_string(),
                (false, None) => "ok_unverified".to_string(),
                (false, Some(false)) => "warning:signature_mismatch".to_string(),
                (true, Some(false)) => "warning:deleted;signature_mismatch".to_string(),
                (true, _) => "warning:deleted".to_string(),
            };
            assert_eq!(
                outcome.status.to_string(),
                expected,
                "{name}: {}",
                want.path
            );
            ok += usize::from(outcome.status.is_ok());
            verified += usize::from(outcome.status == FileStatus::Ok);
            deleted += usize::from(want.deleted);
        }
        assert_eq!(
            summary.ok + summary.warnings + summary.failed + summary.skipped,
            summary.files_total
        );
        eprintln!(
            "{name}: {total} files recovered bit-identical ({ok} ok, {verified} of them verified by signature; {deleted} deleted; {compressed} compressed skipped; {host_limit} names beyond the Wine host's limit)"
        );
        assert!(ok > 0);
    }
}

#[test]
fn a_wrong_geometry_never_reports_wrong_content_as_ok() {
    let Some(dir) = fixtures_dir() else { return };
    for name in SCENARIOS {
        let fx = load(&dir, name);
        let image = std::fs::read(&fx.image).unwrap();
        let mut geometries = vec![
            (
                "shifted by one cluster",
                fx.cluster_size,
                fx.volume_offset + fx.cluster_size,
            ),
            (
                "shifted by one sector",
                fx.cluster_size,
                fx.volume_offset + 512,
            ),
        ];
        if fx.cluster_size * 2 <= 2 << 20 {
            geometries.push(("double cluster size", fx.cluster_size * 2, fx.volume_offset));
        }
        for (what, cluster_size, volume_offset) in geometries {
            // As if the wrong geometry had been validated: only the signature check is left.
            let (plan, expect) = build_plan(
                &fx,
                &image,
                cluster_size,
                volume_offset,
                RatingContext::default(),
            );
            let tmp = TempDir::new(name);
            let dest = tmp.join("wrong");
            let (summary, outcomes) = run(&fx, plan, &dest);
            assert_eq!(summary.status, hds_recover::JobStatus::Completed);
            let (mut wrong_unverified, mut caught, mut verified, mut same_type) =
                (0, 0, 0, Vec::new());
            for outcome in &outcomes {
                let want = &expect[&outcome.node];
                if !outcome.status.is_written() {
                    continue;
                }
                let content = std::fs::read(dest.join(&outcome.output_path)).unwrap();
                let right = sha256_hex(&content) == want.sha256;
                let ext = want
                    .path
                    .rsplit_once('.')
                    .map(|(_, e)| e.to_ascii_lowercase())
                    .unwrap_or_default();
                let opinion =
                    hds_carve::matches_extension(&content[..content.len().min(1024)], &ext);
                match outcome.status {
                    FileStatus::Ok if right => verified += 1,
                    FileStatus::Ok => {
                        // The one thing a first-bytes check (H9) cannot see: the wrong
                        // place holds the start of another file of the same type.
                        assert_eq!(
                            opinion,
                            Some(true),
                            "{name} ({what}): {} reported ok with wrong content",
                            want.path
                        );
                        same_type.push(want.path.clone());
                    }
                    FileStatus::OkUnverified if !right => {
                        assert_eq!(opinion, None, "{name} ({what}): {}", want.path);
                        wrong_unverified += 1;
                    }
                    FileStatus::Warning { .. } if !right => caught += 1,
                    _ => {}
                }
                if opinion == Some(false) {
                    assert!(
                        !outcome.status.is_ok(),
                        "{name} ({what}): {} has a mismatching signature",
                        want.path
                    );
                }
            }
            eprintln!(
                "{name} ({what}): {caught} wrong files caught as warnings, {wrong_unverified} wrong files without a signature to check (ok_unverified), {verified} ok files correct, {} wrong files that start like another file of their type: {same_type:?}",
                same_type.len()
            );

            // And as the catalog would rate it: an unvalidated geometry is never ok.
            let unvalidated = RatingContext {
                geometry_not_validated: true,
                carved_end_unverified: false,
            };
            let (plan, _) = build_plan(&fx, &image, cluster_size, volume_offset, unvalidated);
            let tmp = TempDir::new(name);
            let (summary, outcomes) = run(&fx, plan, &tmp.join("unvalidated"));
            assert_eq!(summary.ok, 0, "{name} ({what})");
            for outcome in &outcomes {
                assert!(!outcome.status.is_ok(), "{name} ({what}): {outcome:?}");
            }
        }
    }
}
