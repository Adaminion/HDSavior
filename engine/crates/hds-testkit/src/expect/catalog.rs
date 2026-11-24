//! The instances a correct catalog builds (ALGORITHMS D4 to D9) and the
//! geometry each of them must find (F2 to F6).
//!
//! Joining (D4) is decided by evidence an engine computes from references,
//! contents and geometries. The builder knows which file system wrote every
//! record, and its scenarios are made so that the evidence of D4 holds
//! exactly between the pieces of one file system; so pieces join here when
//! they come from the same file system and none of D4's prohibitions
//! applies (intersecting record numbers, a kept cut between them, slots the
//! format destroyed). The scenario tests check the evidence itself.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::facts::ArtifactState;
use crate::image::Image;
use crate::manifest::{
    CatalogInstance, Entry, EntryKind, ExpectedGeometry, InstanceKind, InstanceRole, Manifest, RecordClass,
};
use crate::time;

use super::records::{self, side, Analysis, CurrentMft, Side};
use super::{intersects, signature_matches};

/// Fewer old records than this make an instance a stray (D6).
const MIN_RECORDS: usize = 16;

/// What the catalog assembly decided.
#[derive(Clone, Debug, Default)]
pub(crate) struct Catalog {
    pub instances: Vec<CatalogInstance>,
    /// Per record: its catalog instance (old records only).
    pub of_record: Vec<Option<u32>>,
    /// Per `(fragment, segment)`: its catalog instance.
    pub of_segment: HashMap<(usize, usize), u32>,
    /// Entries of container files that D7 calls stale (`overwritten`).
    pub stale_containers: BTreeSet<u32>,
    /// Physical instances whose kind the rules leave undecided here; the
    /// scenario has to state it.
    pub undecided: Vec<String>,
}

/// A set of old records on its way to becoming a catalog instance.
#[derive(Clone, Debug, Default)]
struct Union {
    records: Vec<usize>,
    numbers: BTreeSet<u64>,
    segments: BTreeSet<(usize, usize)>,
    physical: BTreeSet<u32>,
    /// Oldest and newest plausible change time.
    span: Option<(u64, u64)>,
    /// Lowest number among its records at the origin of a current `$MFT`.
    at_current_origin: Option<u64>,
    /// Unnumbered records on no grid: strays whatever happens.
    stray: bool,
}

impl Union {
    fn absorb(&mut self, other: Union) {
        self.records.extend(other.records);
        self.numbers.extend(other.numbers);
        self.segments.extend(other.segments);
        self.physical.extend(other.physical);
        self.span = match (self.span, other.span) {
            (Some(a), Some(b)) => Some((a.0.min(b.0), a.1.max(b.1))),
            (a, b) => a.or(b),
        };
        self.at_current_origin = match (self.at_current_origin, other.at_current_origin) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self.stray |= other.stray;
    }
}

/// Inputs every step needs.
pub(crate) struct Inputs<'a> {
    pub m: &'a Manifest,
    pub image: &'a Image,
    pub current: &'a [CurrentMft],
    pub classes: &'a [Option<RecordClass>],
    pub analysis: &'a Analysis,
    pub overwritten: &'a [(u64, u64)],
    /// Per entry: the index of its base record, if it is in the image.
    pub base_record: &'a [Option<usize>],
    /// Per entry: the indexes of all its records in the image.
    pub entry_records: &'a [Vec<usize>],
}

impl Inputs<'_> {
    fn number(&self, record: usize) -> Option<u64> {
        self.analysis.numbers[record]
    }
}

