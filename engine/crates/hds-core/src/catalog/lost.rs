//! ALGORITHMS E6: the names and parents of lost folders, from index
//! entries (a) and stale copies among the strays (b).

use std::collections::HashMap;

use hds_io::RangeSet;
use hds_ntfs::{FileRef, Namespace};

use crate::instances::{InstanceId, Layout};
use crate::store::{EntryId, EntrySourceKind, RecordId, ScanStore};

use super::groups::{self, Groups};
use super::NameSrc;

/// One day in FILETIME ticks.
const FT_DAY: u64 = 864_000_000_000;

/// How an index block is attributed to instances (E6 "belongs to").
#[derive(Debug, Clone)]
enum BlockAttr {
    /// Some entry names a record exactly: the instances so named.
    Exact(Vec<InstanceId>),
    /// No entry names a record exactly: the block's owner.
    Owner(Option<FileRef>),
}

/// A name found for a lost folder.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Named {
    /// Where the name is.
    pub src: NameSrc,
    /// The parent reference that comes with it.
    pub parent: FileRef,
}

/// Everything E6 looks up, built once per catalog.
pub(crate) struct LostNames<'a> {
    store: &'a ScanStore,
    layout: &'a Layout,
    /// Non-current entries by child reference.
    by_child: Vec<(u64, EntryId)>,
    /// Old directory base records with a name, strays left out: (number,
    /// sequence, record).
    exact: Vec<(u32, u16, RecordId)>,
    /// Record numbers held by any instance (strays included).
    numbers: Vec<u32>,
    /// In-use directory records of the strays: (number, sequence, record).
    stray_dirs: Vec<(u32, u16, RecordId)>,
    blocks: HashMap<u64, BlockAttr>,
    extents: HashMap<u32, RangeSet>,
}

