//! A volume with a million MFT records, written straight to a file.
//!
//! **Tests the scale targets of ARCHITECTURE §5.5** (ten million records on
//! a 16 TB disk: no per-record heap allocation in the scan state, bounded
//! memory, listings in O(limit)) with something a CI machine can afford:
//! one million records, one tenth of the target, in a 5 GiB sparse file of
//! which about 1.2 GiB are written.
//!
//! The volume is the usual accident: an old Windows 10 volume,
//! quick-formatted by Windows 10. The old tree has a fixed shape so that
//! every record can be described without building the tree in memory:
//!
//! ```text
//! root ── 100 × "Area NNN" ── 20 × "Unit NN" ── 20 × "Batch NN" ── 8 … 40 files
//!                                                (every 1000th batch: 600 files)
//! ```
//!
//! Record numbers follow creation order level by level: the 100 areas are
//! records 38–137, the 2000 units 138–2137, the 40 000 batches 2138–42 137,
//! then the files, batch by batch. The format destroys records 0–255: all
//! areas and the first 118 units become lost folders that surviving index
//! blocks still name. Four in five files are resident (up to 600 bytes of
//! tagged content); the others own one or two runs in a data area that is
//! never written (their content reads as zeros), except every 500th of
//! them, which is a small valid PNG or JPEG. One file in a hundred is
//! deleted.
//!
//! [`ScaleSpec::small`] builds the same structure with about 8000
//! records and can return a full [`Manifest`], which the cross-check test
//! decodes with the independent inspector; the million-record run uses the
//! identical code path and returns only the [`ScaleManifest`] summary.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::content::{fill_tagged, ContentKind, ContentSpec};
use crate::disk::{Built, Disk};
use crate::facts::{
    index_entry_facts, record_fact, ArtifactState, IndexBlockFact, RecordFact, RecordRole,
};
use crate::image::{FileSink, Image, Sink};
use crate::manifest::{
    Entry, EntryKind, EntryName, EntryTimes, Expect, Extent, Geometry, Instance, InstanceKind,
    InstanceRole, Manifest, MftExtentFact, Presence, RecordState, Truth,
};
use crate::ondisk::attr::{file_name, standard_information, FileNameValue, SiV3, FN_DIRECTORY};
use crate::ondisk::index::{
    build_tree, index_root_value, indx_block, indx_capacity, IndexEntry, Tree,
};
use crate::ondisk::record::{
    attr_type, AttrBody, AttrModel, NonResident, RecordModel, END_TAIL_WINDOWS, FLAG_DIRECTORY,
    FLAG_IN_USE,
};
use crate::ondisk::runlist::Run;
use crate::ondisk::{utf16, FileRef};
use crate::rng::{derive, derive_n, mix64};
use crate::time;
use crate::volume::params::{Flavor, VolumeParams};

const CLUSTER: u64 = 4096;
const RECORD: u64 = 1024;
const FIRST_USER: u64 = 38;
const DESTROYED: u64 = 256;
const SLOT_CLUSTERS: u64 = 4;

/// Shape of the generated volume.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScaleSpec {
    /// Seed of names, sizes, times and contents.
    pub seed: u64,
    /// Top-level folders.
    pub areas: u64,
    /// Folders per area.
    pub units_per_area: u64,
    /// Folders per unit.
    pub batches_per_unit: u64,
    /// One batch in this many gets 600 files instead of 8 to 40.
    pub big_batch_every: u64,
}

impl ScaleSpec {
    /// About one million records.
    pub fn million(seed: u64) -> Self {
        ScaleSpec {
            seed,
            areas: 100,
            units_per_area: 20,
            batches_per_unit: 20,
            big_batch_every: 1000,
        }
    }

    /// The same structure with about 8000 records.
    pub fn small(seed: u64) -> Self {
        ScaleSpec {
            seed,
            areas: 100,
            units_per_area: 2,
            batches_per_unit: 1,
            big_batch_every: 40,
        }
    }
}

/// What kind of record a number denotes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScaleKind {
    /// A top-level folder.
    Area,
    /// A second-level folder.
    Unit,
    /// A third-level folder (holds files).
    Batch,
    /// A file.
    File,
}

/// One record of the old volume, described without looking at the image.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScaleEntry {
    /// Record number.
    pub record: u64,
    /// What it is.
    pub kind: ScaleKind,
    /// Its name.
    pub name: String,
    /// Names from the root down to it.
    pub path: Vec<String>,
    /// Record number of its parent folder (5 for an area).
    pub parent: u64,
    /// Absolute offset of its record in the image.
    pub record_offset: u64,
    /// The record survived the format (its number is 256 or more).
    pub record_intact: bool,
    /// Files: deleted before the format.
    pub deleted: bool,
    /// Files: size in bytes.
    pub size: u64,
    /// Files: the data is resident.
    pub resident: bool,
    /// Files: runs of the data.
    pub runs: Vec<Run>,
    /// Files: recipe of the content (zeros for the unwritten data area).
    pub content: Option<ContentSpec>,
    /// MFT change time.
    pub mft_changed: u64,
}