fn union_of(inputs: &Inputs, records: Vec<usize>, segments: BTreeSet<(usize, usize)>, stray: bool) -> Union {
    let m = inputs.m;
    let current_origins: BTreeSet<(u64, i64)> = inputs.current.iter().flat_map(|c| c.origins()).collect();
    let mut u = Union { segments, stray, ..Union::default() };
    for &i in &records {
        let r = &m.records[i];
        if let Some(n) = inputs.number(i) {
            u.numbers.insert(n);
        }
        u.physical.insert(r.instance);
        if let Some(t) = records::timed(r) {
            u.span = Some(u.span.map_or((t, t), |(a, b)| (a.min(t), b.max(t))));
        }
        if let (Some(f), Some(n)) = (inputs.analysis.fragment_of[i], inputs.number(i)) {
            let frag = &inputs.analysis.fragments[f];
            if current_origins.contains(&(frag.record_size, frag.origin)) {
                u.at_current_origin = Some(u.at_current_origin.map_or(n, |a| a.min(n)));
            }
        }
    }
    u.records = records;
    u
}

/// D4 (c): `holder` has a record 0 named `$MFT` whose runs place at least
/// 16 records of `other` where they were found, with one volume offset.
fn mft_map_count(inputs: &Inputs, holder: &[usize], other: &[usize], cluster_size: u64) -> (u32, Option<u64>) {
    let m = inputs.m;
    let Some(&zero) = holder.iter().find(|&&i| {
        inputs.number(i) == Some(0) && m.records[i].names.iter().any(|n| n.name == "$MFT")
    }) else {
        return (0, None);
    };
    // the $MFT's $DATA extents: record 0 and its extension records
    let mut extents: Vec<(i64, Vec<crate::ondisk::runlist::Run>)> = Vec::new();
    for &i in holder {
        let r = &m.records[i];
        let belongs = i == zero || (r.base.number == 0 && r.base.seq == m.records[zero].sequence);
        if !belongs {
            continue;
        }
        for s in r.data.iter().filter(|s| s.name.is_empty()) {
            if let Some(x) = &s.non_resident {
                extents.push((x.lowest_vcn, x.runs.clone()));
            }
        }
    }
    extents.sort_by_key(|e| e.0);
    let runs: Vec<crate::ondisk::runlist::Run> = extents.into_iter().flat_map(|e| e.1).collect();
    let map = |byte: u64| -> Option<u64> {
        let mut at = 0u64;
        for run in &runs {
            let len = run.len * cluster_size;
            if byte < at + len {
                return run.lcn.map(|lcn| lcn * cluster_size + (byte - at));
            }
            at += len;
        }
        None
    };
    let mut votes: BTreeMap<i64, u32> = BTreeMap::new();
    for &i in other {
        if i == zero {
            continue;
        }
        let r = &m.records[i];
        let Some(n) = inputs.number(i) else { continue };
        if let Some(b) = map(n * r.size as u64) {
            *votes.entry(r.offset as i64 - b as i64).or_default() += 1;
        }
    }
    votes
        .into_iter()
        .max_by_key(|&(offset, count)| (count, std::cmp::Reverse(offset)))
        .filter(|&(offset, _)| offset >= 0)
        .map_or((0, None), |(offset, count)| (count, Some(offset as u64)))
}

/// D4's prohibitions between two unions that come from the same file system.
fn forbidden(inputs: &Inputs, a: &Union, b: &Union) -> bool {
    if a.stray || b.stray || !a.numbers.is_disjoint(&b.numbers) {
        return true;
    }
    let fragments = &inputs.analysis.fragments;
    for (x, y) in [(a, b), (b, a)] {
        for &(f, s) in &x.segments {
            for (j, cut) in fragments[f].cuts.iter().enumerate() {
                // cut j separates segment j (below) and j + 1 (above)
                if s == j && side(cut, y.span) != Side::After {
                    return true;
                }
                if s == j + 1 && side(cut, y.span) != Side::Before {
                    return true;
                }
            }
            for &(g, t) in &y.segments {
                if g == f && t != s {
                    return true;
                }
            }
        }
        // slots the format destroyed, unless an MFT map ties them together
        if let (Some(lowest), Some(&least)) = (x.at_current_origin, y.numbers.iter().next()) {
            if least < lowest {
                let cs = inputs.m.instances[*x.physical.iter().next().expect("not empty") as usize].geometry.cluster_size;
                let (forward, _) = mft_map_count(inputs, &y.records, &x.records, cs as u64);
                let (backward, _) = mft_map_count(inputs, &x.records, &y.records, cs as u64);
                if forward < 16 && backward < 16 {
                    return true;
                }
            }
        }
    }
    false
}

