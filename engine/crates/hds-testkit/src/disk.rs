//! A whole disk under construction: the image, the file systems rendered
//! into it one after the other, and the bookkeeping that turns "what was
//! written" into a [`Manifest`].
//!
//! ```
//! use hds_testkit::disk::Disk;
//! use hds_testkit::volume::params::VolumeParams;
//! use hds_testkit::volume::spec::FileSpec;
//!
//! let mut disk = Disk::new("example", 7, 64 << 20);
//! // an old volume with a folder and a file in it ...
//! let old = disk.format(VolumeParams::new(0, 64 << 20).label("old"));
//! let v = disk.volume(old);
//! let root = v.root();
//! let photos = v.mkdir(root, "Photos");
//! v.add_file(photos, "a.bin", FileSpec::tagged(1, 100_000));
//! disk.render(old);
//! // ... quick-formatted the way Windows does it
//! let new = disk.format(disk.reformat_params(old).label("new"));
//! disk.render(new);
//! let built = disk.finish();
//! let file = built.manifest.entry("old", "Photos/a.bin");
//! assert!(file.must_recover_exactly());
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::content::ContentSpec;
use crate::facts::{ArtifactState, BootFact, IndexBlockFact, RecordFact, RecordRole};
use crate::image::Image;
use crate::manifest::{
    Entry, EntryKind, EntryName, EntryTimes, Expect, Extent, Flag, Geometry, Instance, InstanceKind, InstanceRole,
    Manifest, MftExtentFact, PartitionFact, Presence, RecordState, Truth,
};
use crate::ondisk::attr::reparse_tag;
use crate::ondisk::FileRef;
use crate::rng::derive;
use crate::time;
use crate::volume::params::VolumeParams;
use crate::volume::spec::FileSpec;
use crate::volume::{Node, NodeId, ParentRef, RenderSink, Source, Volume};

/// Handle of a volume inside its [`Disk`]; also its instance id in the
/// manifest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VolumeId(pub usize);

/// A finished image with its ground truth.
#[derive(Clone, Debug)]
pub struct Built {
    /// The image.
    pub image: Image,
    /// What it contains.
    pub manifest: Manifest,
}

#[derive(Default)]
struct Registry {
    records: BTreeMap<u64, (Vec<u8>, RecordFact)>,
    index_blocks: BTreeMap<u64, (Vec<u8>, IndexBlockFact)>,
    boots: BTreeMap<u64, (Vec<u8>, BootFact)>,
}

struct Sink<'a> {
    image: &'a mut Image,
    registry: &'a mut Registry,
}

impl RenderSink for Sink<'_> {
    fn data(&mut self, offset: u64, bytes: &[u8]) {
        self.image.write_at(offset, bytes);
    }

    fn record(&mut self, bytes: Vec<u8>, fact: RecordFact) {
        self.image.write_at(fact.offset, &bytes);
        self.registry.records.insert(fact.offset, (bytes, fact));
    }

    fn index_block(&mut self, bytes: Vec<u8>, fact: IndexBlockFact) {
        self.image.write_at(fact.offset, &bytes);
        self.registry.index_blocks.insert(fact.offset, (bytes, fact));
    }

    fn boot_sector(&mut self, bytes: Vec<u8>, fact: BootFact) {
        self.image.write_at(fact.offset, &bytes);
        self.registry.boots.insert(fact.offset, (bytes, fact));
    }
}

struct VolumeState {
    volume: Volume,
    role: Option<InstanceRole>,
    kind: Option<Option<InstanceKind>>,
}

struct Embedded {
    host_volume: usize,
    host_node: NodeId,
    inner: Arc<Built>,
}

/// A disk image under construction.
pub struct Disk {
    name: String,
    description: String,
    rules: Vec<String>,
    seed: u64,
    sector_size: u32,
    image: Image,
    volumes: Vec<VolumeState>,
    registry: Registry,
    scan_range: Option<(u64, u64)>,
    partitions: Vec<PartitionFact>,
    embedded: Vec<Embedded>,
    notes: Vec<String>,
}

