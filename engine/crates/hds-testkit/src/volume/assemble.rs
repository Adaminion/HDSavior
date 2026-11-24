//! Turns a node of the model into its `FILE` records: one base record and,
//! for multi-record files, an attribute list and extension records.

use crate::facts::RecordRole;
use crate::ondisk::attr::{
    attribute_list, file_name, reparse_point, standard_information, FileNameValue, ListEntry, SiV3,
    FN_DIRECTORY, FN_VIEW_INDEX,
};
use crate::ondisk::index::{index_root_value, IndexEntry, Tree};
use crate::ondisk::record::{
    attr_type, AttrBody, AttrModel, HeaderForm, NonResident, RecordModel, END_TAIL_WINDOWS, FLAG_DIRECTORY,
    FLAG_IN_USE, FLAG_VIEW_INDEX,
};
use crate::ondisk::runlist::{total_clusters, Run};
use crate::ondisk::{next_seq, utf16, FileRef};

use super::params::{Flavor, ListForm};
use super::spec::Piece;
use super::{Node, NodeId, Source, Stream, Volume};

/// A record that does not fit its slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Overflow {
    pub slot: u64,
    pub needed: usize,
    pub record_size: usize,
}

impl std::fmt::Display for Overflow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} bytes needed, {} available", self.needed, self.record_size)
    }
}

/// One record of a node, ready to encode.
#[derive(Clone, Debug)]
pub(crate) struct Assembled {
    pub slot: u64,
    pub model: RecordModel,
    pub role: RecordRole,
    /// Entries of the `$INDEX_ROOT:$I30` this record holds.
    pub index_root: Option<Vec<IndexEntry>>,
    /// The attribute list (base record of a multi-record file): entries and
    /// encoded bytes.
    pub list: Option<(Vec<ListEntry>, Vec<u8>)>,
}

struct Item {
    holder: usize,
    attr: AttrModel,
    lowest_vcn: u64,
    index_root: Option<Vec<IndexEntry>>,
}

/// A self-relative security descriptor granting SYSTEM and Administrators
/// full access (owner and group: Administrators), as formatters put into
/// system records. 104 bytes.
pub(crate) fn security_descriptor() -> Vec<u8> {
    const ADMINS: [u8; 16] = [1, 2, 0, 0, 0, 0, 0, 5, 0x20, 0, 0, 0, 0x20, 0x02, 0, 0]; // S-1-5-32-544
    const SYSTEM: [u8; 12] = [1, 1, 0, 0, 0, 0, 0, 5, 0x12, 0, 0, 0]; // S-1-5-18
    let mut sd = Vec::with_capacity(104);
    sd.extend_from_slice(&[1, 0]); // revision, padding
    sd.extend_from_slice(&0x8004u16.to_le_bytes()); // self-relative, DACL present
    sd.extend_from_slice(&72u32.to_le_bytes()); // owner
    sd.extend_from_slice(&88u32.to_le_bytes()); // group
    sd.extend_from_slice(&0u32.to_le_bytes()); // no SACL
    sd.extend_from_slice(&20u32.to_le_bytes()); // DACL
    // ACL: revision 2, size 52, two ACEs
    sd.extend_from_slice(&[2, 0]);
    sd.extend_from_slice(&52u16.to_le_bytes());
    sd.extend_from_slice(&2u16.to_le_bytes());
    sd.extend_from_slice(&[0, 0]);
    for sid in [&SYSTEM[..], &ADMINS[..]] {
        sd.extend_from_slice(&[0, 0]); // ACCESS_ALLOWED, no flags
        sd.extend_from_slice(&((8 + sid.len()) as u16).to_le_bytes());
        sd.extend_from_slice(&0x001F_01FFu32.to_le_bytes()); // FILE_ALL_ACCESS
        sd.extend_from_slice(sid);
    }
    sd.extend_from_slice(&ADMINS);
    sd.extend_from_slice(&ADMINS);
    debug_assert_eq!(sd.len(), 104);
    sd
}