/// The extensions on which `hds_carve::matches_extension` has an opinion,
/// as far as the scenarios of this crate use them.
const OPINION: [&str; 18] = [
    "jpg", "jpeg", "png", "gif", "bmp", "tif", "tiff", "pdf", "zip", "docx", "xlsx", "pptx", "mp4", "mov",
    "wav", "avi", "mkv", "mp3",
];

fn extension(name: &str) -> Option<String> {
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty()).then(|| ext.to_ascii_lowercase())
}

/// Evidence for the true geometry of a union (F2, F3, F6).
#[derive(Clone, Debug, Default)]
struct Evidence {
    cluster_size: u32,
    volume_offset: u64,
    voting_directories: u32,
    mft_map: u32,
    checkable: u32,
    matches: u32,
    boot_agrees: bool,
    validated: Option<bool>,
}

impl Evidence {
    fn own(&self) -> bool {
        self.voting_directories > 0 || self.mft_map > 0 || self.boot_agrees || self.matches >= 3
    }
}

/// The entries whose base record lies in a union.
fn entries_in(inputs: &Inputs, u: &Union) -> Vec<u32> {
    let members: BTreeSet<usize> = u.records.iter().copied().collect();
    (0..inputs.m.entries.len() as u32)
        .filter(|&e| inputs.base_record[e as usize].is_some_and(|b| members.contains(&b)))
        .collect()
}

/// Every record of an entry is intact and none has a torn block.
pub(crate) fn group_sound(inputs: &Inputs, e: &Entry) -> bool {
    e.truth.record_intact
        && inputs.entry_records[e.id as usize]
            .iter()
            .all(|&i| inputs.m.records[i].torn_blocks.is_empty() && inputs.m.records[i].state == ArtifactState::Intact)
}

fn evidence(inputs: &Inputs, u: &Union, cluster_size: u32, volume_offset: u64, live: &[(u64, u64)]) -> Evidence {
    let m = inputs.m;
    let range = m.scan_range;
    let cs = cluster_size as u64;
    let mut ev = Evidence { cluster_size, volume_offset, ..Evidence::default() };
    let in_union: BTreeSet<usize> = u.records.iter().copied().collect();
    let entries = entries_in(inputs, u);
    // F2 index votes, per distinct directory
    for &e in &entries {
        let entry = &m.entries[e as usize];
        let base = inputs.base_record[e as usize].expect("in the union");
        if entry.kind != EntryKind::Dir || !entry.truth.record_intact || inputs.analysis.marked(base) == Some(true) {
            continue;
        }
        let Some(number) = inputs.number(base) else { continue };
        let votes = m.index_blocks.iter().any(|b| {
            b.state == ArtifactState::Intact
                && !b.stale_copy
                && b.owner.number == number
                && b.owner.seq == entry.sequence
                && b.offset >= range.0
                && b.offset + b.size as u64 <= range.1
                && !intersects(live, b.offset, b.size as u64)
        });
        ev.voting_directories += u32::from(votes);
    }
    // F2 MFT map
    let records: Vec<usize> = in_union.iter().copied().collect();
    let (count, offset) = mft_map_count(inputs, &records, &records, cs);
    if offset == Some(volume_offset) {
        ev.mft_map = count;
    }
    // F3 checkable files
    for &e in &entries {
        let entry = &m.entries[e as usize];
        let checkable = entry.kind == EntryKind::File
            && !entry.deleted
            && !entry.resident
            && !entry.extents.is_empty()
            && entry.extents.iter().all(|x| x.offset.is_some())
            && entry.data_flags & 0x80FF == 0
            && entry.data_flags & 0x4000 == 0
            && !entry.streams.iter().any(|s| s == "WofCompressedData")
            && entry.reparse_tag.is_none_or(|t| t & 0xFFFF_0FFF != 0x9000_001A && t != 0x8000_0013 && t != 0x8000_0017)
            && extension(entry.name()).is_some_and(|x| OPINION.contains(&x.as_str()))
            && group_sound(inputs, entry);
        if !checkable {
            continue;
        }
        let first = entry.extents[0].offset.expect("checked");
        if first < range.0 || first + cs > range.1 || intersects(inputs.overwritten, first, cs) {
            continue;
        }
        ev.checkable += 1;
        let ext = extension(entry.name()).expect("checked");
        let head = inputs.image.read_vec(first, 1024.min((inputs.image.len() - first) as usize));
        ev.matches += u32::from(signature_matches(&head, &ext));
    }
    // F2 boot sectors that agree with the geometry
    ev.boot_agrees = !agreeing_boot_totals(m, cluster_size, volume_offset).is_empty();
    // F6
    let structural = ev.voting_directories >= 3 || ev.mft_map >= 16;
    let fraction = if ev.checkable == 0 { 0.0 } else { ev.matches as f64 / ev.checkable as f64 };
    let near = |x: f64, t: f64| (x - t).abs() < 0.05;
    ev.validated = if ev.checkable >= 20 {
        if ev.checkable > 200 && (near(fraction, 0.8) || near(fraction, 0.5)) {
            None // which 200 files F3 samples decides
        } else {
            Some(fraction >= 0.8 || (fraction >= 0.5 && structural))
        }
    } else {
        Some(structural)
    };
    ev
}

