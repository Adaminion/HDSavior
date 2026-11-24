//! ALGORITHMS E3-E9 and E11: the tree of every instance.

use std::collections::HashMap;

use hds_ntfs::FileRef;

use crate::instances::{Instance, InstanceKind};
use crate::store::{RecordId, ScanStore};

use super::groups::{self, Groups, NO_NUMBER};
use super::lost::{self, LostNames};
use super::{
    bits, fallback_geometry, BuildError, CatalogInput, CatalogInstance, LostKey, NameSrc, NodeId,
    NodeKind, NodeRec, Tree, LOST_FOLDERS, NONE, ORPHANED_FILES,
};

/// E4 ranking of a name: namespace, parent not resolving, attribute id,
/// record position in the group.
type NameRank = (u8, u8, u16, usize);

/// Parent sentinel: the node is omitted (below `$Extend`, E5).
const OMIT: u32 = u32::MAX - 1;

/// A node while the trees are built.
#[derive(Debug, Clone, Copy)]
struct TNode {
    parent: u32,
    rec: u32,
    name: NameSrc,
    inst: u16,
    kind: NodeKind,
    deleted: bool,
    /// Orders siblings (E11 ids are deterministic): kind rank, then this.
    key: u64,
}

/// Where E5 attaches a parent reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attach {
    /// The instance root.
    Root,
    /// Omitted (under `$Extend`).
    Omit,
    /// A directory node.
    Dir(u32),
    /// The lost folder of this reference.
    Lost(FileRef),
}

/// Per-instance state while its tree is built.
struct Inst<'a> {
    ci: u16,
    layout: &'a Instance,
    strays: bool,
    groups: Groups,
    /// Directory targets: (number, group index), sorted by number.
    dirs: Vec<(u32, u32)>,
    /// Node of each group (NONE: omitted or not created).
    group_node: Vec<u32>,
    root: u32,
    lost_vf: Option<u32>,
    orphan_vf: Option<u32>,
    geometry: Option<(u64, u64)>,
}

struct Builder<'a> {
    store: &'a ScanStore,
    heads: &'a crate::geometry::Heads,
    nodes: Vec<TNode>,
    texts: Vec<String>,
    text_ids: HashMap<String, u32>,
    lost: HashMap<(u16, u64), u32>,
    lost_keys: Vec<(u32, LostKey)>,
    pending: Vec<(u32, FileRef)>,
    /// Records of nodes without a number in their header and their
    /// positional number (D1): (record, number).
    positional: Vec<(u32, u32)>,
}

impl Builder<'_> {
    fn text(&mut self, s: &str) -> NameSrc {
        if let Some(&t) = self.text_ids.get(s) {
            return NameSrc::Text(t);
        }
        let t = u32::try_from(self.texts.len()).unwrap_or(NONE);
        self.texts.push(s.to_string());
        self.text_ids.insert(s.to_string(), t);
        NameSrc::Text(t)
    }

    fn push(&mut self, n: TNode) -> Result<u32, BuildError> {
        let id = u32::try_from(self.nodes.len())
            .ok()
            .filter(|&i| i < OMIT.saturating_sub(1))
            .ok_or(BuildError::TooManyNodes)?;
        self.nodes.push(n);
        Ok(id)
    }

    fn set_parent(&mut self, node: u32, parent: u32) {
        if let Some(n) = self
            .nodes
            .get_mut(usize::try_from(node).unwrap_or(usize::MAX))
        {
            n.parent = parent;
        }
    }

    fn virtual_folder(&mut self, inst: &mut Inst<'_>, lost: bool) -> Result<u32, BuildError> {
        let slot = if lost { inst.lost_vf } else { inst.orphan_vf };
        if let Some(v) = slot {
            return Ok(v);
        }
        let name = self.text(if lost { LOST_FOLDERS } else { ORPHANED_FILES });
        let v = self.push(TNode {
            parent: inst.root,
            rec: NONE,
            name,
            inst: inst.ci,
            kind: NodeKind::Virtual,
            deleted: false,
            key: if lost { 0 } else { 1 },
        })?;
        if lost {
            inst.lost_vf = Some(v);
        } else {
            inst.orphan_vf = Some(v);
        }
        Ok(v)
    }

    /// The node of the lost folder `(inst, P, s)`, created on first use
    /// (its name is looked up later, from `pending`).
    fn lost_folder(&mut self, inst: &Inst<'_>, r: FileRef) -> Result<u32, BuildError> {
        if let Some(&n) = self.lost.get(&(inst.ci, r.raw())) {
            return Ok(n);
        }
        let n = self.push(TNode {
            parent: NONE,
            rec: NONE,
            name: NameSrc::Text(NONE),
            inst: inst.ci,
            kind: NodeKind::LostDir,
            deleted: false,
            key: r.raw(),
        })?;
        self.lost.insert((inst.ci, r.raw()), n);
        self.lost_keys.push((
            n,
            LostKey {
                instance: u32::from(inst.ci),
                number: r.number(),
                sequence: r.sequence(),
            },
        ));
        self.pending.push((n, r));
        Ok(n)
    }

    fn attach_node(&mut self, inst: &Inst<'_>, a: Attach) -> Result<u32, BuildError> {
        Ok(match a {
            Attach::Root => inst.root,
            Attach::Omit => OMIT,
            Attach::Dir(d) => d,
            Attach::Lost(r) => self.lost_folder(inst, r)?,
        })
    }
}

