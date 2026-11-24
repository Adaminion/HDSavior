//! What a correct catalog shows of every entry: presence (C3, E5, E9), the
//! catalog instance, the health flags that need the whole picture (G1 kind
//! flags, `overwritten`, G2 overlap conflicts) and the expected path (E4 to
//! E8).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::facts::ArtifactState;
use crate::manifest::{
    CatalogInstance, Entry, EntryKind, Expect, Flag, InstanceKind, Presence, RecordClass,
};
use crate::ondisk::attr::FN_DIRECTORY;
use crate::ondisk::FileRef;

use super::catalog::{Catalog, Inputs};
use super::intersects;
use super::records::{self, side, Side};

/// E9 and E5 for a record of class `Old`: hidden by its number, or because
/// it lives below `$Extend`. `None`: the number a scanner needs is missing.
fn omitted(inputs: &Inputs, e: &Entry, base: usize) -> Option<bool> {
    // E5: everything below $Extend (the folder itself is E9's business)
    if e.components.len() > 1 && e.components[0] == "$Extend" {
        return Some(true);
    }
    match inputs.analysis.numbers[base] {
        Some(n) if n < 16 => Some(true),
        Some(n) if n < 24 && e.names.is_empty() => Some(true),
        Some(_) => Some(false),
        // without a record number E9 cannot pick out the system records
        None if e.record < 24 => None,
        None => Some(false),
    }
}

fn presence(inputs: &Inputs, e: &Entry, base: Option<usize>) -> Presence {
    let Some(b) = base else { return Presence::Gone };
    if !e.truth.record_in_range {
        return Presence::Gone;
    }
    match inputs.classes[b] {
        None | Some(RecordClass::Dropped) => Presence::Gone,
        Some(RecordClass::Live) | Some(RecordClass::PostFormat) => Presence::Hidden,
        Some(RecordClass::Old) => match omitted(inputs, e, b) {
            Some(true) => Presence::Hidden,
            Some(false) => Presence::Shown,
            None => Presence::Unspecified,
        },
    }
}

fn flag(x: &mut Expect, f: Flag, value: Option<bool>) {
    match value {
        Some(true) => x.set(f),
        Some(false) => x.clear(f),
        None => x.open(f),
    }
}

/// The G2 rank of an instance kind: lower is stronger.
fn rank(kind: InstanceKind) -> u8 {
    match kind {
        InstanceKind::Main => 0,
        InstanceKind::Other => 1,
        InstanceKind::Older => 2,
        InstanceKind::Strays | InstanceKind::Embedded => 3,
    }
}

/// One file taking part in the overlap sweep of G2.
#[derive(Clone, Copy, Debug)]
struct Party {
    kind: InstanceKind,
    marked: Option<bool>,
    in_use: bool,
    time: u64,
    lsn: u64,
}

/// The two readings of "loser of overlap conflicts" for records D2 marks:
/// they lose every conflict (0), or they rank like files of an `older`
/// instance (1). The key of the stronger file is greater.
fn key(p: &Party, reading: usize) -> (u8, u8, u8, u64, u64) {
    let marked = p.marked == Some(true);
    match reading {
        0 => (
            u8::from(!marked),
            3 - rank(p.kind),
            u8::from(p.in_use),
            p.time,
            p.lsn,
        ),
        _ => {
            let r = if marked {
                rank(InstanceKind::Older).max(rank(p.kind))
            } else {
                rank(p.kind)
            };
            (1, 3 - r, u8::from(p.in_use), p.time, p.lsn)
        }
    }
}

/// G2 outcome for one file: `(lost, overlap)` per reading, or `None` when a
/// party of one of its conflicts is not known well enough.
type Outcome = Option<[(bool, bool); 2]>;

