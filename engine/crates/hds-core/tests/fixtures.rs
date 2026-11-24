//! The scanner against real NTFS volumes.
//!
//! * `HDS_FIXTURES` names a directory of images built by
//!   `scripts/linux/make-fixtures.sh` (with their JSON manifests). Every
//!   image is scanned and compared with `scripts/linux/inspect-ntfs.py
//!   --json` (an independent reader; a cached `<name>.inspect.json` next to
//!   the image is used when present) and with the manifest. Unset: the
//!   tests say SKIPPED and pass; set but an image missing: they fail.
//! * `HDS_WIN_SAMPLES` names a directory of Windows-written sample volumes
//!   (`*.raw`, `*.dd`, `*.vhd`, `*.img`); each is compared with the inspector
//!   the same way.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use hds_core::scan::{scan, NoObserver, ScanOptions, ScanStatus};
use hds_core::session::{Session, SessionGuard, SessionMeta, SourceIdentity};
use hds_core::store::{
    AttrListName, AttributeListState, DataExtent, EntrySourceKind, RecordId, ScanStore,
};
use hds_io::testing::FaultySource;
use hds_io::{BlockSource, ImageFile, Protected, RangedSource};
use hds_ntfs::{FileRef, FixupStatus, RunlistStatus};
use serde_json::Value;

/// Runs as `(cluster, length)`; `None` for a hole.
type Runs = Vec<(Option<u64>, u64)>;

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

