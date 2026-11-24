//! Properties every manifest must have, checked on every scenario of the
//! catalog; determinism; the million-record volume's time and size.

mod common;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use common::TempDir;
use hds_testkit::disk::Built;
use hds_testkit::facts::{ArtifactState, RecordRole};
use hds_testkit::manifest::{Flag, InstanceKind, InstanceRole, Manifest, Presence, RecordClass};
use hds_testkit::scenarios::{self, scale};

/// Runs `check` on every scenario (in parallel) and collects failures.
fn every_scenario(seed: u64, check: impl Fn(&Built) -> Result<(), String> + Sync) {
    let catalog = scenarios::catalog();
    let next = AtomicUsize::new(0);
    let failures: Mutex<Vec<String>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                let Some(s) = catalog.get(i) else { break };
                let built = (s.build)(seed);
                if let Err(e) = check(&built) {
                    failures.lock().unwrap().push(format!("{}: {e}", s.name));
                }
            });
        }
    });
    let failures = failures.into_inner().unwrap();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

macro_rules! ensure {
    ($cond:expr, $($arg:tt)*) => {
        if !$cond {
            return Err(format!($($arg)*));
        }
    };
}

fn invariants(m: &Manifest) -> Result<(), String> {
    let (start, end) = m.scan_range;
    // ---- records
    let current: Vec<&hds_testkit::manifest::Instance> = m.instances.iter().filter(|i| i.role == InstanceRole::Current).collect();
    for r in &m.records {
        let in_range = r.offset >= start && r.offset + r.size as u64 <= end;
        if !in_range || r.state == ArtifactState::Damaged {
            ensure!(r.expect.class.is_none(), "record at {} is not scanned but has a class", r.offset);
            continue;
        }
        let class = r.expect.class.ok_or(format!("record at {} has no class", r.offset))?;
        match class {
            RecordClass::Dropped => ensure!(
                (r.flags & 1 == 0 && r.attr_types.is_empty()) || r.torn_blocks.len() as u32 == r.size / 512,
                "record at {} is dropped without being never-used or torn throughout",
                r.offset
            ),
            RecordClass::Live | RecordClass::PostFormat => {
                let inside = current.iter().any(|c| {
                    let g = &c.geometry;
                    let mirror = g.volume_offset + g.mftmirr_lcn * g.cluster_size as u64;
                    let in_mirror = r.offset >= mirror && r.offset < mirror + g.mftmirr_clusters * g.cluster_size as u64;
                    let in_mft = g.mft_extents.iter().any(|x| r.offset >= x.offset && r.offset < x.offset + x.records * g.record_size as u64);
                    in_mirror || in_mft
                });
                ensure!(inside, "{class:?} record at {} lies in no current $MFT", r.offset);
                ensure!(r.expect.instance.is_none(), "{class:?} record at {} belongs to an instance", r.offset);
            }
            RecordClass::Old => {}
        }
        if let Some(c) = r.expect.instance {
            ensure!((c as usize) < m.catalog.len(), "record at {} names instance {c}", r.offset);
            ensure!(class == RecordClass::Old, "a {class:?} record in an instance");
        }
    }
    // ---- catalog
    let mains = m.catalog.iter().filter(|c| c.kind == InstanceKind::Main).count();
    ensure!(mains <= 1, "{mains} main instances");
    let others = m.catalog.iter().filter(|c| !matches!(c.kind, InstanceKind::Strays | InstanceKind::Embedded)).count();
    ensure!(others == 0 || mains == 1, "instances without a main");
    ensure!(m.catalog.iter().filter(|c| c.kind == InstanceKind::Strays).count() <= 1, "several strays instances");
    for (i, c) in m.catalog.iter().enumerate() {
        ensure!(c.id as usize == i, "catalog ids are dense");
        let nodes = m.entries.iter().any(|e| e.expect.presence == Presence::Shown && e.expect.instance == Some(c.id))
            || (c.kind == InstanceKind::Strays && m.records.iter().any(|r| r.expect.instance == Some(c.id) && !r.names.is_empty()));
        ensure!(nodes, "catalog instance {} produces no node", c.label);
        if let (Some(g), Some((a, b))) = (&c.geometry, c.lost_volume_range) {
            ensure!(a == g.volume_offset && a < b && b <= m.image_size, "lost-volume range of {} is {a}..{b}", c.label);
        }
        if c.protected == Some(true) {
            ensure!(matches!(c.kind, InstanceKind::Main | InstanceKind::Other), "{} is protected", c.label);
            ensure!(c.geometry.as_ref().and_then(|g| g.validated) == Some(true), "{} protected unvalidated", c.label);
        }
        ensure!(c.container.is_some() == (c.kind == InstanceKind::Embedded), "container of {}", c.label);
    }
    // ---- entries
    for e in &m.entries {
        match e.expect.presence {
            Presence::Shown => {
                let c = e.expect.instance.ok_or(format!("shown entry {} has no instance", e.path))?;
                let kind = m.catalog[c as usize].kind;
                let has = |f: Flag| e.expect.flags_set.contains(&f);
                let lacks = |f: Flag| e.expect.flags_clear.contains(&f);
                ensure!(has(Flag::Stray) == (kind == InstanceKind::Strays) && (has(Flag::Stray) || lacks(Flag::Stray)), "stray flag of {}", e.path);
                ensure!(has(Flag::Embedded) == (kind == InstanceKind::Embedded), "embedded flag of {}", e.path);
                ensure!(kind != InstanceKind::Older || has(Flag::OlderGeneration), "{} of an older instance", e.path);
                if kind == InstanceKind::Strays {
                    ensure!(e.expect.path.is_none(), "{}: strays have no stated path", e.path);
                }
                let base = m.records.iter().find(|r| Some(r.offset) == e.record_offset && r.entry == Some(e.id));
                if let Some(base) = base {
                    ensure!(base.expect.instance == Some(c), "base record of {} is in another instance", e.path);
                    ensure!(base.expect.class == Some(RecordClass::Old), "base record of {} is not old", e.path);
                }
                for f in &e.expect.flags_set {
                    ensure!(!e.expect.flags_clear.contains(f), "{} both sets and clears {f:?}", e.path);
                }
            }
            Presence::Hidden | Presence::Gone | Presence::Unspecified => {
                ensure!(e.expect.path.is_none(), "{} has a path but no node", e.path);
            }
        }
    }
    // ---- C5 and D2
    let set = &m.overwritten_set;
    for c in &current {
        for &(s, l) in &c.live_allocated {
            ensure!(set.iter().any(|&(a, b)| a <= s && s + l <= a + b), "live range {s}+{l} not in the overwritten set");
        }
    }
    ensure!(set.windows(2).all(|w| w[0].0 + w[0].1 < w[1].0), "the overwritten set is not merged");
    for f in &m.fragments {
        for cut in &f.cuts {
            ensure!(cut.support >= 2, "a kept cut with support {}", cut.support);
        }
        for c in &f.contradictions {
            ensure!(c.lo < c.hi, "contradiction {}..{}", c.lo, c.hi);
            if let Some(p) = c.explained_by {
                ensure!(c.lo < p && p <= c.hi && f.cuts.iter().any(|k| k.position == p), "explained by {p}");
            }
        }
        ensure!(f.segment_instances.len() == f.cuts.len() + 1, "segments of fragment {}", f.origin);
    }
    let mixed = m.fragments.iter().any(|f| f.contradictions.iter().any(|c| c.explained_by.is_none()));
    if let Some(w) = m.warnings.iter().find(|w| w.code == "mixed_generations") {
        ensure!(w.raised == mixed, "mixed_generations is {} but unexplained contradictions: {mixed}", w.raised);
    }
    Ok(())
}

