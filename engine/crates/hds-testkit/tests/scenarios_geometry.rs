//! What the geometry scenarios promise in their documentation, checked on
//! the image and the manifest they return, for several seeds.

mod common;

use std::collections::BTreeSet;

use common::{assert_reads_back, build, expected_path, intact_records, must_not, read_structure, shown, slots, MIB, SEEDS};
use hds_testkit::disk::Built;
use hds_testkit::facts::{ArtifactState, IndexBlockFact, RecordFact, RecordRole};
use hds_testkit::manifest::{Entry, EntryKind, Flag, Instance, Manifest, Presence, RecordState};

/// Runs `check` on the scenario built with every test seed.
fn each(name: &str, check: impl Fn(&Built)) {
    for seed in SEEDS {
        eprintln!("{name}, seed {seed}");
        check(&build(name, seed));
    }
}

/// Position of an index block inside its attribute, from the runs in the
/// owner's surviving records.
fn block_position(m: &Manifest, instance: &Instance, block: &IndexBlockFact) -> Option<u64> {
    let cs = instance.geometry.cluster_size as u64;
    let mut extents: Vec<_> = m
        .records
        .iter()
        .filter(|r| r.instance == instance.id && r.state == ArtifactState::Intact && r.role != RecordRole::StaleCopy)
        .filter(|r| (r.slot == block.owner.number && r.base.number == 0) || r.base.number == block.owner.number)
        .flat_map(|r| r.index_allocation.iter().filter_map(|s| s.non_resident.as_ref()))
        .collect();
    extents.sort_by_key(|x| x.lowest_vcn);
    let mut position = 0;
    for x in extents {
        for run in &x.runs {
            let start = instance.geometry.volume_offset + run.lcn? * cs;
            if (start..start + run.len * cs).contains(&block.offset) {
                return Some(position + block.offset - start);
            }
            position += run.len * cs;
        }
    }
    None
}

#[test]
fn index_block_numbers_follow_the_cluster_size() {
    // scenario, cluster size, VCN step of 4 KiB blocks, folders with index blocks that survive
    for (name, cluster, step, voters) in [
        ("cluster_512", 512u64, 8u64, 4usize),
        ("cluster_1k", 1024, 4, 4),
        ("cluster_2k", 2048, 2, 4),
        ("quick_format_same_geometry", 4096, 1, 1),
        ("cluster_8k", 8192, 8, 4),
        ("cluster_64k", 65_536, 8, 4),
        ("cluster_2m", 2 << 20, 8, 1),
    ] {
        each(name, |built| {
            let m = &built.manifest;
            let old = m.instance("old");
            assert_eq!(old.geometry.cluster_size as u64, cluster, "{name}");
            let mut checked = 0;
            let mut owners = BTreeSet::new();
            for block in m.index_blocks.iter().filter(|b| b.instance == old.id && b.state == ArtifactState::Intact) {
                assert_eq!(block.vcn % step, 0, "{name}: VCN {}", block.vcn);
                if let Some(position) = block_position(m, old, block) {
                    // ALGORITHMS F2: v x cluster when the cluster is not larger
                    // than the block, else v x 512
                    let unit = if cluster <= 4096 { cluster } else { 512 };
                    assert_eq!(block.vcn * unit, position, "{name}: block at {}", block.offset);
                    checked += 1;
                    owners.insert(block.owner);
                }
            }
            assert_eq!(owners.len(), voters, "{name}: folders whose index blocks can vote");
            assert!(checked >= 3, "{name}: {checked} blocks checked");
            // record and index block size as the boot sector encodes them
            let boot = built.image.read_vec(0, 512);
            let expect = match cluster {
                512 => (0x01, 0x02, 0x08),
                1024 => (0x02, 0x01, 0x04),
                2048 => (0x04, 0xF6, 0x02),
                4096 => (0x08, 0xF6, 0x01),
                8192 => (0x10, 0xF6, 0xF4),
                65_536 => (0x80, 0xF6, 0xF4),
                _ => (0xF4, 0xF6, 0xF4),
            };
            assert_eq!((boot[0x0D], boot[0x40], boot[0x44]), expect, "{name}");
            // every surviving file with clusters reads back with this cluster size
            let mut read = 0;
            for e in m.entries_of("old").filter(|e| shown(e) && e.kind == EntryKind::File && !e.resident && !e.deleted) {
                assert!(e.must_recover_exactly(), "{name}: {}", e.path);
                assert!(e.extents.iter().all(|x| x.bytes == x.clusters * cluster));
                if read < 40 {
                    assert_reads_back(built, e);
                    read += 1;
                }
            }
            assert!(read >= 20, "{name}: only {read} surviving files with clusters");
        });
    }
}