impl Disk {
    /// An empty (all-zero) disk of `size` bytes. Everything generated for it
    /// derives from `seed`.
    pub fn new(name: &str, seed: u64, size: u64) -> Self {
        assert!(size.is_multiple_of(512), "disk size must be whole sectors");
        Disk {
            name: name.to_string(),
            description: String::new(),
            rules: Vec::new(),
            seed,
            sector_size: 512,
            image: Image::new(size),
            volumes: Vec::new(),
            registry: Registry::default(),
            scan_range: None,
            partitions: Vec::new(),
            embedded: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// Sets the description and the ALGORITHMS paragraphs the image tests.
    pub fn describe(&mut self, description: &str, rules: &[&str]) {
        self.description = description.to_string();
        self.rules = rules.iter().map(|r| r.to_string()).collect();
    }

    /// Adds a remark to the manifest.
    pub fn note(&mut self, note: &str) {
        self.notes.push(note.to_string());
    }

    /// The seed of the disk.
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Size of the disk in bytes.
    pub fn size(&self) -> u64 {
        self.image.len()
    }

    /// Sets the logical sector size the manifest reports (512 or 4096).
    pub fn set_sector_size(&mut self, bytes: u32) {
        assert!(bytes == 512 || bytes == 4096);
        self.sector_size = bytes;
    }

    /// Formats a volume. Nothing is written before [`Disk::render`]. The
    /// order of `format` calls is the order of history: a later volume whose
    /// partition overlaps an earlier one was formatted over it.
    pub fn format(&mut self, params: VolumeParams) -> VolumeId {
        assert!(
            params.offset + params.partition_bytes() <= self.image.len(),
            "the partition of volume {:?} does not fit the disk",
            params.label
        );
        assert!(
            self.volumes.iter().all(|v| v.volume.params().label != params.label),
            "two volumes are labelled {:?}",
            params.label
        );
        let seed = derive(self.seed, &params.label);
        self.volumes.push(VolumeState { volume: Volume::format(params, seed), role: None, kind: None });
        VolumeId(self.volumes.len() - 1)
    }

    /// The volume behind a handle.
    pub fn volume(&mut self, id: VolumeId) -> &mut Volume {
        &mut self.volumes[id.0].volume
    }

    /// Read access to a volume.
    pub fn volume_ref(&self, id: VolumeId) -> &Volume {
        &self.volumes[id.0].volume
    }

    /// Parameters for formatting the partition of `old` again the way
    /// Windows 10 does: same place, same sizes, a fresh 256-record `$MFT`,
    /// a day after the last thing that happened on the old volume.
    pub fn reformat_params(&self, old: VolumeId) -> VolumeParams {
        let v = &self.volumes[old.0].volume;
        let p = v.params();
        let mut params = VolumeParams::new(p.offset, p.partition_bytes()).label("new").volume_name("New Volume");
        params.bytes_per_sector = p.bytes_per_sector;
        params.partition_sectors = p.partition_sectors;
        params.cluster_size = p.cluster_size;
        params.record_size = p.record_size;
        params.hidden_sectors = p.hidden_sectors;
        params.format_time = v.clock().max(p.format_time) + time::DAY;
        let records = 256u64.max(p.cluster_size as u64 / p.record_size as u64);
        params.mft = vec![crate::volume::params::MftFragment { lcn: None, records }];
        params
    }

    /// Writes a volume into the image as its model describes it now. May be
    /// called again after further changes: everything is rewritten.
    pub fn render(&mut self, id: VolumeId) {
        let Disk { image, registry, volumes, .. } = self;
        let mut sink = Sink { image, registry };
        volumes[id.0].volume.render(id.0 as u32, &mut sink);
    }

    /// The image as it is now.
    pub fn image(&self) -> &Image {
        &self.image
    }

    /// Writes raw bytes: deliberate damage, partition tables, anything.
    pub fn write(&mut self, offset: u64, bytes: &[u8]) {
        self.image.write_at(offset, bytes);
    }

    /// Fills a range with one byte.
    pub fn fill(&mut self, offset: u64, len: u64, byte: u8) {
        self.image.fill(offset, len, byte);
    }

    /// Copies `len` bytes from `src` to `dst`. `FILE` records, `INDX` blocks
    /// and boot sectors that lie completely inside the source range are
    /// tracked at the destination as stale copies (shadow-copy blocks, old
    /// MFT clusters, …).
    pub fn copy(&mut self, src: u64, dst: u64, len: u64) {
        self.image.copy_within(src, dst, len as usize);
        let inside = |offset: u64, size: u64| offset >= src && offset + size <= src + len;
        let records: Vec<(Vec<u8>, RecordFact)> = self
            .registry
            .records
            .range(src..src + len)
            .filter(|(o, (b, _))| inside(**o, b.len() as u64))
            .map(|(_, v)| v.clone())
            .collect();
        for (bytes, mut fact) in records {
            fact.offset = fact.offset - src + dst;
            if fact.role != RecordRole::Mirror {
                fact.role = RecordRole::StaleCopy;
            }
            self.registry.records.insert(fact.offset, (bytes, fact));
        }
        let blocks: Vec<(Vec<u8>, IndexBlockFact)> = self
            .registry
            .index_blocks
            .range(src..src + len)
            .filter(|(o, (b, _))| inside(**o, b.len() as u64))
            .map(|(_, v)| v.clone())
            .collect();
        for (bytes, mut fact) in blocks {
            fact.offset = fact.offset - src + dst;
            fact.stale_copy = true;
            self.registry.index_blocks.insert(fact.offset, (bytes, fact));
        }
        let boots: Vec<(Vec<u8>, BootFact)> = self
            .registry
            .boots
            .range(src..src + len)
            .filter(|(o, (b, _))| inside(**o, b.len() as u64))
            .map(|(_, v)| v.clone())
            .collect();
        for (bytes, mut fact) in boots {
            fact.offset = fact.offset - src + dst;
            self.registry.boots.insert(fact.offset, (bytes, fact));
        }
    }

    /// Registers a boot sector written with [`Disk::write`] (for example a
    /// deliberately broken one) so that the manifest lists it.
    pub fn track_boot_sector(&mut self, fact: BootFact) {
        let bytes = self.image.read_vec(fact.offset, 512);
        self.registry.boots.insert(fact.offset, (bytes, fact));
    }

    /// Records a partition written by [`crate::partition`].
    pub fn add_partition(&mut self, fact: PartitionFact) {
        self.partitions.push(fact);
    }

    /// Sets the session range tests should scan (default: the whole disk).
    pub fn set_scan_range(&mut self, start: u64, end: u64) {
        assert!(start < end && end <= self.image.len() && start.is_multiple_of(512) && end.is_multiple_of(512));
        self.scan_range = Some((start, end));
    }

    /// States what a volume is physically, where the default (the newest
    /// volume at a place is current, everything below it is lost) is wrong.
    pub fn set_role(&mut self, id: VolumeId, role: InstanceRole) {
        self.volumes[id.0].role = Some(role);
    }

    /// States the instance kind a correct scan assigns to a volume (`None`:
    /// leave it open).
    pub fn expect_kind(&mut self, id: VolumeId, kind: Option<InstanceKind>) {
        self.volumes[id.0].kind = Some(kind);
    }

    /// Stores a finished image as a file of `volume`: a disk image or VM
    /// file lying on the drive (ALGORITHMS D7). `spec` decides where its
    /// clusters go; its content is replaced by the inner image.
    pub fn embed(
        &mut self,
        volume: VolumeId,
        parent: NodeId,
        name: &str,
        inner: Built,
        mut spec: FileSpec,
    ) -> NodeId {
        let bytes = Arc::new(inner.image.to_vec());
        spec.content = ContentSpec::blob(bytes);
        spec.storage = crate::volume::spec::StorageSpec::NonResident;
        let node = self.volumes[volume.0].volume.add_file(parent, name, spec);
        self.embedded.push(Embedded { host_volume: volume.0, host_node: node, inner: Arc::new(inner) });
        node
    }

    /// Finishes the image and establishes its ground truth.
    pub fn finish(self) -> Built {
        let range = self.scan_range.unwrap_or((0, self.image.len()));
        for state in &self.volumes {
            assert!(
                state.volume.rendered.is_some() && !state.volume.dirty,
                "volume {:?} was changed after it was last rendered",
                state.volume.params().label
            );
        }
        let image = &self.image;
        let volume_count = self.volumes.len();

        // ---- instances ---------------------------------------------------
        let overlaps = |a: &Volume, b: &Volume| {
            let (a0, a1) = (a.offset(), a.offset() + a.params().partition_bytes());
            let (b0, b1) = (b.offset(), b.offset() + b.params().partition_bytes());
            a0 < b1 && b0 < a1
        };
        let mut generation = vec![0u32; volume_count];
        for i in (0..volume_count).rev() {
            // the next volume formatted over this one decides
            if let Some(j) = (i + 1..volume_count).find(|&j| overlaps(&self.volumes[i].volume, &self.volumes[j].volume)) {
                generation[i] = generation[j] + 1;
            }
        }
        let mut instances: Vec<Instance> = Vec::with_capacity(volume_count);
        for (i, state) in self.volumes.iter().enumerate() {
            let v = &state.volume;
            let rendered = v.rendered.as_ref().expect("checked");
            let role = state.role.unwrap_or(if generation[i] == 0 { InstanceRole::Current } else { InstanceRole::Lost });
            let default_kind = match (role, generation[i]) {
                (InstanceRole::Lost, 1) => Some(InstanceKind::Main),
                (InstanceRole::Lost, g) if g >= 2 => Some(InstanceKind::Older),
                (InstanceRole::Embedded, _) => Some(InstanceKind::Embedded),
                _ => None,
            };
            let times: Vec<u64> = v.nodes.iter().filter(|n| !n.system).map(|n| n.times[2]).collect();
            let cs = v.cluster_size();
            instances.push(Instance {
                id: i as u32,
                label: v.params().label.clone(),
                role,
                expected_kind: state.kind.unwrap_or(default_kind),
                generation: generation[i],
                flavor: v.params().flavor,
                geometry: geometry_of(v),
                format_time: v.params().format_time,
                embedded_in: None,
                offset_in_host: None,
                live_allocated: if role == InstanceRole::Current {
                    rendered.used_ranges.iter().map(|&(lcn, n)| (v.lcn_to_offset(lcn), n * cs)).collect()
                } else {
                    Vec::new()
                },
                live_slots: if role == InstanceRole::Current { ranges(&rendered.slots_in_use) } else { Vec::new() },
                time_range: times.iter().min().copied().zip(times.iter().max().copied()),
            });
        }

        // ---- entries -----------------------------------------------------
        let mut entry_ids: Vec<Vec<Option<u32>>> = Vec::with_capacity(volume_count);
        let mut next_id = 0u32;
        for state in &self.volumes {
            let ids = state
                .volume
                .nodes
                .iter()
                .map(|n| {
                    (!n.names.is_empty()).then(|| {
                        next_id += 1;
                        next_id - 1
                    })
                })
                .collect();
            entry_ids.push(ids);
        }
        let mut entries: Vec<Entry> = Vec::with_capacity(next_id as usize);
        for (vi, state) in self.volumes.iter().enumerate() {
            let v = &state.volume;
            for ni in 0..v.nodes.len() {
                if let Some(id) = entry_ids[vi][ni] {
                    entries.push(build_entry(v, vi, ni, id, &entry_ids[vi], image, range, &instances[vi]));
                }
            }
        }

        // ---- structures --------------------------------------------------
        let slot_entry = |instance: u32, slot: u64| -> Option<u32> {
            let v = &self.volumes.get(instance as usize)?.volume;
            let node = v.rendered.as_ref()?.records.get(&slot)?.node?;
            entry_ids[instance as usize][node]
        };
        let mut records = Vec::new();
        for (offset, (bytes, fact)) in &self.registry.records {
            if let Some(state) = artifact_state(image, *offset, bytes, b"FILE") {
                let mut fact = fact.clone();
                fact.state = state;
                fact.entry = slot_entry(fact.instance, fact.slot);
                records.push(fact);
            }
        }
        let mut index_blocks = Vec::new();
        for (offset, (bytes, fact)) in &self.registry.index_blocks {
            if let Some(state) = artifact_state(image, *offset, bytes, b"INDX") {
                let mut fact = fact.clone();
                fact.state = state;
                fact.owner_entry = slot_entry(fact.instance, fact.owner.number);
                index_blocks.push(fact);
            }
        }
        let mut boot_sectors = Vec::new();
        for (offset, (bytes, fact)) in &self.registry.boots {
            if image.read_vec(*offset, bytes.len()) == *bytes {
                boot_sectors.push(fact.clone());
            }
        }

        // ---- embedded file systems ---------------------------------------
        for embedded in &self.embedded {
            let host = &self.volumes[embedded.host_volume].volume;
            let host_entry = entry_ids[embedded.host_volume][embedded.host_node.0].expect("the host file has a name");
            let map = HostMap::new(host, embedded.host_node);
            adopt_embedded(
                &embedded.inner,
                &map,
                host_entry,
                image,
                &mut instances,
                &mut entries,
                &mut records,
                &mut index_blocks,
                &mut boot_sectors,
            );
        }
        records.sort_by_key(|r| r.offset);
        index_blocks.sort_by_key(|b| b.offset);
        boot_sectors.sort_by_key(|b| b.offset);

        // ---- overwritten set (ALGORITHMS C5) -------------------------------
        let mut overwritten: Vec<(u64, u64)> = Vec::new();
        let mut live: Vec<(u64, u64)> = Vec::new();
        for (vi, state) in self.volumes.iter().enumerate() {
            let v = &state.volume;
            if instances[vi].role != InstanceRole::Current {
                continue;
            }
            // The volume has to be found: a boot sector of it and its record
            // 0 inside the scan range (C1).
            let found = boot_sectors
                .iter()
                .any(|b| b.instance == vi as u32 && b.offset >= range.0 && b.offset + 512 <= range.1)
                && records.iter().any(|r| {
                    r.instance == vi as u32
                        && r.slot == 0
                        && r.role == RecordRole::Base
                        && r.state == ArtifactState::Intact
                        && r.offset >= range.0
                        && r.offset < range.1
                });
            if !found {
                continue;
            }
            overwritten.extend(instances[vi].live_allocated.iter().copied());
            live.extend(instances[vi].live_allocated.iter().copied());
            // files deleted on the current volume: their runlists are the
            // only trace of what they overwrote
            for (ni, node) in v.nodes.iter().enumerate() {
                let Some(id) = entry_ids[vi][ni] else { continue };
                let entry = &entries[id as usize];
                if !node.in_use && !node.system && entry.truth.record_state == RecordState::Intact {
                    overwritten.extend(entry.extents.iter().filter_map(|e| e.offset.map(|o| (o, e.bytes))));
                }
            }
        }
        let overwritten_set = merge_ranges(overwritten);
        let live_ranges = merge_ranges(live);

        // ---- expectations that need the whole picture ----------------------
        surviving_index_entries(&mut entries, &records, &index_blocks);
        global_expectations(&mut entries, &instances, &overwritten_set);
        expected_paths(&mut entries, &instances, &records, &index_blocks, &live_ranges);

        let manifest = Manifest {
            manifest_version: 1,
            scenario: self.name,
            description: self.description,
            rules: self.rules,
            seed: self.seed,
            image_size: self.image.len(),
            sector_size: self.sector_size,
            scan_range: range,
            partitions: self.partitions,
            instances,
            entries,
            records,
            index_blocks,
            boot_sectors,
            overwritten_set,
            notes: self.notes,
        };
        Built { image: self.image, manifest }
    }
}

fn geometry_of(v: &Volume) -> Geometry {
    let p = v.params();
    let boot = v.boot_params();
    Geometry {
        volume_offset: p.offset,
        cluster_size: p.cluster_size,
        bytes_per_sector: p.bytes_per_sector,
        record_size: p.record_size,
        index_block_size: p.index_block_size,
        total_sectors: p.total_sectors(),
        total_clusters: p.total_clusters(),
        partition_bytes: p.partition_bytes(),
        hidden_sectors: boot.hidden_sectors,
        serial: boot.serial,
        mft_extents: v
            .mft_extents()
            .iter()
            .map(|e| MftExtentFact {
                lcn: e.lcn,
                clusters: e.clusters,
                first_record: e.first_record,
                records: e.records,
                offset: v.lcn_to_offset(e.lcn),
            })
            .collect(),
        mftmirr_lcn: boot.mftmirr_lcn,
        mft_records: v.mft_records(),
        mft_initialized_records: v.mft_initialized_records(),
    }
}

fn ranges(set: &BTreeSet<u64>) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for &v in set {
        match out.last_mut() {
            Some((start, count)) if *start + *count == v => *count += 1,
            _ => out.push((v, 1)),
        }
    }
    out
}

/// Sorts `(start, length)` ranges and merges touching or overlapping ones.
pub(crate) fn merge_ranges(mut ranges: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    ranges.retain(|r| r.1 > 0);
    ranges.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (start, len) in ranges {
        match out.last_mut() {
            Some((s, l)) if start <= *s + *l => *l = (*l).max(start + len - *s),
            _ => out.push((start, len)),
        }
    }
    out
}

fn intersects(sorted: &[(u64, u64)], start: u64, len: u64) -> bool {
    let at = sorted.partition_point(|&(s, l)| s + l <= start);
    sorted.get(at).is_some_and(|&(s, _)| s < start + len)
}

/// State of a structure in the finished image: `None` when its signature is
/// gone.
fn artifact_state(image: &Image, offset: u64, bytes: &[u8], magic: &[u8; 4]) -> Option<ArtifactState> {
    let now = image.read_vec(offset, bytes.len());
    if now == bytes {
        Some(ArtifactState::Intact)
    } else if now[..4] == magic[..] {
        Some(ArtifactState::Damaged)
    } else {
        None
    }
}

fn under_extend(v: &Volume, node: &Node) -> bool {
    let mut at = node;
    for _ in 0..64 {
        let Some(link) = at.names.first() else {
            return false;
        };
        match link.parent {
            ParentRef::Node(p) => {
                let parent = v.node(p);
                if parent.record == 11 {
                    return true;
                }
                if parent.record == 5 || std::ptr::eq(parent, at) {
                    return false;
                }
                at = parent;
            }
            ParentRef::Raw(r) => return r.number == 11,
        }
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn build_entry(
    v: &Volume,
    vi: usize,
    ni: usize,
    id: u32,
    ids: &[Option<u32>],
    image: &Image,
    range: (u64, u64),
    instance: &Instance,
) -> Entry {
    let node = &v.nodes[ni];
    let rendered = v.rendered.as_ref().expect("checked");
    let cs = v.cluster_size();
    let rs = v.record_size();
    let in_range = |offset: u64, len: u64| offset >= range.0 && offset + len <= range.1;
    let rendered_intact = |slot: u64| -> Option<bool> {
        let r = rendered.records.get(&slot).filter(|r| r.node == Some(ni))?;
        Some(image.read_vec(r.offset, r.bytes.len()) == r.bytes)
    };

    let base_intact = if node.superseded { None } else { rendered_intact(node.record) };
    let record_state = match base_intact {
        None => RecordState::Reused,
        Some(true) => RecordState::Intact,
        Some(false) => RecordState::Destroyed,
    };
    let lost_extensions: Vec<u64> = node
        .extensions
        .iter()
        .filter(|e| e.superseded || rendered_intact(e.record) != Some(true))
        .map(|e| e.record)
        .collect();
    let names_holder = node.multi.as_ref().map_or(0, |m| m.names_in);
    let holder_record = |holder: usize| if holder == 0 { node.record } else { node.extensions[holder - 1].record };
    // attribute ids as rendered; a record that was never written (its slot
    // was reused before the first render) has them in storage order
    let name_ids: Vec<u16> = rendered
        .records
        .get(&holder_record(names_holder))
        .filter(|r| r.node == Some(ni) && r.fact.names.len() == node.names.len())
        .map(|r| r.fact.names.iter().map(|n| n.attr_id).collect())
        .unwrap_or_else(|| (0..node.names.len() as u16).collect());
    let names: Vec<EntryName> = node
        .names
        .iter()
        .zip(name_ids)
        .map(|(l, attr_id)| EntryName {
            name: String::from_utf16_lossy(&l.name),
            name_utf16: l.name.clone(),
            namespace: l.namespace,
            parent: node.quirks.raw_parent.unwrap_or_else(|| v.parent_ref(&l.parent)),
            holder_record: holder_record(names_holder),
            attr_id,
        })
        .collect();
    let parent = node
        .names
        .iter()
        .find(|l| l.namespace != 2)
        .or(node.names.first())
        .and_then(|l| match l.parent {
            ParentRef::Node(p) if p.0 != ni => ids[p.0],
            _ => None,
        });

    // the unnamed data stream
    let data = node.data();
    let mut extents = Vec::new();
    let mut allocated_size = 0;
    let mut resident_at = None;
    let mut content_intact = None;
    let mut data_in_range = true;
    if let Some(stream) = data {
        match &stream.runs {
            Some(runs) => {
                let mut vcn = 0;
                for run in runs {
                    let offset = run.lcn.map(|lcn| v.lcn_to_offset(lcn));
                    if let Some(o) = offset {
                        data_in_range &= in_range(o, run.len * cs) && o + run.len * cs <= image.len();
                    }
                    extents.push(Extent { vcn, clusters: run.len, lcn: run.lcn, offset, bytes: run.len * cs });
                    vcn += run.len;
                }
                allocated_size = vcn * cs;
            }
            None => {
                let holder = node.multi.as_ref().map_or(0, |m| m.resident_data_in);
                let slot = holder_record(holder);
                if let Some(r) = rendered.records.get(&slot).filter(|r| r.node == Some(ni)) {
                    let at = r.fact.data.iter().find(|d| d.name.is_empty()).and_then(|d| d.resident);
                    resident_at = at.map(|(value_offset, len)| (r.offset, value_offset, len));
                }
            }
        }
        if let Source::Spec(content) = &stream.source {
            content_intact = if stream.garbage {
                None
            } else {
                Some(match &stream.runs {
                    None => {
                        let holder = node.multi.as_ref().map_or(0, |m| m.resident_data_in);
                        rendered_intact(holder_record(holder)) == Some(true)
                    }
                    Some(runs) => content_on_disk(image, v, runs, content, stream.initialized.min(stream.real_size)),
                })
            };
        }
    }
    let (content, sha256) = match data.map(|s| &s.source) {
        Some(Source::Spec(c)) => (Some(c.clone()), Some(c.sha256())),
        _ => (None, None),
    };

    // ancestors
    let mut ancestors_intact = true;
    let mut at = node;
    for _ in 0..256 {
        let Some(link) = at.names.iter().find(|l| l.namespace != 2).or(at.names.first()) else {
            break;
        };
        match link.parent {
            ParentRef::Node(p) => {
                let parent = v.node(p);
                if parent.record == 5 || std::ptr::eq(parent, at) {
                    break;
                }
                let parent_index = p.0;
                let intact = !parent.superseded
                    && rendered
                        .records
                        .get(&parent.record)
                        .filter(|r| r.node == Some(parent_index))
                        .is_some_and(|r| image.read_vec(r.offset, r.bytes.len()) == r.bytes && in_range(r.offset, rs));
                if !intact {
                    ancestors_intact = false;
                    break;
                }
                at = parent;
            }
            ParentRef::Raw(r) => {
                ancestors_intact = r.number == 5;
                break;
            }
        }
    }
    // a chain that never reaches the root (cycle) has no intact path
    let components = v.path_components(NodeId(ni));
    let reaches_root = {
        let mut at = node;
        let mut ok = false;
        for _ in 0..256 {
            match at.names.iter().find(|l| l.namespace != 2).or(at.names.first()).map(|l| l.parent) {
                Some(ParentRef::Node(p)) => {
                    if v.node(p).record == 5 {
                        ok = true;
                        break;
                    }
                    if std::ptr::eq(v.node(p), at) {
                        break;
                    }
                    at = v.node(p);
                }
                Some(ParentRef::Raw(r)) => {
                    ok = r.number == 5;
                    break;
                }
                None => break,
            }
        }
        ok || node.record == 5
    };

    let record_offset = (!node.superseded).then(|| v.record_offset(node.record));
    let record_in_range = record_offset.is_some_and(|o| in_range(o, rs));
    let truth = Truth {
        record_state,
        record_intact: base_intact == Some(true) && lost_extensions.is_empty(),
        lost_extension_records: lost_extensions.clone(),
        content_intact,
        ancestors_intact: ancestors_intact && reaches_root,
        record_in_range,
        data_in_range,
        surviving_index_entries: 0,
    };

    // ---- expectations that follow from the entry alone -------------------
    let mut expect = Expect { presence: Presence::Unspecified, path: None, flags_set: Vec::new(), flags_clear: Vec::new() };
    let hidden_by_number = node.record < 16 || (node.record < 24 && node.names.is_empty());
    if instance.role == InstanceRole::Current {
        expect.presence = Presence::Hidden;
    } else if record_state != RecordState::Intact || !record_in_range {
        expect.presence = Presence::Gone;
    } else if hidden_by_number || under_extend(v, node) {
        expect.presence = Presence::Hidden;
    } else {
        expect.presence = Presence::Shown;
        if truth.ancestors_intact {
            expect.path = Some(components.clone());
        }
        let mut flag = |f: Flag, on: bool| if on { expect.set(f) } else { expect.clear(f) };
        flag(Flag::Deleted, !node.in_use);
        let torn = !node.quirks.torn_blocks.is_empty()
            || node.quirks.torn_extension_blocks.iter().any(|(_, b)| !b.is_empty());
        flag(Flag::TornRecord, torn);
        flag(Flag::BadSectors, false);
        if !node.is_dir {
            let tag = node.reparse.as_ref().map(|(t, _)| *t);
            let wof_stream = node.streams.iter().any(|s| s.name == crate::ondisk::utf16("WofCompressedData"));
            let non_resident = data.is_some_and(|s| s.runs.is_some());
            let data_flags = data.map_or(0, |s| s.flags);
            flag(
                Flag::Compressed,
                (non_resident && data_flags & 0x00FF != 0) || tag == Some(reparse_tag::WOF) || wof_stream,
            );
            flag(Flag::Encrypted, data_flags & 0x4000 != 0);
            flag(
                Flag::Placeholder,
                tag.is_some_and(|t| t & 0xFFFF_0FFF == reparse_tag::CLOUD || t == reparse_tag::DEDUP),
            );
            let hole = data.and_then(|s| s.runs.as_ref()).is_some_and(|r| r.iter().any(|x| x.lcn.is_none()));
            flag(Flag::Sparse, hole || data_flags & 0x8000 != 0);
            flag(Flag::NoData, data.is_none() && node.multi.is_none());
            flag(Flag::OutOfBounds, !data_in_range);
            // Extents live in the records the MultiSpec names; a lost
            // extension record takes its extents with it (ALGORITHMS E2).
            let lost_holders: Vec<usize> = node
                .extensions
                .iter()
                .enumerate()
                .filter(|(_, e)| lost_extensions.contains(&e.record))
                .map(|(k, _)| k + 1)
                .collect();
            let pieces = node.multi.as_ref().map(|m| m.data.clone()).unwrap_or_default();
            let lost_first = pieces.first().is_some_and(|p| lost_holders.contains(&p.holder));
            let lost_any = pieces.iter().any(|p| lost_holders.contains(&p.holder));
            let list_without_data = data.is_none() && node.multi.is_some();
            flag(Flag::MissingFragments, lost_any || list_without_data);
            flag(Flag::SizeEstimated, lost_first && pieces.len() > 1);
        }
    }

    Entry {
        id,
        instance: vi as u32,
        kind: if node.is_dir { EntryKind::Dir } else { EntryKind::File },
        system: node.system,
        path: components.join("/"),
        components,
        record: node.record,
        sequence: node.seq,
        parent,
        names,
        record_offset,
        extension_records: node
            .extensions
            .iter()
            .filter(|e| !e.superseded)
            .map(|e| (e.record, v.record_offset(e.record)))
            .collect(),
        size: data.map_or(0, |s| s.real_size),
        allocated_size,
        initialized_size: data.map_or(0, |s| s.initialized),
        resident: data.is_some_and(|s| s.runs.is_none()),
        resident_at,
        extents,
        data_flags: data.map_or(0, |s| s.flags),
        streams: node.streams.iter().filter(|s| !s.name.is_empty()).map(|s| String::from_utf16_lossy(&s.name)).collect(),
        reparse_tag: node.reparse.as_ref().map(|(t, _)| *t),
        file_attributes: node.file_attributes,
        times: EntryTimes {
            created: node.times[0],
            modified: node.times[1],
            mft_changed: node.times[2],
            accessed: node.times[3],
        },
        lsn: node.lsn,
        content,
        sha256,
        deleted: !node.in_use,
        truth,
        expect,
        tags: node.tags.clone(),
    }
}

/// Compares what the clusters of a stream hold in the finished image with
/// its content, up to `valid` bytes; holes hold nothing and always match
/// (the content is zero there by construction).
fn content_on_disk(
    image: &Image,
    v: &Volume,
    runs: &[crate::ondisk::runlist::Run],
    content: &ContentSpec,
    valid: u64,
) -> bool {
    const CHUNK: u64 = 1 << 20;
    let cs = v.cluster_size();
    let mut start = 0u64;
    let mut expected = vec![0u8; CHUNK as usize];
    let mut actual = vec![0u8; CHUNK as usize];
    for run in runs {
        let run_len = run.len * cs;
        if let Some(lcn) = run.lcn {
            let end = (start + run_len).min(valid);
            let mut pos = start;
            while pos < end {
                let take = (end - pos).min(CHUNK) as usize;
                let at = v.lcn_to_offset(lcn) + (pos - start);
                if at + take as u64 > image.len() {
                    return false;
                }
                content.read_at(pos, &mut expected[..take]);
                image.read_at(at, &mut actual[..take]).expect("range was checked");
                if expected[..take] != actual[..take] {
                    return false;
                }
                pos += take as u64;
            }
        }
        start += run_len;
    }
    // bytes the runs do not cover would have to be zero
    start >= valid || {
        let mut tail = vec![0u8; (valid - start) as usize];
        content.read_at(start, &mut tail);
        tail.iter().all(|&b| b == 0)
    }
}

fn surviving_index_entries(entries: &mut [Entry], records: &[RecordFact], index_blocks: &[IndexBlockFact]) {
    let mut counts: BTreeMap<(u32, FileRef), u32> = BTreeMap::new();
    for block in index_blocks.iter().filter(|b| b.state == ArtifactState::Intact) {
        for e in &block.entries {
            *counts.entry((block.instance, e.child)).or_default() += 1;
        }
    }
    for record in records.iter().filter(|r| r.state == ArtifactState::Intact) {
        for e in record.index_root.iter().flatten() {
            *counts.entry((record.instance, e.child)).or_default() += 1;
        }
    }
    for entry in entries {
        let key = (entry.instance, FileRef::new(entry.record, entry.sequence));
        entry.truth.surviving_index_entries = counts.get(&key).copied().unwrap_or(0);
    }
}

/// Generation, overwritten and overlap flags (ALGORITHMS C5, D9, G2).
fn global_expectations(entries: &mut [Entry], instances: &[Instance], overwritten_set: &[(u64, u64)]) {
    // instance kinds
    for entry in entries.iter_mut().filter(|e| e.expect.presence == Presence::Shown) {
        let instance = &instances[entry.instance as usize];
        match instance.expected_kind {
            Some(kind) => {
                for (flag, on) in [
                    (Flag::OlderGeneration, kind == InstanceKind::Older),
                    (Flag::Embedded, kind == InstanceKind::Embedded),
                    (Flag::Stray, kind == InstanceKind::Strays),
                ] {
                    if on {
                        entry.expect.set(flag);
                    } else {
                        entry.expect.clear(flag);
                    }
                }
            }
            None => {
                for flag in [Flag::OlderGeneration, Flag::Embedded, Flag::Stray] {
                    entry.expect.open(flag);
                }
            }
        }
    }

    // Overlap conflicts between the files of all non-embedded lost
    // instances, compared in absolute bytes.
    let candidates: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            e.kind == EntryKind::File
                && e.expect.presence == Presence::Shown
                && instances[e.instance as usize].role == InstanceRole::Lost
        })
        .map(|(i, _)| i)
        .collect();
    let mut segments: Vec<(u64, u64, usize)> = Vec::new();
    for &i in &candidates {
        for extent in &entries[i].extents {
            if let Some(offset) = extent.offset {
                segments.push((offset, offset + extent.bytes, i));
            }
        }
    }
    segments.sort_unstable();
    let mut pairs: BTreeSet<(usize, usize)> = BTreeSet::new();
    let mut active: Vec<(u64, usize)> = Vec::new();
    for &(start, end, i) in &segments {
        active.retain(|&(active_end, _)| active_end > start);
        for &(_, j) in &active {
            if i != j {
                pairs.insert((i.min(j), i.max(j)));
            }
        }
        active.push((end, i));
    }
    let rank = |kind: InstanceKind| match kind {
        InstanceKind::Main => 0,
        InstanceKind::Other => 1,
        InstanceKind::Older => 2,
        InstanceKind::Strays | InstanceKind::Embedded => 3,
    };
    let mut lost: BTreeSet<usize> = BTreeSet::new();
    let mut overlap: BTreeSet<usize> = BTreeSet::new();
    let mut uncertain: BTreeSet<usize> = BTreeSet::new();
    for &(a, b) in &pairs {
        let (ea, eb) = (&entries[a], &entries[b]);
        let (Some(ka), Some(kb)) =
            (instances[ea.instance as usize].expected_kind, instances[eb.instance as usize].expected_kind)
        else {
            uncertain.insert(a);
            uncertain.insert(b);
            continue;
        };
        // winner: instance kind, in use over deleted, later MFT change
        // time, higher LSN
        let key = |e: &Entry, k: InstanceKind| (std::cmp::Reverse(rank(k)), !e.deleted, e.times.mft_changed, e.lsn);
        match key(ea, ka).cmp(&key(eb, kb)) {
            std::cmp::Ordering::Equal => {
                overlap.insert(a);
                overlap.insert(b);
            }
            order => {
                let (winner, loser) = if order == std::cmp::Ordering::Greater { (a, b) } else { (b, a) };
                lost.insert(loser);
                if !ea.deleted && !eb.deleted && rank(ka) == rank(kb) {
                    overlap.insert(winner);
                }
            }
        }
    }
    for &i in &candidates {
        let hit = entries[i]
            .extents
            .iter()
            .any(|e| e.offset.is_some_and(|o| intersects(overwritten_set, o, e.bytes)));
        let expect = &mut entries[i].expect;
        if hit || lost.contains(&i) {
            expect.set(Flag::Overwritten);
        } else if uncertain.contains(&i) {
            expect.open(Flag::Overwritten);
        } else {
            expect.clear(Flag::Overwritten);
        }
        if overlap.contains(&i) {
            expect.set(Flag::Overlap);
        } else if uncertain.contains(&i) {
            expect.open(Flag::Overlap);
        } else {
            expect.clear(Flag::Overlap);
        }
    }
}

/// One step of an expected path, from the entry upwards.
enum Step {
    Name(String),
    Lost(u64, u16),
}

/// Where a correct catalog puts every shown entry of a lost file system
/// (ALGORITHMS E4 to E6), worked out from what survived:
///
/// * the entry's name is one of its best namespace (Win32 or Win32+DOS,
///   then POSIX, then DOS): one whose parent survived, then the one with
///   the lowest attribute id;
/// * a parent whose folder record survived contributes its name;
/// * a parent whose record is gone is a lost folder: it takes name and
///   parent from the surviving index entry for it with the highest
///   `$LogFile` sequence number (DOS names last), else from a stale copy of
///   its record, else it is `Folder #N` (`Folder #N.s` when lost folders
///   with several sequence numbers share the number) under `[Lost folders]`;
/// * reference 5 is the root.
///
/// Paths are left open where the rules give no single answer: parent cycles
/// (E8 cuts them "where the cycle is detected") and index entries of equal
/// sequence number that disagree.
fn expected_paths(
    entries: &mut [Entry],
    instances: &[Instance],
    records: &[RecordFact],
    index_blocks: &[IndexBlockFact],
    live_ranges: &[(u64, u64)],
) {
    type Key = (u32, u64, u16);
    let lost_instance = |i: u32| instances[i as usize].role == InstanceRole::Lost;
    // folders by the reference their children use
    let mut folders: BTreeMap<Key, usize> = BTreeMap::new();
    for (i, e) in entries.iter().enumerate() {
        if e.kind == EntryKind::Dir && e.truth.record_state == RecordState::Intact && e.truth.record_in_range {
            folders.insert((e.instance, e.record, e.sequence), i);
        }
    }
    // names offered by surviving index entries: (lsn, namespace, name, parent)
    let mut offers: BTreeMap<Key, Vec<(u64, u8, String, FileRef)>> = BTreeMap::new();
    let mut offer = |instance: u32, lsn: u64, entry: &crate::facts::IndexEntryFact| {
        if entry.key.flags & crate::ondisk::attr::FN_DIRECTORY != 0 {
            offers.entry((instance, entry.child.number, entry.child.seq)).or_default().push((
                lsn,
                entry.key.namespace,
                entry.key.name.clone(),
                entry.key.parent,
            ));
        }
    };
    for block in index_blocks.iter().filter(|b| b.state == ArtifactState::Intact) {
        // an index block in space the current file system has allocated
        // counts as current data (C4)
        if intersects(live_ranges, block.offset, block.size as u64) {
            continue;
        }
        for e in &block.entries {
            offer(block.instance, block.lsn, e);
        }
    }
    for record in records.iter().filter(|r| r.state == ArtifactState::Intact) {
        for e in record.index_root.iter().flatten() {
            offer(record.instance, record.lsn, e);
        }
    }
    // stale copies of folder records (E6 b): in use, directory
    let mut copies: BTreeMap<Key, (String, FileRef)> = BTreeMap::new();
    for record in records {
        if record.role == RecordRole::StaleCopy && record.state == ArtifactState::Intact && record.flags & 3 == 3 {
            let names: Vec<NameChoice> =
                record.names.iter().map(|n| (n.namespace, n.name.clone(), n.parent, n.attr_id)).collect();
            if let (Some(number), Some(name)) = (record.number, best_name(&names, |_| true)) {
                copies.entry((record.instance, number, record.sequence)).or_insert(name);
            }
        }
    }

    let survives = |instance: u32, parent: FileRef| parent.number == 5 || folders.contains_key(&(instance, parent.number, parent.seq));
    let own_name = |e: &Entry| -> Option<(String, FileRef)> {
        let names: Vec<NameChoice> =
            e.names.iter().map(|n| (n.namespace, n.name.clone(), n.parent, n.attr_id)).collect();
        best_name(&names, |parent| survives(e.instance, parent))
    };
    let lost_name = |key: Key| -> Result<Option<(String, FileRef)>, ()> {
        if let Some(list) = offers.get(&key) {
            let non_dos = list.iter().any(|o| o.1 != 2);
            let usable: Vec<&(u64, u8, String, FileRef)> = list.iter().filter(|o| !non_dos || o.1 != 2).collect();
            let top = usable.iter().map(|o| o.0).max().expect("not empty");
            let mut best: Vec<(&String, FileRef)> = usable.iter().filter(|o| o.0 == top).map(|o| (&o.2, o.3)).collect();
            best.dedup();
            return if best.windows(2).all(|w| w[0] == w[1]) { Ok(Some((best[0].0.clone(), best[0].1))) } else { Err(()) };
        }
        Ok(copies.get(&key).cloned())
    };

    let mut chains: Vec<Option<Vec<Step>>> = Vec::with_capacity(entries.len());
    let mut lost_sequences: BTreeMap<(u32, u64), BTreeSet<u16>> = BTreeMap::new();
    for e in entries.iter() {
        if e.expect.presence != Presence::Shown || !lost_instance(e.instance) {
            chains.push(None);
            continue;
        }
        let chain = (|| {
            let (name, mut parent) = own_name(e)?;
            let mut steps = vec![Step::Name(name)];
            for _ in 0..512 {
                if parent.number == 5 {
                    return Some(steps);
                }
                let key = (e.instance, parent.number, parent.seq);
                if let Some(&folder) = folders.get(&key) {
                    let (name, next) = own_name(&entries[folder])?;
                    steps.push(Step::Name(name));
                    parent = next;
                    continue;
                }
                match lost_name(key).ok()? {
                    Some((name, next)) => {
                        // a named lost folder still counts as one for the
                        // "several sequences share a number" rule
                        lost_sequences.entry((e.instance, parent.number)).or_default().insert(parent.seq);
                        steps.push(Step::Name(name));
                        parent = next;
                    }
                    None => {
                        lost_sequences.entry((e.instance, parent.number)).or_default().insert(parent.seq);
                        steps.push(Step::Lost(parent.number, parent.seq));
                        return Some(steps);
                    }
                }
            }
            None
        })();
        chains.push(chain);
    }
    for (e, chain) in entries.iter_mut().zip(chains) {
        if e.expect.presence != Presence::Shown || !lost_instance(e.instance) {
            continue;
        }
        e.expect.path = chain.map(|steps| {
            let mut path: Vec<String> = Vec::with_capacity(steps.len() + 1);
            for step in steps.into_iter().rev() {
                match step {
                    Step::Name(name) => path.push(name),
                    Step::Lost(number, seq) => {
                        path.push("[Lost folders]".to_string());
                        let several = lost_sequences.get(&(e.instance, number)).is_some_and(|s| s.len() > 1);
                        path.push(if several { format!("Folder #{number}.{seq}") } else { format!("Folder #{number}") });
                    }
                }
            }
            path
        });
    }
}

/// A `$FILE_NAME` to choose from: namespace, name, parent, attribute id.
type NameChoice = (u8, String, FileRef, u16);

/// ALGORITHMS E4: among the names of the best namespace (Win32 or
/// Win32+DOS, then POSIX, then DOS) one whose parent passes `survives`,
/// then the lowest attribute id.
fn best_name(names: &[NameChoice], survives: impl Fn(FileRef) -> bool) -> Option<(String, FileRef)> {
    let rank = |namespace: u8| match namespace {
        1 | 3 => 0,
        0 => 1,
        _ => 2,
    };
    let best = names.iter().map(|n| rank(n.0)).min()?;
    let chosen = names.iter().filter(|n| rank(n.0) == best).min_by_key(|n| (!survives(n.2), n.3))?;
    Some((chosen.1.clone(), chosen.2))
}

/// Maps offsets of a file's content to absolute offsets of the host image.
struct HostMap {
    /// `(file offset, absolute offset, length)` of every allocated run.
    pieces: Vec<(u64, u64, u64)>,
}

impl HostMap {
    fn new(host: &Volume, node: NodeId) -> Self {
        let cs = host.cluster_size();
        let mut pieces = Vec::new();
        let mut at = 0;
        for run in host.runs(node) {
            if let Some(lcn) = run.lcn {
                pieces.push((at, host.lcn_to_offset(lcn), run.len * cs));
            }
            at += run.len * cs;
        }
        HostMap { pieces }
    }

