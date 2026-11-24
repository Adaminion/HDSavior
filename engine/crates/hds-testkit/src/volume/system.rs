//! Formatting: the layout of a fresh volume and its system files.

use std::collections::BTreeMap;

use crate::ondisk::attr::volume_information;
use crate::ondisk::index::{index_root_value, IndexEntry};
use crate::ondisk::record::{attr_type, AttrModel, FLAG_IS_4, FLAG_VIEW_INDEX};
use crate::ondisk::runlist::Run;
use crate::ondisk::utf16;
use crate::rng::{derive, Rng};
use crate::time;

use super::alloc::Allocator;
use super::params::{Flavor, VolumeParams};
use super::spec::Quirks;
use super::{
    DirState, MftExtent, NameLink, Node, NodeId, ParentRef, Plan, Slot, Source, Stream, SystemData,
    Volume,
};

/// Collation rules of the view indexes (`layout.h` `COLLATION_RULES`).
const COLLATION_ULONG: u32 = 16;
const COLLATION_SID: u32 = 17;
const COLLATION_SECURITY_HASH: u32 = 18;
const COLLATION_ULONGS: u32 = 19;

/// Size of `$Secure:$SDS` on a freshly formatted Windows 10 volume.
const SDS_BYTES: u64 = 263_268;