impl<'a> LostNames<'a> {
    pub fn new(store: &'a ScanStore, layout: &'a Layout) -> LostNames<'a> {
        let live = layout.live();
        let mut by_child: Vec<(u64, EntryId)> = store
            .index_entries()
            .filter(|(id, _)| !live.is_current_entry(*id))
            .map(|(id, v)| (v.child.raw(), id))
            .collect();
        by_child.sort_unstable();
        let mut exact = Vec::new();
        let mut numbers = Vec::new();
        let mut stray_dirs = Vec::new();
        for inst in layout.instances() {
            let strays = inst.kind == crate::instances::InstanceKind::Strays;
            for &(n, id) in &inst.records {
                let Some(r) = store.record(id) else { continue };
                let num = layout.number(id).unwrap_or(n);
                if num == groups::NO_NUMBER {
                    continue;
                }
                numbers.push(num);
                if r.is_extension() || !groups::is_directory(store, id, &[]) {
                    continue;
                }
                // The strays are no instance for the exact-name test: a
                // stale copy of a volume's MFT blocks must not take that
                // volume's index blocks away from it.
                if !strays && store.names(id).next().is_some() {
                    exact.push((num, r.sequence(), id));
                }
                if strays && r.is_in_use() {
                    stray_dirs.push((num, r.sequence(), id));
                }
            }
        }
        exact.sort_unstable();
        numbers.sort_unstable();
        numbers.dedup();
        stray_dirs.sort_unstable_by_key(|&(n, s, id)| {
            (n, s, store.record(id).map_or(u64::MAX, |r| r.disk_offset()))
        });
        LostNames {
            store,
            layout,
            by_child,
            exact,
            numbers,
            stray_dirs,
            blocks: HashMap::new(),
            extents: HashMap::new(),
        }
    }

    /// The instances whose records an entry names exactly (number,
    /// sequence, name and parent reference equal to one of the record's
    /// `$FILE_NAME`s).
    fn exact_instances(&self, e: EntryId, out: &mut Vec<InstanceId>) {
        let Some(v) = self.store.index_entry(e) else {
            return;
        };
        let Ok(n) = u32::try_from(v.child.number()) else {
            return;
        };
        let s = v.child.sequence();
        let lo = self.exact.partition_point(|&(m, q, _)| (m, q) < (n, s));
        for &(m, q, id) in self.exact.get(lo..).unwrap_or(&[]) {
            if (m, q) != (n, s) {
                break;
            }
            let hit = self
                .store
                .names(id)
                .any(|nm| nm.parent.raw() == v.parent.raw() && nm.name == v.name);
            if hit {
                if let Some(i) = self.layout.instance_of(id) {
                    if !out.contains(&i) {
                        out.push(i);
                    }
                }
            }
        }
    }

    fn block_attr(&mut self, offset: u64) -> BlockAttr {
        if let Some(a) = self.blocks.get(&offset) {
            return a.clone();
        }
        let by_source = self.store.index_entries_by_source();
        let lo = by_source.partition_point(|&e| {
            self.store
                .index_entry(e)
                .is_none_or(|v| v.source.offset < offset)
        });
        let mut set = Vec::new();
        for &e in by_source.get(lo..).unwrap_or(&[]) {
            let Some(v) = self.store.index_entry(e) else {
                break;
            };
            if v.source.offset != offset {
                break;
            }
            if v.source.kind == EntrySourceKind::Block {
                self.exact_instances(e, &mut set);
            }
        }
        let attr = if set.is_empty() {
            BlockAttr::Owner(self.store.index_block_at(offset).and_then(|b| b.owner))
        } else {
            set.sort_unstable();
            BlockAttr::Exact(set)
        };
        self.blocks.insert(offset, attr.clone());
        attr
    }

    /// Whether `e` is usable for instance `inst` (E6). `resolves_dir`
    /// answers E5 for `inst`: does `(P, s)` resolve to one of its
    /// directories. `groups` and `geometry` describe `inst`'s files for the
    /// last condition (no geometry: not checked).
    #[allow(clippy::too_many_arguments)]
    fn usable(
        &mut self,
        inst: InstanceId,
        e: EntryId,
        resolves_dir: &dyn Fn(u64, u16) -> bool,
        files: &Groups,
        geometry: Option<(u64, u64)>,
    ) -> bool {
        let Some(v) = self.store.index_entry(e) else {
            return false;
        };
        let time = v.mft_changed;
        let offset = v.source.offset;
        let belongs = match v.source.kind {
            EntrySourceKind::RecordRoot => {
                self.store
                    .record_at(offset)
                    .and_then(|r| self.layout.instance_of(r))
                    == Some(inst)
            }
            EntrySourceKind::Block => match self.block_attr(offset) {
                BlockAttr::Exact(set) => set == [inst],
                BlockAttr::Owner(None) => false,
                BlockAttr::Owner(Some(owner)) => {
                    if resolves_dir(owner.number(), owner.sequence()) {
                        true
                    } else {
                        let present = u32::try_from(owner.number())
                            .is_ok_and(|n| self.numbers.binary_search(&n).is_ok());
                        let oldest = self
                            .layout
                            .instance(inst)
                            .and_then(|i| i.times)
                            .map(|t| t.0);
                        !present && oldest.is_none_or(|o| time.saturating_add(FT_DAY) >= o)
                    }
                }
            },
        };
        if !belongs || !self.layout.on_side_of_cuts(inst, time) {
            return false;
        }
        if let Some((cs, vo)) = geometry {
            let set = self
                .extents
                .entry(inst.0)
                .or_insert_with(|| in_use_file_extents(self.store, files, cs, vo));
            if set.contains(offset) {
                return false;
            }
        }
        true
    }

    /// The name and parent of the lost folder `(inst, P, s)`: E6 (a), then
    /// (b); `None` leaves it `Folder #P`.
    pub fn name_for(
        &mut self,
        inst: InstanceId,
        p: u64,
        s: u16,
        resolves_dir: &dyn Fn(u64, u16) -> bool,
        files: &Groups,
        geometry: Option<(u64, u64)>,
    ) -> Option<Named> {
        let key = FileRef::new(p, s).raw();
        let lo = self.by_child.partition_point(|&(c, _)| c < key);
        let cands: Vec<EntryId> = self
            .by_child
            .get(lo..)
            .unwrap_or(&[])
            .iter()
            .take_while(|&&(c, _)| c == key)
            .map(|&(_, e)| e)
            .collect();
        let mut usable: Vec<EntryId> = Vec::new();
        for e in cands {
            if self.usable(inst, e, resolves_dir, files, geometry) {
                usable.push(e);
            }
        }
        let store = self.store;
        let is_dos = |e: &EntryId| {
            store
                .index_entry(*e)
                .is_some_and(|v| v.namespace == Namespace::Dos)
        };
        if usable.iter().any(|e| !is_dos(e)) {
            usable.retain(|e| !is_dos(e));
        }
        let best = usable.into_iter().max_by_key(|&e| {
            let v = store.index_entry(e);
            let time = v.map_or(0, |v| v.mft_changed);
            let lsn = v.map_or(0, |v| v.source.lsn);
            let off = v.map_or(u64::MAX, |v| v.source.offset);
            (time, lsn, std::cmp::Reverse(off), std::cmp::Reverse(e))
        });
        if let Some(e) = best {
            let parent = store.index_entry(e)?.parent;
            return Some(Named {
                src: NameSrc::Entry(e),
                parent,
            });
        }
        // (b) a stale copy of the instance among the strays.
        let n = u32::try_from(p).ok()?;
        let lo = self
            .stray_dirs
            .partition_point(|&(m, q, _)| (m, q) < (n, s));
        for &(m, q, id) in self.stray_dirs.get(lo..).unwrap_or(&[]) {
            if (m, q) != (n, s) {
                break;
            }
            let copy = self
                .layout
                .stray_group(id)
                .is_some_and(|g| g.copy_of.contains(&inst));
            if !copy {
                continue;
            }
            if let Some((idx, parent)) = best_name(store, id) {
                return Some(Named {
                    src: NameSrc::Record(id, idx),
                    parent,
                });
            }
        }
        None
    }
}

