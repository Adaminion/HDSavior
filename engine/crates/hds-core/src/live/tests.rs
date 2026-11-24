//! ALGORITHMS C1-C5 on constructed volumes, and the `LIVE` payload.

#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::single_range_in_vec_init,
    clippy::cast_possible_wrap,
    clippy::manual_is_multiple_of,
    clippy::cloned_ref_to_slice_refs
)]

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use hds_io::testing::{FaultySource, MemSource};
use hds_io::{BlockSource, RangedSource};

use super::*;
use crate::instances::testlab::*;

const MIB: u64 = 1 << 20;
const CS: u64 = 4096;

fn snap(lab: &Lab, range: Option<Range<u64>>) -> LiveSnapshot {
    let src = lab.source();
    let range = range.unwrap_or(0..src.size());
    let ranged = RangedSource::new(src, range).unwrap();
    take_snapshot(&ranged, SCAN, &AtomicBool::new(false)).unwrap()
}

#[test]
fn c1_c2_a_current_volume_is_read_through_its_own_structures() {
    let mut lab = Lab::new(16);
    let cl = lab.current_volume(0, 4096, 256, 64, t(1500), &[20, 33], &[1000..1100]);
    let s = snap(&lab, None);
    assert!(!s.incomplete && !s.writes_not_detectable);
    assert_eq!(s.volumes.len(), 1);
    let v = &s.volumes[0];
    assert_eq!((v.offset, v.cluster_size, v.record_size), (0, 4096, 1024));
    assert_eq!(v.mft_origin(), Some(MIB));
    assert_eq!(cl.origin, MIB);
    assert_eq!(
        v.mft_runs,
        vec![MftRun {
            vcn: 0,
            clusters: 16,
            lcn: Some(256)
        }]
    );
    assert_eq!(v.mft_initialized, 64 * 1024);
    assert_eq!(v.slot_count(), 64);
    assert!(v.slot_in_use(0) && v.slot_in_use(20) && v.slot_in_use(33));
    assert!(!v.slot_in_use(16) && !v.slot_in_use(21) && !v.slot_in_use(64));
    assert_eq!(v.slot_offset(21), Some(MIB + 21 * 1024));
    assert_eq!(v.slot_offset(64), None);
    assert_eq!(v.format_time, t(1500));
    assert!(v.allocated.contains_range(1000 * CS..1100 * CS));
    assert!(v.allocated.contains_range(MIB..MIB + 16 * CS));
    assert!(!v.allocated.contains(999 * CS) && !v.allocated.contains(1100 * CS));
    assert_eq!(v.mirror, vec![cl.mirror..cl.mirror + CS]);
}

#[test]
fn c1_only_the_range_start_and_partition_starts_are_candidates() {
    // A complete volume at 4 MiB, but no partition starts there.
    let mut lab = Lab::new(16);
    lab.current_volume(4 * MIB, 2048, 256, 64, t(1500), &[], &[]);
    let s = snap(&lab, None);
    assert!(s.volumes.is_empty());
    assert!(!s.writes_not_detectable);
    // With a partition table that names it, it is a current volume.
    lab.put(0, &mbr(&[(8192, 16384, 7)]));
    let s = snap(&lab, None);
    assert_eq!(s.partition_starts, vec![4 * MIB]);
    assert_eq!(s.volumes.len(), 1);
    assert_eq!(s.volumes[0].offset, 4 * MIB);
    // A partition session reads only its own start.
    let s = snap(&lab, Some(4 * MIB..12 * MIB));
    assert_eq!(s.volumes.len(), 1);
}

#[test]
fn c1_record_0_must_be_an_in_use_mft() {
    let mut lab = Lab::new(16);
    lab.current_volume(0, 4096, 256, 64, t(1500), &[], &[]);
    // Record 0 renamed: not a current volume, and writes are not detectable.
    lab.rec(
        MIB,
        &Rec::new(0).si([t(1); 4], 6).name(ROOT, "$NotMft", 3, 6),
    );
    let s = snap(&lab, None);
    assert!(s.volumes.is_empty());
    assert!(s.writes_not_detectable);
}