/// Summary of a generated volume.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScaleManifest {
    /// The shape it was built from.
    pub spec: ScaleSpec,
    /// Image size in bytes.
    pub image_size: u64,
    /// Bytes actually written to the image file.
    pub bytes_written: u64,
    /// Geometry of the old (lost) volume.
    pub geometry: Geometry,
    /// MFT record slots of the old volume (system slots included).
    pub mft_records: u64,
    /// Old `FILE` records present in the image (numbers 256 and up).
    pub surviving_records: u64,
    /// Old folders whose record survived.
    pub surviving_dirs: u64,
    /// Old files, deleted ones included, whose record survived (all of them).
    pub surviving_files: u64,
    /// Of those, deleted before the format.
    pub deleted_files: u64,
    /// Old folders whose record was destroyed and whose children survive.
    pub lost_folders: u64,
    /// Old `INDX` blocks in the image.
    pub index_blocks: u64,
    /// Files that are valid PNG or JPEG files.
    pub media_files: u64,
    /// `FILE` records of the current volume (records 0–255 of the new MFT
    /// and its `$MFTMirr`).
    pub current_records: u64,
    /// MFT change times of the old volume: lowest and highest.
    pub time_range: (u64, u64),
}

/// The arithmetic of a spec: which record is what, and where everything
/// lies.
#[derive(Clone, Debug)]
pub struct ScaleLayout {
    spec: ScaleSpec,
    units: u64,
    batches: u64,
    files: u64,
    /// Number of the first file of every batch, plus the total at the end.
    first_file: Vec<u64>,
    mft_lcn: u64,
    mft_clusters: u64,
    index_lcn: u64,
    data_front: (u64, u64),
    data_back: u64,
    partition_bytes: u64,
}

impl ScaleLayout {
    /// Works out the layout of a spec.
    pub fn new(spec: &ScaleSpec) -> Self {
        let units = spec.areas * spec.units_per_area;
        let batches = units * spec.batches_per_unit;
        let mut first_file = Vec::with_capacity(batches as usize + 1);
        let mut total = 0u64;
        for j in 0..batches {
            first_file.push(total);
            total += if j % spec.big_batch_every == spec.big_batch_every - 1 {
                600
            } else {
                8 + mix64(spec.seed ^ (j << 20)) % 33
            };
        }
        first_file.push(total);
        let records = FIRST_USER + spec.areas + units + batches + total;
        let mft_records = records.div_ceil(64) * 64;
        // room for everything: MFT, index blocks, a data slot per file
        let partition_bytes = ((mft_records * 5000).max(96 << 20)).div_ceil(1 << 20) * (1 << 20);
        let mft_lcn = (partition_bytes / 3).min(3 << 30) / CLUSTER;
        let mft_clusters = mft_records * RECORD / CLUSTER;
        let index_lcn = mft_lcn + mft_clusters;
        // index blocks: at most one per 30 entries plus one per folder
        let index_clusters =
            (total + spec.areas + units + batches) / 30 + spec.areas + units + batches + 64;
        let front_start = (16 << 20) / CLUSTER;
        let front_end = mft_lcn - (8 << 20) / CLUSTER;
        ScaleLayout {
            spec: spec.clone(),
            units,
            batches,
            files: total,
            first_file,
            mft_lcn,
            mft_clusters,
            index_lcn,
            data_front: (front_start, (front_end - front_start) / SLOT_CLUSTERS),
            data_back: index_lcn + index_clusters,
            partition_bytes,
        }
    }

    /// Size of the image in bytes.
    pub fn image_size(&self) -> u64 {
        self.partition_bytes
    }

    /// Number of the first record that is a file.
    pub fn first_file_record(&self) -> u64 {
        FIRST_USER + self.spec.areas + self.units + self.batches
    }

    /// One past the highest record number in use.
    pub fn end_record(&self) -> u64 {
        self.first_file_record() + self.files
    }

    fn record_offset(&self, record: u64) -> u64 {
        self.mft_lcn * CLUSTER + record * RECORD
    }

    fn kind(&self, record: u64) -> ScaleKind {
        let n = record - FIRST_USER;
        if n < self.spec.areas {
            ScaleKind::Area
        } else if n < self.spec.areas + self.units {
            ScaleKind::Unit
        } else if n < self.spec.areas + self.units + self.batches {
            ScaleKind::Batch
        } else {
            ScaleKind::File
        }
    }