/// E5 for a parent reference `(P, s)` of instance `inst`.
fn attach(
    store: &ScanStore,
    layout: &crate::instances::Layout,
    inst: &Inst<'_>,
    r: FileRef,
) -> Attach {
    let p = r.number();
    if p == 11 {
        return Attach::Omit;
    }
    if inst.strays {
        return Attach::Lost(r);
    }
    if p == 5 {
        return Attach::Root;
    }
    match resolve_dir(store, layout, inst, r) {
        Some(d) => Attach::Dir(d),
        None => Attach::Lost(r),
    }
}

/// The directory node `(P, s)` resolves to in `inst` by the reference rule
/// (never a D2-marked record, never a non-directory, never an omitted one).
fn resolve_dir(
    store: &ScanStore,
    layout: &crate::instances::Layout,
    inst: &Inst<'_>,
    r: FileRef,
) -> Option<u32> {
    if inst.strays {
        return None;
    }
    let p = u32::try_from(r.number()).ok().filter(|&p| p != NO_NUMBER)?;
    let i = inst.dirs.binary_search_by_key(&p, |&(n, _)| n).ok()?;
    let &(_, g) = inst.dirs.get(i)?;
    let gi = usize::try_from(g).ok()?;
    let group = inst.groups.groups.get(gi)?;
    let node = *inst.group_node.get(gi)?;
    if node == NONE || layout.is_older_generation(group.base) {
        return None;
    }
    let rec = store.record(group.base)?;
    r.resolves_to(rec.sequence(), rec.is_in_use())
        .then_some(node)
}

/// Builds the trees of all instances (E3-E9, E11).
pub(crate) fn build(input: &CatalogInput<'_>) -> Result<(Tree, Vec<CatalogInstance>), BuildError> {
    let store = input.store;
    let layout = input.layout;
    // Catalog order: main first, then the layout's order (strays last).
    let mut order: Vec<&Instance> = Vec::new();
    if let Some(m) = layout.main().and_then(|m| layout.instance(m)) {
        order.push(m);
    }
    for i in layout.instances() {
        if Some(i.id) != layout.main() && !i.records.is_empty() {
            order.push(i);
        }
    }
    let mut b = Builder {
        store,
        heads: input.heads,
        nodes: Vec::new(),
        texts: Vec::new(),
        text_ids: HashMap::new(),
        lost: HashMap::new(),
        lost_keys: Vec::new(),
        pending: Vec::new(),
        positional: Vec::new(),
    };
    let mut names = LostNames::new(store, layout);
    let mut ext_pairs: Vec<(u32, RecordId)> = Vec::new();
    let mut insts: Vec<Inst<'_>> = Vec::new();
    for (ci, li) in order.iter().enumerate() {
        let ci = u16::try_from(ci).map_err(|_| BuildError::TooManyNodes)?;
        let label = b.text(&li.label);
        let root = b.push(TNode {
            parent: NONE,
            rec: NONE,
            name: label,
            inst: ci,
            kind: NodeKind::Virtual,
            deleted: false,
            key: 0,
        })?;
        let geometry = li
            .geometry
            .as_ref()
            .map(|g| (u64::from(g.cluster_size), g.volume_offset));
        let mut inst = Inst {
            ci,
            layout: li,
            strays: li.kind == InstanceKind::Strays,
            groups: Groups::build(store, layout, &li.records),
            dirs: Vec::new(),
            group_node: Vec::new(),
            root,
            lost_vf: None,
            orphan_vf: None,
            geometry,
        };
        build_instance(&mut b, &mut inst, &mut names, layout, &mut ext_pairs)?;
        insts.push(inst);
    }
    cut_cycles_and_omit(&mut b, &mut insts)?;
    finish(b, insts, ext_pairs, input)
}