#[test]
fn every_manifest_is_consistent() {
    every_scenario(0x5EED, |built| invariants(&built.manifest));
}

#[test]
fn every_manifest_survives_json() {
    every_scenario(9, |built| {
        let json = built.manifest.to_json();
        let back = Manifest::from_json(&json).map_err(|e| e.to_string())?;
        ensure!(back.to_json() == json, "JSON round trip changes the manifest");
        ensure!(back.records == built.manifest.records && back.catalog == built.manifest.catalog, "records or catalog differ");
        Ok(())
    });
}

#[test]
fn scenarios_are_deterministic() {
    // the same seed gives the same bytes and the same manifest, on every run
    let names: Vec<&str> = scenarios::catalog().iter().map(|s| s.name).collect();
    let digests = |seed: u64| -> Vec<(String, String)> {
        let out = Mutex::new(Vec::new());
        every_scenario(seed, |built| {
            out.lock().unwrap().push((built.manifest.scenario.clone(), format!("{} {}", built.image.digest(), hds_testkit::hash::sha256_hex(built.manifest.to_json().as_bytes()))));
            Ok(())
        });
        let mut v = out.into_inner().unwrap();
        v.sort();
        v
    };
    let first = digests(77);
    assert_eq!(first.len(), names.len());
    assert_eq!(first, digests(77));
    // another seed changes the content
    let other = digests(78);
    let differ = first.iter().zip(&other).filter(|(a, b)| a.1.split(' ').next() != b.1.split(' ').next()).count();
    assert_eq!(differ, names.len(), "every scenario depends on its seed");
}