    fn area_record(&self, area: u64) -> u64 {
        FIRST_USER + area
    }
    fn unit_record(&self, unit: u64) -> u64 {
        FIRST_USER + self.spec.areas + unit
    }
    fn batch_record(&self, batch: u64) -> u64 {
        FIRST_USER + self.spec.areas + self.units + batch
    }

    fn batch_of_file(&self, file: u64) -> u64 {
        (self.first_file.partition_point(|&f| f <= file) - 1) as u64
    }

    fn times(&self, record: u64) -> [u64; 4] {
        // one record every 30 seconds from 2019 on; the change time is the
        // one that orders the records
        let created = time::ymd(2019, 1, 1) + record * 30 * time::SECOND;
        let jitter = mix64(self.spec.seed ^ record) % (20 * time::SECOND);
        [
            created,
            created.saturating_sub(jitter * 1000),
            created + jitter,
            created + 2 * jitter,
        ]
    }

    fn lsn(&self, record: u64) -> u64 {
        0x40_0000 + record * 0x1C8
    }

    fn file_deleted(&self, file: u64) -> bool {
        mix64(self.spec.seed ^ 0xDE1E_7ED0 ^ file).is_multiple_of(100)
    }

    /// `(size, slot)` of a file: `slot` is `Some` for the non-resident ones.
    fn file_shape(&self, file: u64) -> (u64, Option<u64>) {
        let h = mix64(self.spec.seed ^ 0xF11E ^ (file << 8));
        if !file.is_multiple_of(5) {
            (h % 601, None)
        } else {
            let slot = file / 5;
            let clusters = 1 + (h >> 8) % SLOT_CLUSTERS;
            // media files are never tiny: every format has a minimum size
            let cut = if self.media_kind(file).is_some() {
                (h >> 16) % 1000
            } else {
                (h >> 16) % CLUSTER
            };
            (clusters * CLUSTER - cut, Some(slot))
        }
    }

    fn slot_lcn(&self, slot: u64) -> u64 {
        if slot < self.data_front.1 {
            self.data_front.0 + slot * SLOT_CLUSTERS
        } else {
            self.data_back + (slot - self.data_front.1) * SLOT_CLUSTERS
        }
    }

    fn media_kind(&self, file: u64) -> Option<ContentKind> {
        (file.is_multiple_of(2500)).then_some(if (file / 2500).is_multiple_of(2) {
            ContentKind::Png
        } else {
            ContentKind::Jpeg
        })
    }

    fn file_runs(&self, file: u64) -> Vec<Run> {
        let (size, slot) = self.file_shape(file);
        let Some(slot) = slot else {
            return Vec::new();
        };
        let lcn = self.slot_lcn(slot);
        let clusters = size.div_ceil(CLUSTER);
        if clusters >= 3 && self.media_kind(file).is_none() {
            // two runs: the tail lies in front of the head inside the slot
            vec![Run::at(lcn + 1, clusters - 1), Run::at(lcn, 1)]
        } else {
            vec![Run::at(lcn, clusters)]
        }
    }

    fn file_content(&self, file: u64) -> ContentSpec {
        let (size, slot) = self.file_shape(file);
        match (slot, self.media_kind(file)) {
            (None, _) => ContentSpec::tagged(derive_n(self.spec.seed, file), size),
            (Some(_), Some(kind)) => ContentSpec::media(kind, derive_n(self.spec.seed, file), size),
            (Some(_), None) => ContentSpec::zeros(size),
        }
    }

    fn name(&self, record: u64) -> String {
        let n = record - FIRST_USER;
        match self.kind(record) {
            ScaleKind::Area => format!("Area {n:03}"),
            ScaleKind::Unit => format!(
                "Unit {:02}",
                (n - self.spec.areas) % self.spec.units_per_area
            ),
            ScaleKind::Batch => {
                format!(
                    "Batch {:02}",
                    (n - self.spec.areas - self.units) % self.spec.batches_per_unit
                )
            }
            ScaleKind::File => {
                let file = record - self.first_file_record();
                let ext = match self.media_kind(file) {
                    Some(kind) => kind.extension().expect("media kind"),
                    None => ["dat", "txt", "log", "db", "bin"][(file % 5) as usize],
                };
                format!("f-{file:07}.{ext}")
            }
        }
    }

    fn parent(&self, record: u64) -> u64 {
        let n = record - FIRST_USER;
        match self.kind(record) {
            ScaleKind::Area => 5,
            ScaleKind::Unit => self.area_record((n - self.spec.areas) / self.spec.units_per_area),
            ScaleKind::Batch => {
                self.unit_record((n - self.spec.areas - self.units) / self.spec.batches_per_unit)
            }
            ScaleKind::File => {
                self.batch_record(self.batch_of_file(record - self.first_file_record()))
            }
        }
    }