fn split_runs(runs: &[Run], pieces: &[Piece], what: &str, record: u64) -> Vec<(usize, Vec<Run>)> {
    if pieces.is_empty() {
        return vec![(0, runs.to_vec())];
    }
    let mut out = Vec::with_capacity(pieces.len());
    let mut at = 0;
    for piece in pieces {
        let take = if piece.runs == 0 { runs.len() - at } else { piece.runs };
        assert!(
            take > 0 && at + take <= runs.len(),
            "the {what} of record {record} has {} runs; its pieces ask for more",
            runs.len()
        );
        out.push((piece.holder, runs[at..at + take].to_vec()));
        at += take;
    }
    assert!(at == runs.len(), "the pieces of the {what} of record {record} leave {} runs out", runs.len() - at);
    out
}

impl Volume {
    fn name_flags(&self, node: &Node) -> u32 {
        let mut flags = node.file_attributes;
        if node.is_dir {
            flags |= FN_DIRECTORY;
        }
        if node.header_flags & FLAG_VIEW_INDEX != 0 {
            flags |= FN_VIEW_INDEX;
        }
        flags
    }

    /// The bytes of a resident stream.
    pub(crate) fn resident_bytes(&self, stream: &Stream) -> Vec<u8> {
        match &stream.source {
            Source::Spec(content) => content.to_vec(),
            Source::System(_) | Source::Unwritten => vec![0; stream.real_size as usize],
        }
    }

    fn stream_items(&self, node: &Node, stream: &Stream, items: &mut Vec<Item>) {
        let cs = self.cluster_size();
        let multi = node.multi.as_ref();
        let is_data = stream.is_unnamed_data();
        let Some(runs) = &stream.runs else {
            let holder = if is_data { multi.map_or(0, |m| m.resident_data_in) } else { 0 };
            let attr = AttrModel::resident(stream.type_code, self.resident_bytes(stream))
                .named(&stream.name)
                .with_flags(stream.flags);
            items.push(Item { holder, attr, lowest_vcn: 0, index_root: None });
            return;
        };
        let pieces = match multi {
            Some(m) if is_data => split_runs(runs, &m.data, "$DATA", node.record),
            _ => vec![(0, runs.clone())],
        };
        let clusters = total_clusters(runs);
        let stored: u64 = runs.iter().filter(|r| r.lcn.is_some()).map(|r| r.len).sum();
        let single = pieces.len() == 1;
        let mut vcn = 0u64;
        for (holder, piece_runs) in pieces {
            let len = total_clusters(&piece_runs);
            let first = vcn == 0;
            let later_sizes = is_data && node.quirks.sizes_in_later_extents && !first;
            let body = NonResident {
                lowest_vcn: vcn as i64,
                highest_vcn: if is_data && single && node.quirks.highest_vcn_zero {
                    0
                } else {
                    (vcn + len) as i64 - 1
                },
                runs: piece_runs,
                compression_unit: if stream.wide_header { 4 } else { 0 },
                allocated_size: if first {
                    clusters * cs
                } else if later_sizes {
                    len * cs
                } else {
                    0
                },
                real_size: if first {
                    stream.real_size
                } else if later_sizes {
                    len * cs - 1
                } else {
                    0
                },
                initialized_size: if first {
                    stream.initialized
                } else if later_sizes {
                    len * cs - 1
                } else {
                    0
                },
                compressed_size: (stream.wide_header && first).then_some(stored * cs),
                tail: if is_data { node.quirks.runlist_tail.clone() } else { Vec::new() },
            };
            let attr = AttrModel::non_resident(stream.type_code, body).named(&stream.name).with_flags(stream.flags);
            items.push(Item { holder, attr, lowest_vcn: vcn, index_root: None });
            vcn += len;
        }
    }