/// The time the million-record volume may take; `HDS_TESTKIT_SCALE_SECONDS`
/// overrides it for slow machines.
fn scale_budget() -> Duration {
    let seconds = std::env::var("HDS_TESTKIT_SCALE_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(60);
    Duration::from_secs(seconds)
}

#[test]
fn a_million_records_stream_to_a_sparse_file_in_well_under_a_minute() {
    let dir = TempDir::new("scale");
    let path = dir.path("million.img");
    let started = Instant::now();
    let summary = scale::million_records(&path, 2026).expect("write the scale image");
    let took = started.elapsed();
    println!("{} surviving records, {} bytes written of {} in {took:?}", summary.surviving_records, summary.bytes_written, summary.image_size);
    assert!(took < scale_budget(), "took {took:?}");
    assert!(summary.surviving_records >= 1_000_000, "{}", summary.surviving_records);
    assert!(summary.mft_records >= 1_000_000);
    assert!(summary.lost_folders > 100 && summary.index_blocks > 30_000 && summary.media_files > 50);
    let meta = std::fs::metadata(&path).unwrap();
    assert_eq!(meta.len(), summary.image_size);
    assert!(summary.bytes_written < summary.image_size / 3, "most of the image is never written");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let allocated = meta.blocks() * 512;
        assert!(allocated < summary.image_size / 2, "the file is sparse: {allocated} bytes allocated");
    }
    // spot checks against the layout: records where the summary says, with
    // the number they claim
    let layout = scale::ScaleLayout::new(&scale::ScaleSpec::million(2026));
    let file = std::fs::File::open(&path).unwrap();
    for record in [256u64, 300_000, layout.end_record() - 1] {
        let e = layout.entry(record);
        let mut head = [0u8; 48];
        read_at(&file, e.record_offset, &mut head);
        assert_eq!(&head[..4], b"FILE", "record {record}");
        assert_eq!(u32::from_le_bytes(head[0x2C..0x30].try_into().unwrap()) as u64, record);
    }
}

fn read_at(file: &std::fs::File, offset: u64, buf: &mut [u8]) {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = file;
    f.seek(SeekFrom::Start(offset)).unwrap();
    f.read_exact(buf).unwrap();
}

#[test]
fn the_small_scale_volume_has_the_layout_of_the_large_one() {
    let spec = scale::ScaleSpec::small(5);
    let built = scale::scale_in_memory(&spec);
    let m = &built.manifest;
    invariants(m).unwrap();
    let layout = scale::ScaleLayout::new(&spec);
    let old = m.instance("old").id;
    let shown: BTreeSet<u64> = m.entries.iter().filter(|e| e.instance == old && e.expect.presence == Presence::Shown).map(|e| e.record).collect();
    assert_eq!(shown, (256..layout.end_record()).collect(), "every surviving record is shown");
    let main = m.main_instance().expect("a main instance");
    assert!(main.anchor && main.geometry.as_ref().unwrap().validated == Some(true));
    // files of surviving folders keep their path; areas are lost folders
    let file = m.entries.iter().find(|e| e.instance == old && e.record == layout.end_record() - 1).unwrap();
    let expected = file.expect.path.as_ref().expect("a path");
    assert_eq!(expected.last(), file.components.last());
    assert!(m.records.iter().filter(|r| r.role == RecordRole::Base && r.instance == old).count() > 7000);
}
