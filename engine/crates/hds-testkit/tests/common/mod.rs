//! Helpers shared by the integration tests.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use hds_testkit::disk::Built;
use hds_testkit::facts::{ArtifactState, RecordFact, RecordRole};
use hds_testkit::manifest::{Entry, Flag, Manifest, Presence};

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