/// Nodes, names and parents of one instance (E3-E7, E9).
fn build_instance<'a>(
    b: &mut Builder<'_>,
    inst: &mut Inst<'a>,
    names: &mut LostNames<'_>,
    layout: &crate::instances::Layout,
    ext_pairs: &mut Vec<(u32, RecordId)>,
) -> Result<(), BuildError> {
    let store = b.store;
    let n_groups = inst.groups.groups.len();
    inst.group_node = vec![NONE; n_groups];
    // Pass 1: a node per kept group (E7, E9), directory targets.
    for gi in 0..n_groups {
        let Some(g) = inst.groups.groups.get(gi).copied() else {
            continue;
        };
        let exts = inst.groups.exts(&g);
        let has_names =
            groups::group_records(g.base, exts).any(|id| store.names(id).next().is_some());
        if g.orphan {
            let has_data = groups::group_records(g.base, exts)
                .any(|id| store.data_extents(id).next().is_some());
            if g.number < 24 || !has_data {
                continue; // E7
            }
        } else if g.number != NO_NUMBER && (g.number < 16 || (g.number < 24 && !has_names)) {
            continue; // E9
        }
        let Some(base) = store.record(g.base) else {
            continue;
        };
        if base.record_number().is_none() {
            if let Some(n) = layout.number(g.base) {
                b.positional.push((g.base.0, n));
            }
        }
        let is_dir = !g.orphan && groups::is_directory(store, g.base, exts);
        let node = b.push(TNode {
            parent: NONE,
            rec: g.base.0,
            name: NameSrc::Text(NONE),
            inst: inst.ci,
            kind: if is_dir {
                NodeKind::Dir
            } else {
                NodeKind::File
            },
            deleted: !base.is_in_use(),
            key: base.disk_offset(),
        })?;
        if let Some(slot) = inst.group_node.get_mut(gi) {
            *slot = node;
        }
        for &x in exts {
            ext_pairs.push((node, x));
        }
        if is_dir && !inst.strays && g.number != NO_NUMBER {
            inst.dirs
                .push((g.number, u32::try_from(gi).unwrap_or(NONE)));
        }
    }
    inst.dirs.sort_unstable();
    inst.dirs.dedup_by_key(|&mut (n, _)| n);
    // Pass 2: names (E4) and parents (E5, E7).
    for gi in 0..n_groups {
        let node = inst.group_node.get(gi).copied().unwrap_or(NONE);
        let Some(g) = inst.groups.groups.get(gi).copied() else {
            continue;
        };
        if node == NONE {
            continue;
        }
        let exts = inst.groups.exts(&g);
        let mut best: Option<(NameRank, NameSrc, FileRef)> = None;
        for (k, id) in groups::group_records(g.base, exts).enumerate() {
            for (i, n) in store.names(id).enumerate() {
                let resolv = u8::from(resolve_dir(store, layout, inst, n.parent).is_none());
                let key = (lost::ns_rank(n.namespace), resolv, n.attr_id, k);
                if best.as_ref().is_none_or(|(bk, _, _)| key < *bk) {
                    let i = u16::try_from(i).unwrap_or(u16::MAX);
                    best = Some((key, NameSrc::Record(id, i), n.parent));
                }
            }
        }
        match best {
            Some((_, src, parent)) => {
                // A null reference is no usable parent (E7).
                let p = if parent.raw() == 0 {
                    b.virtual_folder(inst, false)?
                } else {
                    let a = attach(store, layout, inst, parent);
                    b.attach_node(inst, a)?
                };
                if let Some(n) = b.nodes.get_mut(usize::try_from(node).unwrap_or(usize::MAX)) {
                    n.name = src;
                }
                b.set_parent(node, p);
            }
            None => {
                // No usable parent reference: [Orphaned files] (E7).
                let ext = orphan_extension(store, b.heads, inst, g.base, exts);
                let label = match (g.number, ext) {
                    (NO_NUMBER, _) => format!(
                        "File @{}",
                        store.record(g.base).map_or(0, |r| r.disk_offset())
                    ),
                    (n, Some(e)) => format!("File #{n}.{e}"),
                    (n, None) => format!("File #{n}"),
                };
                let name = b.text(&label);
                let vf = b.virtual_folder(inst, false)?;
                if let Some(n) = b.nodes.get_mut(usize::try_from(node).unwrap_or(usize::MAX)) {
                    n.name = name;
                }
                b.set_parent(node, vf);
            }
        }
    }
    // Lost folders (E6), including the ones their names lead to.
    let mut k = 0usize;
    while let Some(&(node, r)) = b.pending.get(k) {
        k = k.saturating_add(1);
        let resolves = |p: u64, s: u16| -> bool {
            resolve_dir(store, layout, inst, FileRef::new(p, s)).is_some()
        };
        let named = names.name_for(
            inst.layout.id,
            r.number(),
            r.sequence(),
            &resolves,
            &inst.groups,
            inst.geometry,
        );
        match named {
            Some(nm) => {
                if let Some(n) = b.nodes.get_mut(usize::try_from(node).unwrap_or(usize::MAX)) {
                    n.name = nm.src;
                }
                let parent = if inst.strays {
                    inst.root
                } else {
                    let a = attach(store, layout, inst, nm.parent);
                    b.attach_node(inst, a)?
                };
                b.set_parent(node, parent);
            }
            None => {
                let parent = if inst.strays {
                    inst.root
                } else {
                    b.virtual_folder(inst, true)?
                };
                b.set_parent(node, parent);
            }
        }
    }
    b.pending.clear();
    // `Folder #P`, or `Folder #P.s` when several sequences share P.
    let mut by_number: HashMap<u64, u32> = HashMap::new();
    for &(_, key) in b
        .lost_keys
        .iter()
        .filter(|(_, k)| k.instance == u32::from(inst.ci))
    {
        let c = by_number.entry(key.number).or_insert(0);
        *c = c.saturating_add(1);
    }
    let unnamed: Vec<(u32, LostKey)> = b
        .lost_keys
        .iter()
        .filter(|(_, k)| k.instance == u32::from(inst.ci))
        .copied()
        .collect();
    for (node, key) in unnamed {
        let idx = usize::try_from(node).unwrap_or(usize::MAX);
        if !matches!(b.nodes.get(idx).map(|n| n.name), Some(NameSrc::Text(NONE))) {
            continue;
        }
        let label = if by_number.get(&key.number).copied().unwrap_or(0) > 1 {
            format!("Folder #{}.{}", key.number, key.sequence)
        } else {
            format!("Folder #{}", key.number)
        };
        let name = b.text(&label);
        if let Some(n) = b.nodes.get_mut(idx) {
            n.name = name;
        }
    }
    Ok(())
}