/// Volume sizes in clusters of the boot hits inside the scanned range that
/// agree with a geometry (as a primary at the volume offset, or as a backup
/// behind it).
fn agreeing_boot_totals(m: &Manifest, cluster_size: u32, volume_offset: u64) -> BTreeSet<u64> {
    let (start, end) = m.scan_range;
    m.boot_sectors
        .iter()
        .filter(|b| b.state == ArtifactState::Intact && b.offset >= start && b.offset + 512 <= end)
        .filter(|b| b.cluster_size == cluster_size)
        .filter(|b| {
            let bytes = b.total_sectors * b.bytes_per_sector as u64;
            b.offset == volume_offset || b.offset.checked_sub(bytes) == Some(volume_offset)
        })
        .map(|b| b.total_sectors * b.bytes_per_sector as u64 / cluster_size as u64)
        .collect()
}

/// F5 for one reading of "the in-use files of the instance".
fn total_clusters(m: &Manifest, ends: &mut [u64], cluster_size: u32, volume_offset: u64) -> Option<u64> {
    ends.sort_unstable_by(|a, b| b.cmp(a));
    let end_by_files = if ends.len() >= 4 { ends[3] } else { 0 };
    let candidates: Vec<u64> = agreeing_boot_totals(m, cluster_size, volume_offset)
        .into_iter()
        .filter(|&total| end_by_files <= total)
        .collect();
    match candidates.as_slice() {
        [one] => Some(*one),
        [] => {
            let by_range = m.scan_range.1.saturating_sub(volume_offset) / cluster_size as u64;
            Some(end_by_files.max(by_range))
        }
        _ => None,
    }
}

