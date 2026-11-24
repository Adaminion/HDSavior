//! Helpers shared by the integration tests.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use hds_testkit::disk::Built;
use hds_testkit::facts::{ArtifactState, RecordFact, RecordRole};
use hds_testkit::manifest::{CatalogInstance, Entry, Flag, FragmentFact, Manifest, Presence, RecordClass};

/// The seeds the scenario tests run with: what a scenario promises must
/// not depend on the seed.
pub const SEEDS: [u64; 3] = [1, 0x5EED, 20_261_124];

/// One mebibyte.
pub const MIB: u64 = 1 << 20;

/// Builds a catalog scenario.
pub fn build(name: &str, seed: u64) -> Built {
    hds_testkit::scenarios::build(name, seed).unwrap_or_else(|| panic!("the catalog has no scenario {name:?}"))
}

/// The records of an instance that are intact where the file system wrote
/// them (no `$MFTMirr` copies, no stale copies).
pub fn intact_records<'a>(m: &'a Manifest, label: &str) -> Vec<&'a RecordFact> {
    let id = m.instance(label).id;
    m.records
        .iter()
        .filter(|r| {
            r.instance == id
                && r.state == ArtifactState::Intact
                && r.role != RecordRole::Mirror
                && r.role != RecordRole::StaleCopy
        })
        .collect()
}

/// The sorted slots of some records.
pub fn slots(records: &[&RecordFact]) -> Vec<u64> {
    let mut s: Vec<u64> = records.iter().map(|r| r.slot).collect();
    s.sort_unstable();
    s
}

/// `true` when a correct catalog has a node for the entry.
pub fn shown(e: &Entry) -> bool {
    e.expect.presence == Presence::Shown
}

/// Asserts that the manifest requires `flag` for the entry.
#[track_caller]
pub fn must(e: &Entry, flag: Flag) {
    assert!(e.expect.flags_set.contains(&flag), "{} should be expected {flag:?}: {:?}", e.path, e.expect);
}

/// Asserts that the manifest forbids `flag` for the entry.
#[track_caller]
pub fn must_not(e: &Entry, flag: Flag) {
    assert!(e.expect.flags_clear.contains(&flag), "{} should be expected without {flag:?}: {:?}", e.path, e.expect);
}

/// The expected path of an entry, joined with `/`.
#[track_caller]
pub fn expected_path(e: &Entry) -> String {
    e.expect.path.as_ref().unwrap_or_else(|| panic!("{} has no expected path", e.path)).join("/")
}

/// `true` when `[start, start + len)` meets one of the `(start, length)`
/// ranges.
pub fn meets(ranges: &[(u64, u64)], start: u64, len: u64) -> bool {
    ranges.iter().any(|&(s, l)| s < start + len && start < s + l)
}

/// A `FILE` record or `INDX` block read from the image with its fixups
/// undone, the way a parser sees it.
pub fn read_structure(built: &Built, offset: u64, size: usize) -> Vec<u8> {
    let mut bytes = built.image.read_vec(offset, size);
    let usa = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    let count = u16::from_le_bytes([bytes[6], bytes[7]]) as usize;
    for block in 1..count {
        let saved = [bytes[usa + 2 * block], bytes[usa + 2 * block + 1]];
        bytes[block * 512 - 2..block * 512].copy_from_slice(&saved);
    }
    bytes
}

/// The content of a file read from the image through what the manifest says
/// about its record: the resident value, or the runs, zeros for holes and
/// for everything from the initialized size on, cut at the real size. This
/// is what a recovery has to produce.
pub fn read_back(built: &Built, e: &Entry) -> Vec<u8> {
    if let Some((record, value, len)) = e.resident_at {
        let size = built.manifest.instances[e.instance as usize].geometry.record_size as usize;
        let bytes = read_structure(built, record, size);
        return bytes[value as usize..(value + len) as usize].to_vec();
    }
    let mut out = Vec::new();
    for x in &e.extents {
        match x.offset {
            Some(offset) => out.extend(built.image.read_vec(offset, x.bytes as usize)),
            None => out.extend(std::iter::repeat_n(0u8, x.bytes as usize)),
        }
    }
    out.resize(e.size as usize, 0);
    for b in out.iter_mut().skip(e.initialized_size as usize) {
        *b = 0;
    }
    out
}

/// Asserts that reading the file back gives the manifest's SHA-256.
#[track_caller]
pub fn assert_reads_back(built: &Built, e: &Entry) {
    let hash = hds_testkit::hash::sha256_hex(&read_back(built, e));
    assert_eq!(Some(&hash), e.sha256.as_ref(), "{} does not read back from the image", e.path);
}

