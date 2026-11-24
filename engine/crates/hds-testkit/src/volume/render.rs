//! Writing a volume model into an image.

use std::collections::{BTreeMap, BTreeSet};

use crate::content::fill_tagged;
use crate::facts::{
    index_entry_facts, record_fact, ArtifactState, BootFact, IndexBlockFact, ListEntryFact,
    RecordFact, RecordRole,
};
use crate::ondisk::align8;
use crate::ondisk::attr::{file_name, FileNameValue, FN_DIRECTORY, FN_VIEW_INDEX};
use crate::ondisk::attrdef::attrdef_table;
use crate::ondisk::boot::{encode_boot_sector, BootParams};
use crate::ondisk::index::{build_tree, indx_block, indx_capacity, vcn_step, IndexEntry};
use crate::ondisk::record::{blank_model, BlankForm, FLAG_VIEW_INDEX};
use crate::ondisk::runlist::{total_clusters, Run};
use crate::ondisk::upcase::{collate, upcase_table};

use super::params::{BlankSlots, Flavor, ListForm};
use super::{NameLink, Node, NodeId, ParentRef, Slot, Source, Stream, SystemData, Volume};

/// Receives everything a volume writes. Structures a scanner looks for go
/// through their own methods so that the receiver can keep track of them.
pub(crate) trait RenderSink {
    /// Plain bytes (file content, bitmaps, logs).
    fn data(&mut self, offset: u64, bytes: &[u8]);
    /// A `FILE` record; `fact.offset` says where.
    fn record(&mut self, bytes: Vec<u8>, fact: RecordFact);
    /// An `INDX` block; `fact.offset` says where.
    fn index_block(&mut self, bytes: Vec<u8>, fact: IndexBlockFact);
    /// A boot sector; `fact.offset` says where.
    fn boot_sector(&mut self, bytes: Vec<u8>, fact: BootFact);
}

/// One record as written by the last render.
#[derive(Clone, Debug)]
pub(crate) struct RenderedRecord {
    pub offset: u64,
    pub bytes: Vec<u8>,
    /// The node the record belongs to.
    pub node: Option<usize>,
    /// Its description.
    pub fact: RecordFact,
}

/// What the last render wrote, for the manifest.
#[derive(Clone, Debug, Default)]
pub(crate) struct Rendered {
    /// Records of nodes and raw records by slot (blank slots are not listed).
    pub records: BTreeMap<u64, RenderedRecord>,
    /// Index blocks per folder: `(offset, bytes)`.
    pub index_blocks: BTreeMap<usize, Vec<(u64, Vec<u8>)>>,
    /// Allocated clusters as `(first, count)`.
    pub used_ranges: Vec<(u64, u64)>,
    /// Slots marked in use in the MFT bitmap.
    pub slots_in_use: BTreeSet<u64>,
    /// Where non-resident attribute lists were written: node, offset, bytes.
    pub lists: Vec<(usize, u64, Vec<u8>)>,
}

impl Volume {
    fn index_entry(&self, node: &Node, link: &NameLink, parent: NodeId) -> IndexEntry {
        let cs = self.cluster_size();
        let (allocated_size, real_size) = match node.data() {
            Some(Stream {
                runs: Some(runs),
                real_size,
                ..
            }) => (
                runs.iter()
                    .filter(|r| r.lcn.is_some())
                    .map(|r| r.len)
                    .sum::<u64>()
                    * cs,
                *real_size,
            ),
            Some(Stream {
                runs: None,
                real_size,
                ..
            }) => (align8(*real_size as usize) as u64, *real_size),
            None => (0, 0),
        };
        let mut flags = node.file_attributes;
        if node.is_dir {
            flags |= FN_DIRECTORY;
        }
        if node.header_flags & FLAG_VIEW_INDEX != 0 {
            flags |= FN_VIEW_INDEX;
        }
        let key = file_name(&FileNameValue {
            parent: self.node(parent).reference(),
            // the index copy is kept current (notes section 7)
            times: node.times,
            allocated_size,
            real_size,
            flags,
            ea_or_reparse: node.reparse.as_ref().map_or(0, |(tag, _)| *tag),
            namespace: link.namespace,
            name: link.name.clone(),
        });
        IndexEntry::new(node.reference(), key)
    }

