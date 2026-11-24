//! Ready-made images: one function per situation, each returning an image
//! and its manifest.
//!
//! Every function documents the paragraphs of `docs/ALGORITHMS.md` it
//! exists to test; the same list is in the manifest (`Manifest::rules`).
//! [`catalog`] lists them all, so that a test can run a property over every
//! situation at once.
//!
//! The functions are deterministic: the same seed gives the same image and
//! the same manifest. Images are between 32 and 200 MiB of mostly empty
//! space and take a few milliseconds to a second to build; the one
//! exception, the million-record volume, lives in [`scale`] and is written
//! to a file.

pub mod accidents;
pub mod damage;
pub mod geometry;
pub mod names;
pub mod records;
pub mod scale;

use crate::disk::{Built, Disk, VolumeId};
use crate::populate::{populate, Tree, TreeSpec};
use crate::volume::params::VolumeParams;

/// One mebibyte.
pub const MIB: u64 = 1 << 20;

/// A ready-made scenario.
#[derive(Clone, Copy)]
pub struct ScenarioInfo {
    /// Name; equal to `Manifest::scenario` of what `build` returns.
    pub name: &'static str,
    /// Builds the image for a seed.
    pub build: fn(u64) -> Built,
}

impl std::fmt::Debug for ScenarioInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScenarioInfo").field("name", &self.name).finish()
    }
}

macro_rules! catalog {
    ($($module:ident :: $name:ident),* $(,)?) => {
        /// Every ready-made scenario except the million-record volume.
        pub fn catalog() -> Vec<ScenarioInfo> {
            vec![$(ScenarioInfo { name: stringify!($name), build: $module::$name }),*]
        }
    };
}

catalog![
    accidents::current_volume,
    accidents::quick_format_same_geometry,
    accidents::quick_format_blank_records,
    accidents::quick_format_untouched_slots,
    accidents::quick_format_cluster_change,
    accidents::repartitioned_other_offset,
    accidents::post_format_activity,
    accidents::new_mft_grew,
    accidents::live_mft_with_attribute_list,
    accidents::live_mft_partly_initialized,
    accidents::two_generations,
    accidents::three_generations,
    accidents::generations_not_separable,
    accidents::embedded_volume,
    accidents::embedded_volume_fragmented,
    accidents::old_volume_larger_than_range,
    accidents::whole_disk_two_volumes,
    geometry::cluster_512,
    geometry::cluster_1k,
    geometry::cluster_2k,
    geometry::cluster_8k,
    geometry::cluster_64k,
    geometry::cluster_2m,
    geometry::records_4k,
    geometry::sector_4k,
    geometry::partition_at_sector_63,
    geometry::fragmented_mft,
    geometry::old_mft_elsewhere,
    geometry::old_volume_larger_whole_disk,
    geometry::only_resident_survivors,
    geometry::signature_anchors,
    records::attribute_list_resident,
    records::attribute_list_nonresident,
    records::vcn0_in_extension,
    records::name_in_extension,
    records::extensions_in_other_fragment,
    records::deleted_file_with_extensions,
    records::free_extension_leftover,
    records::lost_base_with_extensions,
    records::lost_extension_records,
    records::index_allocation_split,
    records::runlist_oddities,
    names::namespaces,
    names::hard_links,
    names::odd_names,
    damage::torn_records,
    damage::pre31_volume,
    damage::pre31_mixed,
    damage::never_used_records,
    damage::stale_copies,
    damage::wrong_record_numbers,
    damage::parent_cycles,
    damage::reparse_and_flags,
    damage::bad_boot_sectors,
    damage::sparse_and_initialized,
    damage::deleted_and_reused,
    damage::lost_folder_names,
    damage::overlap_conflicts,
    damage::timestamp_edges,
    damage::streams_and_no_data,
];

/// Builds the scenario called `name`.
pub fn build(name: &str, seed: u64) -> Option<Built> {
    catalog().into_iter().find(|s| s.name == name).map(|s| (s.build)(seed))
}

/// Formats a volume, fills it with a tree and writes it.
pub(crate) fn old_volume(disk: &mut Disk, params: VolumeParams, tree: &TreeSpec) -> (VolumeId, Tree) {
    let id = disk.format(params);
    let tree = populate(disk.volume(id), tree);
    disk.render(id);
    (id, tree)
}

/// A 64 MiB Windows 10 volume prepared for a scenario about individual
/// records: its first 218 user records (38–255, the ones a quick format
/// destroys) are a folder of throw-away files, and record 256 is the folder
/// `Test`, which is returned. Everything a scenario adds afterwards
/// survives [`finish_formatted`].
pub(crate) fn stage(name: &str, seed: u64, description: &str, rules: &[&str]) -> (Disk, VolumeId, crate::volume::NodeId) {
    stage_with(name, seed, description, rules, VolumeParams::new(0, 64 * MIB).label("old"))
}

/// [`stage`] with explicit volume parameters.
pub(crate) fn stage_with(
    name: &str,
    seed: u64,
    description: &str,
    rules: &[&str],
    params: VolumeParams,
) -> (Disk, VolumeId, crate::volume::NodeId) {
    let size = params.offset + params.partition_bytes();
    let mut disk = Disk::new(name, seed, size);
    disk.describe(description, rules);
    let old = disk.format(params);
    let v = disk.volume(old);
    let root = v.root();
    let first = v.mkdir(root, "First things");
    let mut i = 0;
    while v.reference(first).number + i + 1 < 256 {
        i += 1;
        v.add_tagged(first, format!("early-{i:03}.dat").as_str(), 64 + i);
    }
    let test = v.mkdir(root, "Test");
    assert_eq!(v.reference(test).number, 256);
    (disk, old, test)
}

/// Writes the staged volume, quick-formats it and finishes the image.
pub(crate) fn finish_formatted(mut disk: Disk, old: VolumeId) -> Built {
    disk.render(old);
    reformat(&mut disk, old);
    disk.finish()
}

/// Quick-formats the partition of `old` the way Windows 10 does and writes
/// the new volume.
pub(crate) fn reformat(disk: &mut Disk, old: VolumeId) -> VolumeId {
    let id = disk.format(disk.reformat_params(old));
    disk.render(id);
    id
}