fn volume_size(inputs: &Inputs, u: &Union, cluster_size: u32, volume_offset: u64) -> Option<u64> {
    let m = inputs.m;
    let mut files = Vec::new();
    let mut with_dirs = Vec::new();
    for e in entries_in(inputs, u) {
        let entry = &m.entries[e as usize];
        if entry.deleted || !group_sound(inputs, entry) {
            continue;
        }
        if entry.expect.flags_set.contains(&crate::manifest::Flag::MissingFragments) {
            continue;
        }
        let end = match entry.kind {
            EntryKind::File => entry.extents.iter().filter_map(|x| x.lcn.map(|l| l + x.clusters)).max(),
            EntryKind::Dir => inputs.entry_records[e as usize]
                .iter()
                .flat_map(|&i| m.records[i].index_allocation.iter())
                .filter_map(|s| s.non_resident.as_ref())
                .flat_map(|x| x.runs.iter().filter_map(|r| r.lcn.map(|l| l + r.len)))
                .max(),
        };
        if let Some(end) = end {
            if entry.kind == EntryKind::File {
                files.push(end);
            }
            with_dirs.push(end);
        }
    }
    let a = total_clusters(m, &mut files, cluster_size, volume_offset);
    let b = total_clusters(m, &mut with_dirs, cluster_size, volume_offset);
    (a == b).then_some(a).flatten()
}