    pub(crate) fn ensure_list_allocated(&mut self, id: NodeId) {
        let node = self.node(id);
        let wanted = node
            .multi
            .as_ref()
            .is_some_and(|m| m.list == ListForm::NonResident);
        if !wanted || !node.list_runs.is_empty() || node.superseded || !node.in_use {
            return;
        }
        let records = self
            .assemble(id, self.trees.get(&id.0))
            .unwrap_or_else(|e| panic!("record {} overflows: {e}", e.slot));
        let len = records
            .iter()
            .find_map(|a| a.list.as_ref().map(|(_, bytes)| bytes.len() as u64))
            .expect("a multi-record file has a list");
        let clusters = len.div_ceil(self.cluster_size()).max(1);
        let run = self.alloc.alloc(clusters);
        self.node_mut(id).list_runs = vec![run];
    }

    /// Brings everything that depends on the whole volume up to date: the
    /// B-tree of every folder, the clusters of grown indexes, the clusters
    /// of non-resident attribute lists.
    pub(crate) fn finalize(&mut self) {
        let cs = self.cluster_size();
        let ibs = self.params.index_block_size;
        let block_clusters = (ibs as u64).div_ceil(cs);
        let mut children: BTreeMap<usize, Vec<(Vec<u16>, IndexEntry)>> = BTreeMap::new();
        for node in self.nodes.iter().filter(|n| n.in_use && !n.superseded) {
            for link in &node.names {
                if let (ParentRef::Node(parent), true) = (link.parent, link.indexed) {
                    children
                        .entry(parent.0)
                        .or_default()
                        .push((link.name.clone(), self.index_entry(node, link, parent)));
                }
            }
        }
        let folders: Vec<usize> = (0..self.nodes.len())
            .filter(|&i| {
                let n = &self.nodes[i];
                n.dir.is_some() && n.in_use && !n.superseded
            })
            .collect();
        self.trees.clear();
        for d in folders {
            let mut entries = children.remove(&d).unwrap_or_default();
            entries.sort_by(|a, b| collate(&a.0, &b.0).then(a.1.child.cmp(&b.1.child)));
            let entries: Vec<IndexEntry> = entries.into_iter().map(|e| e.1).collect();
            let (small, large) = self.root_capacities(NodeId(d));
            let dir = self.nodes[d].dir.as_ref().expect("filtered");
            let step = vcn_step(ibs, cs as u32);
            let (fill, force, fragment) = (dir.fill_percent, dir.force_blocks, dir.fragment);
            let mut tree = build_tree(
                entries.clone(),
                small,
                indx_capacity(ibs),
                step,
                fill,
                force,
            );
            if !tree.blocks.is_empty() {
                tree = build_tree(entries, large, indx_capacity(ibs), step, fill, true);
            }
            let need = (tree.blocks.len() as u64 * ibs as u64).div_ceil(cs);
            let have = total_clusters(&dir.runs);
            if need > have {
                let more = need - have;
                let new_runs = if fragment && more > block_clusters {
                    // one fragment per block-sized group of clusters
                    let mut runs = Vec::new();
                    let mut left = more;
                    while left > 0 {
                        let take = left.min(block_clusters);
                        runs.extend(self.alloc.alloc_fragmented(take, 1));
                        left -= take;
                    }
                    runs
                } else {
                    vec![self.alloc.alloc(more)]
                };
                self.nodes[d]
                    .dir
                    .as_mut()
                    .expect("filtered")
                    .runs
                    .extend(new_runs);
            }
            self.trees.insert(d, tree);
            self.fit_or_split(NodeId(d));
        }
        for i in 0..self.nodes.len() {
            self.ensure_list_allocated(NodeId(i));
        }
    }

    /// Absolute offset of byte `position` of a stream stored in `runs`, and
    /// the bytes that follow it contiguously in the same run.
    fn stream_position(&self, runs: &[Run], position: u64) -> Option<(u64, u64)> {
        let cs = self.cluster_size();
        let mut start = 0u64;
        for run in runs {
            let len = run.len * cs;
            if position < start + len {
                let lcn = run.lcn?;
                return Some((
                    self.lcn_to_offset(lcn) + (position - start),
                    start + len - position,
                ));
            }
            start += len;
        }
        None
    }

    /// Writes `len` bytes of a stream, produced by `fill`, into its runs.
    fn write_runs(
        &self,
        runs: &[Run],
        len: u64,
        sink: &mut dyn RenderSink,
        fill: &mut dyn FnMut(u64, &mut [u8]),
    ) {
        const CHUNK: u64 = 1 << 20;
        let cs = self.cluster_size();
        let mut buf = vec![0u8; CHUNK.min(len.max(1)) as usize];
        let mut start = 0u64;
        for run in runs {
            let run_len = run.len * cs;
            if let Some(lcn) = run.lcn {
                let end = (start + run_len).min(len);
                let mut pos = start;
                while pos < end {
                    let take = (end - pos).min(CHUNK) as usize;
                    fill(pos, &mut buf[..take]);
                    sink.data(self.lcn_to_offset(lcn) + (pos - start), &buf[..take]);
                    pos += take as u64;
                }
            }
            start += run_len;
        }
    }