fn overlap_conflicts(inputs: &Inputs, catalog: &Catalog, out: &[Expect]) -> Vec<Outcome> {
    let m = inputs.m;
    let mut parties: Vec<Option<Party>> = vec![None; m.entries.len()];
    let mut segments: Vec<(u64, u64, usize)> = Vec::new();
    for (k, e) in m.entries.iter().enumerate() {
        if out[k].presence != Presence::Shown || e.kind != EntryKind::File {
            continue;
        }
        let kind = out[k].instance.map(|c| catalog.instances[c as usize].kind);
        if kind == Some(InstanceKind::Embedded) {
            continue;
        }
        let base = inputs.base_record[k].expect("shown entries have a base record");
        parties[k] = kind.map(|kind| Party {
            kind,
            marked: inputs.analysis.marked(base),
            in_use: !e.deleted,
            // G2: the record time of the base record
            time: records::record_time(&m.records[base]).unwrap_or(0),
            lsn: e.lsn,
        });
        for x in &e.extents {
            if let Some(offset) = x.offset {
                segments.push((offset, offset + x.bytes, k));
            }
        }
    }
    segments.sort_unstable();
    let mut pairs: BTreeSet<(usize, usize)> = BTreeSet::new();
    let mut active: Vec<(u64, usize)> = Vec::new();
    for &(start, end, k) in &segments {
        active.retain(|&(e, _)| e > start);
        for &(_, j) in &active {
            if j != k {
                pairs.insert((j.min(k), j.max(k)));
            }
        }
        active.push((end, k));
    }
    let mut outcome: Vec<Outcome> = (0..m.entries.len())
        .map(|_| Some([(false, false); 2]))
        .collect();
    for &(a, b) in &pairs {
        let (Some(pa), Some(pb)) = (parties[a], parties[b]) else {
            outcome[a] = None;
            outcome[b] = None;
            continue;
        };
        if pa.marked.is_none() || pb.marked.is_none() {
            outcome[a] = None;
            outcome[b] = None;
            continue;
        }
        for reading in 0..2 {
            let both_overlap = pa.in_use && pb.in_use && pa.kind == pb.kind;
            match key(&pa, reading).cmp(&key(&pb, reading)) {
                std::cmp::Ordering::Equal => {
                    for k in [a, b] {
                        if let Some(o) = outcome[k].as_mut() {
                            o[reading].1 = true;
                        }
                    }
                }
                order => {
                    let (winner, loser) = if order == std::cmp::Ordering::Greater {
                        (a, b)
                    } else {
                        (b, a)
                    };
                    if let Some(o) = outcome[loser].as_mut() {
                        o[reading].0 = true;
                    }
                    if both_overlap {
                        if let Some(o) = outcome[winner].as_mut() {
                            o[reading].1 = true;
                        }
                    }
                }
            }
        }
    }
    outcome
}

/// ALGORITHMS E4: among the names of the best namespace (Win32 or
/// Win32+DOS, then POSIX, then DOS) one whose parent passes `survives`,
/// then the lowest attribute id.
pub(crate) fn best_name(
    names: &[NameChoice],
    survives: impl Fn(FileRef) -> bool,
) -> Option<(String, FileRef)> {
    let rank = |namespace: u8| match namespace {
        1 | 3 => 0,
        0 => 1,
        _ => 2,
    };
    let best = names.iter().map(|n| rank(n.0)).min()?;
    let chosen = names
        .iter()
        .filter(|n| rank(n.0) == best)
        .min_by_key(|n| (!survives(n.2), n.3))?;
    Some((chosen.1.clone(), chosen.2))
}

/// A `$FILE_NAME` to choose from: namespace, name, parent, attribute id.
pub(crate) type NameChoice = (u8, String, FileRef, u16);

/// One step of an expected path, from the entry upwards.
enum Step {
    Name(String),
    Lost(u64, u16),
}

/// An index entry that may name a lost folder: source LSN, namespace, name,
/// parent reference.
type Offer = (u64, u8, String, FileRef);