    /// Describes record `record` (38 ≤ record < [`ScaleLayout::end_record`]).
    pub fn entry(&self, record: u64) -> ScaleEntry {
        assert!(
            (FIRST_USER..self.end_record()).contains(&record),
            "record {record} is not a user record"
        );
        let kind = self.kind(record);
        let mut path = vec![self.name(record)];
        let mut at = record;
        while self.parent(at) != 5 {
            at = self.parent(at);
            path.push(self.name(at));
        }
        path.reverse();
        let file = (kind == ScaleKind::File).then(|| record - self.first_file_record());
        let shape = file.map(|f| self.file_shape(f));
        ScaleEntry {
            record,
            kind,
            name: self.name(record),
            path,
            parent: self.parent(record),
            record_offset: self.record_offset(record),
            record_intact: record >= DESTROYED,
            deleted: file.is_some_and(|f| self.file_deleted(f)),
            size: shape.map_or(0, |s| s.0),
            resident: shape.is_some_and(|s| s.1.is_none()),
            runs: file.map(|f| self.file_runs(f)).unwrap_or_default(),
            content: file.map(|f| self.file_content(f)),
            mft_changed: self.times(record)[2],
        }
    }
}

struct Collector {
    records: Vec<RecordFact>,
    index_blocks: Vec<IndexBlockFact>,
}

struct Writer<'a> {
    layout: &'a ScaleLayout,
    sink: &'a mut dyn Sink,
    collect: Option<Collector>,
    next_index_cluster: u64,
    index_blocks: u64,
}