#[test]
fn large_clusters_hold_many_blocks_and_records() {
    each("cluster_64k", |built| {
        let m = &built.manifest;
        // the one large directory: many blocks, few clusters
        let big = m.entry("old", "Many files");
        let blocks = m.index_blocks.iter().filter(|b| b.owner_entry == Some(big.id)).count();
        let record = m.records.iter().find(|r| Some(r.offset) == big.record_offset).unwrap();
        let clusters: u64 = record.index_allocation[0].non_resident.as_ref().unwrap().runs.iter().map(|r| r.len).sum();
        assert!(blocks >= 17 && clusters == (blocks as u64).div_ceil(16), "{blocks} blocks in {clusters} clusters");
    });
    each("cluster_2m", |built| {
        let m = &built.manifest;
        assert_eq!(m.image_size, 2052 * MIB);
        assert!(built.image.stored_bytes() < 64 * MIB, "the image is sparse: {} bytes stored", built.image.stored_bytes());
        // one cluster holds 2048 records: nothing below record 2048 survives
        assert_eq!(slots(&intact_records(m, "old")).first(), Some(&2048));
        assert_eq!(m.instance("new").geometry.mft_records, 2048);
        // $MFTMirr is one cluster: a copy of all 2048 slots of the fresh MFT
        let new = m.instance("new").id;
        let originals = m.records.iter().filter(|r| r.instance == new && r.role != RecordRole::Mirror).count();
        let mirror: Vec<&RecordFact> = m.records.iter().filter(|r| r.instance == new && r.role == RecordRole::Mirror).collect();
        assert_eq!(mirror.len(), originals, "every record of the new MFT has its copy");
        assert!(mirror.iter().any(|r| r.slot == 37) && mirror.len() == 30);
        let two = m.entry("old", "Late/two-clusters.bin");
        assert!(two.must_recover_exactly() && two.extents.iter().map(|x| x.clusters).sum::<u64>() == 2);
    });
}

#[test]
fn large_records_and_large_sectors() {
    each("records_4k", |built| {
        let m = &built.manifest;
        assert!(m.records.iter().all(|r| r.size == 4096));
        let r = intact_records(m, "old")[0];
        let bytes = built.image.read_vec(r.offset, 4096);
        assert_eq!(u16::from_le_bytes([bytes[6], bytes[7]]), 9, "eight fixups and the number");
        for block in 0..8 {
            assert_eq!(bytes[block * 512 + 510..block * 512 + 512], bytes[0x30..0x32], "fixup stride is 512");
        }
        assert_eq!(u16::from_le_bytes([bytes[0x14], bytes[0x15]]), 0x48, "first attribute behind the 9-entry array");
        assert_eq!(slots(&intact_records(m, "old")).first(), Some(&256));
        // resident files of up to 3 KiB
        assert!(m.entries_of("old").any(|e| e.resident && e.size > 2000));
    });

    each("sector_4k", |built| {
        let m = &built.manifest;
        assert_eq!(m.sector_size, 4096);
        let g = &m.instance("old").geometry;
        assert_eq!(g.bytes_per_sector, 4096);
        assert_eq!(g.total_sectors, 96 * MIB / 4096 - 1);
        let backup = m.boot_sectors.iter().find(|b| b.backup).unwrap();
        assert_eq!(backup.offset, g.total_sectors * 4096);
        assert!(m.records.iter().all(|r| r.offset % 4096 == 0) && m.index_blocks.iter().all(|b| b.offset % 4096 == 0));
        assert_eq!(built.image.read_vec(0, 512)[0x0B..0x0E], [0x00, 0x10, 0x01]);
    });
}

