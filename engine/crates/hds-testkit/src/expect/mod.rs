//! Expectations: what a correct scan and catalog make of an image
//! (ALGORITHMS A3, C to G), worked out from the structures the manifest
//! lists and the truth the builder knows.
//!
//! The rules are applied to the records, index blocks and boot sectors that
//! are physically in the image, the way a scanner finds them. Where a rule
//! depends on something the manifest cannot predict (a sample, a tie that
//! the text does not break, readings of the text that disagree), the
//! expectation is left open instead of guessed: a flag in neither list, a
//! `None` path, instance or verdict.
//!
//! Written from `docs/ALGORITHMS.md` alone; nothing here comes from the
//! engine crates.

mod catalog;
mod entries;
mod records;

use std::collections::{BTreeMap, HashMap};

use crate::image::Image;
use crate::manifest::{
    FragmentFact, InstanceKind, InstanceRole, Manifest, RecordClass, WarningExpect,
};

/// The time the manifest assumes the scan takes place at: 2026-11-24
/// 00:00 UTC as FILETIME. ALGORITHMS D2 calls a change time "timed" when it
/// is not more than a day after the scan; scenario times are years before
/// this and the edge values of `timestamp_edges` decades after it, so the
/// expectations hold for any scan between 2026 and 2037.
pub const ASSUMED_SCAN_TIME: u64 = 134_399_520_000_000_000;

/// `true` when `[start, start + len)` meets one of the sorted, merged
/// `(start, length)` ranges.
pub(crate) fn intersects(sorted: &[(u64, u64)], start: u64, len: u64) -> bool {
    let at = sorted.partition_point(|&(s, l)| s + l <= start);
    sorted.get(at).is_some_and(|&(s, _)| s < start + len)
}

/// Whether bytes that start a file show the signature its extension
/// promises, for the formats the scenarios store: what
/// `hds_carve::matches_extension` answers for them (PDF: `%PDF-` within the
/// first kilobyte).
pub(crate) fn signature_matches(head: &[u8], ext: &str) -> bool {
    match ext {
        "jpg" | "jpeg" => head.starts_with(&[0xFF, 0xD8, 0xFF]),
        "png" => head.starts_with(b"\x89PNG\r\n\x1a\n"),
        "gif" => head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a"),
        "bmp" => head.starts_with(b"BM"),
        "pdf" => head[..head.len().min(1024)]
            .windows(5)
            .any(|w| w == b"%PDF-"),
        "zip" | "docx" | "xlsx" | "pptx" => {
            head.len() >= 4
                && head[0] == b'P'
                && head[1] == b'K'
                && matches!((head[2], head[3]), (3, 4) | (5, 6) | (7, 8))
        }
        "wav" => {
            head.len() >= 12
                && matches!(&head[..4], b"RIFF" | b"RF64" | b"BW64" | b"RIFX")
                && &head[8..12] == b"WAVE"
        }
        _ => false,
    }
}

/// `true` when an old record of the strays would be a node there: a base
/// record with a name that E9 and E5 do not omit.
fn stray_node(m: &Manifest, analysis: &records::Analysis, i: usize) -> bool {
    let r = &m.records[i];
    r.base == crate::ondisk::FileRef::NONE
        && !r.names.is_empty()
        && analysis.numbers[i].is_some_and(|n| n >= 24)
        && r.names.iter().all(|n| n.parent.number != 11)
}