impl Writer<'_> {
    fn header(&self, record: u64, in_use: bool, dir: bool) -> RecordModel {
        let mut m = RecordModel::new(record, if in_use { 1 } else { 2 }, RECORD as u32);
        m.flags = if in_use { FLAG_IN_USE } else { 0 } | if dir { FLAG_DIRECTORY } else { 0 };
        m.link_count = 1;
        m.lsn = self.layout.lsn(record);
        m.usn = 2;
        m.end_tail = END_TAIL_WINDOWS;
        m
    }

    fn name_value(&self, record: u64, index_copy: bool) -> Vec<u8> {
        let l = self.layout;
        let dir = l.kind(record) != ScaleKind::File;
        let times = l.times(record);
        let (allocated, real) = if dir || !index_copy {
            (0, 0)
        } else {
            let file = record - l.first_file_record();
            let (size, slot) = l.file_shape(file);
            (
                if slot.is_some() {
                    size.div_ceil(CLUSTER) * CLUSTER
                } else {
                    size.div_ceil(8) * 8
                },
                size,
            )
        };
        let parent = l.parent(record);
        file_name(&FileNameValue {
            parent: FileRef::new(parent, if parent == 5 { 5 } else { 1 }),
            times: if index_copy { times } else { [times[0]; 4] },
            allocated_size: allocated,
            real_size: real,
            flags: if dir { FN_DIRECTORY } else { 0x20 },
            ea_or_reparse: 0,
            namespace: 0,
            name: utf16(&l.name(record)),
        })
    }

    fn common_attrs(&self, record: u64, model: &mut RecordModel) {
        let l = self.layout;
        let dir = l.kind(record) != ScaleKind::File;
        let v3 = SiV3 {
            owner_id: 0,
            security_id: 0x107,
            quota_charged: 0,
            usn: 0,
        };
        let mut si = AttrModel::resident(
            attr_type::STANDARD_INFORMATION,
            standard_information(&l.times(record), if dir { 0 } else { 0x20 }, Some(v3)),
        );
        si.unnamed_name_offset = Some(0);
        let mut name = AttrModel::resident(attr_type::FILE_NAME, Vec::new()).with_id(1);
        name.body = AttrBody::Resident {
            value: self.name_value(record, false),
            indexed: true,
        };
        name.unnamed_name_offset = Some(0);
        model.attrs.push(si);
        model.attrs.push(name);
    }

    fn emit_record(
        &mut self,
        model: &RecordModel,
        role: RecordRole,
        index_root: Option<&[IndexEntry]>,
        out: &mut Vec<u8>,
    ) {
        let bytes = model
            .encode()
            .unwrap_or_else(|e| panic!("scale record: {e}"));
        if let Some(c) = self.collect.as_mut() {
            let number = match model.header {
                crate::ondisk::record::HeaderForm::V31 { number, .. } => number as u64,
                crate::ondisk::record::HeaderForm::V30 => {
                    unreachable!("scale records are numbered")
                }
            };
            if number >= DESTROYED {
                let mut fact = record_fact(model, &bytes);
                fact.offset = self.layout.record_offset(number);
                fact.slot = number;
                fact.role = role;
                fact.index_root = index_root.map(index_entry_facts);
                c.records.push(fact);
            }
        }
        out.extend_from_slice(&bytes);
    }

    /// Writes the index blocks of one folder and returns the entries of its
    /// index root and the run of its index allocation.
    fn write_index(
        &mut self,
        owner: FileRef,
        entries: Vec<IndexEntry>,
        skip_first_block: bool,
    ) -> io::Result<(Tree, Option<Run>)> {
        let mut tree = build_tree(entries.clone(), 600, indx_capacity(4096), 1, 100, false);
        if tree.blocks.is_empty() {
            return Ok((tree, None));
        }
        tree = build_tree(entries, 440, indx_capacity(4096), 1, 100, true);
        let run = Run::at(self.next_index_cluster, tree.blocks.len() as u64);
        self.next_index_cluster += run.len;
        for (i, block) in tree.blocks.iter().enumerate() {
            if skip_first_block && i == 0 {
                continue;
            }
            let lsn = self.layout.lsn(owner.number) + 0x40;
            let bytes = indx_block(4096, i as u64, lsn, 3, block);
            let offset = (run.lcn.expect("allocated") + i as u64) * CLUSTER;
            if let Some(c) = self.collect.as_mut() {
                c.index_blocks.push(IndexBlockFact {
                    offset,
                    size: 4096,
                    instance: 0,
                    owner,
                    owner_entry: None,
                    vcn: i as u64,
                    lsn,
                    leaf: block.iter().all(|e| e.subnode.is_none()),
                    entries: index_entry_facts(block),
                    stale_copy: false,
                    state: ArtifactState::Intact,
                });
            }
            self.sink.put(offset, &bytes)?;
            self.index_blocks += 1;
        }
        Ok((tree, Some(run)))
    }

    fn folder_record(
        &mut self,
        record: u64,
        children: Vec<u64>,
        out: &mut Vec<u8>,
    ) -> io::Result<()> {
        let entries: Vec<IndexEntry> = children
            .into_iter()
            .map(|child| IndexEntry::new(FileRef::new(child, 1), self.name_value(child, true)))
            .collect();
        let (tree, run) = self.write_index(FileRef::new(record, 1), entries, false)?;
        let mut model = self.header(record, true, true);
        self.common_attrs(record, &mut model);
        let i30 = utf16("$I30");
        model.attrs.push(
            AttrModel::resident(
                attr_type::INDEX_ROOT,
                index_root_value(attr_type::FILE_NAME, 1, 4096, 4096, &tree.root),
            )
            .named(&i30)
            .with_id(2),
        );
        if let Some(run) = run {
            let blocks = run.len;
            model.attrs.push(
                AttrModel::non_resident(
                    attr_type::INDEX_ALLOCATION,
                    NonResident {
                        lowest_vcn: 0,
                        highest_vcn: blocks as i64 - 1,
                        runs: vec![run],
                        compression_unit: 0,
                        allocated_size: blocks * CLUSTER,
                        real_size: blocks * CLUSTER,
                        initialized_size: blocks * CLUSTER,
                        compressed_size: None,
                        tail: Vec::new(),
                    },
                )
                .named(&i30)
                .with_id(3),
            );
            let mut bits = vec![0u8; (blocks as usize).div_ceil(64) * 8];
            for b in 0..blocks as usize {
                bits[b / 8] |= 1 << (b % 8);
            }
            model.attrs.push(
                AttrModel::resident(attr_type::BITMAP, bits)
                    .named(&i30)
                    .with_id(4),
            );
        }
        model.next_attr_id = model.attrs.len() as u16;
        self.emit_record(&model, RecordRole::Base, Some(&tree.root), out);
        Ok(())
    }

    fn file_record(&mut self, record: u64, out: &mut Vec<u8>) -> io::Result<()> {
        let l = self.layout;
        let file = record - l.first_file_record();
        let (size, slot) = l.file_shape(file);
        let mut model = self.header(record, !l.file_deleted(file), false);
        self.common_attrs(record, &mut model);
        let data = match slot {
            None => {
                let mut value = vec![0u8; size as usize];
                fill_tagged(b"hds:", derive_n(l.spec.seed, file), 0, &mut value);
                AttrModel::resident(attr_type::DATA, value)
            }
            Some(_) => {
                let runs = l.file_runs(file);
                let clusters: u64 = runs.iter().map(|r| r.len).sum();
                if let Some(kind) = l.media_kind(file) {
                    let content =
                        ContentSpec::media(kind, derive_n(l.spec.seed, file), size).to_vec();
                    self.sink
                        .put(runs[0].lcn.expect("allocated") * CLUSTER, &content)?;
                }
                AttrModel::non_resident(
                    attr_type::DATA,
                    NonResident {
                        lowest_vcn: 0,
                        highest_vcn: clusters as i64 - 1,
                        runs,
                        compression_unit: 0,
                        allocated_size: clusters * CLUSTER,
                        real_size: size,
                        initialized_size: size,
                        compressed_size: None,
                        tail: Vec::new(),
                    },
                )
            }
        };
        model.attrs.push(data.with_id(2));
        model.next_attr_id = 3;
        self.emit_record(&model, RecordRole::Base, None, out);
        Ok(())
    }
}