fn env_dir(var: &str) -> Option<PathBuf> {
    match std::env::var_os(var) {
        Some(dir) => Some(PathBuf::from(dir)),
        None => {
            eprintln!("SKIPPED: {var} is not set");
            None
        }
    }
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

/// The inspector's view of an image: cached next to it, or computed.
fn inspect(image: &Path) -> Value {
    let cached = image.with_extension("inspect.json");
    if cached.is_file() {
        return read_json(&cached);
    }
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../scripts/linux/inspect-ntfs.py");
    let out = std::process::Command::new("python3")
        .arg("-I")
        .arg(&script)
        .arg("--json")
        .arg(image)
        .output()
        .unwrap_or_else(|e| panic!("cannot run {}: {e}", script.display()));
    assert!(
        out.status.success(),
        "inspector failed on {}",
        image.display()
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn scan_range(source: Arc<dyn BlockSource>, range: std::ops::Range<u64>) -> Session {
    let options = ScanOptions::default();
    let meta =
        SessionMeta::new(SourceIdentity::from(&source.info()), range.clone(), options).unwrap();
    let mut session = Session::new(meta);
    let ranged = RangedSource::new(source, range).unwrap();
    let summary = scan(
        &ranged,
        &options,
        &mut session,
        &mut NoObserver,
        &AtomicBool::new(false),
    );
    assert_eq!(summary.status, ScanStatus::Completed);
    session
}

fn u(v: &Value) -> u64 {
    v.as_u64().unwrap_or_else(|| panic!("not a number: {v}"))
}

/// The latest of the four times of an inspector `$FILE_NAME` object.
fn fn_latest(f: &Value) -> u64 {
    ["created", "modified", "mft_changed", "accessed"]
        .iter()
        .map(|k| u(&f[*k]))
        .max()
        .unwrap()
}

fn units(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

#[derive(Default, Debug)]
struct Tally {
    records: u64,
    dropped_never_used: u64,
    dropped_log_copies: u64,
    names: u64,
    extents: u64,
    runs: u64,
    root_entries: u64,
    blocks: u64,
    block_entries: u64,
    boots: u64,
}

/// Compares the store with the inspector, item by item.
fn compare_with_inspector(
    label: &str,
    store: &ScanStore,
    inspector: &Value,
    range_end: u64,
) -> Tally {
    let mut tally = Tally::default();
    // FILE records.
    let mut expected_records = 0_u64;
    for rec in inspector["file_records"].as_array().unwrap() {
        if rec["valid"] != true {
            continue;
        }
        let offset = u(&rec["offset"]);
        let size = u(&rec["record_size"]);
        if offset + size > range_end || size > 4096 {
            continue;
        }
        let at = format!("{label} FILE at {offset:#x}");
        let attributes = rec["attributes"].as_array().unwrap();
        let blocks = size / 512;
        let mismatches = rec["fixup_mismatch_sectors"].as_array().unwrap();
        if mismatches.len() as u64 == blocks {
            tally.dropped_log_copies += 1;
            assert!(
                store.record_at(offset).is_none(),
                "{at}: a log copy was stored"
            );
            continue;
        }
        if rec["in_use"] == false && attributes.is_empty() {
            tally.dropped_never_used += 1;
            assert!(
                store.record_at(offset).is_none(),
                "{at}: a never-used record was stored"
            );
            continue;
        }
        expected_records += 1;
        let id = store
            .record_at(offset)
            .unwrap_or_else(|| panic!("{at}: not stored"));
        let r = store.record(id).unwrap();
        tally.records += 1;
        assert_eq!(r.record_size() as u64, size, "{at}");
        assert_eq!(
            r.record_number().map(u64::from),
            rec["record_number"].as_u64(),
            "{at}"
        );
        assert_eq!(u64::from(r.sequence()), u(&rec["sequence"]), "{at}");
        assert_eq!(u64::from(r.header_flags()), u(&rec["flags"]), "{at}");
        assert_eq!(u64::from(r.link_count()), u(&rec["link_count"]), "{at}");
        assert_eq!(r.lsn(), u(&rec["lsn"]), "{at}");
        assert_eq!(
            r.base(),
            FileRef::new(u(&rec["base_record"]), u(&rec["base_sequence"]) as u16),
            "{at}"
        );
        match r.fixup() {
            FixupStatus::Ok => assert!(mismatches.is_empty(), "{at}"),
            FixupStatus::Torn {
                first_bad_block,
                bad_blocks,
            } => {
                assert_eq!(u64::from(bad_blocks), mismatches.len() as u64, "{at}");
                assert_eq!(u64::from(first_bad_block), u(&mismatches[0]), "{at}");
            }
        }
        // $STANDARD_INFORMATION: the first one.
        let si = attributes
            .iter()
            .find_map(|a| a.get("standard_information"));
        match (r.standard_information(), si) {
            (Some(mine), Some(theirs)) => {
                assert_eq!(mine.times.created, u(&theirs["created"]), "{at}");
                assert_eq!(mine.times.modified, u(&theirs["modified"]), "{at}");
                assert_eq!(mine.times.mft_changed, u(&theirs["mft_changed"]), "{at}");
                assert_eq!(mine.times.accessed, u(&theirs["accessed"]), "{at}");
                assert_eq!(u64::from(mine.flags), u(&theirs["file_attributes"]), "{at}");
            }
            (None, None) => {}
            (mine, theirs) => panic!("{at}: $SI {mine:?} vs {theirs:?}"),
        }
        // Names, in on-disk order.
        let mine: Vec<_> = store.names(id).collect();
        let theirs = rec["names"].as_array().unwrap();
        // The latest $FILE_NAME time (A4) and the record time.
        let latest = theirs.iter().map(fn_latest).max().filter(|&t| t != 0);
        assert_eq!(r.latest_name_time(), latest, "{at}");
        let record_time = match (r.mft_changed(), latest) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        assert_eq!(r.record_time(), record_time, "{at}");
        assert_eq!(mine.len(), theirs.len(), "{at}: names");
        for (m, t) in mine.iter().zip(theirs) {
            assert_eq!(
                m.name.to_units(),
                units(t["name"].as_str().unwrap()),
                "{at}"
            );
            assert_eq!(u64::from(m.namespace.as_u8()), u(&t["namespace"]), "{at}");
            assert_eq!(
                m.parent,
                FileRef::new(u(&t["parent_record"]), u(&t["parent_sequence"]) as u16),
                "{at}"
            );
            assert_eq!(u64::from(m.flags), u(&t["file_attributes"]), "{at}");
            tally.names += 1;
        }
        // Unnamed $DATA extents.
        let mine: Vec<_> = store.data_extents(id).collect();
        let theirs: Vec<&Value> = rec["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|d| d["name"] == "")
            .collect();
        assert_eq!(mine.len(), theirs.len(), "{at}: data extents");
        for (m, t) in mine.iter().zip(theirs) {
            tally.extents += 1;
            match m {
                DataExtent::Resident(res) => {
                    assert_eq!(t["non_resident"], false, "{at}");
                    assert_eq!(u64::from(res.value_offset), u(&t["value_offset"]), "{at}");
                    assert_eq!(u64::from(res.value_len), u(&t["value_length"]), "{at}");
                    assert_eq!(u64::from(res.flags), u(&t["flags"]), "{at}");
                }
                DataExtent::NonResident(e) => {
                    assert_eq!(t["non_resident"], true, "{at}");
                    assert_eq!(e.start_vcn, u(&t["lowest_vcn"]), "{at}");
                    assert_eq!(e.last_vcn, t["highest_vcn"].as_i64().unwrap(), "{at}");
                    assert_eq!(u64::from(e.flags), u(&t["flags"]), "{at}");
                    if e.start_vcn == 0 {
                        let s = e.sizes.unwrap();
                        assert_eq!(s.allocated, u(&t["allocated_size"]), "{at}");
                        assert_eq!(s.real, u(&t["data_size"]), "{at}");
                        assert_eq!(s.initialized, u(&t["initialized_size"]), "{at}");
                    }
                    let runs: Vec<(Option<u64>, u64)> =
                        e.runs().map(|r| (r.lcn, r.length)).collect();
                    let expected: Vec<(Option<u64>, u64)> = t["runs"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|r| (r["lcn"].as_u64(), u(&r["length"])))
                        .collect();
                    assert_eq!(runs, expected, "{at}: runs");
                    assert_eq!(
                        e.runlist_status == RunlistStatus::Complete,
                        t["runlist_ok"] == true,
                        "{at}"
                    );
                    tally.runs += runs.len() as u64;
                }
            }
        }
        // $INDEX_ALLOCATION:$I30 extents.
        let mine: Vec<_> = store.index_allocation_extents(id).collect();
        let theirs: Vec<&Value> = attributes
            .iter()
            .filter(|a| a["type"] == 0xA0 && a["name"] == "$I30" && a["non_resident"] == true)
            .collect();
        assert_eq!(mine.len(), theirs.len(), "{at}: index allocation");
        for (m, t) in mine.iter().zip(theirs) {
            assert_eq!(m.start_vcn, u(&t["lowest_vcn"]), "{at}");
            let runs: Vec<(Option<u64>, u64)> = m.runs().map(|r| (r.lcn, r.length)).collect();
            let expected: Vec<(Option<u64>, u64)> = t["runs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| (r["lcn"].as_u64(), u(&r["length"])))
                .collect();
            assert_eq!(runs, expected, "{at}: index allocation runs");
        }
        // $ATTRIBUTE_LIST.
        let list = attributes.iter().find(|a| a["type"] == 0x20);
        match (r.attribute_list(), list) {
            (AttributeListState::Absent, None) => {}
            (AttributeListState::NonResident, Some(l)) => {
                assert_eq!(l["non_resident"], true, "{at}")
            }
            (AttributeListState::Resident { .. }, Some(l)) => {
                let theirs: Vec<(u64, AttrListName, u64, FileRef, u64)> = l["attribute_list"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|e| e["type"] == 0x80 || e["type"] == 0xA0)
                    .map(|e| {
                        let name = match e["name"].as_str().unwrap() {
                            "" => AttrListName::Unnamed,
                            "$I30" => AttrListName::I30,
                            _ => AttrListName::Other,
                        };
                        (
                            u(&e["type"]),
                            name,
                            u(&e["lowest_vcn"]),
                            FileRef::new(u(&e["record"]), u(&e["sequence"]) as u16),
                            u(&e["attr_id"]),
                        )
                    })
                    .collect();
                let mine: Vec<_> = store
                    .attribute_list_entries(id)
                    .map(|e| {
                        (
                            u64::from(e.type_code),
                            e.name,
                            e.start_vcn,
                            e.holder,
                            u64::from(e.attr_id),
                        )
                    })
                    .collect();
                assert_eq!(mine, theirs, "{at}: attribute list");
            }
            (mine, theirs) => panic!("{at}: attribute list {mine:?} vs {theirs:?}"),
        }
        // Directory entries of $INDEX_ROOT:$I30.
        for a in attributes
            .iter()
            .filter(|a| a["type"] == 0x90 && a["name"] == "$I30")
        {
            let Some(root) = a.get("index_root") else {
                continue;
            };
            let theirs: Vec<&Value> = root["entries"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| {
                    e["file_name"].is_object()
                        && u(&e["file_name"]["file_attributes"]) & 0x1000_0000 != 0
                })
                .collect();
            let mine: Vec<_> = store
                .index_entries()
                .filter(|(_, e)| {
                    e.source.offset == offset && e.source.kind == EntrySourceKind::RecordRoot
                })
                .map(|(_, e)| e)
                .collect();
            assert_eq!(mine.len(), theirs.len(), "{at}: index root entries");
            for (m, t) in mine.iter().zip(theirs) {
                assert_eq!(
                    m.child,
                    FileRef::new(u(&t["record"]), u(&t["sequence"]) as u16),
                    "{at}"
                );
                let f = &t["file_name"];
                assert_eq!(
                    m.name.to_units(),
                    units(f["name"].as_str().unwrap()),
                    "{at}"
                );
                assert_eq!(
                    m.parent,
                    FileRef::new(u(&f["parent_record"]), u(&f["parent_sequence"]) as u16),
                    "{at}"
                );
                assert_eq!(m.time, fn_latest(f), "{at}");
                assert_eq!(m.source.lsn, r.lsn(), "{at}");
                tally.root_entries += 1;
            }
        }
    }
    assert_eq!(
        store.record_count() as u64,
        expected_records,
        "{label}: records"
    );

    // INDX blocks.
    let mut expected_blocks = 0_u64;
    for block in inspector["index_blocks"].as_array().unwrap() {
        if block["valid"] != true {
            continue;
        }
        let offset = u(&block["offset"]);
        let size = u(&block["block_size"]);
        if offset + size > range_end {
            continue;
        }
        expected_blocks += 1;
        let at = format!("{label} INDX at {offset:#x}");
        let mine = store
            .index_block_at(offset)
            .unwrap_or_else(|| panic!("{at}: not stored"));
        assert_eq!(u64::from(mine.size), size, "{at}");
        assert_eq!(mine.vcn, u(&block["vcn"]), "{at}");
        assert_eq!(mine.lsn, u(&block["lsn"]), "{at}");
        assert_eq!(mine.fixup.is_ok(), block["fixup_ok"] == true, "{at}");
        let names: Vec<&Value> = block["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["file_name"].is_object())
            .collect();
        // The owner: the parent shared by most entries, the first among equals.
        let mut counts: Vec<(FileRef, usize)> = Vec::new();
        for e in &names {
            let p = FileRef::new(
                u(&e["file_name"]["parent_record"]),
                u(&e["file_name"]["parent_sequence"]) as u16,
            );
            match counts.iter_mut().find(|(q, _)| *q == p) {
                Some((_, n)) => *n += 1,
                None => counts.push((p, 1)),
            }
        }
        let mut owner: Option<(FileRef, usize)> = None;
        for &(p, n) in &counts {
            if owner.is_none_or(|(_, best)| n > best) {
                owner = Some((p, n));
            }
        }
        assert_eq!(mine.owner, owner.map(|(p, _)| p), "{at}: owner");
        let theirs: Vec<&&Value> = names
            .iter()
            .filter(|e| u(&e["file_name"]["file_attributes"]) & 0x1000_0000 != 0)
            .collect();
        let entries: Vec<_> = store
            .index_entries()
            .filter(|(_, e)| e.source.offset == offset && e.source.kind == EntrySourceKind::Block)
            .map(|(_, e)| e)
            .collect();
        assert_eq!(entries.len(), theirs.len(), "{at}: directory entries");
        for (m, t) in entries.iter().zip(theirs) {
            assert_eq!(
                m.child,
                FileRef::new(u(&t["record"]), u(&t["sequence"]) as u16),
                "{at}"
            );
            assert_eq!(
                m.name.to_units(),
                units(t["file_name"]["name"].as_str().unwrap()),
                "{at}"
            );
            assert_eq!(
                u64::from(m.namespace.as_u8()),
                u(&t["file_name"]["namespace"]),
                "{at}"
            );
            assert_eq!(m.time, fn_latest(&t["file_name"]), "{at}");
            assert_eq!(m.source.lsn, mine.lsn, "{at}");
            tally.block_entries += 1;
        }
        tally.blocks += 1;
    }
    assert_eq!(
        store.index_blocks().len() as u64,
        expected_blocks,
        "{label}: index blocks"
    );

    // Boot sectors.
    let mut boots: Vec<&Value> = inspector["boot_sectors"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| b["valid"] == true)
        .collect();
    boots.retain(|b| u(&b["offset"]) + 512 <= range_end);
    let mut mine: Vec<_> = store.boot_hits().to_vec();
    mine.sort_by_key(|b| b.disk_offset);
    assert_eq!(mine.len(), boots.len(), "{label}: boot sectors");
    for (m, t) in mine.iter().zip(boots) {
        assert_eq!(m.disk_offset, u(&t["offset"]), "{label}");
        assert_eq!(
            u64::from(m.boot.cluster_size),
            u(&t["cluster_size"]),
            "{label}"
        );
        assert_eq!(m.boot.total_sectors, u(&t["total_sectors"]), "{label}");
        assert_eq!(m.boot.mft_lcn, u(&t["mft_lcn"]), "{label}");
        assert_eq!(m.boot.mftmirr_lcn, u(&t["mftmirr_lcn"]), "{label}");
        assert_eq!(
            u64::from(m.boot.mft_record_size),
            u(&t["mft_record_size"]),
            "{label}"
        );
        assert_eq!(
            u64::from(m.boot.index_block_size),
            u(&t["index_block_size"]),
            "{label}"
        );
        assert_eq!(
            u64::from(m.boot.hidden_sectors),
            u(&t["hidden_sectors"]),
            "{label}"
        );
        assert_eq!(
            format!("{:016X}", m.boot.serial),
            t["serial"].as_str().unwrap().to_uppercase(),
            "{label}"
        );
        tally.boots += 1;
    }
    let c = store.counters();
    assert_eq!(
        c.never_used, tally.dropped_never_used,
        "{label}: never-used count"
    );
    assert_eq!(
        c.log_copies, tally.dropped_log_copies,
        "{label}: log-copy count"
    );
    tally
}

/// Checks every manifest entry whose record survived.
fn compare_with_manifest(label: &str, store: &ScanStore, manifest: &Value) -> u64 {
    let by_number: HashMap<u64, Vec<RecordId>> = {
        let mut map: HashMap<u64, Vec<RecordId>> = HashMap::new();
        for (id, r) in store.records() {
            if let Some(n) = r.record_number() {
                map.entry(u64::from(n)).or_default().push(id);
            }
        }
        map
    };
    let entries = manifest["entries"].as_array().unwrap();
    let by_path: HashMap<&str, &Value> = entries
        .iter()
        .map(|e| (e["path"].as_str().unwrap(), e))
        .collect();
    let mut checked = 0;
    for e in entries {
        if e["after"]["record_intact"] != true {
            continue;
        }
        let path = e["path"].as_str().unwrap();
        let at = format!("{label} {path}");
        let ondisk = &e["ondisk"];
        let id = store
            .record_at(u(&ondisk["record_offset"]))
            .unwrap_or_else(|| panic!("{at}: record not found"));
        let r = store.record(id).unwrap();
        let number = u(&e["mft_record"]);
        assert_eq!(r.record_number().map(u64::from), Some(number), "{at}");
        let deleted = e["deleted"] == true;
        if let Some(seq) = e["mft_sequence"].as_u64() {
            let expected = if deleted {
                hds_ntfs::next_seq(seq as u16)
            } else {
                seq as u16
            };
            assert_eq!(r.sequence(), expected, "{at}");
            assert_eq!(r.is_in_use(), !deleted, "{at}");
        }
        // The extension records, by number, with a base reference to this record.
        let holder = |n: u64| -> RecordId {
            if n == number {
                return id;
            }
            *by_number
                .get(&n)
                .and_then(|ids| {
                    ids.iter()
                        .find(|&&x| store.record(x).unwrap().base().number() == number)
                })
                .unwrap_or_else(|| panic!("{at}: extension record {n} not found"))
        };
        let extensions: Vec<RecordId> = ondisk["extension_records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| holder(u(n)))
            .collect();
        // Names, each in the record the manifest says, with the parent's reference.
        let parent_ref = match path.rsplit_once('/') {
            None => Some(FileRef::new(5, 5)),
            Some((parent, _)) => by_path.get(parent).and_then(|p| {
                p["mft_sequence"]
                    .as_u64()
                    .map(|s| FileRef::new(u(&p["mft_record"]), s as u16))
            }),
        };
        // (Deleted entries list no names: their record keeps the last one.)
        let expected_names: Vec<(String, Option<u8>, u64)> = match ondisk["names"].as_array() {
            Some(names) => names
                .iter()
                .map(|n| {
                    let namespace = match n["namespace"].as_str().unwrap() {
                        "POSIX" => 0,
                        "WIN32" => 1,
                        "DOS" => 2,
                        _ => 3,
                    };
                    (
                        n["name"].as_str().unwrap().to_string(),
                        Some(namespace),
                        u(&n["record"]),
                    )
                })
                .collect(),
            None => vec![(path.rsplit('/').next().unwrap().to_string(), None, number)],
        };
        for (name, namespace, record) in expected_names {
            let holders: Vec<RecordId> = if record == number {
                std::iter::once(id)
                    .chain(extensions.iter().copied())
                    .collect()
            } else {
                vec![holder(record)]
            };
            let wanted = units(&name);
            let found = holders
                .iter()
                .find_map(|&h| store.names(h).find(|n| n.name.to_units() == wanted))
                .unwrap_or_else(|| panic!("{at}: name {name:?} not found"));
            if let Some(namespace) = namespace {
                assert_eq!(found.namespace.as_u8(), namespace, "{at}");
            }
            if let Some(parent) = parent_ref {
                assert_eq!(found.parent, parent, "{at}: parent reference");
            }
        }
        // Runs, in VCN order over the base and its extension records.
        if let Some(runs) = ondisk["runs"].as_array() {
            let mut extents: Vec<(u64, Runs)> = std::iter::once(id)
                .chain(extensions.iter().copied())
                .flat_map(|h| store.data_extents(h).collect::<Vec<_>>())
                .filter_map(|d| match d {
                    DataExtent::NonResident(x) => {
                        Some((x.start_vcn, x.runs().map(|r| (r.lcn, r.length)).collect()))
                    }
                    DataExtent::Resident(_) => None,
                })
                .collect();
            extents.sort_by_key(|(vcn, _)| *vcn);
            let mine: Vec<(Option<u64>, u64)> = extents.into_iter().flat_map(|(_, r)| r).collect();
            let theirs: Vec<(Option<u64>, u64)> =
                runs.iter().map(|r| (r[0].as_u64(), u(&r[1]))).collect();
            assert_eq!(mine, theirs, "{at}: runs");
        }
        if ondisk["resident"] == true {
            let resident = std::iter::once(id)
                .chain(extensions.iter().copied())
                .any(|h| {
                    store
                        .data_extents(h)
                        .any(|d| matches!(d, DataExtent::Resident(_)))
                });
            assert!(resident, "{at}: resident data");
        }
        checked += 1;
    }
    checked
}

#[test]
fn every_fixture_matches_the_inspector_and_its_manifest() {
    let Some(dir) = env_dir("HDS_FIXTURES") else {
        return;
    };
    for name in SCENARIOS {
        let image_path = dir.join(format!("{name}.img"));
        let manifest_path = dir.join(format!("{name}.manifest.json"));
        assert!(
            image_path.is_file() && manifest_path.is_file(),
            "HDS_FIXTURES is set but {name} is missing"
        );
        let manifest = read_json(&manifest_path);
        let inspector = inspect(&image_path);
        let source: Arc<dyn BlockSource> = Arc::new(ImageFile::open(&image_path, None).unwrap());
        let size = source.size();
        let session = scan_range(source.clone(), 0..size);
        let store = session.store();
        let tally = compare_with_inspector(name, store, &inspector, size);
        let checked = compare_with_manifest(name, store, &manifest);
        assert!(
            checked > 0,
            "{name}: no manifest entry with an intact record"
        );
        eprintln!(
            "{name}: {tally:?}; {checked} manifest entries; {} bytes per record",
            store.record_bytes() / store.record_count().max(1)
        );
    }
}

#[test]
fn a_partition_scanned_alone_finds_its_records_and_reads_nothing_else() {
    let Some(dir) = env_dir("HDS_FIXTURES") else {
        return;
    };
    for name in ["offset-mbr", "offset-mbr63", "offset-gpt"] {
        let image_path = dir.join(format!("{name}.img"));
        let manifest_path = dir.join(format!("{name}.manifest.json"));
        assert!(
            image_path.is_file() && manifest_path.is_file(),
            "HDS_FIXTURES is set but {name} is missing"
        );
        let manifest = read_json(&manifest_path);
        let image: Arc<dyn BlockSource> = Arc::new(ImageFile::open(&image_path, None).unwrap());
        let start = u(&manifest["old_volume"]["offset"]);
        let end = start + u(&manifest["old_volume"]["length"]);
        let faulty = Arc::new(FaultySource::new(image.clone()));
        let part = scan_range(faulty.clone(), start..end);
        assert!(
            faulty
                .touched()
                .iter()
                .all(|r| r.start >= start && r.end <= end),
            "{name}: read outside the partition"
        );
        let whole = scan_range(image, 0..faulty.size());
        let inside = |s: &ScanStore| -> Vec<u64> {
            s.records_by_offset()
                .iter()
                .map(|&id| s.record(id).unwrap())
                .filter(|r| r.disk_offset() >= start && r.end_offset() <= end)
                .map(|r| r.disk_offset())
                .collect()
        };
        assert_eq!(inside(part.store()), inside(whole.store()), "{name}");
        assert!(part.store().record_count() > 500, "{name}");
        let checked = compare_with_manifest(name, part.store(), &manifest);
        assert!(checked > 500, "{name}: {checked}");
    }
}

#[test]
fn fixture_sessions_round_trip() {
    let Some(dir) = env_dir("HDS_FIXTURES") else {
        return;
    };
    let scratch =
        std::env::var_os("HDS_CORE_TEST_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
    let out = scratch.join(format!("hds-core-fixture-sessions-{}", std::process::id()));
    std::fs::create_dir_all(&out).unwrap();
    for name in ["basic", "fragmented", "wiped"] {
        let image_path = dir.join(format!("{name}.img"));
        assert!(
            image_path.is_file(),
            "HDS_FIXTURES is set but {name} is missing"
        );
        let source: Arc<dyn BlockSource> = Arc::new(ImageFile::open(&image_path, None).unwrap());
        let size = source.size();
        let mut session = scan_range(source.clone(), 0..size);
        let protected = [Protected::session(0..size)];
        let guard = SessionGuard {
            source: source.as_ref(),
            protected: &protected,
            allow_same_disk: false,
            allow_unknown_disk: false,
        };
        let path = out.join(format!("{name}.hds"));
        session.save_as(&path, &guard).unwrap();
        let (loaded, report) = Session::load(&path).unwrap();
        assert_eq!(report.ignored_bytes, 0);
        assert_eq!(loaded.store(), session.store(), "{name}");
        assert_eq!(loaded.meta(), session.meta(), "{name}");
    }
    std::fs::remove_dir_all(&out).unwrap();
}

#[test]
fn windows_written_samples_match_the_inspector() {
    let Some(dir) = env_dir("HDS_WIN_SAMPLES") else {
        return;
    };
    let mut images: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.extension()
                .is_some_and(|x| x == "raw" || x == "dd" || x == "vhd" || x == "img")
        })
        .collect();
    images.sort();
    assert!(!images.is_empty(), "no images in {}", dir.display());
    for image_path in images {
        let label = image_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let source: Arc<dyn BlockSource> = Arc::new(ImageFile::open(&image_path, None).unwrap());
        // Scan whole sectors only (a VHD has a 512-byte footer).
        let size = source.size() / 512 * 512;
        let session = scan_range(source, 0..size);
        let tally = compare_with_inspector(&label, session.store(), &inspect(&image_path), size);
        eprintln!(
            "{label}: {tally:?}; {} bytes per record",
            session.store().record_bytes() / session.store().record_count().max(1)
        );
    }
}

/// Never-used and torn records are what the inspector says, also in partitions.
#[test]
fn dropped_records_are_counted() {
    let Some(dir) = env_dir("HDS_FIXTURES") else {
        return;
    };
    let image_path = dir.join("wiped.img");
    assert!(
        image_path.is_file(),
        "HDS_FIXTURES is set but wiped is missing"
    );
    let source: Arc<dyn BlockSource> = Arc::new(ImageFile::open(&image_path, None).unwrap());
    let size = source.size();
    let session = scan_range(source, 0..size);
    // The wipe left blank records in slots 28-255: never used, counted, not stored.
    assert!(
        session.store().counters().never_used >= 228 - 16,
        "{:?}",
        session.store().counters()
    );
}