/// The extension of the kind sniffed from a stored head of a nameless
/// file's first data cluster (E7: "plus the extension of the sniffed kind
/// once content has been read"); nothing is read here.
fn orphan_extension(
    store: &ScanStore,
    heads: &crate::geometry::Heads,
    inst: &Inst<'_>,
    base: RecordId,
    exts: &[RecordId],
) -> Option<&'static str> {
    let (cs, vo) = inst.geometry?;
    let m = groups::merge(store, base, exts, cs);
    let off = lost::run_bytes(m.first_lcn(), 1, cs, vo)?.start;
    match heads.stored(off)? {
        crate::geometry::Head::Bytes(head) => hds_carve::sniff(&head).map(|k| k.default_ext()),
        crate::geometry::Head::Unreadable => None,
    }
}

/// E8 (cycles) and the omission below `$Extend` (E5): every chain must
/// reach a root. A node whose chain runs into itself is attached to its
/// instance's `[Lost folders]` where the cycle is detected; a chain that
/// reaches an omitted node is omitted with it.
fn cut_cycles_and_omit(b: &mut Builder<'_>, insts: &mut [Inst<'_>]) -> Result<(), BuildError> {
    const NEW: u8 = 0;
    const PATH: u8 = 1;
    const OK: u8 = 2;
    const OMITTED: u8 = 3;
    let mut state: Vec<u8> = vec![NEW; b.nodes.len()];
    let mut stack: Vec<u32> = Vec::new();
    let total = b.nodes.len();
    let mut v = 0usize;
    while v < total.max(b.nodes.len()) {
        if state.len() < b.nodes.len() {
            state.resize(b.nodes.len(), NEW);
        }
        let start = u32::try_from(v).unwrap_or(NONE);
        v = v.saturating_add(1);
        if state
            .get(usize::try_from(start).unwrap_or(usize::MAX))
            .copied()
            != Some(NEW)
        {
            continue;
        }
        stack.clear();
        let mut x = start;
        let outcome = loop {
            let xi = usize::try_from(x).unwrap_or(usize::MAX);
            match state.get(xi).copied() {
                Some(OK) => break OK,
                Some(OMITTED) => break OMITTED,
                Some(PATH) => {
                    // A cycle: cut at the node that points into the path.
                    let Some(&last) = stack.last() else {
                        break OK;
                    };
                    let li = usize::try_from(last).unwrap_or(usize::MAX);
                    let ci = b.nodes.get(li).map_or(0, |n| n.inst);
                    let Some(inst) = insts.get_mut(usize::from(ci)) else {
                        break OK;
                    };
                    let vf = b.virtual_folder(inst, true)?;
                    if state.len() < b.nodes.len() {
                        state.resize(b.nodes.len(), NEW);
                    }
                    if let Some(s) = state.get_mut(usize::try_from(vf).unwrap_or(usize::MAX)) {
                        *s = OK;
                    }
                    b.set_parent(last, vf);
                    break OK;
                }
                Some(_) => {}
                None => break OK,
            }
            if let Some(s) = state.get_mut(xi) {
                *s = PATH;
            }
            stack.push(x);
            let parent = b.nodes.get(xi).map_or(NONE, |n| n.parent);
            if parent == NONE {
                break OK;
            }
            if parent == OMIT {
                break OMITTED;
            }
            x = parent;
        };
        for &y in &stack {
            if let Some(s) = state.get_mut(usize::try_from(y).unwrap_or(usize::MAX)) {
                *s = outcome;
            }
        }
    }
    for (i, s) in state.iter().enumerate() {
        if *s == OMITTED {
            if let Some(n) = b.nodes.get_mut(i) {
                n.parent = OMIT;
            }
        }
    }
    Ok(())
}