/// Namespace rank of E4: Win32 and Win32+DOS, then POSIX, then DOS.
pub(crate) fn ns_rank(ns: Namespace) -> u8 {
    match ns {
        Namespace::Win32 | Namespace::Win32AndDos => 0,
        Namespace::Posix => 1,
        Namespace::Dos => 2,
        #[allow(unreachable_patterns)]
        _ => 3,
    }
}

/// The best name of one record by namespace, then attribute id: its
/// position among the record's names and its parent reference.
pub(crate) fn best_name(store: &ScanStore, id: RecordId) -> Option<(u16, FileRef)> {
    let mut best: Option<((u8, u16), u16, FileRef)> = None;
    for (i, n) in store.names(id).enumerate() {
        let key = (ns_rank(n.namespace), n.attr_id);
        let i = u16::try_from(i).unwrap_or(u16::MAX);
        if best.as_ref().is_none_or(|(k, _, _)| key < *k) {
            best = Some((key, i, n.parent));
        }
    }
    best.map(|(_, i, p)| (i, p))
}

/// The absolute byte ranges of the data of the in-use files (not
/// directories) of an instance under geometry `(cs, vo)`.
pub(crate) fn in_use_file_extents(store: &ScanStore, files: &Groups, cs: u64, vo: u64) -> RangeSet {
    let mut set = RangeSet::new();
    for g in &files.groups {
        let exts = files.exts(g);
        let in_use = store.record(g.base).is_some_and(|r| r.is_in_use());
        if !in_use || groups::is_directory(store, g.base, exts) {
            continue;
        }
        let m = groups::merge(store, g.base, exts, cs);
        for r in &m.runs {
            if let Some(range) = run_bytes(r.lcn, r.len, cs, vo) {
                set.insert(range);
            }
        }
    }
    set
}

/// The absolute byte range of a run, `None` for a sparse run or one that
/// does not fit 64 bits.
pub(crate) fn run_bytes(
    lcn: Option<u64>,
    len: u64,
    cs: u64,
    vo: u64,
) -> Option<std::ops::Range<u64>> {
    let lcn = lcn?;
    let a = lcn.checked_mul(cs)?.checked_add(vo)?;
    let b = a.checked_add(len.checked_mul(cs)?)?;
    (b > a).then_some(a..b)
}