/// A directory for the files of one test; removed when dropped.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(name: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!("hds-testkit-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir(dir)
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// With `HDS_TESTKIT_REQUIRE_TOOLS=1` a missing external tool fails the
/// test instead of skipping it.
pub fn tools_required() -> bool {
    std::env::var("HDS_TESTKIT_REQUIRE_TOOLS").is_ok_and(|v| v == "1")
}

/// Tells the reader of the test output that something was not checked.
pub fn skip(what: &str, why: &str) {
    assert!(!tools_required(), "{what} cannot run: {why}");
    eprintln!("SKIPPED {what}: {why}");
}

fn runs(program: &str, arg: &str) -> bool {
    Command::new(program).arg(arg).output().is_ok_and(|o| o.status.success())
}

/// The Python interpreter, if there is one.
pub fn python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|p| runs(p, "--version"))
}

/// `scripts/linux/inspect-ntfs.py` of this repository, if it is there.
pub fn inspector() -> Option<PathBuf> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../scripts/linux/inspect-ntfs.py");
    path.is_file().then_some(path)
}

/// A script that lives next to the tests.
pub fn test_script(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join(name)
}

/// `true` when `program` can be started.
pub fn have(program: &str) -> bool {
    Command::new(program).arg("--help").output().is_ok()
}

/// Writes image and manifest of `built` into `dir` and returns their paths.
pub fn write_built(dir: &TempDir, name: &str, built: &Built) -> (PathBuf, PathBuf) {
    let image = dir.path(&format!("{name}.img"));
    let manifest = dir.path(&format!("{name}.json"));
    built.image.write_to_file(&image).expect("write image");
    std::fs::write(&manifest, built.manifest.to_json()).expect("write manifest");
    (image, manifest)
}

/// Runs `crosscheck.py`; returns its success and its output.
pub fn crosscheck(python: &str, inspector: &Path, image: &Path, manifest: &Path) -> (bool, String) {
    let output = Command::new(python)
        .arg(test_script("crosscheck.py"))
        .arg(inspector)
        .arg(image)
        .arg(manifest)
        .output()
        .expect("start python");
    let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    (output.status.success(), text)
}

/// The records of one MFT fragment that a correct scan groups there: old
/// records with a stated number at `origin`.
pub fn fragment_records<'a>(m: &'a Manifest, fragment: &FragmentFact) -> Vec<&'a RecordFact> {
    m.records
        .iter()
        .filter(|r| r.expect.class == Some(RecordClass::Old) && r.size == fragment.record_size)
        .filter(|r| r.number.is_some_and(|n| r.offset as i64 - (n * r.size as u64) as i64 == fragment.origin))
        .collect()
}

/// A plausible MFT-change time in the sense of ALGORITHMS D2 (the manifest
/// assumes a scan between 2026 and 2037).
fn timed(r: &RecordFact) -> Option<u64> {
    let t = r.standard_information?.0[2];
    let scan = hds_testkit::expect::ASSUMED_SCAN_TIME;
    (t != 0 && t >= hds_testkit::time::ymd(1980, 1, 1) && t <= scan + hds_testkit::time::DAY).then_some(t)
}

/// ALGORITHMS D2 written down once more, by brute force and independently
/// of `hds_testkit::expect`: the contradictions between in-use records
/// with intact fixups, reduced to the highest `lo` per `hi`, as a map
/// `hi -> lo`.
pub fn contradictions(records: &[&RecordFact]) -> BTreeMap<u64, u64> {
    let usable: Vec<&RecordFact> =
        records.iter().copied().filter(|r| r.flags & 1 == 1 && r.torn_blocks.is_empty()).collect();
    let clusters = |r: &RecordFact| -> Vec<(u64, u64)> {
        r.data
            .iter()
            .filter(|s| s.name.is_empty())
            .chain(r.index_allocation.iter().filter(|s| s.name == "$I30"))
            .filter_map(|s| s.non_resident.as_ref())
            .flat_map(|x| x.runs.iter().filter_map(|run| run.lcn.map(|l| (l, l + run.len))))
            .collect()
    };
    let directory = |r: &RecordFact| r.flags & 2 != 0 || r.names.iter().any(|n| n.flags & 0x1000_0000 != 0);
    let mut out: BTreeMap<u64, u64> = BTreeMap::new();
    let mut note = |a: u64, b: u64| {
        let (lo, hi) = (a.min(b), a.max(b));
        let entry = out.entry(hi).or_insert(lo);
        *entry = (*entry).max(lo);
    };
    for (i, x) in usable.iter().enumerate() {
        let xn = x.number.expect("numbered");
        for y in &usable[i + 1..] {
            let yn = y.number.expect("numbered");
            let shared = clusters(x).iter().any(|&(a0, a1)| clusters(y).iter().any(|&(b0, b1)| a0 < b1 && b0 < a1));
            if shared {
                note(xn, yn);
            }
        }
        for (target, parent) in x.names.iter().map(|n| (n.parent, true)).chain(std::iter::once((x.base, false))) {
            if target.number == xn || target.seq == 0 || (!parent && target == hds_testkit::ondisk::FileRef::NONE) {
                continue;
            }
            if let Some(y) = usable.iter().find(|y| y.number == Some(target.number)) {
                if y.sequence != target.seq || (parent && !directory(y)) {
                    note(xn, target.number);
                }
            }
        }
    }
    out
}