fn kind_rank(k: NodeKind) -> u8 {
    match k {
        NodeKind::Virtual => 0,
        NodeKind::LostDir => 1,
        NodeKind::Dir => 2,
        NodeKind::File | NodeKind::Carved => 3,
    }
}

/// Display name of a name source.
pub(crate) fn display_name(store: &ScanStore, texts: &[String], src: NameSrc) -> String {
    match src {
        NameSrc::Record(r, i) => store
            .names(r)
            .nth(usize::from(i))
            .map(|n| n.name.to_string_lossy())
            .unwrap_or_default(),
        NameSrc::Entry(e) => store
            .index_entry(e)
            .map(|v| v.name.to_string_lossy())
            .unwrap_or_default(),
        NameSrc::Text(t) => texts
            .get(usize::try_from(t).unwrap_or(usize::MAX))
            .cloned()
            .unwrap_or_default(),
    }
}

/// The UTF-16 units of a name source (exact, for recovery).
pub(crate) fn name_units(store: &ScanStore, texts: &[String], src: NameSrc) -> Vec<u16> {
    match src {
        NameSrc::Record(r, i) => store
            .names(r)
            .nth(usize::from(i))
            .map(|n| n.name.to_units())
            .unwrap_or_default(),
        NameSrc::Entry(e) => store
            .index_entry(e)
            .map(|v| v.name.to_units())
            .unwrap_or_default(),
        NameSrc::Text(t) => texts
            .get(usize::try_from(t).unwrap_or(usize::MAX))
            .map(|s| s.encode_utf16().collect())
            .unwrap_or_default(),
    }
}

/// The folded (case-insensitive) form of a display name for search and
/// sorting: lower case, without `\0` (the arena separator).
pub(crate) fn fold(name: &str) -> String {
    name.to_lowercase().replace('\0', "")
}