/// Builds the catalog. `kind_override` holds the kinds scenarios state for
/// physical instances.
pub(crate) fn assemble(inputs: &Inputs, kind_override: &BTreeMap<u32, InstanceKind>) -> Catalog {
    let m = inputs.m;
    let analysis = inputs.analysis;
    // ---- groups: one per segment of every fragment, plus the unplaced
    let mut unions: Vec<Union> = Vec::new();
    for (f, frag) in analysis.fragments.iter().enumerate() {
        let mut per_segment: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (&n, &i) in &frag.members {
            per_segment.entry(frag.segment(n)).or_default().push(i);
        }
        for (s, records) in per_segment {
            unions.push(union_of(inputs, records, [(f, s)].into_iter().collect(), false));
        }
    }
    if !analysis.unplaced.is_empty() {
        unions.push(union_of(inputs, analysis.unplaced.clone(), BTreeSet::new(), true));
    }

    // ---- joining (D4) between pieces of one file system
    loop {
        let mut order: Vec<usize> = (0..unions.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(unions[i].records.len()));
        let mut merged = None;
        'search: for (pos, &a) in order.iter().enumerate() {
            for &b in &order[pos + 1..] {
                if unions[a].physical.is_disjoint(&unions[b].physical) {
                    continue;
                }
                if !forbidden(inputs, &unions[a], &unions[b]) {
                    merged = Some((a, b));
                    break 'search;
                }
            }
        }
        let Some((a, b)) = merged else { break };
        let other = std::mem::take(&mut unions[b]);
        unions[a].absorb(other);
        unions.remove(b);
    }

    let live: Vec<(u64, u64)> = inputs
        .current
        .iter()
        .flat_map(|c| m.instances[c.instance as usize].live_allocated.iter().copied())
        .collect();
    let live = crate::disk::merge_ranges(live);

    // ---- geometry evidence (D5 step 3), for unions of 16 records and more
    let truth_geometry = |u: &Union| -> Option<(u32, u64)> {
        let mut g = u.physical.iter().map(|&p| {
            let i = &m.instances[p as usize];
            (i.geometry.cluster_size, i.geometry.volume_offset, i.role == InstanceRole::Embedded)
        });
        let first = g.next()?;
        if g.any(|x| x != first) {
            return None;
        }
        if first.2 {
            // a volume inside a file has one volume offset only while the
            // file is stored in one piece
            let inst = &m.instances[*u.physical.iter().next().expect("not empty") as usize];
            let host = &m.entries[inst.embedded_in? as usize];
            if host.extents.len() != 1 {
                return None;
            }
        }
        Some((first.0, first.1))
    };
    let evidences: Vec<Option<Evidence>> = unions
        .iter()
        .map(|u| {
            if u.stray || u.records.len() < MIN_RECORDS {
                return None;
            }
            let (cs, offset) = truth_geometry(u)?;
            Some(evidence(inputs, u, cs, offset, &live))
        })
        .collect();

    // ---- anchors (D8)
    let current_origins: Vec<(u64, i64)> = inputs.current.iter().flat_map(|c| c.origins()).collect();
    let mut anchor_of: Vec<bool> = vec![false; unions.len()];
    for (f, frag) in analysis.fragments.iter().enumerate() {
        if current_origins.contains(&(frag.record_size, frag.origin)) {
            if let Some(u) = unions.iter().position(|u| u.segments.contains(&(f, 0))) {
                anchor_of[u] = true;
            }
        }
    }

    // ---- embedded (D7)
    let mut stale_containers: BTreeSet<u32> = BTreeSet::new();
    let union_of_record = |unions: &[Union], record: usize| unions.iter().position(|u| u.records.contains(&record));
    let mut embedded: Vec<Option<u32>> = vec![None; unions.len()];
    let mut undecided: Vec<String> = Vec::new();
    for (x, u) in unions.iter().enumerate() {
        let all_embedded = u.physical.iter().all(|&p| m.instances[p as usize].role == InstanceRole::Embedded);
        if !all_embedded || u.stray || anchor_of[x] {
            continue;
        }
        let inst = &m.instances[*u.physical.iter().next().expect("not empty") as usize];
        let Some(host_entry) = inst.embedded_in else { continue };
        let Some(host_union) = inputs.base_record[host_entry as usize].and_then(|b| union_of_record(&unions, b))
        else {
            continue;
        };
        let host = &unions[host_union];
        let validated = match &evidences[host_union] {
            Some(e) => e.validated,
            None if host.stray || host.records.len() < MIN_RECORDS => Some(false),
            None => None,
        };
        // records inside data extents of in-use non-directory files of the
        // host; the system files (whose extents hold the host's own MFT)
        // are no containers a user could recover
        let files: Vec<u32> = entries_in(inputs, host)
            .into_iter()
            .filter(|&e| {
                let entry = &m.entries[e as usize];
                entry.kind == EntryKind::File && !entry.deleted && !entry.system
            })
            .collect();
        let mut holders: BTreeMap<u32, usize> = BTreeMap::new();
        let mut inside = 0usize;
        for &i in &u.records {
            let at = m.records[i].offset;
            if let Some(&e) = files.iter().find(|&&e| {
                m.entries[e as usize].extents.iter().any(|x| x.offset.is_some_and(|o| at >= o && at < o + x.bytes))
            }) {
                inside += 1;
                *holders.entry(e).or_default() += 1;
            }
        }
        let Some((&container, _)) = holders.iter().max_by_key(|(_, &count)| count) else { continue };
        if inside * 10 < u.records.len() * 9 {
            continue;
        }
        let newest = u.span.map(|s| s.1);
        let container_time = m.entries[container as usize].times.mft_changed;
        let fresh = newest.is_none_or(|newest| container_time + time::DAY >= newest);
        match validated {
            Some(true) if fresh => embedded[x] = Some(container),
            Some(true) => {
                stale_containers.insert(container);
            }
            Some(false) => {}
            None => undecided.push(inst.label.clone()),
        }
    }

    // ---- strays and copy sets (D6)
    let mut stray: Vec<bool> = unions
        .iter()
        .enumerate()
        .map(|(i, u)| embedded[i].is_none() && (u.stray || u.records.len() < MIN_RECORDS))
        .collect();
    let pairs: Vec<BTreeSet<(u64, u16)>> = unions
        .iter()
        .map(|u| {
            u.records
                .iter()
                .filter_map(|&i| Some((inputs.number(i)?, m.records[i].sequence)))
                .collect()
        })
        .collect();
    for x in 0..unions.len() {
        if stray[x] || embedded[x].is_some() || anchor_of[x] {
            continue;
        }
        let size = unions[x].records.len();
        let copies = unions.iter().enumerate().any(|(y, other)| {
            y != x && other.records.len() > size && {
                let repeated = pairs[x].intersection(&pairs[y]).count();
                repeated * 10 >= size * 9
            }
        });
        if copies {
            stray[x] = true;
        }
    }

    // ---- main (D8) and kinds (D9)
    let starts: BTreeSet<u64> =
        std::iter::once(m.scan_range.0).chain(m.partitions.iter().map(|p| p.offset)).collect();
    let candidates: Vec<usize> = (0..unions.len()).filter(|&i| !stray[i] && embedded[i].is_none()).collect();
    let newest = |i: usize| unions[i].span.map_or(0, |s| s.1);
    let anchors: Vec<usize> = candidates.iter().copied().filter(|&i| anchor_of[i]).collect();
    let main = if !anchors.is_empty() {
        anchors.iter().copied().max_by_key(|&i| unions[i].records.len())
    } else {
        let at_start: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|&i| evidences[i].as_ref().is_some_and(|e| starts.contains(&e.volume_offset)))
            .collect();
        let unknown = at_start.iter().any(|&i| evidences[i].as_ref().is_some_and(|e| e.validated.is_none()));
        let validated: Vec<usize> = at_start
            .iter()
            .copied()
            .filter(|&i| evidences[i].as_ref().and_then(|e| e.validated) == Some(true))
            .collect();
        if unknown {
            undecided.push("main (a validation the manifest cannot predict)".into());
        }
        let by_offset = validated.iter().copied().max_by_key(|&i| newest(i));
        if by_offset.is_none() && !unknown {
            candidates.iter().copied().max_by_key(|&i| (newest(i), unions[i].records.len()))
        } else {
            by_offset
        }
    };
    let mut kinds: Vec<Option<InstanceKind>> = vec![None; unions.len()];
    for i in 0..unions.len() {
        if stray[i] {
            kinds[i] = Some(InstanceKind::Strays);
        } else if embedded[i].is_some() {
            kinds[i] = Some(InstanceKind::Embedded);
        }
    }
    if let Some(main) = main {
        kinds[main] = Some(InstanceKind::Main);
        let main_offset = evidences[main].as_ref().map(|e| e.volume_offset);
        let main_oldest = unions[main].span.map(|s| s.0);
        for &i in &candidates {
            if i == main {
                continue;
            }
            // (i) a kept cut puts it on the older side of main
            let mut older = false;
            for &(f, s) in &unions[main].segments {
                let frag = &analysis.fragments[f];
                older |= unions[i].segments.iter().any(|&(g, t)| g == f && t > s);
                if let Some(cut) = frag.cuts.get(s) {
                    older |= side(cut, unions[i].span) == Side::Before;
                }
            }
            // (ii) older than main and not validated at another offset
            let elsewhere = evidences[i].as_ref().map(|e| (e.validated, Some(e.volume_offset) != main_offset));
            let before_main = matches!((unions[i].span, main_oldest), (Some(s), Some(o)) if s.1 < o);
            if before_main {
                match elsewhere {
                    Some((Some(true), true)) => {}
                    Some((None, true)) => undecided.push(format!("older or other ({:?})", unions[i].physical)),
                    _ => older = true,
                }
            }
            kinds[i] = Some(if older { InstanceKind::Older } else { InstanceKind::Other });
        }
    }
    // stated kinds settle what the rules leave open here, and must agree
    // with what they decide
    for (&physical, &kind) in kind_override {
        let Some(i) = (0..unions.len())
            .filter(|&i| unions[i].physical.contains(&physical))
            .max_by_key(|&i| unions[i].records.iter().filter(|&&r| m.records[r].instance == physical).count())
        else {
            continue;
        };
        let u = &unions[i];
        {
            match kinds[i] {
                Some(found) if found != kind => panic!(
                    "the scenario states kind {kind:?} for {:?}, ALGORITHMS D gives {found:?}",
                    u.physical.iter().map(|&p| m.instances[p as usize].label.clone()).collect::<Vec<_>>()
                ),
                _ => kinds[i] = Some(kind),
            }
        }
    }

    // ---- the catalog instances: strays merged into one
    let mut catalog = Catalog { stale_containers, undecided, of_record: vec![None; m.records.len()], ..Catalog::default() };
    let mut order: Vec<usize> = (0..unions.len()).collect();
    // main first, then by kind, then by size
    let rank = |k: Option<InstanceKind>| match k {
        Some(InstanceKind::Main) => 0,
        Some(InstanceKind::Older) => 1,
        Some(InstanceKind::Other) => 2,
        Some(InstanceKind::Embedded) => 3,
        None => 4,
        Some(InstanceKind::Strays) => 5,
    };
    order.sort_by_key(|&i| (rank(kinds[i]), std::cmp::Reverse(unions[i].records.len())));
    let mut strays_id: Option<u32> = None;
    let main_geometry = main.and_then(|i| evidences[i].clone());
    for i in order {
        let u = &unions[i];
        let Some(kind) = kinds[i] else {
            let labels: Vec<String> = u.physical.iter().map(|&p| m.instances[p as usize].label.clone()).collect();
            catalog.undecided.push(format!("kind of {labels:?}"));
            continue;
        };
        let id = if kind == InstanceKind::Strays {
            if let Some(id) = strays_id {
                let c = &mut catalog.instances[id as usize];
                c.records += u.records.len() as u32;
                for &p in &u.physical {
                    if !c.sources.contains(&p) {
                        c.sources.push(p);
                    }
                }
                c.sources.sort_unstable();
                id
            } else {
                let id = catalog.instances.len() as u32;
                strays_id = Some(id);
                let geometry = main_geometry.as_ref().map(|g| ExpectedGeometry {
                    cluster_size: g.cluster_size,
                    volume_offset: g.volume_offset,
                    inherited: true,
                    validated: None,
                    voting_directories: 0,
                    mft_map_records: 0,
                    signature_checkable: 0,
                    signature_matches: 0,
                    total_clusters: None,
                });
                catalog.instances.push(CatalogInstance {
                    id,
                    kind,
                    label: "strays".into(),
                    sources: u.physical.iter().copied().collect(),
                    records: u.records.len() as u32,
                    anchor: false,
                    container: None,
                    geometry,
                    lost_volume_range: None,
                    protected: Some(false),
                });
                id
            }
        } else {
            let id = catalog.instances.len() as u32;
            let labels: Vec<String> = u.physical.iter().map(|&p| m.instances[p as usize].label.clone()).collect();
            let own = evidences[i].as_ref();
            let inherit = kind == InstanceKind::Older && own.is_none_or(|e| !e.own());
            let geometry = if inherit {
                main_geometry.as_ref().map(|g| (g, true))
            } else {
                own.map(|g| (g, false))
            }
            .map(|(g, inherited)| ExpectedGeometry {
                cluster_size: g.cluster_size,
                volume_offset: g.volume_offset,
                inherited,
                validated: if inherited { None } else { g.validated },
                voting_directories: if inherited { 0 } else { g.voting_directories },
                mft_map_records: if inherited { 0 } else { g.mft_map },
                signature_checkable: if inherited { 0 } else { g.checkable },
                signature_matches: if inherited { 0 } else { g.matches },
                total_clusters: volume_size(inputs, u, g.cluster_size, g.volume_offset),
            });
            let lost_volume_range = geometry.as_ref().and_then(|g| {
                let total = g.total_clusters?;
                Some((g.volume_offset, (g.volume_offset + total * g.cluster_size as u64).min(m.image_size)))
            });
            let protected = match kind {
                InstanceKind::Main | InstanceKind::Other => geometry.as_ref().and_then(|g| g.validated),
                _ => Some(false),
            };
            catalog.instances.push(CatalogInstance {
                id,
                kind,
                label: labels.join("+"),
                sources: u.physical.iter().copied().collect(),
                records: u.records.len() as u32,
                anchor: anchor_of[i],
                container: embedded[i],
                geometry,
                lost_volume_range,
                protected,
            });
            id
        };
        for &r in &u.records {
            catalog.of_record[r] = Some(id);
        }
        for &seg in &u.segments {
            catalog.of_segment.insert(seg, id);
        }
    }
    catalog
}