fn geometry(l: &ScaleLayout, boot: &crate::ondisk::boot::BootParams) -> Geometry {
    Geometry {
        volume_offset: 0,
        cluster_size: CLUSTER as u32,
        bytes_per_sector: 512,
        record_size: RECORD as u32,
        index_block_size: 4096,
        total_sectors: boot.total_sectors,
        total_clusters: boot.total_sectors * 512 / CLUSTER,
        partition_bytes: l.partition_bytes,
        hidden_sectors: 0,
        serial: derive(l.spec.seed, "old serial"),
        mft_extents: vec![MftExtentFact {
            lcn: l.mft_lcn,
            clusters: l.mft_clusters,
            first_record: 0,
            records: l.mft_clusters * CLUSTER / RECORD,
            offset: l.mft_lcn * CLUSTER,
        }],
        mftmirr_lcn: boot.mftmirr_lcn,
        mftmirr_clusters: (4 * RECORD).div_ceil(CLUSTER),
        mft_records: l.mft_clusters * CLUSTER / RECORD,
        mft_initialized_records: l.mft_clusters * CLUSTER / RECORD,
    }
}

/// Writes the volume of `spec` to `sink`. With `collect` the full manifest
/// is built as well (only sensible for small specs).
pub fn write_scale(
    spec: &ScaleSpec,
    sink: &mut dyn Sink,
    collect: bool,
) -> io::Result<(ScaleManifest, Option<Manifest>)> {
    let layout = ScaleLayout::new(spec);
    let l = &layout;
    // The current volume: an ordinary quick format, rendered in memory.
    let mut overlay = Disk::new("scale", spec.seed, l.partition_bytes);
    let params = VolumeParams::new(0, l.partition_bytes)
        .label("current")
        .flavor(Flavor::Win10)
        .formatted_at(l.times(l.end_record())[2] + time::DAY)
        .mft_capacity(256);
    let current = overlay.format(params);
    assert_eq!(
        overlay.volume_ref(current).mft_extents()[0].lcn,
        l.mft_lcn,
        "old and new MFT must coincide"
    );
    overlay.render(current);
    let boot = overlay.volume_ref(current).boot_params();
    let Built {
        image: overlay_image,
        manifest: overlay_manifest,
    } = overlay.finish();
    let mut written = 0u64;
    // Whole pages of the new volume go first; the old structures that
    // survive are written afterwards, on top of the pages' unused parts.
    for (offset, page) in overlay_image.pages() {
        sink.put(offset, page)?;
        written += page.len() as u64;
    }

    let mut w = Writer {
        layout: l,
        sink,
        collect: collect.then(|| Collector {
            records: Vec::new(),
            index_blocks: Vec::new(),
        }),
        next_index_cluster: l.index_lcn,
        index_blocks: 0,
    };
    // The old root's index: block 0 was where the new root index block is
    // now; its later blocks survive in the index area.
    let root_entries: Vec<IndexEntry> = (0..spec.areas)
        .map(|a| {
            IndexEntry::new(
                FileRef::new(l.area_record(a), 1),
                w.name_value(l.area_record(a), true),
            )
        })
        .collect();
    w.write_index(FileRef::new(5, 5), root_entries, true)?;

    // Folders: their records are collected and written in one piece.
    let folders = spec.areas + l.units + l.batches;
    let mut folder_bytes: Vec<u8> = Vec::with_capacity((folders * RECORD) as usize);
    for record in FIRST_USER..FIRST_USER + folders {
        let n = record - FIRST_USER;
        let children: Vec<u64> = match l.kind(record) {
            ScaleKind::Area => (0..spec.units_per_area)
                .map(|i| l.unit_record(n * spec.units_per_area + i))
                .collect(),
            ScaleKind::Unit => {
                let unit = n - spec.areas;
                (0..spec.batches_per_unit)
                    .map(|i| l.batch_record(unit * spec.batches_per_unit + i))
                    .collect()
            }
            ScaleKind::Batch => {
                let batch = (n - spec.areas - l.units) as usize;
                (l.first_file[batch]..l.first_file[batch + 1])
                    .filter(|&f| !l.file_deleted(f))
                    .map(|f| l.first_file_record() + f)
                    .collect()
            }
            ScaleKind::File => unreachable!("folders come first"),
        };
        w.folder_record(record, children, &mut folder_bytes)?;
    }
    let skip = ((DESTROYED - FIRST_USER) * RECORD) as usize;
    w.sink.put(
        l.record_offset(DESTROYED),
        &folder_bytes[skip.min(folder_bytes.len())..],
    )?;
    written += (folder_bytes.len() - skip.min(folder_bytes.len())) as u64;
    drop(folder_bytes);

    // Files, a few thousand records at a time.
    let mut chunk: Vec<u8> = Vec::with_capacity(4096 * RECORD as usize);
    let mut chunk_start = l.first_file_record();
    let mut deleted = 0u64;
    let mut media = 0u64;
    for record in l.first_file_record()..l.end_record() {
        let file = record - l.first_file_record();
        deleted += u64::from(l.file_deleted(file));
        if l.file_shape(file).1.is_some() && l.media_kind(file).is_some() {
            media += 1;
            written += l.file_shape(file).0;
        }
        w.file_record(record, &mut chunk)?;
        if chunk.len() >= 4096 * RECORD as usize || record + 1 == l.end_record() {
            w.sink.put(l.record_offset(chunk_start), &chunk)?;
            written += chunk.len() as u64;
            chunk_start = record + 1;
            chunk.clear();
        }
    }
    written += w.index_blocks * 4096;
    assert!(w.next_index_cluster <= l.data_back, "index area overflow");
    assert!(
        l.slot_lcn(l.files / 5 + 1) + SLOT_CLUSTERS < (l.partition_bytes - (2 << 20)) / CLUSTER,
        "data area overflow"
    );

    let surviving_dirs = folders - (DESTROYED - FIRST_USER).min(folders);
    let summary = ScaleManifest {
        spec: spec.clone(),
        image_size: l.partition_bytes,
        bytes_written: written,
        geometry: geometry(l, &boot),
        mft_records: l.mft_clusters * CLUSTER / RECORD,
        surviving_records: l.end_record() - DESTROYED,
        surviving_dirs,
        surviving_files: l.files,
        deleted_files: deleted,
        lost_folders: (DESTROYED - FIRST_USER).min(folders),
        index_blocks: w.index_blocks,
        media_files: media,
        current_records: overlay_manifest.records.len() as u64,
        time_range: (
            l.times(DESTROYED)[2],
            l.times(l.end_record() - 1)[2] + 20 * time::SECOND,
        ),
    };
    let full = w
        .collect
        .take()
        .map(|c| full_manifest(l, &summary, overlay_manifest, c));
    Ok((summary, full))
}