/// Final ids in preorder (E11), the folded-name arena, instances.
fn finish(
    mut b: Builder<'_>,
    insts: Vec<Inst<'_>>,
    ext_pairs: Vec<(u32, RecordId)>,
    input: &CatalogInput<'_>,
) -> Result<(Tree, Vec<CatalogInstance>), BuildError> {
    let store = input.store;
    let layout = input.layout;
    // Children lists.
    let n = b.nodes.len();
    let mut child_count: Vec<u32> = vec![0; n.saturating_add(1)];
    for t in &b.nodes {
        if t.parent != NONE && t.parent != OMIT {
            if let Some(c) = child_count.get_mut(usize::try_from(t.parent).unwrap_or(usize::MAX)) {
                *c = c.saturating_add(1);
            }
        }
    }
    let mut start: Vec<u32> = Vec::with_capacity(n.saturating_add(1));
    let mut acc = 0u32;
    for c in &child_count {
        start.push(acc);
        acc = acc.saturating_add(*c);
    }
    let mut fill = start.clone();
    let mut children: Vec<u32> = vec![NONE; usize::try_from(acc).unwrap_or(0)];
    for (i, t) in b.nodes.iter().enumerate() {
        if t.parent == NONE || t.parent == OMIT {
            continue;
        }
        let Some(f) = fill.get_mut(usize::try_from(t.parent).unwrap_or(usize::MAX)) else {
            continue;
        };
        if let Some(slot) = children.get_mut(usize::try_from(*f).unwrap_or(usize::MAX)) {
            *slot = u32::try_from(i).unwrap_or(NONE);
        }
        *f = f.saturating_add(1);
    }
    for p in 0..n {
        let a = usize::try_from(start.get(p).copied().unwrap_or(0)).unwrap_or(0);
        let z = usize::try_from(start.get(p.saturating_add(1)).copied().unwrap_or(0)).unwrap_or(0);
        if let Some(list) = children.get_mut(a..z) {
            list.sort_unstable_by_key(|&c| {
                let t = b.nodes.get(usize::try_from(c).unwrap_or(usize::MAX));
                (
                    t.map_or(9, |t| kind_rank(t.kind)),
                    t.map_or(u64::MAX, |t| t.key),
                    c,
                )
            });
        }
    }
    // Preorder from the roots, instance by instance.
    let mut new_id: Vec<u32> = vec![NONE; n];
    let mut order: Vec<u32> = Vec::with_capacity(n);
    let mut stack: Vec<u32> = Vec::new();
    // Instances that produce no node besides their root are not reported
    // (D9); main always is. `ci_new` renumbers the reported ones.
    let mut ci_new: Vec<u16> = Vec::with_capacity(insts.len());
    let mut reported: Vec<usize> = Vec::new();
    for (k, inst) in insts.iter().enumerate() {
        let ri = usize::try_from(inst.root).unwrap_or(usize::MAX);
        let kids = child_count.get(ri).copied().unwrap_or(0);
        let keep = kids > 0 || inst.layout.kind == InstanceKind::Main;
        ci_new.push(u16::try_from(reported.len()).unwrap_or(u16::MAX));
        if keep {
            reported.push(k);
        }
    }
    let remap = |ci: u16| ci_new.get(usize::from(ci)).copied().unwrap_or(u16::MAX);
    for &k in &reported {
        let Some(inst) = insts.get(k) else { continue };
        stack.push(inst.root);
        while let Some(x) = stack.pop() {
            let xi = usize::try_from(x).unwrap_or(usize::MAX);
            if let Some(slot) = new_id.get_mut(xi) {
                *slot = u32::try_from(order.len()).unwrap_or(NONE);
            }
            order.push(x);
            let a = usize::try_from(start.get(xi).copied().unwrap_or(0)).unwrap_or(0);
            let z =
                usize::try_from(start.get(xi.saturating_add(1)).copied().unwrap_or(0)).unwrap_or(0);
            for &c in children.get(a..z).unwrap_or(&[]).iter().rev() {
                stack.push(c);
            }
        }
    }
    let total = u32::try_from(order.len())
        .ok()
        .filter(|&t| t < NONE)
        .ok_or(BuildError::TooManyNodes)?;
    // Subtree ends: walk the preorder backwards.
    let mut end: Vec<u32> = (1..=total).collect();
    for k in (0..order.len()).rev() {
        let old = order.get(k).copied().unwrap_or(NONE);
        let p = b
            .nodes
            .get(usize::try_from(old).unwrap_or(usize::MAX))
            .map_or(NONE, |t| t.parent);
        if p == NONE || p == OMIT {
            continue;
        }
        let pk = new_id
            .get(usize::try_from(p).unwrap_or(usize::MAX))
            .copied()
            .unwrap_or(NONE);
        let e = end.get(k).copied().unwrap_or(0);
        if let Some(pe) = end.get_mut(usize::try_from(pk).unwrap_or(usize::MAX)) {
            *pe = (*pe).max(e);
        }
    }
    let mut nodes: Vec<NodeRec> = Vec::with_capacity(order.len());
    // Folded names in node order: offsets never decrease, so the names of
    // a subtree are one stretch of the arena. Past the u32 limit, names
    // are left empty.
    let mut fold_arena = String::new();
    let limit = usize::try_from(NONE).unwrap_or(usize::MAX);
    for (k, &old) in order.iter().enumerate() {
        let Some(t) = b
            .nodes
            .get(usize::try_from(old).unwrap_or(usize::MAX))
            .copied()
        else {
            continue;
        };
        let parent = if t.parent == NONE {
            NONE
        } else {
            new_id
                .get(usize::try_from(t.parent).unwrap_or(usize::MAX))
                .copied()
                .unwrap_or(NONE)
        };
        let folded = fold(&display_name(store, &b.texts, t.name));
        let fold_off = u32::try_from(fold_arena.len()).unwrap_or(NONE);
        if fold_arena
            .len()
            .saturating_add(folded.len())
            .saturating_add(1)
            < limit
        {
            fold_arena.push_str(&folded);
            fold_arena.push('\0');
        }
        nodes.push(NodeRec {
            parent,
            end: end.get(k).copied().unwrap_or(0),
            rec: t.rec,
            fold: fold_off,
            name: t.name.encode(),
            inst: remap(t.inst),
            kind: t.kind,
            bits: if t.deleted { bits::DELETED } else { 0 },
        });
    }
    let map = |old: u32| {
        new_id
            .get(usize::try_from(old).unwrap_or(usize::MAX))
            .copied()
            .unwrap_or(NONE)
    };
    let mut exts: Vec<(u32, RecordId)> = ext_pairs
        .into_iter()
        .map(|(nd, r)| (map(nd), r))
        .filter(|&(nd, _)| nd != NONE)
        .collect();
    exts.sort_unstable();
    let mut lost_keys: Vec<(u32, LostKey)> = b
        .lost_keys
        .iter()
        .map(|&(nd, k)| {
            let instance = u32::from(remap(u16::try_from(k.instance).unwrap_or(u16::MAX)));
            (map(nd), LostKey { instance, ..k })
        })
        .filter(|&(nd, _)| nd != NONE)
        .collect();
    lost_keys.sort_unstable_by_key(|&(nd, _)| nd);
    let mut inst_of_layout = vec![NONE; layout.instances().len()];
    let mut out: Vec<CatalogInstance> = Vec::new();
    for (ci, &k) in reported.iter().enumerate() {
        let Some(inst) = insts.get(k) else { continue };
        let li = inst.layout;
        if let Some(slot) = inst_of_layout.get_mut(li.id.index()) {
            *slot = u32::try_from(ci).unwrap_or(NONE);
        }
        let opt = |v: Option<u32>| v.map(map).filter(|&x| x != NONE).map(NodeId);
        out.push(CatalogInstance {
            id: u32::try_from(ci).unwrap_or(NONE),
            layout_id: li.id,
            kind: li.kind,
            manual: li.manual,
            label: li.label.clone(),
            root: NodeId(map(inst.root)),
            records: u64::try_from(li.records.len()).unwrap_or(u64::MAX),
            times: li.times,
            embedded_in: None,
            geometry: li
                .geometry
                .clone()
                .unwrap_or_else(|| fallback_geometry(layout.range())),
            candidates: li.candidates.clone(),
            lost: opt(inst.lost_vf),
            orphans: opt(inst.orphan_vf),
            carved: None,
        });
    }
    let texts = std::mem::take(&mut b.texts);
    let mut positional = std::mem::take(&mut b.positional);
    positional.sort_unstable();
    let mut tree = Tree {
        nodes,
        fold: fold_arena,
        texts,
        exts,
        lost: lost_keys,
        inst_of_layout,
        range: layout.range().clone(),
        node_of_rec: Vec::new(),
        positional,
    };
    tree.index_records(store.record_count());
    // Embedded instances: the display path of their container.
    for ci in &mut out {
        let Some(c) = layout
            .instance(ci.layout_id)
            .and_then(|i| i.embedded_in.as_ref())
        else {
            continue;
        };
        ci.embedded_in = Some(match tree.node_of_record(c.record) {
            Some(node) => tree.path(store, node),
            None => c.name.clone().unwrap_or_default(),
        });
    }
    Ok((tree, out))
}