#[test]
fn structures_straddle_chunk_ends_at_sector_63() {
    each("partition_at_sector_63", |built| {
        let m = &built.manifest;
        assert_eq!(m.partitions[0].offset, 63 * 512);
        assert_eq!(m.scan_range, (0, m.image_size));
        let record = m.records.iter().find(|r| r.offset == 8 * MIB - 512).expect("a record across the 8 MiB mark");
        assert_eq!((record.slot, record.state, record.role), (560, ArtifactState::Intact, RecordRole::Base));
        let block =
            m.index_blocks.iter().find(|b| b.offset == 16 * MIB - 512).expect("an index block across the 16 MiB mark");
        assert_eq!(block.state, ArtifactState::Intact);
        assert_eq!(m.entries[block.owner_entry.unwrap() as usize].path, "Many files");
        assert!(m.records.iter().all(|r| r.offset % 1024 == 512));
        // the MBR is there
        assert_eq!(built.image.read_vec(510, 2), [0x55, 0xAA]);
    });
}

#[test]
fn mft_fragments_with_a_negative_origin() {
    each("fragmented_mft", |built| {
        let m = &built.manifest;
        let g = &m.instance("old").geometry;
        assert_eq!(g.mft_extents.len(), 4);
        let origin = |slot: u64| {
            let e = g.mft_extents.iter().find(|e| (e.first_record..e.first_record + e.records).contains(&slot)).unwrap();
            e.offset as i64 - (e.first_record * 1024) as i64
        };
        assert!(origin(5200) < 0, "the last fragment has a negative origin");
        let records = intact_records(m, "old");
        let origins: BTreeSet<i64> = records.iter().map(|r| origin(r.slot)).collect();
        assert_eq!(origins.len(), 3, "three fragments survive");
        for o in &origins {
            let n = records.iter().filter(|r| origin(r.slot) == *o).count();
            assert!(n >= 64, "the fragment with origin {o} has {n} records");
        }
        assert!(!records.iter().any(|r| r.slot < 256), "the first fragment died in the format");
        // files in one fragment have their folder in another, both ways
        let old_entries: Vec<&Entry> = m.entries_of("old").collect();
        let crossing: Vec<(i64, i64)> = old_entries
            .iter()
            .filter(|e| shown(e) && e.names[0].parent.number >= 256)
            .map(|e| (origin(e.record), origin(e.names[0].parent.number)))
            .filter(|(a, b)| a != b)
            .collect();
        assert!(crossing.len() >= 80, "{}", crossing.len());
        assert!(crossing.iter().any(|&(file, _)| file < 0) && crossing.iter().any(|&(_, folder)| folder < 0));
        // and with their folders alive they keep their paths
        let late = m.entry("old", "Late arrivals");
        assert_eq!((late.record, shown(late)), (5120, true));
        let moved = old_entries.iter().filter(|e| e.path.starts_with("Late arrivals/file-")).count();
        assert_eq!(moved, 40);
        for e in old_entries.iter().filter(|e| e.path.starts_with("Late arrivals/") || e.path.starts_with("Many files/")) {
            assert_eq!(expected_path(e), e.path);
            assert!(e.must_recover_exactly(), "{}", e.path);
        }
    });
}

