//! Tests of the store: the aux encoding, the read API, the fragment index,
//! hit packing, logical equality and rollback.

#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::panic
)]

use std::sync::atomic::AtomicBool;

use hds_carve::FileKind;
use hds_io::testing::XorShift;
use hds_ntfs::{parse_boot_sector, parse_file_record, FileRef, RunlistError, RunlistStatus};

use super::record::{decode_item, validate_region, AuxBuilder, AuxItems, ExtentTarget};
use super::*;
use crate::scan::tests::{mem, meta_for, put, rich_file, scan_into, MIB};
use crate::scan::{ScanOptions, ScanTuning};
use crate::session::Session;
use crate::test_build::*;

fn scan_image(img: Vec<u8>) -> ScanStore {
    let src = mem(img);
    let range = 0..src.size();
    let mut session = Session::new(meta_for(
        src.as_ref(),
        range.clone(),
        ScanOptions::default(),
    ));
    scan_into(
        src,
        range,
        ScanOptions::default(),
        &ScanTuning::default(),
        &mut session,
        &AtomicBool::new(false),
    );
    session.store().clone()
}

/// Encodes the aux region of a hand-built record.
fn region_of(rec: &[u8]) -> (Vec<u8>, AuxBuilder) {
    let mut bytes = rec.to_vec();
    let record = parse_file_record(&mut bytes).unwrap();
    let mut aux = AuxBuilder::default();
    for attr in record.attributes() {
        match attr.type_code {
            FILE_NAME => aux.push_name(&attr.file_name().unwrap(), attr.id),
            DATA => match attr.body {
                hds_ntfs::AttrBody::Resident(v) => {
                    aux.push_resident_data(attr.id, attr.flags, v.offset, v.len())
                }
                hds_ntfs::AttrBody::NonResident(e) => {
                    aux.push_extent(ExtentTarget::Data, attr.id, attr.flags, &e)
                }
                hds_ntfs::AttrBody::Malformed => {}
            },
            INDEX_ALLOCATION => aux.push_extent(
                ExtentTarget::IndexAllocation,
                attr.id,
                0,
                &attr.extent().unwrap(),
            ),
            ATTRIBUTE_LIST => {
                if let Some(hds_ntfs::AttributeList::Resident(entries)) = attr.attribute_list() {
                    for e in entries {
                        aux.push_attr_list_entry(&e);
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    aux.assemble(&mut out);
    (out, aux)
}

#[test]
fn aux_items_round_trip_in_canonical_order() {
    let long: String = "x".repeat(255);
    let rec = Rec::new(1)
        .attr_list(&[
            (0x80, "", 0, fref(1, 1), 3),
            (0xA0, "$I30", 0, fref(1, 1), 4),
            (0x80, "s", 0, fref(1, 1), 5),
        ])
        .name(fref(5, 5), &long, 0, 0x20)
        .name(fref(9, 2), "\u{263A}", 1, 0x1000_0000)
        .data_extent(0, 1, (8192, 5000, 4000), &runlist(&[(Some(10), 2)]), 0)
        .index_allocation(7, 8, 0, &runlist(&[(Some(99), 2)]))
        .size(2048)
        .build();
    let (region, aux) = region_of(&rec);
    let counts = validate_region(&region).unwrap();
    assert_eq!(counts, aux.counts);
    assert_eq!(
        (counts.names, counts.data, counts.ia, counts.al),
        (2, 1, 1, 3)
    );
    // Names first, then data, index allocation and the list entries.
    let kinds: Vec<u8> = AuxItems::new(&region)
        .map(|item| match item {
            record::AuxItem::Name(_) => 0,
            record::AuxItem::Data(_) => 1,
            record::AuxItem::IndexAllocation(_) => 2,
            record::AuxItem::AttrList(_) => 3,
        })
        .collect();
    assert_eq!(kinds, vec![0, 0, 1, 2, 3, 3, 3]);
    let names: Vec<_> = AuxItems::new(&region)
        .filter_map(|i| {
            if let record::AuxItem::Name(n) = i {
                Some(n)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(names[0].name.len(), 255);
    assert!(names[0].name.eq_str(&long));
    assert_eq!(names[1].name.to_string_lossy(), "\u{263A}");
    assert_eq!(names[1].parent, FileRef::new(9, 2));
    // A 255-unit narrow name takes 18 + 255 bytes; the wide one 18 + 2.
    let ia = AuxItems::new(&region)
        .find_map(|i| {
            if let record::AuxItem::IndexAllocation(e) = i {
                Some(e)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!((ia.start_vcn, ia.last_vcn, ia.sizes), (7, 8, None));
}

/// An extent item with start VCN 0, encoded by hand.
fn extent_item(status: u8, runlist: &[u8]) -> Vec<u8> {
    let mut v = vec![record::TAG_EXTENT0, 0, 3, 0, 0, 0, status];
    v.extend_from_slice(&1i64.to_le_bytes());
    for size in [8192u64, 5000, 4000] {
        v.extend_from_slice(&size.to_le_bytes());
    }
    v.extend_from_slice(&(runlist.len() as u16).to_le_bytes());
    v.extend_from_slice(runlist);
    v
}

#[test]
fn every_runlist_status_is_kept_and_must_agree_with_the_bytes() {
    let statuses = [
        RunlistStatus::Complete,
        RunlistStatus::Invalid(RunlistError::Unterminated),
        RunlistStatus::Invalid(RunlistError::Truncated),
        RunlistStatus::Invalid(RunlistError::FieldTooLarge),
        RunlistStatus::Invalid(RunlistError::BadLength),
        RunlistStatus::Invalid(RunlistError::NegativeLcn),
        RunlistStatus::Invalid(RunlistError::Overflow),
    ];
    let complete = [0x11, 0x02, 0x05, 0x00];
    let prefix = [0x11, 0x02, 0x05];
    for status in statuses {
        let code = record::status_code(status);
        assert_eq!(record::status_from_code(code), Some(status));
        let bytes = if status == RunlistStatus::Complete {
            &complete[..]
        } else {
            &prefix[..]
        };
        let item = extent_item(code, bytes);
        assert!(validate_region(&item).is_some(), "{status:?}");
        let Some((record::AuxItem::Data(DataExtent::NonResident(e)), len)) = decode_item(&item)
        else {
            panic!()
        };
        assert_eq!(len, item.len());
        assert_eq!(e.runlist_status, status);
        assert_eq!(
            e.runs().map(|r| (r.lcn, r.length)).collect::<Vec<_>>(),
            vec![(Some(5), 2)]
        );
        // Bytes that contradict the status are refused.
        let other = if status == RunlistStatus::Complete {
            &prefix[..]
        } else {
            &complete[..]
        };
        assert!(
            validate_region(&extent_item(code, other)).is_none(),
            "{status:?}"
        );
    }
    assert_eq!(record::status_from_code(7), None);
    assert!(validate_region(&extent_item(7, &complete)).is_none());
    // Trailing bytes behind a complete list are not part of it.
    assert!(validate_region(&extent_item(0, &[0x11, 0x02, 0x05, 0x00, 0x00])).is_none());
}

#[test]
fn hostile_aux_bytes_never_panic() {
    let mut rng = XorShift::new(3);
    let rec = Rec::new(1)
        .name(fref(5, 5), "file", 0, 0)
        .data_extent(0, 1, (8192, 5000, 4000), &runlist(&[(Some(10), 2)]), 0)
        .build();
    let (valid, _) = region_of(&rec);
    for round in 0..20_000 {
        let bytes: Vec<u8> = if round % 2 == 0 {
            let mut b = vec![0u8; rng.below(120) as usize];
            rng.fill(&mut b);
            if !b.is_empty() {
                b[0] = 1 + rng.below(5) as u8;
            }
            b
        } else {
            let mut b = valid.clone();
            let at = rng.below(b.len() as u64) as usize;
            b[at] = rng.next_u64() as u8;
            b.truncate(rng.below(b.len() as u64 + 1) as usize);
            b
        };
        let _ = validate_region(&bytes);
        for item in AuxItems::new(&bytes) {
            if let record::AuxItem::Data(DataExtent::NonResident(e)) = item {
                let _ = e.runs().count();
                let _ = e.clusters();
            }
            if let record::AuxItem::Name(n) = item {
                let _ = n.name.to_string_lossy();
            }
        }
    }
}

#[test]
fn the_fragment_index_groups_by_size_and_origin() {
    let mut img = vec![0u8; 4 * MIB];
    // Fragment A: records 10-14 at origin 0x10_0000 - 10 KiB.
    for n in 10..15u32 {
        put(
            &mut img,
            0x10_0000 + (n as usize - 10) * 1024,
            &rich_file(n).build(),
        );
    }
    // Fragment B: records 3000-3001 early on the disk: negative origin.
    for n in 3000..3002u32 {
        put(
            &mut img,
            0x2_0000 + (n as usize - 3000) * 1024,
            &rich_file(n).build(),
        );
    }
    // 4 KiB records at their own grid, and one without a number.
    put(&mut img, 0x20_0000, &rich_file(5).size(4096).build());
    put(
        &mut img,
        0x30_0000,
        &Rec::new(0).unnumbered().si([1, 1, 1, 1], 0).build(),
    );
    let store = scan_image(img);
    let index = store.fragment_index(|_, _| true);
    let groups: Vec<(u32, i64, Vec<Option<u32>>)> = index
        .fragments()
        .map(|f| {
            (
                f.record_size,
                f.origin,
                f.records
                    .iter()
                    .map(|&id| store.record(id).unwrap().record_number())
                    .collect(),
            )
        })
        .collect();
    assert_eq!(
        groups,
        vec![
            (1024, 0x2_0000 - 3000 * 1024, vec![Some(3000), Some(3001)]),
            (
                1024,
                0x10_0000 - 10 * 1024,
                vec![Some(10), Some(11), Some(12), Some(13), Some(14)]
            ),
            (4096, 0x20_0000 - 5 * 4096, vec![Some(5)]),
        ]
    );
    assert!(groups[0].1 < 0, "negative origin");
    assert_eq!(index.unnumbered().len(), 1);
    assert_eq!(index.numbered().len(), 8);
    // The filter leaves records out.
    let odd = store.fragment_index(|_, r| r.record_number().is_some_and(|n| n % 2 == 1));
    assert_eq!(odd.numbered().len(), 4);
    assert!(odd.unnumbered().is_empty());
}

#[test]
fn carve_hits_pack_offset_and_kind() {
    let codes: Vec<u8> = FileKind::all()
        .iter()
        .map(|&k| carve_kind_code(k))
        .collect();
    let mut sorted = codes.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), codes.len(), "codes are unique");
    assert!(!codes.contains(&0));
    for &kind in FileKind::all() {
        assert_eq!(carve_kind_from_code(carve_kind_code(kind)), Some(kind));
        let hit = CarveHit::new(512 * 777, kind).unwrap();
        assert_eq!((hit.disk_offset(), hit.kind()), (512 * 777, kind));
        assert_eq!(CarveHit::from_raw(hit.raw()), Some(hit));
    }
    assert_eq!(carve_kind_from_code(0), None);
    assert_eq!(carve_kind_from_code(200), None);
    assert_eq!(CarveHit::from_raw(777 << 8), None);
    assert_eq!(CarveHit::new(513, FileKind::Png), None);
    let top = CarveHit::new(u64::MAX - 511, FileKind::Rar).unwrap();
    assert_eq!(
        (top.disk_offset(), top.kind()),
        (u64::MAX - 511, FileKind::Rar)
    );
    assert_eq!(CarveHit::from_raw(top.raw()), Some(top));
    assert_eq!(CarveHit::from_raw(((1u64 << 55) << 8) | 2), None);
    // Sorting packed values sorts by offset.
    let a = CarveHit::new(1024, FileKind::Zip).unwrap();
    let b = CarveHit::new(1536, FileKind::Jpeg).unwrap();
    assert!(a < b);
}

#[test]
fn boot_sectors_from_files_are_checked_like_parsed_ones() {
    let parsed = parse_boot_sector(&boot_sector(512, 8, 1_000_000, 4, 2, -10, 1, 63)).unwrap();
    assert!(hits::boot_sector_is_sound(&parsed));
    let big = parse_boot_sector(&boot_sector(4096, 0xF7, 10_000, 4, 2, -12, -12, 0)).unwrap();
    assert!(hits::boot_sector_is_sound(&big));
    let mut bad = parsed;
    bad.cluster_size = 0;
    assert!(!hits::boot_sector_is_sound(&bad));
    let mut bad = parsed;
    bad.mft_lcn = parsed.total_clusters();
    assert!(!hits::boot_sector_is_sound(&bad));
    let mut bad = parsed;
    bad.total_sectors = u64::MAX;
    assert!(!hits::boot_sector_is_sound(&bad));
    let mut bad = parsed;
    bad.mft_record_size = 3000;
    assert!(!hits::boot_sector_is_sound(&bad));
    let mut bad = parsed;
    bad.sectors_per_cluster = 3;
    assert!(!hits::boot_sector_is_sound(&bad));
}

#[test]
fn equality_ignores_storage_order_and_sees_every_field() {
    let mut img = vec![0u8; 2 * MIB];
    for (i, at) in [0x1000usize, 0x1_0000, 0x8_0000].iter().enumerate() {
        put(&mut img, *at, &rich_file(i as u32 + 1).build());
    }
    let a = scan_image(img.clone());
    // The same records found in another order: a store built from the
    // records of `a` pushed in reverse.
    let mut b = ScanStore::new();
    for &id in a.records_by_offset().iter().rev() {
        assert!(b.push_record_raw(*a.record(id).unwrap(), a.aux_region(id)));
    }
    b.done = a.done.clone();
    b.drops = a.drops;
    b.zero = a.zero;
    b.warnings = a.warnings.clone();
    for hit in a.carve_hits() {
        b.push_carve(hit);
    }
    for hit in a.boot_hits() {
        b.push_boot(*hit);
    }
    assert_eq!(a, b);
    assert_eq!(b.record(RecordId(0)).unwrap().disk_offset(), 0x8_0000);
    assert_eq!(
        b.records_by_offset()
            .iter()
            .map(|id| id.0)
            .collect::<Vec<_>>(),
        vec![2, 1, 0]
    );
    // One differing field makes them differ.
    let mut c = b.clone();
    let mut r = *c.record(RecordId(0)).unwrap();
    r.lsn ^= 1;
    c.records.truncate(0);
    c.aux = Default::default();
    c.files = 0;
    c.dirs = 0;
    for &id in b.records_by_offset() {
        let mut rec = *b.record(id).unwrap();
        if id == RecordId(0) {
            rec.lsn = r.lsn;
        }
        c.push_record_raw(rec, b.aux_region(id));
    }
    c.invalidate();
    assert_ne!(b, c);
}

#[test]
fn rollback_restores_everything_including_the_counters() {
    let mut img = vec![0u8; 2 * MIB];
    put(&mut img, 0x1000, &rich_file(1).build());
    put(
        &mut img,
        0x2000,
        &Rec::new(2).flags(3).si([1, 1, 1, 1], 0).build(),
    );
    let a = scan_image(img);
    let mut s = a.clone();
    let mark = s.mark();
    let records: Vec<_> = a
        .records()
        .map(|(id, r)| (*r, a.aux_region(id).to_vec()))
        .collect();
    for (r, region) in &records {
        let mut r = *r;
        r.disk_offset += 0x10_0000;
        s.push_record_raw(r, region);
    }
    s.add_bad(0..512);
    s.push_carve(CarveHit::new(4096, FileKind::Gif).unwrap());
    assert_eq!(s.counters().files, 2);
    assert_eq!(s.counters().dirs, 2);
    s.rollback(mark);
    assert_eq!(s, a);
    assert_eq!(s.counters(), a.counters());
    assert!(s.bad_ranges().is_empty());
}

#[test]
fn lookups_by_offset() {
    let mut img = vec![0u8; 2 * MIB];
    put(&mut img, 0x1000, &rich_file(1).build());
    put(&mut img, 0x8000, &indx_block(0, 1, 4096, &[]));
    let s = scan_image(img);
    assert!(s.record_at(0x1000).is_some());
    assert!(s.record_at(0x1200).is_none());
    assert!(s.record_at(u64::MAX).is_none());
    assert!(s.index_block_at(0x8000).is_some());
    assert!(s.index_block_at(0x8200).is_none());
    assert_eq!(s.index_block_at(0x8000).unwrap().owner, None);
    assert!(s.record(RecordId(99)).is_none());
    assert_eq!(s.names(RecordId(99)).count(), 0);
    assert!(s.index_entry(EntryId(0)).is_none());
    assert!(s.memory_bytes() > s.record_bytes());
    assert_eq!(s.record_bytes(), 104 + s.aux_region(RecordId(0)).len());
}