/// E4 to E8 for every shown entry of an instance that is not `strays`.
fn paths(inputs: &Inputs, catalog: &Catalog, out: &mut [Expect]) {
    let m = inputs.m;
    let instances: &[CatalogInstance] = &catalog.instances;
    let is_tree = |c: u32| instances[c as usize].kind != InstanceKind::Strays;
    type Key = (u32, u64, u16);
    // folders by the reference their children use
    let mut folders: HashMap<Key, usize> = HashMap::new();
    let mut uncertain_folders: BTreeSet<usize> = BTreeSet::new();
    for (k, e) in m.entries.iter().enumerate() {
        let Some(c) = out[k].instance.filter(|&c| is_tree(c)) else {
            continue;
        };
        if out[k].presence != Presence::Shown || e.kind != EntryKind::Dir {
            continue;
        }
        let base = inputs.base_record[k].expect("shown entries have a base record");
        let Some(number) = inputs.analysis.numbers[base] else {
            continue;
        };
        match inputs.analysis.marked(base) {
            Some(true) => continue, // E5: never a record marked by D2
            None => {
                uncertain_folders.insert(k);
            }
            Some(false) => {}
        }
        folders.insert((c, number, e.sequence), k);
    }
    // record numbers present in any instance
    let present: BTreeSet<u64> = (0..m.records.len())
        .filter(|&i| catalog.of_record[i].is_some())
        .filter_map(|i| inputs.analysis.numbers[i])
        .collect();
    // per instance: in-use non-directory files' extents, and the kept cuts
    // that concern it with the side it is on
    let mut files_of: BTreeMap<u32, Vec<(u64, u64)>> = BTreeMap::new();
    for (k, e) in m.entries.iter().enumerate() {
        if let (Some(c), Presence::Shown, EntryKind::File, false) =
            (out[k].instance, out[k].presence, e.kind, e.deleted)
        {
            files_of.entry(c).or_default().extend(
                e.extents
                    .iter()
                    .filter_map(|x| x.offset.map(|o| (o, x.bytes))),
            );
        }
    }
    let files_of: BTreeMap<u32, Vec<(u64, u64)>> = files_of
        .into_iter()
        .map(|(c, r)| (c, crate::disk::merge_ranges(r)))
        .collect();
    let mut cuts_of: BTreeMap<u32, Vec<(crate::manifest::CutFact, bool)>> = BTreeMap::new();
    for (&(f, s), &c) in &catalog.of_segment {
        for (j, cut) in inputs.analysis.fragments[f].cuts.iter().enumerate() {
            // segment s lies below cut j when s <= j
            cuts_of.entry(c).or_default().push((cut.clone(), s <= j));
        }
    }
    let on_side = |c: u32, t: u64| {
        cuts_of.get(&c).is_none_or(|cuts| {
            cuts.iter().all(|(cut, below)| {
                let s = side(cut, Some((t, t)));
                if *below {
                    s == Side::After
                } else {
                    s == Side::Before
                }
            })
        })
    };
    let inside_file =
        |c: u32, offset: u64| files_of.get(&c).is_some_and(|r| intersects(r, offset, 1));
    let format_min = inputs.current.iter().map(|c| c.format_time).min();
    let current_data = |t: u64| format_min.is_some_and(|f| t >= f);
    let live: Vec<(u64, u64)> = crate::disk::merge_ranges(
        inputs
            .current
            .iter()
            .flat_map(|c| {
                m.instances[c.instance as usize]
                    .live_allocated
                    .iter()
                    .copied()
            })
            .collect(),
    );
    let trees: Vec<u32> = (0..instances.len() as u32)
        .filter(|&c| is_tree(c))
        .collect();
    let (range_start, range_end) = m.scan_range;

    let mut offers: HashMap<Key, Vec<Offer>> = HashMap::new();
    let mut offer = |c: u32, lsn: u64, e: &crate::facts::IndexEntryFact, source: u64| {
        let t = records::entry_time(&e.key.times);
        if e.key.flags & FN_DIRECTORY == 0
            || current_data(t)
            || !on_side(c, t)
            || inside_file(c, source)
        {
            return;
        }
        offers
            .entry((c, e.child.number, e.child.seq))
            .or_default()
            .push((lsn, e.key.namespace, e.key.name.clone(), e.key.parent));
    };
    for b in m
        .index_blocks
        .iter()
        .filter(|b| b.state == ArtifactState::Intact)
    {
        if b.offset < range_start
            || b.offset + b.size as u64 > range_end
            || intersects(&live, b.offset, b.size as u64)
        {
            continue;
        }
        let owners: Vec<u32> = trees
            .iter()
            .copied()
            .filter(|&c| folders.contains_key(&(c, b.owner.number, b.owner.seq)))
            .collect();
        let users: Vec<u32> = if !owners.is_empty() {
            owners
        } else if !present.contains(&b.owner.number) {
            trees.clone()
        } else {
            Vec::new()
        };
        for &c in &users {
            for e in &b.entries {
                offer(c, b.lsn, e, b.offset);
            }
        }
    }
    for (i, r) in m.records.iter().enumerate() {
        let Some(c) = catalog.of_record[i].filter(|&c| is_tree(c)) else {
            continue;
        };
        if r.state != ArtifactState::Intact || r.offset < range_start || r.offset >= range_end {
            continue;
        }
        for e in r.index_root.iter().flatten() {
            offer(c, r.lsn, e, r.offset);
        }
    }
    // E6 (b): in-use directory records among the strays
    let mut copies: HashMap<(u64, u16), Option<(String, FileRef)>> = HashMap::new();
    for (i, r) in m.records.iter().enumerate() {
        let Some(c) = catalog.of_record[i] else {
            continue;
        };
        if is_tree(c) || r.flags & 3 != 3 {
            continue;
        }
        let Some(number) = inputs.analysis.numbers[i] else {
            continue;
        };
        let names: Vec<NameChoice> = r
            .names
            .iter()
            .map(|n| (n.namespace, n.name.clone(), n.parent, n.attr_id))
            .collect();
        let Some(name) = best_name(&names, |_| true) else {
            continue;
        };
        copies
            .entry((number, r.sequence))
            .and_modify(|v| {
                if v.as_ref() != Some(&name) {
                    *v = None;
                }
            })
            .or_insert(Some(name));
    }

    let survives = |c: u32, parent: FileRef| {
        parent.number == 5 || folders.contains_key(&(c, parent.number, parent.seq))
    };
    let own_name = |k: usize, c: u32| -> Option<(String, FileRef)> {
        let names: Vec<NameChoice> = m.entries[k]
            .names
            .iter()
            .map(|n| (n.namespace, n.name.clone(), n.parent, n.attr_id))
            .collect();
        best_name(&names, |parent| survives(c, parent))
    };
    let lost_name = |key: Key| -> Result<Option<(String, FileRef)>, ()> {
        if let Some(list) = offers.get(&key) {
            let non_dos = list.iter().any(|o| o.1 != 2);
            let usable: Vec<&Offer> = list.iter().filter(|o| !non_dos || o.1 != 2).collect();
            let top = usable.iter().map(|o| o.0).max().expect("not empty");
            let mut best: Vec<(&String, FileRef)> = usable
                .iter()
                .filter(|o| o.0 == top)
                .map(|o| (&o.2, o.3))
                .collect();
            best.sort();
            best.dedup();
            return if best.len() == 1 {
                Ok(Some((best[0].0.clone(), best[0].1)))
            } else {
                Err(())
            };
        }
        match copies.get(&(key.1, key.2)) {
            Some(Some(v)) => Ok(Some(v.clone())),
            Some(None) => Err(()),
            None => Ok(None),
        }
    };

    // chains from every shown entry to the root
    let mut chains: Vec<Option<Vec<Step>>> = Vec::with_capacity(m.entries.len());
    // lost folders that exist in each instance: (instance, number) -> (all
    // sequences, sequences of nameless ones)
    type Seqs = (BTreeSet<u16>, BTreeSet<u16>);
    let mut lost: BTreeMap<(u32, u64), Seqs> = BTreeMap::new();
    for (k, _) in m.entries.iter().enumerate() {
        let Some(c) = out[k].instance.filter(|&c| is_tree(c)) else {
            chains.push(None);
            continue;
        };
        if out[k].presence != Presence::Shown {
            chains.push(None);
            continue;
        }
        let chain = (|| {
            if uncertain_folders.contains(&k) {
                return None;
            }
            let (name, mut parent) = own_name(k, c)?;
            let mut steps = vec![Step::Name(name)];
            for _ in 0..512 {
                if parent.number == 5 {
                    return Some(steps);
                }
                if let Some(&folder) = folders.get(&(c, parent.number, parent.seq)) {
                    if uncertain_folders.contains(&folder) {
                        return None;
                    }
                    let (name, next) = own_name(folder, c)?;
                    steps.push(Step::Name(name));
                    parent = next;
                    continue;
                }
                let seqs = lost.entry((c, parent.number)).or_default();
                seqs.0.insert(parent.seq);
                match lost_name((c, parent.number, parent.seq)).ok()? {
                    Some((name, next)) => {
                        steps.push(Step::Name(name));
                        parent = next;
                    }
                    None => {
                        seqs.1.insert(parent.seq);
                        steps.push(Step::Lost(parent.number, parent.seq));
                        return Some(steps);
                    }
                }
            }
            None // E8: a cycle; the rules cut it where it is detected
        })();
        chains.push(chain);
    }
    for (k, chain) in chains.into_iter().enumerate() {
        let Some(c) = out[k].instance else { continue };
        if out[k].presence != Presence::Shown || !is_tree(c) {
            continue;
        }
        out[k].path = chain.and_then(|steps| {
            let mut path: Vec<String> = Vec::with_capacity(steps.len() + 1);
            for step in steps.into_iter().rev() {
                match step {
                    Step::Name(name) => path.push(name),
                    Step::Lost(number, seq) => {
                        let (all, nameless) = &lost[&(c, number)];
                        if all.len() > 1 && nameless.len() == 1 {
                            // "several sequences share a number": counted
                            // with or without the named ones?
                            return None;
                        }
                        path.push("[Lost folders]".to_string());
                        path.push(if nameless.len() > 1 {
                            format!("Folder #{number}.{seq}")
                        } else {
                            format!("Folder #{number}")
                        });
                    }
                }
            }
            Some(path)
        });
    }
}