#[test]
fn an_mft_that_was_somewhere_else_survives_completely() {
    each("old_mft_elsewhere", |built| {
        let m = &built.manifest;
        let old = m.instance("old");
        let records = intact_records(m, "old");
        assert_eq!(slots(&records)[..16], (0..16).collect::<Vec<u64>>()[..]);
        assert_ne!(old.geometry.mft_extents[0].offset, m.instance("new").geometry.mft_extents[0].offset);
        let mirror: Vec<&RecordFact> = m
            .records
            .iter()
            .filter(|r| r.instance == old.id && r.role == RecordRole::Mirror && r.state == ArtifactState::Intact)
            .collect();
        assert_eq!(mirror.iter().map(|r| r.slot).collect::<Vec<_>>(), [0, 1, 2, 3], "the old $MFTMirr survives");
        // the old root directory record and everything below $Extend: hidden
        let root = m.entries_of("old").find(|e| e.record == 5).expect("the old root");
        assert_eq!(root.expect.presence, Presence::Hidden);
        let extend: Vec<&Entry> = m.entries_of("old").filter(|e| e.path.starts_with("$Extend")).collect();
        assert!(extend.len() >= 4 && extend.iter().all(|e| e.expect.presence == Presence::Hidden));
        // no old record was lost, but old data was overwritten by new metadata
        assert!(m.entries_of("old").all(|e| e.truth.record_state == RecordState::Intact));
        let ruined: Vec<&Entry> = m.entries_of("old").filter(|e| e.truth.content_intact == Some(false)).collect();
        assert!(ruined.len() > 20, "{}", ruined.len());
        for e in ruined.iter().filter(|e| shown(e) && !e.resident) {
            assert!(e.expect.flags_set.contains(&Flag::Overwritten), "{}", e.path);
        }
        for e in m.entries_of("old").filter(|e| shown(e)) {
            assert_eq!(expected_path(e), e.path);
        }
    });
}

#[test]
fn geometry_without_the_usual_evidence() {
    each("only_resident_survivors", |built| {
        let m = &built.manifest;
        assert_eq!(m.instance("old").geometry.cluster_size, 8192);
        let records = intact_records(m, "old");
        assert_eq!(records.len(), 300);
        assert!(
            records.iter().all(|r| r.index_allocation.is_empty() && r.data.iter().all(|d| d.non_resident.is_none())),
            "no surviving record may carry a runlist"
        );
        for e in m.entries_of("old").filter(|e| shown(e) && e.kind == EntryKind::File) {
            assert!(e.resident && e.must_recover_exactly());
            assert_reads_back(built, e);
        }
        assert_eq!(m.boot_sectors[0].cluster_size, 8192, "the boot sector is the only source of the cluster size");
    });

    each("signature_anchors", |built| {
        let m = &built.manifest;
        let old = m.instance("old");
        let in_range = |offset: u64| offset >= m.scan_range.0 && offset < m.scan_range.1;
        assert!(!m.boot_sectors.iter().any(|b| b.instance == old.id && in_range(b.offset)), "no old boot sector in range");
        let records = intact_records(m, "old");
        assert!(records.iter().all(|r| r.slot >= 256 && r.index_allocation.is_empty()), "no MFT map, no index vote");
        // index blocks of the old volume are in range, but the record of
        // the folder they belong to is gone: they cannot vote
        let ownerless: Vec<_> = m.index_blocks.iter().filter(|b| b.instance == old.id && in_range(b.offset)).collect();
        assert!(!ownerless.is_empty() && ownerless.iter().all(|b| b.owner.number < 256));
        let media: Vec<&Entry> =
            m.entries_of("old").filter(|e| shown(e) && e.content.as_ref().is_some_and(|c| c.kind.is_media())).collect();
        assert_eq!(media.len(), 42);
        for e in &media {
            assert!(e.must_recover_exactly() && e.extents.len() == 1, "{}", e.path);
            let x = &e.extents[0];
            assert_eq!(x.offset.unwrap() - x.lcn.unwrap() * 4096, 63 * 512, "each one anchors the true offset");
            let ext = e.name().rsplit('.').next().unwrap();
            assert_eq!(Some(ext), e.content.as_ref().unwrap().kind.extension());
            assert_reads_back(built, e);
            must_not(e, Flag::Overwritten);
        }
        assert_ne!(old.geometry.volume_offset, m.scan_range.0, "the partition start is the wrong answer");
        assert_eq!(read_structure(built, records[0].offset, 1024)[..4], *b"FILE");
    });
}