impl Volume {
    /// Formats a new volume: plans the layout the way the flavour's
    /// formatter does and creates the system files. Nothing is written until
    /// the volume is rendered.
    pub fn format(params: VolumeParams, seed: u64) -> Volume {
        params.validate();
        let cs = params.cluster_size as u64;
        let rs = params.record_size as u64;
        let total = params.total_clusters();
        let windows_layout = matches!(params.flavor, Flavor::Win7 | Flavor::Win10 | Flavor::WinXp);
        let mut alloc = Allocator::new(total);

        // $Boot: the first 8192 bytes
        let boot = alloc.alloc_at(0, 8192u64.div_ceil(cs));

        // $MFT, first fragment: a third into the partition (at most 3 GiB)
        // on Windows XP and later, 16 KiB on older formatters and mkntfs
        // (docs/notes/quick-format-behaviour.md section 3).
        let default_mft_lcn = if windows_layout {
            (params.partition_bytes() / 3).min(3 << 30) / cs
        } else {
            (16_384u64.div_ceil(cs)).max(2)
        };
        let mut mft = Vec::with_capacity(params.mft.len());
        let mut first_record = 0;
        let first = params.mft[0];
        let first_clusters = first.records * rs / cs;
        let first_lcn = first.lcn.unwrap_or(default_mft_lcn);
        alloc.alloc_at(first_lcn, first_clusters);
        mft.push(MftExtent {
            lcn: first_lcn,
            clusters: first_clusters,
            first_record,
            records: first.records,
        });
        first_record += first.records;

        // $MFTMirr: byte 8192 on Windows 7 and later (LCN 2 with large
        // clusters), the middle of the volume before that and on mkntfs.
        let mirror_clusters = (4 * rs).div_ceil(cs).max(1);
        let mirror_lcn = params.mftmirr_lcn.unwrap_or(match params.flavor {
            Flavor::Win7 | Flavor::Win10 => {
                if cs <= 4096 {
                    8192 / cs
                } else {
                    2
                }
            }
            Flavor::Win2000 | Flavor::WinXp | Flavor::Mkntfs => total / 2,
        });
        let mftmirr = if alloc.is_free(mirror_lcn, mirror_clusters) {
            alloc.alloc_at(mirror_lcn, mirror_clusters)
        } else {
            alloc.set_cursor(boot.len);
            alloc.alloc(mirror_clusters)
        };
        let mirror_records = mirror_clusters * cs / rs;

        // The front area: $UpCase, $AttrDef, the root index block, $Secure.
        alloc.set_cursor(boot.len);
        let upcase = alloc.alloc(131_072u64.div_ceil(cs));
        let attrdef = alloc.alloc(2560u64.div_ceil(cs));
        let root_index = alloc.alloc((params.index_block_size as u64).div_ceil(cs));
        let secure_sds = alloc.alloc(SDS_BYTES.div_ceil(cs));

        // Directly in front of the MFT, back to front: its bitmap, $Bitmap,
        // $LogFile. Where that room does not exist they go to the front area
        // (and the log to the middle, as mkntfs does).
        let front_cursor = alloc.cursor();
        let mft_bitmap_clusters = params.mft_records().div_ceil(8).div_ceil(8) * 8;
        let mft_bitmap_clusters = mft_bitmap_clusters.div_ceil(cs).max(1);
        let bitmap_clusters = (total.div_ceil(8).div_ceil(8) * 8).div_ceil(cs);
        let log_clusters = params.log_bytes.div_ceil(cs);
        let mut before = first_lcn;
        let mut place = |alloc: &mut Allocator, clusters: u64, fallback: Option<u64>| -> Run {
            if before >= clusters && alloc.is_free(before - clusters, clusters) {
                before -= clusters;
                return alloc.alloc_at(before, clusters);
            }
            before = 0;
            if let Some(lcn) = fallback {
                if alloc.is_free(lcn, clusters) {
                    return alloc.alloc_at(lcn, clusters);
                }
            }
            alloc.set_cursor(front_cursor);
            alloc.alloc(clusters)
        };
        let mft_bitmap = place(&mut alloc, mft_bitmap_clusters, None);
        let bitmap = place(&mut alloc, bitmap_clusters, None);
        let logfile = place(&mut alloc, log_clusters, Some(total / 2 + mirror_clusters));

        // Further MFT fragments.
        alloc.set_cursor(front_cursor.max(alloc.cursor()));
        for fragment in &params.mft[1..] {
            let clusters = fragment.records * rs / cs;
            let run = match fragment.lcn {
                Some(lcn) => alloc.alloc_at(lcn, clusters),
                None => alloc.alloc(clusters),
            };
            mft.push(MftExtent {
                lcn: run.lcn.expect("allocated"),
                clusters,
                first_record,
                records: fragment.records,
            });
            first_record += fragment.records;
        }
        // user data starts behind the front area
        let data_start = (0..total)
            .find(|&c| c >= front_cursor && !alloc.is_used(c))
            .unwrap_or(front_cursor);
        alloc.set_cursor(data_start);

        let plan = Plan {
            total_clusters: total,
            mft,
            mftmirr,
            mirror_records,
            logfile,
            bitmap,
            mft_bitmap,
            boot,
            root_index,
        };
        let serial = params
            .serial
            .unwrap_or_else(|| derive(seed, "serial") | 0x0100_0000_0000_0000);
        let first_user_record = params.first_user_record.unwrap_or(match params.flavor {
            Flavor::Win2000 => 24,
            Flavor::WinXp => 28,
            Flavor::Win7 => 35,
            Flavor::Win10 => 38,
            Flavor::Mkntfs => 64,
        });
        let mut volume = Volume {
            seed,
            serial,
            plan,
            alloc,
            nodes: Vec::new(),
            slots: BTreeMap::new(),
            slot_seq: BTreeMap::new(),
            raw: BTreeMap::new(),
            extra_bitmap_slots: std::collections::BTreeSet::new(),
            trees: BTreeMap::new(),
            clock: params.format_time,
            lsn: 0x10_0000 + derive(seed, "lsn") % 0x8_0000 / 8 * 8,
            next_record: first_user_record,
            first_user_record,
            rng: Rng::new(derive(seed, "volume")),
            rendered: None,
            dirty: true,
            params,
        };
        volume.create_system_files(upcase, attrdef, secure_sds);
        if volume.params.first_mount_files {
            volume.create_first_mount_files();
        }
        // user activity starts a while after the format
        volume.clock = volume.params.format_time + 10 * time::MINUTE;
        volume.next_record = volume.next_record.max(first_user_record);
        assert!(
            first_user_record <= volume.mft_records(),
            "the MFT is too small for the system files of this flavour"
        );
        volume
    }