/// Works out every expectation of a finished manifest. `kind_override`
/// holds the instance kinds a scenario states for physical instances; they
/// settle what the rules leave open and must agree with what the rules
/// decide. Returns what remains undecided (the scenario must say).
pub(crate) fn derive(
    m: &mut Manifest,
    image: &Image,
    kind_override: &BTreeMap<u32, InstanceKind>,
) -> Vec<String> {
    let current = records::current_mfts(m);
    let classes = records::classify(m, &current);
    let overwritten = records::overwritten_set(m, &current, &classes);
    let analysis = records::analyse(m, &classes);
    let record_entry: Vec<Option<u32>> = m.records.iter().map(|r| r.entry).collect();
    // the records an entry consists of: its base and extension records
    // where its file system wrote them (copies elsewhere are not part of it)
    let mut entry_records: Vec<Vec<usize>> = vec![Vec::new(); m.entries.len()];
    for (i, e) in record_entry.iter().enumerate() {
        let own = matches!(
            m.records[i].role,
            crate::facts::RecordRole::Base | crate::facts::RecordRole::Extension
        );
        if let (Some(e), true) = (e, own) {
            entry_records[*e as usize].push(i);
        }
    }
    let by_offset: HashMap<u64, usize> = m
        .records
        .iter()
        .enumerate()
        .map(|(i, r)| (r.offset, i))
        .collect();
    let base_record: Vec<Option<usize>> = m
        .entries
        .iter()
        .map(|e| {
            let i = *by_offset.get(&e.record_offset?)?;
            (record_entry[i] == Some(e.id)).then_some(i)
        })
        .collect();
    let inputs = catalog::Inputs {
        m,
        image,
        current: &current,
        classes: &classes,
        analysis: &analysis,
        overwritten: &overwritten,
        base_record: &base_record,
        entry_records: &entry_records,
    };
    let mut catalog = catalog::assemble(&inputs, kind_override);
    let mut expects = entries::expectations(&inputs, &catalog);
    // D9: instances that produce no catalog node are not reported. Nodes
    // come from shown entries, and in the strays also from base records that
    // repeat an entry's record (stale copies).
    let shown: Vec<bool> = (0..catalog.instances.len() as u32)
        .map(|c| {
            expects
                .iter()
                .any(|x| x.presence == crate::manifest::Presence::Shown && x.instance == Some(c))
                || (catalog.instances[c as usize].kind == InstanceKind::Strays
                    && (0..m.records.len())
                        .any(|i| catalog.of_record[i] == Some(c) && stray_node(m, &analysis, i)))
        })
        .collect();
    let mut remap: Vec<Option<u32>> = vec![None; catalog.instances.len()];
    let mut kept = Vec::new();
    for (c, mut inst) in std::mem::take(&mut catalog.instances)
        .into_iter()
        .enumerate()
    {
        if shown[c] {
            remap[c] = Some(kept.len() as u32);
            inst.id = kept.len() as u32;
            kept.push(inst);
        }
    }
    catalog.instances = kept;
    let renumber = |c: Option<u32>| c.and_then(|c| remap[c as usize]);
    for c in &mut catalog.of_record {
        *c = renumber(*c);
    }
    catalog.of_segment = catalog
        .of_segment
        .iter()
        .filter_map(|(&k, &c)| Some((k, remap[c as usize]?)))
        .collect();
    for x in &mut expects {
        x.instance = renumber(x.instance);
    }
    let marked: Vec<Option<bool>> = (0..m.records.len())
        .map(|i| {
            (classes[i] == Some(RecordClass::Old))
                .then(|| analysis.marked(i))
                .flatten()
        })
        .collect();
    let mixed = analysis
        .fragments
        .iter()
        .any(|f| !f.uncertain && !f.marked.is_empty());
    let uncertain = analysis.fragments.iter().any(|f| f.uncertain);
    let fragments: Vec<FragmentFact> = analysis
        .fragments
        .iter()
        .enumerate()
        .map(|(f, frag)| FragmentFact {
            record_size: frag.record_size as u32,
            origin: frag.origin,
            records: frag.members.len() as u32,
            numbers: (
                *frag.members.keys().next().expect("a fragment has members"),
                *frag.members.keys().last().expect("a fragment has members"),
            ),
            contradictions: frag.contradictions.clone(),
            cuts: frag.cuts.clone(),
            segment_instances: (0..frag.segments())
                .map(|s| catalog.of_segment.get(&(f, s)).copied())
                .collect(),
        })
        .collect();
    let undecided = catalog.undecided.clone();

    // ---- write back
    for (i, r) in m.records.iter_mut().enumerate() {
        r.expect.class = classes[i];
        r.expect.instance = catalog.of_record[i];
        r.expect.older_generation = marked[i];
    }
    for (e, x) in m.entries.iter_mut().zip(expects) {
        e.expect = x;
    }
    for inst in &mut m.instances {
        inst.expected_kind = if inst.role == InstanceRole::Current {
            None
        } else {
            let mut kinds = m
                .entries
                .iter()
                .filter(|e| {
                    e.instance == inst.id && e.expect.presence == crate::manifest::Presence::Shown
                })
                .map(|e| {
                    e.expect
                        .instance
                        .map(|c| catalog.instances[c as usize].kind)
                });
            match kinds.next() {
                Some(Some(first)) if kinds.all(|k| k == Some(first)) => Some(first),
                _ => None,
            }
        };
    }
    m.overwritten_set = overwritten;
    m.catalog = catalog.instances;
    m.fragments = fragments;
    m.warnings = if mixed || !uncertain {
        vec![WarningExpect {
            code: "mixed_generations".into(),
            raised: mixed,
        }]
    } else {
        Vec::new()
    };
    undecided
}