fn full_manifest(
    l: &ScaleLayout,
    summary: &ScaleManifest,
    mut current: Manifest,
    c: Collector,
) -> Manifest {
    // instance 0: the old volume; the current volume's facts follow
    let shift = |instance: u32| instance + 1;
    let old = Instance {
        id: 0,
        label: "old".into(),
        role: InstanceRole::Lost,
        expected_kind: Some(InstanceKind::Main),
        generation: 1,
        flavor: Flavor::Win10,
        geometry: summary.geometry.clone(),
        format_time: time::ymd(2019, 1, 1),
        embedded_in: None,
        offset_in_host: None,
        live_allocated: Vec::new(),
        live_slots: Vec::new(),
        time_range: Some(summary.time_range),
    };
    // where the resident values lie inside the records that were written
    let resident_values: std::collections::BTreeMap<u64, (u32, u32)> = c
        .records
        .iter()
        .filter_map(|r| Some((r.slot, r.data.first()?.resident?)))
        .collect();
    let mut entries: Vec<Entry> = Vec::new();
    for record in FIRST_USER..l.end_record() {
        let e = l.entry(record);
        let times = l.times(record);
        let intact = e.record_intact;
        let extents: Vec<Extent> = {
            let mut vcn = 0;
            e.runs
                .iter()
                .map(|r| {
                    let x = Extent {
                        vcn,
                        clusters: r.len,
                        lcn: r.lcn,
                        offset: r.lcn.map(|l| l * CLUSTER),
                        bytes: r.len * CLUSTER,
                    };
                    vcn += r.len;
                    x
                })
                .collect()
        };
        let is_file = e.kind == ScaleKind::File;
        entries.push(Entry {
            id: (record - FIRST_USER) as u32,
            instance: 0,
            kind: if is_file {
                EntryKind::File
            } else {
                EntryKind::Dir
            },
            system: false,
            path: e.path.join("/"),
            components: e.path.clone(),
            record,
            sequence: 1,
            parent: (e.parent != 5).then(|| (e.parent - FIRST_USER) as u32),
            names: vec![EntryName {
                name: e.name.clone(),
                name_utf16: utf16(&e.name),
                namespace: 0,
                parent: FileRef::new(e.parent, if e.parent == 5 { 5 } else { 1 }),
                holder_record: record,
                attr_id: 1,
            }],
            record_offset: Some(e.record_offset),
            extension_records: Vec::new(),
            size: e.size,
            allocated_size: extents.iter().map(|x| x.bytes).sum(),
            initialized_size: e.size,
            resident: e.resident,
            resident_at: resident_values
                .get(&record)
                .map(|&(value, len)| (e.record_offset, value, len)),
            extents,
            data_flags: 0,
            streams: Vec::new(),
            reparse_tag: None,
            file_attributes: if is_file { 0x20 } else { 0 },
            times: EntryTimes {
                created: times[0],
                modified: times[1],
                mft_changed: times[2],
                accessed: times[3],
            },
            lsn: l.lsn(record),
            sha256: e.content.as_ref().map(ContentSpec::sha256),
            content: e.content,
            deleted: e.deleted,
            truth: Truth {
                record_state: if intact {
                    RecordState::Intact
                } else {
                    RecordState::Destroyed
                },
                record_intact: intact,
                lost_extension_records: Vec::new(),
                content_intact: is_file.then_some(intact),
                ancestors_intact: {
                    let mut at = record;
                    let mut ok = true;
                    while l.parent(at) != 5 {
                        at = l.parent(at);
                        ok &= at >= DESTROYED;
                    }
                    ok
                },
                record_in_range: true,
                data_in_range: true,
                surviving_index_entries: 0,
            },
            expect: Expect {
                presence: if intact {
                    Presence::Shown
                } else {
                    Presence::Gone
                },
                instance: None,
                path: None,
                flags_set: Vec::new(),
                flags_clear: Vec::new(),
            },
            tags: Vec::new(),
        });
    }
    let entry_shift = entries.len() as u32;
    let mut records = c.records;
    for r in &mut records {
        r.entry = Some((r.slot - FIRST_USER) as u32);
    }
    let mut index_blocks = c.index_blocks;
    for b in &mut index_blocks {
        b.owner_entry = (b.owner.number != 5).then(|| (b.owner.number - FIRST_USER) as u32);
    }
    for instance in &mut current.instances {
        instance.id = shift(instance.id);
    }
    for e in &mut current.entries {
        e.id += entry_shift;
        e.instance = shift(e.instance);
        e.parent = e.parent.map(|p| p + entry_shift);
    }
    for r in &mut current.records {
        r.instance = shift(r.instance);
        r.entry = r.entry.map(|e| e + entry_shift);
    }
    for b in &mut current.index_blocks {
        b.instance = shift(b.instance);
        b.owner_entry = b.owner_entry.map(|e| e + entry_shift);
    }
    for b in &mut current.boot_sectors {
        b.instance = shift(b.instance);
    }
    let mut instances = vec![old];
    instances.append(&mut current.instances);
    entries.append(&mut current.entries);
    records.append(&mut current.records);
    records.sort_by_key(|r| r.offset);
    index_blocks.append(&mut current.index_blocks);
    index_blocks.sort_by_key(|b| b.offset);
    let mut m = Manifest::blank("scale", l.partition_bytes);
    m.description =
        "Old volume with a fixed tree shape, quick-formatted; see hds_testkit::scenarios::scale."
            .into();
    m.rules = vec!["A4".into(), "A8".into()];
    m.seed = l.spec.seed;
    m.instances = instances;
    m.entries = entries;
    m.records = records;
    m.index_blocks = index_blocks;
    m.boot_sectors = current.boot_sectors;
    m.notes = vec![
        "Health flags that follow from an entry alone (deleted, sparse, ...) are not stated for this image; \
         counts are in ScaleManifest."
            .into(),
    ];
    m
}