#[test]
fn c2_the_mft_attribute_list_is_followed() {
    let mut lab = Lab::new(16);
    lab.current_volume(0, 4096, 256, 64, t(1500), &[20], &[]);
    // Record 0 holds VCN 0..15; extension record 20 holds VCN 16..31 at LCN 600.
    let r0 = Rec::new(0)
        .si([t(1500); 4], 6)
        .name(ROOT, "$MFT", 3, 6)
        .attr_list(&[
            (0x10, "", 0, fref(0, 1), 0),
            (0x80, "", 0, fref(0, 1), 2),
            (0x80, "", 16, fref(20, 1), 0),
        ])
        .data_extent(
            0,
            15,
            (32 * CS, 128 * 1024, 128 * 1024),
            &runlist(&[(Some(256), 16)]),
            0,
        )
        .raw_attr(resident_attr(
            0xB0,
            "",
            9,
            0,
            &[0xFF, 0xFF, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        ));
    lab.rec(MIB, &r0);
    let x = Rec::new(20).base(fref(0, 1)).data_extent(
        16,
        31,
        (0, 0, 0),
        &runlist(&[(Some(600), 16)]),
        0,
    );
    lab.rec(MIB, &x);
    let s = snap(&lab, None);
    let v = &s.volumes[0];
    assert_eq!(v.mft_runs.len(), 2);
    assert_eq!(
        v.mft_runs[1],
        MftRun {
            vcn: 16,
            clusters: 16,
            lcn: Some(600)
        }
    );
    assert_eq!(v.slot_count(), 128);
    assert_eq!(v.slot_offset(64), Some(600 * CS));
}

#[test]
fn c2_reads_stay_inside_the_range_and_what_is_outside_makes_it_incomplete() {
    let mut lab = Lab::new(16);
    // $Bitmap lies beyond 2 MiB: outside a session range of [0, 2 MiB).
    lab.current_volume(0, 4096, 256, 64, t(1500), &[], &[]);
    let faulty = Arc::new(FaultySource::new(Arc::new(MemSource::new(lab.img.clone()))));
    let src: Arc<dyn BlockSource> = faulty.clone();
    let ranged = RangedSource::new(src, 0..MIB + 64 * 1024).unwrap();
    let s = take_snapshot(&ranged, SCAN, &AtomicBool::new(false)).unwrap();
    assert_eq!(s.volumes.len(), 1);
    assert!(s.incomplete && s.volumes[0].incomplete);
    assert_eq!(s.warnings(), vec![WARN_LIVE_VOLUME_INCOMPLETE]);
    for r in faulty.touched().iter() {
        assert!(r.end <= MIB + 64 * 1024, "read outside the range: {r:?}");
    }
    // Unknown allocation counts as written.
    assert!(s.volumes[0].allocated.contains(3000 * CS));
}

#[test]
fn c2_a_lost_device_ends_the_snapshot() {
    let mut lab = Lab::new(16);
    lab.current_volume(0, 4096, 256, 64, t(1500), &[], &[]);
    let faulty = Arc::new(FaultySource::new(Arc::new(MemSource::new(lab.img.clone()))));
    faulty.lose_after_reads(3);
    let src: Arc<dyn BlockSource> = faulty;
    let ranged = RangedSource::new(src, 0..16 * MIB).unwrap();
    let r = take_snapshot(&ranged, SCAN, &AtomicBool::new(false));
    assert!(matches!(r, Err(LiveError::SourceLost(_))));
}

#[test]
fn c2_a_non_ntfs_file_system_is_not_detectable() {
    let mut lab = Lab::new(4);
    let mut s = vec![0u8; 512];
    s[3..11].copy_from_slice(b"EXFAT   ");
    s[510] = 0x55;
    s[511] = 0xAA;
    lab.put(0, &s);
    let snap = snap(&lab, None);
    assert!(snap.volumes.is_empty() && snap.writes_not_detectable);
    assert_eq!(snap.warnings(), vec![WARN_WRITES_NOT_DETECTABLE]);
}

/// A current volume at 0 with old and new records in its slots.
fn classes_lab() -> (Lab, CurrentLayout) {
    let mut lab = Lab::new(16);
    let cl = lab.current_volume(0, 4096, 256, 64, t(1500), &[21], &[3000..3010]);
    let o = cl.origin;
    lab.rec(o, &file(20, 1, ROOT, "old.txt", t(10), Some((2000, 4)))); // free, older: old
    lab.rec(o, &file(21, 1, ROOT, "live.txt", t(1600), None)); // in use: live
    lab.rec(o, &file(22, 1, ROOT, "gone.txt", t(1600), Some((2500, 8)))); // free, newer: post_format
                                                                          // Extension records without $STANDARD_INFORMATION.
    lab.rec(
        o,
        &Rec::new(23).base(fref(20, 1)).data_extent(
            4,
            7,
            (0, 0, 0),
            &runlist(&[(Some(2600), 4)]),
            0,
        ),
    ); // in use flag: old
    lab.rec(
        o,
        &Rec::new(24).flags(0).base(fref(20, 1)).resident_data(b"x"),
    ); // base old & resolves: old
    lab.rec(
        o,
        &Rec::new(25).flags(0).base(fref(22, 1)).resident_data(b"x"),
    ); // base post_format
    lab.rec(
        o,
        &Rec::new(26).flags(0).base(fref(20, 7)).resident_data(b"x"),
    ); // does not resolve
       // A record copied into $MFTMirr.
    lab.rec_at(
        cl.mirror,
        &file(0, 1, ROOT, "mirror", t(1500), None).unnumbered(),
    );
    // Beyond the initialized size: not a current-volume record.
    lab.rec(o, &file(70, 1, ROOT, "beyond.txt", t(1600), None));
    (lab, cl)
}

#[test]
fn c3_classes_of_records_in_current_slots() {
    let (lab, cl) = classes_lab();
    let run = lab.run(None);
    let l = &run.layout;
    let o = cl.origin;
    let class = |n: u64| l.class(run.id(o + n * 1024));
    assert_eq!(class(20), RecordClass::Old);
    assert_eq!(class(21), RecordClass::Live);
    assert_eq!(class(22), RecordClass::PostFormat);
    assert_eq!(class(23), RecordClass::Old);
    assert_eq!(class(24), RecordClass::Old);
    assert_eq!(class(25), RecordClass::PostFormat);
    assert_eq!(class(26), RecordClass::PostFormat);
    assert_eq!(class(70), RecordClass::Old);
    assert_eq!(class(0), RecordClass::Live);
    assert_eq!(l.class(run.id(cl.mirror)), RecordClass::Live);
    // Live and post_format records belong to no instance.
    assert_eq!(l.instance_of(run.id(o + 21 * 1024)), None);
    assert_eq!(l.instance_of(run.id(o + 22 * 1024)), None);
    assert!(l.instance_of(run.id(o + 20 * 1024)).is_some());
    let live = l.live();
    // 0, 1, 3, 6 (system), 21 and the mirror copy; 22, 25, 26.
    assert_eq!(live.live_records(), 6);
    assert_eq!(live.post_format_records(), 3);
}

#[test]
fn c4_c5_current_index_data_and_the_overwritten_set() {
    let (mut lab, cl) = classes_lab();
    // An index block inside live-allocated clusters, one outside.
    let e_new = index_entry(fref(40, 1), ROOT, "newdir", 1, DIR_FLAG, [t(1600); 4]);
    let e_old = index_entry(fref(41, 1), ROOT, "olddir", 1, DIR_FLAG, [t(10); 4]);
    lab.put(3000 * CS, &indx_block(0, 9, 4096, &[e_old.clone()]));
    lab.put(3500 * CS, &indx_block(0, 9, 4096, &[e_old, e_new]));
    let run = lab.run(None);
    let store = run.store();
    let live = run.layout.live();
    let blocks = store.index_blocks();
    for (i, b) in blocks.iter().enumerate() {
        assert_eq!(
            live.is_current_block(i),
            b.disk_offset == 3000 * CS,
            "{b:?}"
        );
    }
    for (id, e) in store.index_entries() {
        let in_current_block = e.source.offset == 3000 * CS;
        let newer = e.mft_changed >= t(1500);
        assert_eq!(
            live.is_current_entry(id),
            in_current_block || newer,
            "{}",
            e.name.to_string_lossy()
        );
    }
    // C5: live-allocated clusters and the post_format record's extents.
    let ow = live.overwritten();
    assert!(ow.contains_range(3000 * CS..3010 * CS));
    assert!(ow.contains_range(2500 * CS..2508 * CS));
    assert!(!ow.contains(2000 * CS));
    let _ = cl;
}

#[test]
fn live_state_round_trips_and_hostile_payloads_are_refused() {
    let (lab, _) = classes_lab();
    let s = snap(&lab, None);
    let state = LiveState {
        scan_time: Some(SCAN),
        snapshot: Some(s),
    };
    let p = state.to_payload();
    assert_eq!(LiveState::from_payload(&p).unwrap(), state);
    assert_eq!(
        LiveState::from_payload(&LiveState::default().to_payload()).unwrap(),
        LiveState::default()
    );
    for cut in 0..p.len() {
        assert!(LiveState::from_payload(&p[..cut]).is_err());
    }
    let mut longer = p.clone();
    longer.push(0);
    assert!(LiveState::from_payload(&longer).is_err());
    // Counts that the payload cannot hold are refused before allocating.
    let mut bomb = p[..14].to_vec();
    bomb.extend_from_slice(&u64::MAX.to_le_bytes());
    assert!(LiveState::from_payload(&bomb).is_err());
    let mut rng = hds_io::testing::XorShift::new(7);
    for _ in 0..2000 {
        let mut m = p.clone();
        let i = rng.below(m.len() as u64) as usize;
        m[i] ^= 1 << rng.below(8);
        let _ = LiveState::from_payload(&m);
    }
}