    /// Builds the records of `node`. `tree` is the folder's index as laid out
    /// by the last `finalize` (`None`: an empty index, which is also what a
    /// deleted folder has).
    pub(crate) fn assemble(&self, id: NodeId, tree: Option<&Tree>) -> Result<Vec<Assembled>, Overflow> {
        let node = self.node(id);
        let cs = self.cluster_size();
        let ibs = self.params.index_block_size;
        let rs = self.params.record_size;
        let windows = self.params.flavor != Flavor::Mkntfs;
        let multi = node.multi.as_ref();
        let holders = 1 + node.extensions.len();
        let i30 = utf16("$I30");
        let mut items: Vec<Item> = Vec::new();
        let plain = |holder: usize, attr: AttrModel| Item { holder, attr, lowest_vcn: 0, index_root: None };

        if node.has_si && !node.quirks.no_standard_information {
            let long = node.quirks.long_standard_information.unwrap_or(node.si_long);
            let v3 =
                long.then_some(SiV3 { owner_id: 0, security_id: node.security_id, quota_charged: 0, usn: 0 });
            let mut attr = AttrModel::resident(
                attr_type::STANDARD_INFORMATION,
                standard_information(&node.times, node.file_attributes, v3),
            );
            if windows {
                attr.unnamed_name_offset = Some(0);
            }
            items.push(plain(0, attr));
        }

        let names_holder = multi.map_or(0, |m| m.names_in);
        for link in &node.names {
            let parent = node.quirks.raw_parent.unwrap_or_else(|| self.parent_ref(&link.parent));
            let value = file_name(&FileNameValue {
                parent,
                times: link.times,
                // The copy in the record is written when the name is
                // created and goes stale (notes section 7).
                allocated_size: 0,
                real_size: 0,
                flags: self.name_flags(node),
                ea_or_reparse: 0,
                namespace: link.namespace,
                name: link.name.clone(),
            });
            let mut attr = AttrModel::resident(attr_type::FILE_NAME, value.clone());
            attr.body = AttrBody::Resident { value, indexed: true };
            if windows {
                attr.unnamed_name_offset = Some(0);
            }
            items.push(plain(names_holder, attr));
        }

        if node.security_descriptor {
            items.push(plain(0, AttrModel::resident(attr_type::SECURITY_DESCRIPTOR, security_descriptor())));
        }
        for attr in &node.extra_attrs {
            items.push(plain(0, attr.clone()));
        }
        for stream in &node.streams {
            self.stream_items(node, stream, &mut items);
        }

        if let Some(dir) = &node.dir {
            let root_holder = multi.map_or(0, |m| m.index_root_in);
            let (root_entries, blocks) = match tree {
                Some(t) if node.in_use => (t.root.clone(), t.blocks.len() as u64),
                _ => (vec![IndexEntry::end(None)], 0),
            };
            let value = index_root_value(attr_type::FILE_NAME, 1, ibs, cs as u32, &root_entries);
            items.push(Item {
                holder: root_holder,
                attr: AttrModel::resident(attr_type::INDEX_ROOT, value).named(&i30),
                lowest_vcn: 0,
                index_root: Some(root_entries),
            });
            if blocks > 0 {
                let clusters = total_clusters(&dir.runs);
                assert!(
                    clusters * cs >= blocks * ibs as u64,
                    "the index of record {} has {blocks} blocks but only {clusters} clusters",
                    node.record
                );
                let pieces = match multi {
                    Some(m) => split_runs(&dir.runs, &m.index_allocation, "$INDEX_ALLOCATION", node.record),
                    None => vec![(0, dir.runs.clone())],
                };
                let mut vcn = 0u64;
                for (holder, piece_runs) in pieces {
                    let len = total_clusters(&piece_runs);
                    let first = vcn == 0;
                    let body = NonResident {
                        lowest_vcn: vcn as i64,
                        highest_vcn: (vcn + len) as i64 - 1,
                        runs: piece_runs,
                        compression_unit: 0,
                        allocated_size: if first { clusters * cs } else { 0 },
                        real_size: if first { blocks * ibs as u64 } else { 0 },
                        initialized_size: if first { blocks * ibs as u64 } else { 0 },
                        compressed_size: None,
                        tail: Vec::new(),
                    };
                    let attr = AttrModel::non_resident(attr_type::INDEX_ALLOCATION, body).named(&i30);
                    items.push(Item { holder, attr, lowest_vcn: vcn, index_root: None });
                    vcn += len;
                }
                let mut bits = vec![0u8; (blocks as usize).div_ceil(64) * 8];
                for block in 0..blocks as usize {
                    bits[block / 8] |= 1 << (block % 8);
                }
                items.push(plain(root_holder, AttrModel::resident(attr_type::BITMAP, bits).named(&i30)));
            }
        }

        if let Some((tag, data)) = &node.reparse {
            items.push(plain(0, AttrModel::resident(attr_type::REPARSE_POINT, reparse_point(*tag, data))));
        }
        if node.efs {
            let mut efs = vec![0u8; 80];
            crate::content::fill_tagged(b"efs:", node.record, 0, &mut efs);
            items.push(plain(
                0,
                AttrModel::resident(attr_type::LOGGED_UTILITY_STREAM, efs).named(&utf16("$EFS")),
            ));
        }

        for item in &items {
            assert!(
                item.holder < holders,
                "record {}: an attribute is assigned to extension {} but the file has {} extension records",
                node.record,
                item.holder,
                holders - 1
            );
        }
        // Attributes are stored sorted by type, then name (notes section 4);
        // the sort is stable, so several names keep their given order.
        items.sort_by(|a, b| {
            (a.attr.type_code, &a.attr.name, a.lowest_vcn).cmp(&(b.attr.type_code, &b.attr.name, b.lowest_vcn))
        });

        // Attribute ids count up per record in storage order; the attribute
        // list takes its place (and id) between the types around 0x20.
        let has_list = multi.is_some();
        let list_position = items.iter().filter(|i| i.holder == 0 && i.attr.type_code < attr_type::ATTRIBUTE_LIST).count();
        let mut next_id = vec![0u16; holders];
        let mut base_seen = 0;
        let mut list_id = 0;
        for item in &mut items {
            if item.holder == 0 {
                if has_list && base_seen == list_position {
                    list_id = next_id[0];
                    next_id[0] += 1;
                }
                base_seen += 1;
            }
            item.attr.id = next_id[item.holder];
            next_id[item.holder] += 1;
        }
        if has_list && base_seen == list_position {
            list_id = next_id[0];
            next_id[0] += 1;
        }
        if node.quirks.reverse_name_ids {
            // all names live in one record; swap their ids end for end
            let at: Vec<usize> =
                (0..items.len()).filter(|&k| items[k].attr.type_code == attr_type::FILE_NAME).collect();
            let ids: Vec<u16> = at.iter().map(|&k| items[k].attr.id).collect();
            for (&k, &id) in at.iter().zip(ids.iter().rev()) {
                items[k].attr.id = id;
            }
        }

        let holder_ref = |holder: usize| -> FileRef {
            if holder == 0 {
                node.reference()
            } else {
                let e = node.extensions[holder - 1];
                FileRef::new(e.record, e.seq)
            }
        };
        let list = has_list.then(|| {
            let entries: Vec<ListEntry> = items
                .iter()
                .map(|i| ListEntry {
                    type_code: i.attr.type_code,
                    name: i.attr.name.clone(),
                    lowest_vcn: i.lowest_vcn,
                    holder: holder_ref(i.holder),
                    id: i.attr.id,
                })
                .collect();
            let bytes = attribute_list(&entries);
            (entries, bytes)
        });

        let header = |number: u64| {
            if node.quirks.old_header || self.params.flavor == Flavor::Win2000 {
                HeaderForm::V30
            } else {
                HeaderForm::V31 { number: number as u32, number_high: (number >> 32) as u16 }
            }
        };
        let mut records: Vec<Option<(RecordModel, Option<Vec<IndexEntry>>)>> = Vec::with_capacity(holders);
        for holder in 0..holders {
            let superseded = if holder == 0 { node.superseded } else { node.extensions[holder - 1].superseded };
            if superseded {
                records.push(None);
                continue;
            }
            let mut model = if holder == 0 {
                let mut m = RecordModel::new(node.record, node.header_seq(), rs);
                m.header = match node.quirks.claimed_number {
                    Some(claimed) if !node.quirks.old_header => {
                        HeaderForm::V31 { number: claimed as u32, number_high: (claimed >> 32) as u16 }
                    }
                    _ => header(node.record),
                };
                m.flags = node.header_flags;
                if node.is_dir && !node.quirks.no_directory_flag {
                    m.flags |= FLAG_DIRECTORY;
                }
                // Windows leaves the link count alone when it deletes a
                // file; ntfs-3g sets it to 0 (notes section 3).
                m.link_count =
                    if node.in_use || windows { node.names.len() as u16 } else { 0 };
                m.torn_blocks = node.quirks.torn_blocks.clone();
                m
            } else {
                let e = node.extensions[holder - 1];
                let mut m = RecordModel::new(e.record, if node.in_use { e.seq } else { next_seq(e.seq) }, rs);
                m.header = header(e.record);
                m.flags = 0;
                // the reference keeps the sequence the base had while in use
                m.base = node.reference();
                m.torn_blocks = node
                    .quirks
                    .torn_extension_blocks
                    .iter()
                    .filter(|(k, _)| *k == holder)
                    .flat_map(|(_, blocks)| blocks.iter().copied())
                    .collect();
                m
            };
            if node.in_use {
                model.flags |= FLAG_IN_USE;
            }
            model.lsn = node.lsn;
            model.usn = node.usn;
            model.end_tail = if windows { END_TAIL_WINDOWS } else { 0 };
            model.next_attr_id = next_id[holder];
            records.push(Some((model, None)));
        }

        for item in items {
            if let Some((model, root)) = records[item.holder].as_mut() {
                if item.index_root.is_some() {
                    *root = item.index_root;
                }
                model.attrs.push(item.attr);
            }
        }
        if let (Some((_, bytes)), Some((model, _))) = (&list, records[0].as_mut()) {
            let attr = match multi.expect("a list implies a MultiSpec").list {
                ListForm::Resident => AttrModel::resident(attr_type::ATTRIBUTE_LIST, bytes.clone()),
                ListForm::NonResident => {
                    let clusters = (bytes.len() as u64).div_ceil(cs).max(1);
                    // Before the clusters are allocated (trial fits) a
                    // stand-in run of the same shape is used.
                    let runs =
                        if node.list_runs.is_empty() { vec![Run::at(1, clusters)] } else { node.list_runs.clone() };
                    AttrModel::non_resident(
                        attr_type::ATTRIBUTE_LIST,
                        NonResident {
                            lowest_vcn: 0,
                            highest_vcn: total_clusters(&runs) as i64 - 1,
                            allocated_size: total_clusters(&runs) * cs,
                            real_size: bytes.len() as u64,
                            initialized_size: bytes.len() as u64,
                            runs,
                            compression_unit: 0,
                            compressed_size: None,
                            tail: Vec::new(),
                        },
                    )
                }
            }
            .with_id(list_id);
            model.attrs.insert(list_position, attr);
        }

        let mut out = Vec::with_capacity(holders);
        for (holder, record) in records.into_iter().enumerate() {
            let Some((model, index_root)) = record else {
                continue;
            };
            let needed = model.bytes_in_use();
            let slot = if holder == 0 { node.record } else { node.extensions[holder - 1].record };
            if needed > rs as usize {
                return Err(Overflow { slot, needed, record_size: rs as usize });
            }
            out.push(Assembled {
                slot,
                model,
                role: if holder == 0 { RecordRole::Base } else { RecordRole::Extension },
                index_root,
                list: if holder == 0 { list.clone() } else { None },
            });
        }
        Ok(out)
    }

    /// Bytes available for index entries in the `$INDEX_ROOT` of a folder:
    /// `(as long as everything fits the root, once INDX blocks are needed)`.
    pub(crate) fn root_capacities(&self, id: NodeId) -> (usize, usize) {
        let node = self.node(id);
        let root_holder = node.multi.as_ref().map_or(0, |m| m.index_root_in);
        let slot = if root_holder == 0 { node.record } else { node.extensions[root_holder - 1].record };
        let free = match self.assemble(id, None) {
            Ok(records) => records.iter().find(|a| a.slot == slot).map_or(0, |a| a.model.free_bytes()),
            Err(overflow) => panic!("record {} overflows even with an empty index: {overflow}", overflow.slot),
        };
        // the trial already contains the 16-byte end entry
        let small = free + 0x10;
        // `$INDEX_ALLOCATION` (header, name, room for a few runs) and the
        // index `$BITMAP` join the record once blocks exist
        let allocation_here = node.multi.as_ref().is_none_or(|m| m.index_allocation.iter().any(|p| p.holder == root_holder));
        let reserve = 0x28 + if allocation_here { 0x78 } else { 0 };
        let large = small.saturating_sub(reserve).max(0x18);
        (small, large)
    }
}