/// `true` when a changepoint of ALGORITHMS D2 lies in `(lo, hi]`.
pub fn changepoint_between(records: &[&RecordFact], lo: u64, hi: u64) -> Option<u64> {
    let timed: Vec<(u64, u64)> = records.iter().filter_map(|r| Some((r.number?, timed(r)?))).collect();
    (lo + 1..=hi).rev().find(|&p| {
        let below: Vec<u64> = timed.iter().filter(|t| t.0 < p).map(|t| t.1).collect();
        let above: Vec<u64> = timed.iter().filter(|t| t.0 >= p).map(|t| t.1).collect();
        !below.is_empty() && !above.is_empty() && above.iter().max() < below.iter().min()
    })
}

/// The catalog instance whose label is `label`.
#[track_caller]
pub fn catalog<'a>(m: &'a Manifest, label: &str) -> &'a CatalogInstance {
    m.catalog.iter().find(|c| c.label == label).unwrap_or_else(|| panic!("no catalog instance {label:?}: {:?}", m.catalog))
}

/// The catalog instance an entry is expected in.
#[track_caller]
pub fn instance_of<'a>(m: &'a Manifest, e: &Entry) -> &'a CatalogInstance {
    m.catalog_of(e).unwrap_or_else(|| panic!("{} has no expected instance", e.path))
}

/// The value of the warning `code` in the manifest, if it is decided.
pub fn warning(m: &Manifest, code: &str) -> Option<bool> {
    m.warnings.iter().find(|w| w.code == code).map(|w| w.raised)
}

/// ALGORITHMS D4 (a) between two groups of old records, written for the
/// tests: the parent references to directories and the base references
/// from records of one group to record numbers present in the other, in
/// both directions, without targets 5 and 11 and without references more
/// than one scanned old record could resolve. Returns `(counted,
/// resolving)`.
pub fn references_between(m: &Manifest, a: &[&RecordFact], b: &[&RecordFact]) -> (usize, usize) {
    let old: Vec<&RecordFact> = m.records.iter().filter(|r| r.expect.class == Some(RecordClass::Old)).collect();
    let resolves = |r: &RecordFact, seq: u16| if r.flags & 1 == 1 { r.sequence == seq } else { r.sequence == seq.wrapping_add(1).max(1) };
    let mut counted = 0;
    let mut resolving = 0;
    for (from, to) in [(a, b), (b, a)] {
        for r in from {
            let parents = r.names.iter().map(|n| (n.parent, true));
            let base = (r.base != hds_testkit::ondisk::FileRef::NONE).then_some((r.base, false));
            for (target, parent) in parents.chain(base) {
                if target.number == 5 || target.number == 11 {
                    continue;
                }
                let Some(t) = to.iter().find(|t| t.number == Some(target.number)) else { continue };
                // identical copies count once
                let candidates: Vec<u64> = old
                    .iter()
                    .filter(|o| o.number == Some(target.number) && resolves(o, target.seq))
                    .map(|o| o.fnv1a64)
                    .collect::<BTreeSet<u64>>()
                    .into_iter()
                    .collect();
                if candidates.len() > 1 {
                    continue;
                }
                counted += 1;
                let directory = t.flags & 2 != 0 || t.names.iter().any(|n| n.flags & 0x1000_0000 != 0);
                if resolves(t, target.seq) && (!parent || directory) {
                    resolving += 1;
                }
            }
        }
    }
    (counted, resolving)
}