    fn system_node(&mut self, record: u64, name: Option<(&str, u64)>, is_dir: bool) -> Node {
        let time = self.clock;
        let lsn = self.next_lsn();
        let windows = self.params.flavor != Flavor::Mkntfs;
        // The root, $Boot and the reserved records carry the short
        // $STANDARD_INFORMATION and a security descriptor of their own
        // (notes section 6; seen on mkntfs and Windows 10 alike).
        let old_style = matches!(record, 5 | 7 | 12..=15);
        let seq = if (1..16).contains(&record) {
            record as u16
        } else {
            1
        };
        self.slot_seq.insert(record, seq);
        let namespace = if record < 16 || !windows { 3 } else { 0 };
        Node {
            record,
            seq,
            in_use: true,
            superseded: false,
            is_dir,
            system: true,
            header_flags: 0,
            names: name
                .map(|(name, parent)| NameLink {
                    parent: ParentRef::Raw(crate::ondisk::FileRef::new(
                        parent,
                        if parent < 16 { parent as u16 } else { 1 },
                    )),
                    name: utf16(name),
                    namespace,
                    times: [time; 4],
                    indexed: true,
                })
                .into_iter()
                .collect(),
            times: [time; 4],
            file_attributes: 0x06,
            has_si: true,
            si_long: !old_style,
            security_id: if old_style { 0 } else { 0x100 },
            security_descriptor: old_style,
            streams: Vec::new(),
            extra_attrs: Vec::new(),
            reparse: None,
            efs: false,
            dir: is_dir.then_some(DirState {
                runs: Vec::new(),
                force_blocks: false,
                fill_percent: 100,
                fragment: false,
                lsn,
                usn: 1,
            }),
            multi: None,
            extensions: Vec::new(),
            list_runs: Vec::new(),
            quirks: Quirks::default(),
            lsn,
            usn: 2,
            tags: Vec::new(),
        }
    }

    fn push_system(&mut self, node: Node) -> NodeId {
        let id = NodeId(self.nodes.len());
        self.slots.insert(node.record, Slot::Base(id));
        self.nodes.push(node);
        id
    }

    fn system_stream(
        &self,
        name: &str,
        size: u64,
        runs: Option<Vec<Run>>,
        data: SystemData,
    ) -> Stream {
        Stream {
            type_code: attr_type::DATA,
            name: utf16(name),
            real_size: size,
            initialized: size,
            runs,
            flags: 0,
            wide_header: false,
            source: Source::System(data),
            garbage: false,
            stale_seed: 0,
        }
    }