    fn system_bytes(
        &self,
        data: SystemData,
        size: u64,
        slots_in_use: &BTreeSet<u64>,
    ) -> Option<Vec<u8>> {
        Some(match data {
            SystemData::Mft | SystemData::MftMirr => return None,
            SystemData::MftBitmap => {
                let mut bits = vec![0u8; size as usize];
                for &slot in slots_in_use {
                    bits[(slot / 8) as usize] |= 1 << (slot % 8);
                }
                bits
            }
            SystemData::Bitmap => self.alloc.bitmap_bytes(),
            SystemData::Boot => {
                let mut bytes = vec![0u8; size as usize];
                fill_tagged(b"bcd:", self.serial, 0, &mut bytes);
                bytes[..512].copy_from_slice(&encode_boot_sector(&self.boot_params()));
                bytes
            }
            SystemData::UpCase => upcase_table(),
            SystemData::AttrDef => attrdef_table(),
            SystemData::Fill(byte) => vec![byte; size as usize],
            SystemData::Filler(tag) => {
                let mut bytes = vec![0u8; size as usize];
                fill_tagged(&tag, self.serial, 0, &mut bytes);
                bytes
            }
        })
    }

    fn write_stream(
        &self,
        stream: &Stream,
        sink: &mut dyn RenderSink,
        slots_in_use: &BTreeSet<u64>,
    ) {
        let Some(runs) = &stream.runs else {
            return;
        };
        let cs = self.cluster_size();
        let allocated = total_clusters(runs) * cs;
        match &stream.source {
            Source::Unwritten => {}
            Source::System(data) => {
                let Some(bytes) = self.system_bytes(*data, stream.real_size, slots_in_use) else {
                    return;
                };
                // a formatter writes whole clusters: zeros behind the data
                self.write_runs(runs, allocated, sink, &mut |pos, buf| {
                    for (i, b) in buf.iter_mut().enumerate() {
                        *b = bytes.get(pos as usize + i).copied().unwrap_or(0);
                    }
                });
            }
            Source::Spec(content) => {
                let valid = if stream.garbage {
                    0
                } else {
                    stream.initialized.min(stream.real_size)
                };
                let tag: &[u8; 4] = if stream.garbage { b"lz1:" } else { b"old:" };
                // Stale bytes are written for at most 64 KiB behind the
                // content (rounded up to 4 KiB): enough to catch a reader
                // that ignores sizes without filling 2 MiB clusters.
                let written = if stream.garbage {
                    allocated
                } else {
                    allocated.min(valid.div_ceil(4096) * 4096 + 65_536)
                };
                self.write_runs(runs, written, sink, &mut |pos, buf| {
                    // content up to the initialized size; whatever the
                    // clusters held before (here: recognisable stale bytes)
                    // from there to the end of the allocation
                    let content_len = valid.saturating_sub(pos).min(buf.len() as u64) as usize;
                    if content_len > 0 {
                        content.read_at(pos, &mut buf[..content_len]);
                    }
                    if content_len < buf.len() {
                        fill_tagged(
                            tag,
                            stream.stale_seed,
                            pos + content_len as u64,
                            &mut buf[content_len..],
                        );
                    }
                });
            }
        }
    }

    /// The fields of this volume's boot sector.
    pub fn boot_params(&self) -> BootParams {
        let p = &self.params;
        BootParams {
            bytes_per_sector: p.bytes_per_sector,
            cluster_size: p.cluster_size,
            total_sectors: p.total_sectors(),
            mft_lcn: self.plan.mft[0].lcn,
            mftmirr_lcn: self.plan.mftmirr.lcn.expect("allocated"),
            record_size: p.record_size,
            index_block_size: p.index_block_size,
            hidden_sectors: p
                .hidden_sectors
                .unwrap_or((p.offset / p.bytes_per_sector as u64) as u32),
            serial: self.serial,
        }
    }