    /// The absolute pieces `(offset, length)` that hold `len` bytes of the
    /// file from `start`; `None` when part of the range is not stored.
    fn map(&self, start: u64, len: u64) -> Option<Vec<(u64, u64)>> {
        let mut out = Vec::new();
        let mut pos = start;
        let end = start + len;
        while pos < end {
            let &(file, abs, piece_len) = self.pieces.iter().find(|&&(f, _, l)| pos >= f && pos < f + l)?;
            let take = (file + piece_len - pos).min(end - pos);
            out.push((abs + (pos - file), take));
            pos += take;
        }
        Some(out)
    }

    /// The absolute offset of a range that is stored contiguously.
    fn map_contiguous(&self, start: u64, len: u64) -> Option<u64> {
        match self.map(start, len)?.as_slice() {
            [(offset, _)] => Some(*offset),
            _ => None,
        }
    }
}

fn outer_equals_inner(image: &Image, inner: &Image, map: &HostMap, start: u64, len: u64) -> bool {
    let Some(pieces) = map.map(start, len) else {
        return false;
    };
    let mut pos = start;
    for (offset, piece_len) in pieces {
        if image.read_vec(offset, piece_len as usize) != inner.read_vec(pos, piece_len as usize) {
            return false;
        }
        pos += piece_len;
    }
    true
}

/// Adds the instances, entries and structures of an image stored inside a
/// file to the host's manifest, with every offset translated through the
/// file's runs.
#[allow(clippy::too_many_arguments)]
fn adopt_embedded(
    inner: &Built,
    map: &HostMap,
    host_entry: u32,
    image: &Image,
    instances: &mut Vec<Instance>,
    entries: &mut Vec<Entry>,
    records: &mut Vec<RecordFact>,
    index_blocks: &mut Vec<IndexBlockFact>,
    boot_sectors: &mut Vec<BootFact>,
) {
    let m = &inner.manifest;
    let instance_base = instances.len() as u32;
    let entry_base = entries.len() as u32;
    for instance in &m.instances {
        let mut instance = instance.clone();
        instance.id += instance_base;
        instance.label = format!("{}/{}", m.scenario, instance.label);
        instance.role = InstanceRole::Embedded;
        instance.expected_kind = Some(InstanceKind::Embedded);
        instance.embedded_in = Some(host_entry);
        let inner_offset = instance.geometry.volume_offset;
        instance.offset_in_host = Some(inner_offset);
        // Where cluster 0 of the inner volume lies; exact for every cluster
        // only while the host file is not fragmented.
        instance.geometry.volume_offset = map.map_contiguous(inner_offset, 512).unwrap_or(0);
        for extent in &mut instance.geometry.mft_extents {
            extent.offset = map.map_contiguous(extent.offset, 512).unwrap_or(0);
        }
        instance.live_allocated.clear();
        instance.live_slots.clear();
        instances.push(instance);
    }
    for entry in &m.entries {
        let mut e = entry.clone();
        e.id += entry_base;
        e.instance += instance_base;
        e.parent = e.parent.map(|p| p + entry_base);
        let record_size = m.instances[entry.instance as usize].geometry.record_size as u64;
        let mut intact = entry.truth.record_intact;
        e.record_offset = entry.record_offset.and_then(|o| map.map_contiguous(o, record_size));
        if let Some(o) = entry.record_offset {
            intact &= outer_equals_inner(image, &inner.image, map, o, record_size);
        }
        e.extension_records = entry
            .extension_records
            .iter()
            .filter_map(|&(n, o)| {
                intact &= outer_equals_inner(image, &inner.image, map, o, record_size);
                Some((n, map.map_contiguous(o, record_size)?))
            })
            .collect();
        e.resident_at = entry.resident_at.and_then(|(o, v, l)| Some((map.map_contiguous(o, record_size)?, v, l)));
        let mut content_ok = true;
        let mut extents = Vec::new();
        for extent in &entry.extents {
            match extent.offset {
                None => extents.push(extent.clone()),
                Some(offset) => {
                    content_ok &= outer_equals_inner(image, &inner.image, map, offset, extent.bytes);
                    // an inner run that crosses a fragment boundary of the
                    // host file becomes several absolute pieces
                    let cluster = extent.bytes / extent.clusters.max(1);
                    let mut vcn = extent.vcn;
                    for (abs, len) in map.map(offset, extent.bytes).unwrap_or_default() {
                        extents.push(Extent {
                            vcn,
                            clusters: len / cluster.max(1),
                            lcn: extent.lcn.map(|l| l + (vcn - extent.vcn)),
                            offset: Some(abs),
                            bytes: len,
                        });
                        vcn += len / cluster.max(1);
                    }
                }
            }
        }
        e.extents = extents;
        e.truth.record_intact = intact;
        if !intact && e.truth.record_state == RecordState::Intact {
            e.truth.record_state = RecordState::Destroyed;
        }
        e.truth.content_intact = entry.truth.content_intact.map(|c| c && content_ok);
        // Files of an embedded file system are browse-only (ALGORITHMS D7).
        // A healthy volume inside the image is "current" only in the inner
        // manifest; seen from the host it is an embedded instance whose
        // files are shown unless they are system files.
        let inner_current = m.instances[entry.instance as usize].role == InstanceRole::Current;
        let inner_presence = if !inner_current {
            entry.expect.presence
        } else if entry.truth.record_state != RecordState::Intact {
            Presence::Gone
        } else if entry.record < 16 || entry.components.first().is_some_and(|c| c == "$Extend") {
            Presence::Hidden
        } else {
            Presence::Shown
        };
        let shown = inner_presence == Presence::Shown && intact;
        e.expect = Expect {
            presence: match inner_presence {
                Presence::Shown if !intact => Presence::Gone,
                other => other,
            },
            path: match (shown, inner_current) {
                (true, true) if entry.truth.ancestors_intact => Some(entry.components.clone()),
                (true, false) => entry.expect.path.clone(),
                _ => None,
            },
            flags_set: Vec::new(),
            flags_clear: Vec::new(),
        };
        if shown {
            e.expect.set(Flag::Embedded);
            if entry.deleted {
                e.expect.set(Flag::Deleted);
            } else {
                e.expect.clear(Flag::Deleted);
            }
        }
        entries.push(e);
    }
    let translate = |offset: u64, len: u64| -> Option<(u64, ArtifactState)> {
        // A structure is found where its first sector lies; it is intact
        // only if the rest follows contiguously and unchanged.
        let first = map.map_contiguous(offset, 512)?;
        if image.read_vec(first, 512) != inner.image.read_vec(offset, 512) {
            return None;
        }
        let whole = map.map_contiguous(offset, len).is_some() && outer_equals_inner(image, &inner.image, map, offset, len);
        Some((first, if whole { ArtifactState::Intact } else { ArtifactState::Damaged }))
    };
    for record in &m.records {
        if let Some((offset, state)) = translate(record.offset, record.size as u64) {
            let mut r = record.clone();
            r.offset = offset;
            r.instance += instance_base;
            r.entry = r.entry.map(|e| e + entry_base);
            if record.state == ArtifactState::Damaged {
                r.state = ArtifactState::Damaged;
            } else {
                r.state = state;
            }
            records.push(r);
        }
    }
    for block in &m.index_blocks {
        if let Some((offset, state)) = translate(block.offset, block.size as u64) {
            let mut b = block.clone();
            b.offset = offset;
            b.instance += instance_base;
            b.owner_entry = b.owner_entry.map(|e| e + entry_base);
            if block.state == ArtifactState::Damaged {
                b.state = ArtifactState::Damaged;
            } else {
                b.state = state;
            }
            index_blocks.push(b);
        }
    }
    for boot in &m.boot_sectors {
        if let Some((offset, ArtifactState::Intact)) = translate(boot.offset, 512) {
            let mut b = boot.clone();
            b.offset = offset;
            b.instance += instance_base;
            boot_sectors.push(b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_choice_follows_namespace_then_parent_then_attribute_id() {
        let name = |namespace: u8, name: &str, parent: u64, id: u16| (namespace, name.to_string(), FileRef::new(parent, 1), id);
        let pick = |names: &[NameChoice], alive: &[u64]| {
            best_name(names, |parent| alive.contains(&parent.number)).map(|(name, _)| name)
        };
        // Win32 beats POSIX beats DOS, wherever they stand
        let mixed = [name(2, "DOS~1", 9, 1), name(0, "posix", 9, 2), name(1, "win32", 9, 3)];
        assert_eq!(pick(&mixed, &[9]).as_deref(), Some("win32"));
        assert_eq!(pick(&mixed[..2], &[9]).as_deref(), Some("posix"));
        assert_eq!(pick(&mixed[..1], &[9]).as_deref(), Some("DOS~1"));
        // Win32+DOS is as good as Win32: the lower id decides
        let both = [name(1, "long", 9, 5), name(3, "SHORT", 9, 4)];
        assert_eq!(pick(&both, &[9]).as_deref(), Some("SHORT"));
        // a surviving parent beats a lower id; among survivors the lower id wins
        let links = [name(1, "first stored", 7, 6), name(1, "second stored", 8, 3), name(1, "third stored", 9, 2)];
        assert_eq!(pick(&links, &[7]).as_deref(), Some("first stored"));
        assert_eq!(pick(&links, &[7, 8]).as_deref(), Some("second stored"));
        assert_eq!(pick(&links, &[7, 8, 9]).as_deref(), Some("third stored"));
        assert_eq!(pick(&links, &[]).as_deref(), Some("third stored"), "no parent survives: the lowest id");
        // a better namespace is preferred even when only the worse one has a parent
        let split = [name(0, "posix here", 7, 1), name(1, "win32 there", 8, 2)];
        assert_eq!(pick(&split, &[7]).as_deref(), Some("win32 there"));
        assert_eq!(pick(&[], &[7]), None);
    }

    #[test]
    fn range_helpers() {
        assert_eq!(merge_ranges(vec![(10, 5), (0, 4), (4, 2), (14, 10), (100, 0), (40, 1)]), [(0, 6), (10, 14), (40, 1)]);
        let set = [(0u64, 6u64), (10, 14), (40, 1)];
        assert!(intersects(&set, 5, 1));
        assert!(!intersects(&set, 6, 4));
        assert!(intersects(&set, 6, 5));
        assert!(intersects(&set, 23, 100));
        assert!(!intersects(&set, 24, 16));
        assert!(intersects(&set, 39, 2));
        assert!(!intersects(&set, 41, 1000));
        let s: BTreeSet<u64> = [0, 1, 2, 5, 7, 8].into_iter().collect();
        assert_eq!(ranges(&s), [(0, 3), (5, 1), (7, 2)]);
    }
}