    fn create_system_files(&mut self, upcase: Run, attrdef: Run, secure_sds: Run) {
        let cs = self.cluster_size();
        let rs = self.record_size();
        let ibs = self.params.index_block_size;
        let total = self.plan.total_clusters;

        // 0 $MFT
        let mut mft = self.system_node(0, Some(("$MFT", 5)), false);
        let mft_runs: Vec<Run> = self
            .plan
            .mft
            .iter()
            .map(|e| Run::at(e.lcn, e.clusters))
            .collect();
        let mut data =
            self.system_stream("", self.mft_records() * rs, Some(mft_runs), SystemData::Mft);
        data.initialized = self.mft_initialized_records() * rs;
        mft.streams.push(data);
        let bitmap_bytes = self.mft_records().div_ceil(8).div_ceil(8) * 8;
        let mut mft_bitmap = self.system_stream(
            "",
            bitmap_bytes,
            Some(vec![self.plan.mft_bitmap]),
            SystemData::MftBitmap,
        );
        mft_bitmap.type_code = attr_type::BITMAP;
        mft.streams.push(mft_bitmap);
        if let Some(form) = self.params.mft_attribute_list {
            // first fragment in record 0, everything else in record 15
            let fragments = self.plan.mft.len();
            assert!(
                fragments >= 2,
                "an $MFT with an attribute list needs at least two fragments"
            );
            mft.multi = Some(
                super::spec::MultiSpec::new(form)
                    .data(&[(0, 1), (1, fragments - 1)])
                    .extension_records(&[15]),
            );
            mft.extensions.push(super::ExtRecord {
                record: 15,
                seq: 15,
                superseded: false,
            });
        }
        let mft_id = self.push_system(mft);
        if self.params.mft_attribute_list.is_some() {
            self.slots.insert(15, Slot::Ext(mft_id, 0));
            self.slot_seq.insert(15, 15);
        }

        // 1 $MFTMirr
        let mut node = self.system_node(1, Some(("$MFTMirr", 5)), false);
        node.streams.push(self.system_stream(
            "",
            self.plan.mftmirr.len * cs,
            Some(vec![self.plan.mftmirr]),
            SystemData::MftMirr,
        ));
        self.push_system(node);

        // 2 $LogFile: 0xFF in every page after a format
        let mut node = self.system_node(2, Some(("$LogFile", 5)), false);
        node.streams.push(self.system_stream(
            "",
            self.plan.logfile.len * cs,
            Some(vec![self.plan.logfile]),
            SystemData::Fill(0xFF),
        ));
        self.push_system(node);

        // 3 $Volume
        let mut node = self.system_node(3, Some(("$Volume", 5)), false);
        let label = utf16(&self.params.volume_name);
        if !label.is_empty() {
            node.extra_attrs.push(AttrModel::resident(
                attr_type::VOLUME_NAME,
                crate::ondisk::utf16_bytes(&label),
            ));
        }
        let (major, minor) = if self.params.flavor == Flavor::Win2000 {
            (3, 0)
        } else {
            (3, 1)
        };
        node.extra_attrs.push(AttrModel::resident(
            attr_type::VOLUME_INFORMATION,
            volume_information(major, minor, 0),
        ));
        node.streams
            .push(self.system_stream("", 0, None, SystemData::Fill(0)));
        self.push_system(node);

        // 4 $AttrDef
        let mut node = self.system_node(4, Some(("$AttrDef", 5)), false);
        node.streams
            .push(self.system_stream("", 2560, Some(vec![attrdef]), SystemData::AttrDef));
        self.push_system(node);

        // 5 the root directory: named ".", its own parent, listed in itself
        let mut node = self.system_node(5, None, true);
        let time = self.clock;
        node.names.push(NameLink {
            parent: ParentRef::Node(NodeId(self.nodes.len())),
            name: utf16("."),
            namespace: 3,
            times: [time; 4],
            indexed: true,
        });
        let dir = node.dir.as_mut().expect("the root is a folder");
        dir.runs.push(self.plan.root_index);
        dir.force_blocks = true;
        let root = self.push_system(node);
        // the system files created so far name the root by a literal
        // reference; make them children of the node so that they are indexed
        for node in &mut self.nodes {
            for link in &mut node.names {
                if link.parent == ParentRef::Raw(crate::ondisk::FileRef::new(5, 5)) {
                    link.parent = ParentRef::Node(root);
                }
            }
        }

        // 6 $Bitmap
        let mut node = self.system_node(6, Some(("$Bitmap", 5)), false);
        let bytes = total.div_ceil(8).div_ceil(8) * 8;
        node.streams.push(self.system_stream(
            "",
            bytes,
            Some(vec![self.plan.bitmap]),
            SystemData::Bitmap,
        ));
        self.adopt_by_root(node, root);

        // 7 $Boot
        let mut node = self.system_node(7, Some(("$Boot", 5)), false);
        node.streams.push(self.system_stream(
            "",
            8192,
            Some(vec![self.plan.boot]),
            SystemData::Boot,
        ));
        self.adopt_by_root(node, root);

        // 8 $BadClus: an empty unnamed stream and the sparse $Bad stream
        // covering the volume, initialized size 0 (notes section 4)
        let mut node = self.system_node(8, Some(("$BadClus", 5)), false);
        node.streams
            .push(self.system_stream("", 0, None, SystemData::Fill(0)));
        let mut bad = self.system_stream(
            "$Bad",
            total * cs,
            Some(vec![Run::hole(total)]),
            SystemData::Fill(0),
        );
        bad.initialized = 0;
        bad.source = Source::Unwritten;
        node.streams.push(bad);
        self.adopt_by_root(node, root);

        // 9 $Secure: the $SDS stream and two empty view indexes
        let mut node = self.system_node(9, Some(("$Secure", 5)), false);
        node.header_flags = FLAG_VIEW_INDEX;
        node.file_attributes = 0x2000_0006;
        node.streams.push(self.system_stream(
            "$SDS",
            SDS_BYTES,
            Some(vec![secure_sds]),
            SystemData::Filler(*b"sds:"),
        ));
        for (name, collation) in [("$SDH", COLLATION_SECURITY_HASH), ("$SII", COLLATION_ULONG)] {
            node.extra_attrs.push(
                AttrModel::resident(
                    attr_type::INDEX_ROOT,
                    index_root_value(0, collation, ibs, cs as u32, &[IndexEntry::end(None)]),
                )
                .named(&utf16(name)),
            );
        }
        self.adopt_by_root(node, root);

        // 10 $UpCase
        let mut node = self.system_node(10, Some(("$UpCase", 5)), false);
        node.streams
            .push(self.system_stream("", 131_072, Some(vec![upcase]), SystemData::UpCase));
        self.adopt_by_root(node, root);

        // 11 $Extend
        let node = self.system_node(11, Some(("$Extend", 5)), true);
        let extend = self.adopt_by_root(node, root);

        // 12-15: reserved, in use, nameless
        for record in 12..16 {
            if record == 15 && self.params.mft_attribute_list.is_some() {
                continue;
            }
            let mut node = self.system_node(record, None, false);
            node.streams
                .push(self.system_stream("", 0, None, SystemData::Fill(0)));
            self.push_system(node);
        }

        // 24-26: $Extend\$Quota, $ObjId, $Reparse with their view indexes
        // (name and collation rule of each)
        type Views<'a> = &'a [(&'a str, u32)];
        let views: [(u64, &str, Views); 3] = [
            (
                24,
                "$Quota",
                &[("$O", COLLATION_SID), ("$Q", COLLATION_ULONG)],
            ),
            (25, "$ObjId", &[("$O", COLLATION_ULONGS)]),
            (26, "$Reparse", &[("$R", COLLATION_ULONGS)]),
        ];
        for (record, name, indexes) in views {
            let mut node = self.system_node(record, Some((name, 11)), false);
            node.header_flags = FLAG_IS_4 | FLAG_VIEW_INDEX;
            node.file_attributes = 0x2000_0006;
            for &(index, collation) in indexes {
                node.extra_attrs.push(
                    AttrModel::resident(
                        attr_type::INDEX_ROOT,
                        index_root_value(0, collation, ibs, cs as u32, &[IndexEntry::end(None)]),
                    )
                    .named(&utf16(index)),
                );
            }
            self.adopt(node, extend);
        }
    }

    /// Adds a system node whose (single) name lives in `parent`.
    fn adopt(&mut self, mut node: Node, parent: NodeId) -> NodeId {
        for link in &mut node.names {
            link.parent = ParentRef::Node(parent);
        }
        self.push_system(node)
    }

    fn adopt_by_root(&mut self, node: Node, root: NodeId) -> NodeId {
        self.adopt(node, root)
    }

    fn system_file(
        &mut self,
        record: u64,
        name: &str,
        parent: NodeId,
        is_dir: bool,
        streams: &[(&str, u64, [u8; 4])],
    ) -> NodeId {
        let cs = self.cluster_size();
        let mut node = self.system_node(record, Some((name, 0)), is_dir);
        for &(stream, size, tag) in streams {
            let resident = size <= 64;
            let runs = if resident {
                None
            } else {
                Some(vec![self.alloc.alloc(size.div_ceil(cs))])
            };
            node.streams
                .push(self.system_stream(stream, size, runs, SystemData::Filler(tag)));
        }
        self.adopt(node, parent)
    }

    /// The files Windows creates at the first mount after a format
    /// (docs/notes/quick-format-behaviour.md section 5, notes section 10).
    fn create_first_mount_files(&mut self) {
        // they appear a second after the format
        self.clock = self.params.format_time + time::SECOND;
        let root = self.root();
        let extend = self.node_at(11).expect("$Extend exists");
        let txf = self.params.txf_log_bytes;
        match self.params.flavor {
            Flavor::Win2000 | Flavor::Mkntfs => {}
            Flavor::WinXp => {
                self.user_visible_dir(27, "System Volume Information", root);
            }
            Flavor::Win7 => {
                let rm = self.system_file(27, "$RmMetadata", extend, true, &[]);
                self.system_file(
                    28,
                    "$Repair",
                    rm,
                    false,
                    &[("", 0, *b"rep:"), ("$Config", 8, *b"cfg:")],
                );
                let log = self.system_file(29, "$TxfLog", rm, true, &[]);
                self.system_file(30, "$Txf", rm, true, &[]);
                self.system_file(
                    31,
                    "$Tops",
                    log,
                    false,
                    &[("", 100, *b"top:"), ("$T", 1 << 20, *b"tpt:")],
                );
                self.system_file(32, "$TxfLog.blf", log, false, &[("", 65_536, *b"blf:")]);
                self.system_file(
                    33,
                    "$TxfLogContainer00000000000000000001",
                    log,
                    false,
                    &[("", txf, *b"tx1:")],
                );
                self.system_file(
                    34,
                    "$TxfLogContainer00000000000000000002",
                    log,
                    false,
                    &[("", txf, *b"tx2:")],
                );
            }
            Flavor::Win10 => {
                let rm = self.system_file(27, "$RmMetadata", extend, true, &[]);
                self.system_file(
                    28,
                    "$Repair",
                    rm,
                    false,
                    &[
                        ("", 0, *b"rep:"),
                        ("$Config", 8, *b"cfg:"),
                        ("$Corrupt", 2 << 20, *b"cor:"),
                        ("$Verify", 300 << 10, *b"ver:"),
                    ],
                );
                self.system_file(29, "$Deleted", extend, true, &[]);
                let log = self.system_file(30, "$TxfLog", rm, true, &[]);
                self.system_file(31, "$Txf", rm, true, &[]);
                self.system_file(
                    32,
                    "$Tops",
                    log,
                    false,
                    &[("", 100, *b"top:"), ("$T", 1 << 20, *b"tpt:")],
                );
                self.system_file(33, "$TxfLog.blf", log, false, &[("", 65_536, *b"blf:")]);
                self.system_file(
                    34,
                    "$TxfLogContainer00000000000000000001",
                    log,
                    false,
                    &[("", txf, *b"tx1:")],
                );
                self.system_file(
                    35,
                    "$TxfLogContainer00000000000000000002",
                    log,
                    false,
                    &[("", txf, *b"tx2:")],
                );
                let svi = self.user_visible_dir(36, "System Volume Information", root);
                let mut node = self.system_node(37, Some(("WPSettings.dat", 0)), false);
                node.file_attributes = 0x20;
                // an ordinary, visible file: its content is part of the
                // ground truth like that of any user file
                let mut data = self.system_stream("", 12, None, SystemData::Fill(0));
                data.source = Source::Spec(crate::content::ContentSpec::zeros(12));
                node.streams.push(data);
                self.adopt(node, svi);
            }
        }
    }

    fn user_visible_dir(&mut self, record: u64, name: &str, parent: NodeId) -> NodeId {
        let node = self.system_node(record, Some((name, 0)), true);
        self.adopt(node, parent)
    }
}