/// Every entry's expectation, starting from what the entry alone tells
/// (`Entry::expect` as the builder set it).
pub(crate) fn expectations(inputs: &Inputs, catalog: &Catalog) -> Vec<Expect> {
    let m = inputs.m;
    let mut out: Vec<Expect> = m.entries.iter().map(|e| e.expect.clone()).collect();
    for (k, e) in m.entries.iter().enumerate() {
        let x = &mut out[k];
        let base = inputs.base_record[k];
        x.presence = presence(inputs, e, base);
        x.path = None;
        if x.presence != Presence::Shown {
            x.instance = None;
            x.flags_set.clear();
            x.flags_clear.clear();
            continue;
        }
        let b = base.expect("shown entries have a base record");
        x.instance = catalog.of_record[b];
        let kind = x.instance.map(|c| catalog.instances[c as usize].kind);
        let marked = inputs.analysis.marked(b);
        match kind {
            Some(kind) => {
                let older = kind == InstanceKind::Older || marked == Some(true);
                flag(
                    x,
                    Flag::OlderGeneration,
                    if older {
                        Some(true)
                    } else {
                        marked.map(|_| false)
                    },
                );
                flag(x, Flag::Embedded, Some(kind == InstanceKind::Embedded));
                flag(x, Flag::Stray, Some(kind == InstanceKind::Strays));
            }
            None => {
                for f in [Flag::OlderGeneration, Flag::Embedded, Flag::Stray] {
                    x.open(f);
                }
            }
        }
    }
    let outcome = overlap_conflicts(inputs, catalog, &out);
    for (k, e) in m.entries.iter().enumerate() {
        let x = &mut out[k];
        if x.presence != Presence::Shown || e.kind != EntryKind::File {
            continue;
        }
        let written = e.extents.iter().any(|ext| {
            ext.offset
                .is_some_and(|o| intersects(inputs.overwritten, o, ext.bytes))
        });
        let stale = catalog.stale_containers.contains(&e.id);
        let (lost, overlap) = match outcome[k] {
            Some([a, b]) => ((a.0 == b.0).then_some(a.0), (a.1 == b.1).then_some(a.1)),
            None => (None, None),
        };
        flag(
            x,
            Flag::Overwritten,
            if written || stale { Some(true) } else { lost },
        );
        flag(x, Flag::Overlap, overlap);
    }
    paths(inputs, catalog, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_choice_follows_namespace_then_parent_then_attribute_id() {
        let name = |namespace: u8, name: &str, parent: u64, id: u16| {
            (namespace, name.to_string(), FileRef::new(parent, 1), id)
        };
        let pick = |names: &[NameChoice], alive: &[u64]| {
            best_name(names, |parent| alive.contains(&parent.number)).map(|(name, _)| name)
        };
        // Win32 beats POSIX beats DOS, wherever they stand
        let mixed = [
            name(2, "DOS~1", 9, 1),
            name(0, "posix", 9, 2),
            name(1, "win32", 9, 3),
        ];
        assert_eq!(pick(&mixed, &[9]).as_deref(), Some("win32"));
        assert_eq!(pick(&mixed[..2], &[9]).as_deref(), Some("posix"));
        assert_eq!(pick(&mixed[..1], &[9]).as_deref(), Some("DOS~1"));
        // Win32+DOS is as good as Win32: the lower id decides
        let both = [name(1, "long", 9, 5), name(3, "SHORT", 9, 4)];
        assert_eq!(pick(&both, &[9]).as_deref(), Some("SHORT"));
        // a surviving parent beats a lower id; among survivors the lower id wins
        let links = [
            name(1, "first stored", 7, 6),
            name(1, "second stored", 8, 3),
            name(1, "third stored", 9, 2),
        ];
        assert_eq!(pick(&links, &[7]).as_deref(), Some("first stored"));
        assert_eq!(pick(&links, &[7, 8]).as_deref(), Some("second stored"));
        assert_eq!(pick(&links, &[7, 8, 9]).as_deref(), Some("third stored"));
        assert_eq!(
            pick(&links, &[]).as_deref(),
            Some("third stored"),
            "no parent survives: the lowest id"
        );
        // a better namespace is preferred even when only the worse one has a parent
        let split = [name(0, "posix here", 7, 1), name(1, "win32 there", 8, 2)];
        assert_eq!(pick(&split, &[7]).as_deref(), Some("win32 there"));
        assert_eq!(pick(&[], &[7]), None);
    }
}