    fn boot_fact(&self, instance: u32, offset: u64, backup: bool) -> BootFact {
        let b = self.boot_params();
        BootFact {
            offset,
            instance,
            backup,
            bytes_per_sector: b.bytes_per_sector,
            cluster_size: b.cluster_size,
            total_sectors: b.total_sectors,
            mft_lcn: b.mft_lcn,
            mftmirr_lcn: b.mftmirr_lcn,
            record_size: b.record_size,
            index_block_size: b.index_block_size,
            hidden_sectors: b.hidden_sectors,
            serial: b.serial,
            state: ArtifactState::Intact,
        }
    }

    /// Absolute offset of the backup boot sector: the last sector of the
    /// partition.
    pub fn backup_boot_offset(&self) -> u64 {
        self.params.offset + self.params.total_sectors() * self.params.bytes_per_sector as u64
    }

    /// Writes the whole volume as the model describes it now.
    pub(crate) fn render(&mut self, instance: u32, sink: &mut dyn RenderSink) {
        self.finalize();
        let rs = self.params.record_size;
        let cs = self.cluster_size();
        let ibs = self.params.index_block_size;
        let initialized = self.mft_initialized_records();
        let mut rendered = Rendered {
            used_ranges: self.alloc.used_ranges(),
            ..Rendered::default()
        };

        // Everything is assembled before anything is written, so that a
        // record overflow cannot leave half a volume behind.
        let mut records: BTreeMap<u64, (Vec<u8>, RecordFact, Option<usize>)> = BTreeMap::new();
        for i in 0..self.nodes.len() {
            let assembled = self
                .assemble(NodeId(i), self.trees.get(&i))
                .unwrap_or_else(|e| panic!("record {} overflows: {e}", e.slot));
            for a in assembled {
                let bytes = a.model.encode().expect("size was checked");
                let mut fact = record_fact(&a.model, &bytes);
                fact.offset = self.record_offset(a.slot);
                fact.instance = instance;
                fact.slot = a.slot;
                fact.role = a.role;
                fact.index_root = a.index_root.as_deref().map(index_entry_facts);
                if let (Some((entries, list_bytes)), Some(list)) =
                    (&a.list, fact.attribute_list.as_mut())
                {
                    if !list.resident {
                        list.entries = entries
                            .iter()
                            .map(|e| ListEntryFact {
                                type_code: e.type_code,
                                name: String::from_utf16_lossy(&e.name),
                                lowest_vcn: e.lowest_vcn,
                                holder: e.holder,
                                attr_id: e.id,
                            })
                            .collect();
                        let runs = &self.nodes[i].list_runs;
                        let (offset, _) = self
                            .stream_position(runs, 0)
                            .expect("the list has clusters");
                        rendered.lists.push((i, offset, list_bytes.clone()));
                    }
                }
                assert!(
                    a.slot < initialized,
                    "record {} lies beyond the initialized part of the MFT",
                    a.slot
                );
                records.insert(a.slot, (bytes, fact, Some(i)));
            }
        }
        for (&slot, raw) in &self.raw {
            let bytes = raw
                .model
                .encode()
                .unwrap_or_else(|e| panic!("raw record {slot}: {e}"));
            let mut fact = record_fact(&raw.model, &bytes);
            fact.offset = self.record_offset(slot);
            fact.instance = instance;
            fact.slot = slot;
            fact.role = RecordRole::Raw;
            records.insert(slot, (bytes, fact, None));
        }
        for (&slot, owner) in &self.slots {
            let in_use = match *owner {
                Slot::Base(id) => {
                    let n = self.node(id);
                    n.in_use && !n.superseded
                }
                Slot::Ext(id, k) => self.node(id).in_use && !self.node(id).extensions[k].superseded,
                Slot::Raw => self.raw[&slot].in_bitmap,
            };
            if in_use {
                rendered.slots_in_use.insert(slot);
            }
        }
        rendered
            .slots_in_use
            .extend(self.extra_bitmap_slots.iter().copied());

        // Stream contents in creation order: where a later file reuses the
        // clusters of a deleted one, the later bytes win, as on a real disk.
        // A non-resident attribute list is written with its file for the
        // same reason.
        for (i, node) in self.nodes.iter().enumerate() {
            for (_, offset, bytes) in rendered.lists.iter().filter(|(owner, _, _)| *owner == i) {
                let mut padded = bytes.clone();
                padded.resize(bytes.len().div_ceil(cs as usize) * cs as usize, 0);
                sink.data(*offset, &padded);
            }
            for stream in &node.streams {
                self.write_stream(stream, sink, &rendered.slots_in_use);
            }
        }

        // Boot sectors (the primary is part of $Boot's clusters, written above).
        let boot = encode_boot_sector(&self.boot_params()).to_vec();
        sink.boot_sector(
            boot.clone(),
            self.boot_fact(instance, self.params.offset, false),
        );
        if self.params.backup_boot {
            let at = self.backup_boot_offset();
            sink.boot_sector(boot, self.boot_fact(instance, at, true));
        }

        // Index blocks.
        let step = vcn_step(ibs, cs as u32);
        for (&d, tree) in &self.trees {
            let node = &self.nodes[d];
            let dir = node.dir.as_ref().expect("only folders have trees");
            let mut written = Vec::with_capacity(tree.blocks.len());
            for (i, entries) in tree.blocks.iter().enumerate() {
                let vcn = i as u64 * step;
                let bytes = indx_block(ibs, vcn, dir.lsn, dir.usn, entries);
                let (offset, contiguous) = self
                    .stream_position(&dir.runs, i as u64 * ibs as u64)
                    .unwrap_or_else(|| {
                        panic!("index block {i} of record {} has no clusters", node.record)
                    });
                assert!(
                    contiguous >= ibs as u64,
                    "index block {i} of record {} is split over two runs; give index runs in whole blocks",
                    node.record
                );
                let fact = IndexBlockFact {
                    offset,
                    size: ibs,
                    instance,
                    owner: node.reference(),
                    owner_entry: None,
                    vcn,
                    lsn: dir.lsn,
                    leaf: entries.iter().all(|e| e.subnode.is_none()),
                    entries: index_entry_facts(entries),
                    stale_copy: false,
                    state: ArtifactState::Intact,
                };
                written.push((offset, bytes.clone()));
                sink.index_block(bytes, fact);
            }
            if !written.is_empty() {
                rendered.index_blocks.insert(d, written);
            }
        }

        // The MFT, slot by slot.
        let blank = self.params.effective_blank_slots();
        let old_header = self.params.flavor == Flavor::Win2000;
        let blank_lsn = if self.params.flavor == Flavor::Win7 {
            self.lsn / 2
        } else {
            0
        };
        let mut mirror: Vec<Option<(Vec<u8>, RecordFact)>> = Vec::new();
        for slot in 0..initialized {
            let offset = self.record_offset(slot);
            let written = match records.remove(&slot) {
                Some((bytes, fact, node)) => {
                    rendered.records.insert(
                        slot,
                        RenderedRecord {
                            offset,
                            bytes: bytes.clone(),
                            node,
                            fact: fact.clone(),
                        },
                    );
                    sink.record(bytes.clone(), fact.clone());
                    Some((bytes, fact))
                }
                None => match blank {
                    BlankSlots::Untouched => None,
                    BlankSlots::Zeros => {
                        sink.data(offset, &vec![0u8; rs as usize]);
                        None
                    }
                    BlankSlots::Blank(form) => {
                        let form = if self.params.flavor == Flavor::Mkntfs
                            && self.params.blank_slots.is_none()
                            && (16..24).contains(&slot)
                        {
                            BlankForm::Mkntfs
                        } else {
                            form
                        };
                        let model = blank_model(
                            form,
                            slot,
                            rs,
                            blank_lsn,
                            self.params.format_time,
                            old_header,
                        );
                        let bytes = model.encode().expect("a blank record always fits");
                        let mut fact = record_fact(&model, &bytes);
                        fact.offset = offset;
                        fact.instance = instance;
                        fact.slot = slot;
                        fact.role = RecordRole::Blank;
                        sink.record(bytes.clone(), fact.clone());
                        Some((bytes, fact))
                    }
                },
            };
            if slot < self.plan.mirror_records {
                mirror.push(written);
            }
        }
        debug_assert!(records.is_empty());

        // $MFTMirr: copies of the first records.
        if self.params.write_mftmirr {
            let base = self.lcn_to_offset(self.plan.mftmirr.lcn.expect("allocated"));
            for (i, copy) in mirror.into_iter().enumerate() {
                let offset = base + i as u64 * rs as u64;
                match copy {
                    Some((bytes, mut fact)) => {
                        fact.offset = offset;
                        fact.role = RecordRole::Mirror;
                        sink.record(bytes, fact);
                    }
                    None => sink.data(offset, &vec![0u8; rs as usize]),
                }
            }
        }
        self.rendered = Some(rendered);
        self.dirty = false;
    }

    /// `true` when `slot` was marked in use by the last render.
    pub fn slot_in_bitmap(&self, slot: u64) -> bool {
        self.rendered
            .as_ref()
            .is_some_and(|r| r.slots_in_use.contains(&slot))
    }
}