/// Writes the million-record volume to `path` (a sparse file of 5 GiB, of
/// which about 1.2 GiB are written) and returns its summary.
pub fn million_records(path: &Path, seed: u64) -> io::Result<ScaleManifest> {
    scale_to_file(path, &ScaleSpec::million(seed))
}

/// Writes the volume of `spec` to `path`.
pub fn scale_to_file(path: &Path, spec: &ScaleSpec) -> io::Result<ScaleManifest> {
    let layout = ScaleLayout::new(spec);
    let mut sink = FileSink::create(path, layout.image_size())?;
    let (mut summary, _) = write_scale(spec, &mut sink, false)?;
    summary.bytes_written = sink.finish()?;
    Ok(summary)
}

/// Builds the volume of `spec` in memory with its full manifest. Meant for
/// [`ScaleSpec::small`]: a million records need gigabytes this way.
pub fn scale_in_memory(spec: &ScaleSpec) -> Built {
    let layout = ScaleLayout::new(spec);
    let mut image = Image::new(layout.image_size());
    let (_, manifest) = write_scale(spec, &mut image, true).expect("writing to memory cannot fail");
    let mut manifest = manifest.expect("collected");
    let undecided =
        crate::expect::derive(&mut manifest, &image, &std::collections::BTreeMap::new());
    assert!(
        undecided.is_empty(),
        "the scale volume leaves {undecided:?} open"
    );
    Built { image, manifest }
}
