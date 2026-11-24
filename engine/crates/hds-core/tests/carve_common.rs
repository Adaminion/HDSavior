//! Helpers shared by the carving tests (`carve.rs`, `carve_fixtures.rs`),
//! included with `#[path]`. Built on its own by cargo as well, where it
//! holds no test.

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use hds_carve::FileKind;
use hds_core::carve::{
    carved_files_folder, ext_folder, CarveGeometry, CarveInput, CarveRun, CarveSummary, CarvedFile,
    Carver, KindSet, NoCarveObserver,
};
use hds_core::scan::{scan, NoObserver, ScanOptions, ScanStatus};
use hds_core::session::{Session, SessionMeta, SourceIdentity};
use hds_io::testing::MemSource;
use hds_io::{BlockSource, RangeSet, RangedSource};
use hds_recover::{
    recover, FileOutcome, FlagSet, Plan, RecoverObserver, RecoverOptions, RecoverProgress,
    RecoverSummary,
};

/// A real file written by a real encoder: the samples of `hds-carve`.
#[derive(Clone)]
pub struct Sample {
    pub name: String,
    pub kind: FileKind,
    pub data: Vec<u8>,
}

pub fn sample_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../hds-carve/tests/data")
}

/// Every sample whose true length is its size on disk.
pub fn samples() -> Vec<Sample> {
    let mut names: Vec<String> = std::fs::read_dir(sample_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| !n.starts_with("special_") && !n.ends_with(".sh"))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let kind = FileKind::from_name(name.split_once('_').unwrap().0).unwrap();
            Sample {
                data: std::fs::read(sample_dir().join(&name)).unwrap(),
                name,
                kind,
            }
        })
        .collect()
}

pub fn sample(name: &str) -> Sample {
    samples()
        .into_iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("{name}"))
}

/// What `hds_carve` measures for the sample alone.
pub fn alone(s: &Sample) -> hds_carve::Measured {
    hds_carve::measure(s.kind, &s.data, s.data.len() as u64).unwrap()
}

/// A deterministic generator.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next_u64() as u8).collect()
    }
}

/// A disk image in memory.
pub struct Disk {
    pub data: Vec<u8>,
}

impl Disk {
    pub fn new(len: usize) -> Disk {
        Disk { data: vec![0; len] }
    }
    pub fn put(&mut self, at: u64, bytes: &[u8]) {
        let at = at as usize;
        self.data[at..at + bytes.len()].copy_from_slice(bytes);
    }
    pub fn source(&self) -> Arc<dyn BlockSource> {
        Arc::new(MemSource::new(self.data.clone()))
    }
}

/// Scans `range` of `source` into an in-memory session, collecting carve
/// hits.
pub fn scan_source(source: &Arc<dyn BlockSource>, range: Range<u64>) -> Session {
    let options = ScanOptions::default();
    let meta =
        SessionMeta::new(SourceIdentity::from(&source.info()), range.clone(), options).unwrap();
    let mut session = Session::new(meta);
    let ranged = RangedSource::new(source.clone(), range).unwrap();
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

/// The input of a carving job over `session`, with the catalog's sets.
pub fn input(
    session: &Session,
    kinds: KindSet,
    geometry: CarveGeometry,
    protected_files: RangeSet,
    overwritten: RangeSet,
    live_allocated: RangeSet,
) -> CarveInput {
    CarveInput {
        kinds,
        geometry,
        hits: session.store().carve_hits_sorted(),
        done: session.store().done().clone(),
        protected_files,
        overwritten,
        live_allocated,
    }
}

/// Runs a carving job to its end.
pub fn carve(
    source: &Arc<dyn BlockSource>,
    session: &Session,
    input: CarveInput,
) -> (CarveRun, CarveSummary) {
    let range = session.meta().range.clone();
    let carver = Carver::new(input, range.clone());
    let ranged = RangedSource::new(source.clone(), range).unwrap();
    carver.run(&ranged, &mut NoCarveObserver, &AtomicBool::new(false))
}

pub fn ranges(list: &[Range<u64>]) -> RangeSet {
    list.iter().cloned().collect()
}

/// A scratch directory that is removed when dropped.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(what: &str) -> TempDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let base =
            std::env::var_os("HDS_CORE_TEST_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
        let dir = base.join(format!(
            "hds-carve-{what}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Outcomes(Vec<FileOutcome>);

impl RecoverObserver for Outcomes {
    fn progress(&mut self, _progress: &RecoverProgress) {}
    fn file_done(&mut self, outcome: &FileOutcome) {
        self.0.push(outcome.clone());
    }
}

/// One carved file after recovery: its outcome and the bytes written.
pub struct Recovered {
    pub file: CarvedFile,
    pub outcome: FileOutcome,
    pub content: Option<Vec<u8>>,
}

/// Recovers `files` through `hds_recover` exactly as a plan built from
/// the catalog would: `[Carved files]/<ext>/<name>`, each with the flags
/// `flags` gives it.
pub fn recover_files(
    source: &Arc<dyn BlockSource>,
    range: Range<u64>,
    files: &[CarvedFile],
    flags: &dyn Fn(&CarvedFile) -> FlagSet,
) -> (RecoverSummary, Vec<Recovered>) {
    let mut plan = Plan::default();
    plan.folders.push(carved_files_folder(1, None));
    let mut ext_ids: BTreeMap<String, u64> = BTreeMap::new();
    for f in files {
        let next = 2 + ext_ids.len() as u64;
        let id = *ext_ids.entry(f.ext().to_string()).or_insert(next);
        if id == next {
            plan.folders.push(ext_folder(id, 1, f.ext()));
        }
        plan.files
            .push(f.plan_file(1000 + plan.files.len() as u64, Some(id), flags(f)));
    }
    let dest = TempDir::new("recover");
    let mut outcomes = Outcomes::default();
    let summary = recover(
        RangedSource::new(source.clone(), range).unwrap(),
        plan,
        &dest.path().join("out"),
        RecoverOptions::new(Vec::new()),
        &mut outcomes,
        &AtomicBool::new(false),
    )
    .unwrap();
    let recovered = outcomes
        .0
        .into_iter()
        .map(|outcome| {
            let file = files[(outcome.node - 1000) as usize];
            let content = std::fs::read(dest.path().join("out").join(&outcome.output_path)).ok();
            Recovered {
                file,
                outcome,
                content,
            }
        })
        .collect();
    (summary, recovered)
}

// ------------------------------------------------------------- SHA-256 --

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// SHA-256 of `data`, as lower-case hex.
pub fn sha256_hex(data: &[u8]) -> String {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());
    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(block[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

#[test]
fn sha256_known_answers() {
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        sha256_hex(&[b'a'; 1000]),
        "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
    );
}
