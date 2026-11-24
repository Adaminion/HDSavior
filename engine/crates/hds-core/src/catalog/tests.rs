//! Tests of ALGORITHMS E: each rule on a constructed image, scanned by the
//! real scanner and analysed by C/D/F, so that dropping the rule fails the
//! test.

#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cast_possible_wrap
)]

use hds_recover::{Flag, FlagSet};

use super::*;
use crate::instances::testlab::*;
use crate::store::ScanStore;

/// Test support shared with the health and query tests.
pub(crate) mod support {
    use super::*;

    /// Origin of the lost volume's MFT (cluster size 4096, volume at 0).
    pub const ORIGIN: u64 = MIB;
    /// One MiB.
    pub const MIB: u64 = 1 << 20;

    /// A lab image with a lost volume at offset 0 whose geometry
    /// validates: `pngs` PNG files (records 64..) in the root, each with
    /// a PNG head in its first cluster (LCN 2048 + 2k).
    pub fn base_lab(mib: usize, pngs: u32) -> Lab {
        let mut lab = Lab::new(mib);
        for k in 0..pngs {
            let lcn = 2048 + 2 * u64::from(k);
            lab.rec(
                ORIGIN,
                &file(64 + k, 1, ROOT, &format!("p{k}.png"), t(10), Some((lcn, 1))),
            );
            lab.put(lcn * 4096, &png_head());
        }
        lab
    }

    /// The catalog of an analysed lab image.
    pub fn catalog(run: &Run) -> Catalog {
        Catalog::build(&CatalogInput::new(
            run.store(),
            &run.layout,
            &run.heads,
            &[],
        ))
        .unwrap()
    }

    /// The node at a display path.
    pub fn at(cat: &Catalog, store: &ScanStore, path: &str) -> NodeId {
        find(cat, store, path).unwrap_or_else(|| {
            let all: Vec<String> = cat.node_ids().map(|i| cat.path(store, i)).collect();
            panic!("no node at {path:?}; nodes: {all:?}")
        })
    }

    /// The node at a display path, if any.
    pub fn find(cat: &Catalog, store: &ScanStore, path: &str) -> Option<NodeId> {
        cat.node_ids().find(|&i| cat.path(store, i) == path)
    }

    /// The node of the record at `off`.
    pub fn node_at(cat: &Catalog, run: &Run, off: u64) -> NodeId {
        cat.node_of_record(run.id(off))
            .unwrap_or_else(|| panic!("no node for the record at {off}"))
    }

    /// Disk offset of record `n` of the lost volume.
    pub fn off(n: u32) -> u64 {
        ORIGIN + u64::from(n) * 1024
    }

    /// A free (deleted) copy of a record builder.
    pub fn freed(r: Rec) -> Rec {
        let f = r.flags & !1;
        r.flags(f)
    }
}

use support::*;

fn ext(n: u32, base: u64) -> Rec {
    Rec::new(n).base(base)
}

#[test]
fn e1_extension_records_join_their_base_and_e7_orphans() {
    let mut lab = base_lab(32, 24);
    // 100: base with an attribute list; its data extent sits in 101.
    lab.rec(
        ORIGIN,
        &Rec::new(100)
            .si([t(10); 4], 0x20)
            .name(ROOT, "big.bin", 1, 0x20)
            .attr_list(&[(0x80, "", 0, fref(101, 1), 0)]),
    );
    lab.rec(
        ORIGIN,
        &ext(101, fref(100, 1)).data_extent(
            0,
            2,
            (3 * 4096, 10_000, 10_000),
            &runlist(&[(Some(3000), 3)]),
            0,
        ),
    );
    // A free extension of an in-use base: a leftover, ignored.
    lab.rec(
        ORIGIN,
        &freed(ext(106, fref(100, 1)).data_extent(
            0,
            0,
            (4096, 4096, 4096),
            &runlist(&[(Some(3100), 1)]),
            0,
        )),
    );
    // Orphans: named (own $FILE_NAME), nameless, base below 24, no data.
    lab.rec(
        ORIGIN,
        &ext(102, fref(150, 3))
            .name(ROOT, "orph.txt", 1, 0x20)
            .resident_data(b"orphan"),
    );
    lab.rec(
        ORIGIN,
        &ext(103, fref(151, 1)).data_extent(
            0,
            0,
            (4096, 100, 100),
            &runlist(&[(Some(3200), 1)]),
            0,
        ),
    );
    lab.put(3200 * 4096, &png_head());
    lab.rec(ORIGIN, &ext(104, fref(20, 1)).resident_data(b"x"));
    lab.rec(
        ORIGIN,
        &ext(105, fref(152, 1)).name(ROOT, "nodata", 1, 0x20),
    );
    let run = lab.run(None);
    let store = run.store();
    let cat = catalog(&run);
    let big = at(&cat, store, "big.bin");
    assert_eq!(cat.tree.exts_of(big.0), vec![run.id(off(101))]);
    let row = cat.row(store, big, false).unwrap();
    assert_eq!(row.size, 10_000, "size from the extension's VCN-0 extent");
    assert!(!row.flags.contains(Flag::MissingFragments));
    // The orphan named by its own $FILE_NAME sits where that name says.
    let orph = at(&cat, store, "orph.txt");
    assert_eq!(cat.row(store, orph, false).unwrap().size, 6);
    // Nameless orphan: [Orphaned files]/File #151 plus the sniffed kind
    // (a head of its first cluster was stored by geometry validation).
    let names: Vec<String> = (0..cat.node_count() as u32)
        .map(|i| cat.path(store, NodeId(i)))
        .filter(|p| p.starts_with(ORPHANED_FILES))
        .collect();
    assert!(
        names
            .iter()
            .any(|p| p.starts_with("[Orphaned files]/File #151")),
        "{names:?}"
    );
    assert!(!names
        .iter()
        .any(|p| p.contains("#20") || p.contains("#152") || p.contains("#100")));
    assert!(
        find(&cat, store, "nodata").is_none(),
        "an orphan without data is dropped"
    );
    // The leftover 106 is in no group.
    assert!(cat.tree.exts.iter().all(|&(_, r)| r != run.id(off(106))));
    assert!(cat.node_of_record(run.id(off(106))).is_none());
}

#[test]
fn e2_extents_merge_by_vcn_and_gaps_are_flagged() {
    let mut lab = base_lab(32, 24);
    let f = |n: u32, name: &str| Rec::new(n).si([t(10); 4], 0x20).name(ROOT, name, 1, 0x20);
    // 110 + 111: VCN 0-1 in the base, VCN 2 in the extension: complete.
    lab.rec(
        ORIGIN,
        &f(110, "two.bin")
            .attr_list(&[
                (0x80, "", 0, fref(110, 1), 0),
                (0x80, "", 2, fref(111, 1), 0),
            ])
            .data_extent(
                0,
                1,
                (3 * 4096, 3 * 4096, 3 * 4096),
                &runlist(&[(Some(4000), 2)]),
                0,
            ),
    );
    lab.rec(
        ORIGIN,
        &ext(111, fref(110, 1)).data_extent(2, 2, (0, 0, 0), &runlist(&[(Some(4100), 1)]), 0),
    );
    // 112: the chain maps fewer clusters than the real size needs.
    lab.rec(
        ORIGIN,
        &f(112, "short.bin").data_extent(
            0,
            1,
            (3 * 4096, 3 * 4096, 3 * 4096),
            &runlist(&[(Some(4200), 2)]),
            0,
        ),
    );
    // 113: only the extent starting at VCN 5 survives (in 114).
    lab.rec(
        ORIGIN,
        &f(113, "tail.bin").attr_list(&[(0x80, "", 5, fref(114, 1), 0)]),
    );
    lab.rec(
        ORIGIN,
        &ext(114, fref(113, 1)).data_extent(5, 6, (0, 0, 0), &runlist(&[(Some(4300), 2)]), 0),
    );
    // 115: an attribute list but no extent anywhere; 116: nothing at all.
    lab.rec(
        ORIGIN,
        &f(115, "listed.bin").attr_list(&[(0x80, "", 0, fref(117, 1), 0)]),
    );
    lab.rec(ORIGIN, &f(116, "empty.bin"));
    let run = lab.run(None);
    let store = run.store();
    let cat = catalog(&run);
    let flags = |p: &str| cat.flags(at(&cat, store, p));
    let two = at(&cat, store, "two.bin");
    assert!(!flags("two.bin").contains(Flag::MissingFragments));
    let (src, _) = cat.file_source(store, two.0).unwrap();
    assert_eq!(
        src,
        hds_recover::FileSource::Extents {
            runs: vec![
                hds_recover::Run::Data {
                    offset: 4000 * 4096,
                    len: 2 * 4096
                },
                hds_recover::Run::Data {
                    offset: 4100 * 4096,
                    len: 4096
                }
            ],
            real_size: 3 * 4096,
            initialized_size: 3 * 4096
        }
    );
    assert!(flags("short.bin").contains(Flag::MissingFragments));
    let tail = flags("tail.bin");
    assert!(tail.contains(Flag::MissingFragments) && tail.contains(Flag::SizeEstimated));
    assert_eq!(
        cat.row(store, at(&cat, store, "tail.bin"), false)
            .unwrap()
            .size,
        7 * 4096
    );
    let (src, _) = cat
        .file_source(store, at(&cat, store, "tail.bin").0)
        .unwrap();
    match src {
        hds_recover::FileSource::Extents { runs, .. } => {
            assert_eq!(
                runs[0],
                hds_recover::Run::Hole { len: 5 * 4096 },
                "VCNs 0-4 read as zeros"
            )
        }
        other => panic!("{other:?}"),
    }
    let listed = flags("listed.bin");
    assert!(listed.contains(Flag::MissingFragments) && !listed.contains(Flag::NoData));
    let empty = flags("empty.bin");
    assert!(empty.contains(Flag::NoData) && !empty.contains(Flag::MissingFragments));
    assert_eq!(
        cat.row(store, at(&cat, store, "empty.bin"), false)
            .unwrap()
            .size,
        0
    );
}

#[test]
fn e3_e4_e5_directories_names_and_parents() {
    let mut lab = base_lab(32, 24);
    // E3: header flag clear, name flag set: a directory.
    lab.rec(
        ORIGIN,
        &Rec::new(120)
            .si([t(10); 4], 0x10)
            .name(ROOT, "byname", 1, DIR_FLAG),
    );
    lab.rec(
        ORIGIN,
        &file(121, 1, fref(120, 1), "in-byname.txt", t(10), None),
    );
    // E4: namespace first (Win32 beats POSIX and DOS even when its parent
    // does not resolve); within a namespace the resolving parent wins.
    lab.rec(ORIGIN, &dir(132, 1, ROOT, "docs", t(10)));
    lab.rec(
        ORIGIN,
        &Rec::new(130)
            .si([t(10); 4], 0x20)
            .name(ROOT, "PHOTO~1.JPG", 2, 0x20)
            .name(fref(999, 1), "Photo one.jpg", 1, 0x20)
            .name(ROOT, "photo-posix.jpg", 0, 0x20)
            .resident_data(b"x"),
    );
    lab.rec(
        ORIGIN,
        &Rec::new(131)
            .si([t(10); 4], 0x20)
            .name(fref(998, 1), "a-link.txt", 1, 0x20)
            .name(fref(132, 1), "b-link.txt", 1, 0x20)
            .resident_data(b"y"),
    );
    // E5: reference rule. 140 in use with sequence 2; 145 free with
    // sequence 3 (resolves (145, 2), not (145, 1)); 144 a free file.
    lab.rec(ORIGIN, &dir(140, 2, ROOT, "two", t(10)));
    lab.rec(
        ORIGIN,
        &file(141, 1, fref(140, 2), "under-two.txt", t(10), None),
    );
    lab.rec(ORIGIN, &freed(dir(145, 3, ROOT, "gone", t(10))));
    lab.rec(
        ORIGIN,
        &file(146, 1, fref(145, 2), "in-gone.txt", t(10), None),
    );
    lab.rec(
        ORIGIN,
        &file(147, 1, fref(145, 1), "wrong-seq.txt", t(10), None),
    );
    lab.rec(
        ORIGIN,
        &freed(file(144, 2, ROOT, "a-file.txt", t(10), None)),
    );
    lab.rec(
        ORIGIN,
        &file(143, 1, fref(144, 1), "under-file.txt", t(10), None),
    );
    // $Extend: omitted with everything below.
    lab.rec(ORIGIN, &dir(150, 1, fref(11, 11), "$Deleted", t(10)));
    lab.rec(
        ORIGIN,
        &file(151, 1, fref(150, 1), "below-extend.txt", t(10), None),
    );
    let run = lab.run(None);
    let store = run.store();
    let cat = catalog(&run);
    assert_eq!(cat.kind(at(&cat, store, "byname")), Some(NodeKind::Dir));
    at(&cat, store, "byname/in-byname.txt");
    at(&cat, store, "[Lost folders]/Folder #999/Photo one.jpg");
    let b = at(&cat, store, "docs/b-link.txt");
    let info = cat.node_info(store, b.wire()).unwrap();
    assert_eq!(info.other_names, vec!["a-link.txt".to_string()]);
    let photo = cat
        .node_info(
            store,
            at(&cat, store, "[Lost folders]/Folder #999/Photo one.jpg").wire(),
        )
        .unwrap();
    assert_eq!(
        photo.other_names,
        vec!["photo-posix.jpg".to_string()],
        "DOS aliases are not links"
    );
    at(&cat, store, "two/under-two.txt");
    at(&cat, store, "gone/in-gone.txt");
    // A reference that does not resolve leads to a lost folder.
    at(&cat, store, "[Lost folders]/Folder #145/wrong-seq.txt");
    at(&cat, store, "[Lost folders]/Folder #144/under-file.txt");
    assert!(cat.node_of_record(run.id(off(150))).is_none());
    assert!(cat.node_of_record(run.id(off(151))).is_none());
    assert!((0..cat.node_count() as u32).all(|i| !cat.name(store, NodeId(i)).contains("extend")));
}

#[test]
fn e6_lost_folders_named_by_index_entries() {
    let mut lab = base_lab(32, 24);
    let times = |d| [t(d); 4];
    // 161 holds entries for the lost 160 in its $INDEX_ROOT: Win32 and DOS.
    lab.rec(
        ORIGIN,
        &dir(161, 1, ROOT, "parent", t(10)).index_root(&[
            index_entry(
                fref(160, 1),
                fref(161, 1),
                "HOLIDA~1",
                2,
                DIR_FLAG,
                times(10),
            ),
            index_entry(
                fref(160, 1),
                fref(161, 1),
                "Holiday",
                1,
                DIR_FLAG,
                times(10),
            ),
        ]),
    );
    lab.rec(
        ORIGIN,
        &file(162, 1, fref(160, 1), "beach.txt", t(10), None),
    );
    // Two entries for 163: the one with the later entry time wins.
    lab.rec(
        ORIGIN,
        &dir(165, 1, ROOT, "a", t(10)).index_root(&[index_entry(
            fref(163, 1),
            ROOT,
            "Old",
            1,
            DIR_FLAG,
            times(5),
        )]),
    );
    lab.rec(
        ORIGIN,
        &dir(166, 1, ROOT, "b", t(10)).index_root(&[index_entry(
            fref(163, 1),
            ROOT,
            "New",
            1,
            DIR_FLAG,
            times(12),
        )]),
    );
    lab.rec(ORIGIN, &file(167, 1, fref(163, 1), "x.txt", t(10), None));
    // Two sequences of one number, no entry: Folder #164.1, #164.2.
    lab.rec(ORIGIN, &file(168, 1, fref(164, 1), "s1.txt", t(10), None));
    lab.rec(ORIGIN, &file(169, 1, fref(164, 2), "s2.txt", t(10), None));
    // An INDX block owned by dir 170 names the lost 171.
    lab.rec(ORIGIN, &dir(170, 1, ROOT, "owner", t(10)));
    lab.rec(
        ORIGIN,
        &file(172, 1, fref(171, 1), "in-171.txt", t(10), None),
    );
    lab.put(
        5000 * 4096,
        &indx_block(
            0,
            9,
            4096,
            &[index_entry(
                fref(171, 1),
                fref(170, 1),
                "FromBlock",
                1,
                DIR_FLAG,
                times(10),
            )],
        ),
    );
    // Owner 173 is held by no instance: usable when the entry time is not
    // more than a day older than the instance's oldest record (t(10)).
    lab.rec(
        ORIGIN,
        &file(175, 1, fref(174, 1), "in-174.txt", t(10), None),
    );
    lab.rec(
        ORIGIN,
        &file(177, 1, fref(176, 1), "in-176.txt", t(10), None),
    );
    lab.put(
        5001 * 4096,
        &indx_block(
            0,
            9,
            4096,
            &[index_entry(
                fref(174, 1),
                fref(173, 1),
                "Recent",
                1,
                DIR_FLAG,
                times(10),
            )],
        ),
    );
    lab.put(
        5002 * 4096,
        &indx_block(
            0,
            9,
            4096,
            &[index_entry(
                fref(176, 1),
                fref(173, 1),
                "Ancient",
                1,
                DIR_FLAG,
                times(3),
            )],
        ),
    );
    let run = lab.run(None);
    let store = run.store();
    let cat = catalog(&run);
    let holiday = at(&cat, store, "parent/Holiday/beach.txt");
    let lost = cat.parent(holiday).unwrap();
    assert_eq!(cat.kind(lost), Some(NodeKind::LostDir));
    assert!(cat.lost_folder_named(lost));
    assert_eq!(
        cat.lost_key(lost).map(|k| (k.number, k.sequence)),
        Some((160, 1))
    );
    at(&cat, store, "New/x.txt");
    at(&cat, store, "[Lost folders]/Folder #164.1/s1.txt");
    at(&cat, store, "[Lost folders]/Folder #164.2/s2.txt");
    at(&cat, store, "owner/FromBlock/in-171.txt");
    // Its parent reference (173, 1) leads to a nameless lost folder.
    at(&cat, store, "[Lost folders]/Folder #173/Recent/in-174.txt");
    at(&cat, store, "[Lost folders]/Folder #176/in-176.txt");
}

#[test]
fn e6_entries_inside_file_data_and_on_the_wrong_side_are_not_used() {
    let mut lab = base_lab(32, 24);
    // An in-use file whose data holds an INDX block naming lost 181 (an
    // embedded file system's index): not usable.
    lab.rec(
        ORIGIN,
        &file(180, 1, ROOT, "disk.img", t(10), Some((6000, 4))),
    );
    lab.rec(ORIGIN, &dir(182, 1, ROOT, "owner", t(10)));
    lab.put(
        6001 * 4096,
        &indx_block(
            0,
            9,
            4096,
            &[index_entry(
                fref(181, 1),
                fref(182, 1),
                "Inside",
                1,
                DIR_FLAG,
                [t(10); 4],
            )],
        ),
    );
    lab.rec(ORIGIN, &file(183, 1, fref(181, 1), "f.txt", t(10), None));
    let run = lab.run(None);
    let store = run.store();
    let cat = catalog(&run);
    at(&cat, store, "[Lost folders]/Folder #181/f.txt");
}

#[test]
fn e6b_a_stale_copy_among_the_strays_names_a_lost_folder() {
    let mut lab = base_lab(48, 40);
    // Main: files 64..103 (base_lab) and 190 in the lost folder (185, 1).
    lab.rec(ORIGIN, &file(190, 1, fref(185, 1), "kept.txt", t(10), None));
    // A copy of main's records 64..87 elsewhere (a copy set: same number,
    // sequence and creation time) plus the directory 185 that main lost.
    let copy_origin = 20 * MIB;
    for k in 0..24u32 {
        let lcn = 2048 + 2 * u64::from(k);
        lab.rec(
            copy_origin,
            &file(64 + k, 1, ROOT, &format!("p{k}.png"), t(10), Some((lcn, 1))),
        );
    }
    lab.rec(copy_origin, &dir(185, 1, ROOT, "Recovered name", t(10)));
    let run = lab.run(None);
    let store = run.store();
    let cat = catalog(&run);
    let strays = run
        .layout
        .instances()
        .iter()
        .find(|i| i.kind == InstanceKind::Strays)
        .unwrap();
    assert!(strays.records.len() >= 25, "the copy went to strays");
    at(&cat, store, "Recovered name/kept.txt");
}

#[test]
fn e8_cycles_are_cut_at_lost_folders() {
    let mut lab = base_lab(32, 24);
    lab.rec(ORIGIN, &dir(200, 1, fref(201, 1), "ping", t(10)));
    lab.rec(ORIGIN, &dir(201, 1, fref(200, 1), "pong", t(10)));
    lab.rec(
        ORIGIN,
        &file(202, 1, fref(200, 1), "in-ping.txt", t(10), None),
    );
    lab.rec(ORIGIN, &dir(203, 1, fref(203, 1), "self", t(10)));
    // Lost folders naming each other.
    lab.rec(
        ORIGIN,
        &dir(212, 1, ROOT, "names", t(10)).index_root(&[
            index_entry(fref(210, 1), fref(211, 1), "L210", 1, DIR_FLAG, [t(10); 4]),
            index_entry(fref(211, 1), fref(210, 1), "L211", 1, DIR_FLAG, [t(10); 4]),
        ]),
    );
    lab.rec(ORIGIN, &file(213, 1, fref(210, 1), "deep.txt", t(10), None));
    let run = lab.run(None);
    let store = run.store();
    let cat = catalog(&run);
    let p = cat.path(store, node_at(&cat, &run, off(202)));
    assert!(
        p == "[Lost folders]/ping/in-ping.txt" || p == "[Lost folders]/pong/ping/in-ping.txt",
        "{p}"
    );
    at(&cat, store, "[Lost folders]/self");
    let d = cat.path(store, node_at(&cat, &run, off(213)));
    assert!(
        d.starts_with("[Lost folders]/L21") && d.ends_with("deep.txt"),
        "{d}"
    );
    // Every node reaches its root.
    for i in 0..cat.node_count() as u32 {
        let (anc, _) = cat.ancestors(i);
        assert!(anc.is_empty() || cat.parent(NodeId(anc[0])).is_none());
    }
}

#[test]
fn e9_system_records_are_omitted() {
    let mut lab = base_lab(32, 24);
    for n in 0..16u32 {
        lab.rec(
            ORIGIN,
            &Rec::new(n)
                .seq(n as u16)
                .si([t(1); 4], 6)
                .name(ROOT, &format!("$sys{n}"), 3, 6),
        );
    }
    lab.rec(
        ORIGIN,
        &Rec::new(17)
            .seq(17)
            .si([t(1); 4], 6)
            .name(ROOT, "seventeen", 1, 0x20)
            .resident_data(b"a"),
    );
    lab.rec(
        ORIGIN,
        &Rec::new(18).seq(18).si([t(1); 4], 6).resident_data(b"b"),
    );
    lab.rec(ORIGIN, &ext(20, fref(17, 17)).resident_data(b"c"));
    let run = lab.run(None);
    let store = run.store();
    let cat = catalog(&run);
    for n in 0..16 {
        assert!(cat.node_of_record(run.id(off(n))).is_none(), "record {n}");
    }
    at(&cat, store, "seventeen");
    assert!(cat.node_of_record(run.id(off(18))).is_none());
    assert!(cat.tree.exts.iter().all(|&(_, r)| r != run.id(off(20))));
}

#[test]
fn e10_aggregates_and_e11_preorder_ids() {
    let mut lab = base_lab(32, 24);
    lab.rec(ORIGIN, &dir(220, 1, ROOT, "agg", t(10)));
    lab.rec(ORIGIN, &dir(221, 1, fref(220, 1), "sub", t(10)));
    lab.rec(
        ORIGIN,
        &file(222, 1, fref(221, 1), "a.bin", t(10), Some((7000, 2))),
    );
    lab.rec(
        ORIGIN,
        &freed(file(
            223,
            1,
            fref(220, 1),
            "gone.bin",
            t(10),
            Some((7100, 1)),
        )),
    );
    let run = lab.run(None);
    let store = run.store();
    let cat = catalog(&run);
    let agg = cat.row(store, at(&cat, store, "agg"), false).unwrap();
    assert_eq!(
        (agg.files, agg.size, agg.deleted_files, agg.deleted_size),
        (1, 8192, 1, 4096)
    );
    assert!(agg.has_children);
    // Preorder: children inside [id + 1, end), parents before children.
    for i in 0..cat.node_count() as u32 {
        let n = cat.rec(i).unwrap();
        if n.parent != NONE {
            let p = cat.rec(n.parent).unwrap();
            assert!(n.parent < i && i < p.end && n.end <= p.end);
        }
    }
    // Deterministic, also from stored state without a source.
    let again = Catalog::build(&CatalogInput::new(
        store,
        &run.reanalyze(&Overrides::new()),
        &run.heads,
        &[],
    ))
    .unwrap();
    for i in 0..cat.node_count() as u32 {
        assert_eq!(
            cat.row(store, NodeId(i), true),
            again.row(store, NodeId(i), true)
        );
    }
}

#[test]
fn ids_of_wire_values_are_checked() {
    assert_eq!(NodeId::from_wire(u64::from(u32::MAX)), None);
    assert_eq!(NodeId::from_wire(1 << 40), None);
    assert_eq!(NodeId::from_wire(7), Some(NodeId(7)));
    let f = FlagSet::of(&[Flag::Deleted]);
    assert!(f.contains(Flag::Deleted));
}

#[test]
fn hostile_duplicate_names_and_unpaired_surrogates() {
    let mut lab = base_lab(32, 24);
    lab.rec(ORIGIN, &dir(100, 1, ROOT, "dups", t(10)));
    lab.rec(ORIGIN, &file(101, 1, fref(100, 1), "same.txt", t(10), None));
    lab.rec(ORIGIN, &file(102, 1, fref(100, 1), "same.txt", t(11), None));
    lab.rec(ORIGIN, &file(103, 1, fref(100, 1), "SAME.TXT", t(12), None));
    // A name with a lone high surrogate (written as raw UTF-16 below).
    let mut r = file(104, 1, fref(100, 1), "abXd.txt", t(10), None).build();
    let pos = r
        .windows(8)
        .position(|w| w == [b'a', 0, b'b', 0, b'X', 0, b'd', 0])
        .unwrap();
    r[pos + 4] = 0x00;
    r[pos + 5] = 0xD8;
    // Re-protect: the record was built with fixups applied; a changed
    // byte inside a block is fine (fixups cover the block tails only).
    lab.put(off(104), &r);
    let run = lab.run(None);
    let store = run.store();
    let cat = catalog(&run);
    let dups = at(&cat, store, "dups");
    let rows = cat
        .list_children(
            store,
            &crate::query::ListArgs {
                node: dups.wire(),
                limit: 10,
                ..Default::default()
            },
        )
        .unwrap()
        .rows;
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        names,
        ["ab\u{FFFD}d.txt", "same.txt", "same.txt", "SAME.TXT"]
    );
    // Ties by name go by id.
    assert!(rows[1].id < rows[2].id);
    // The exact units go to recovery; search finds the name.
    let odd = rows[0].id;
    assert_eq!(
        cat.name_units(store, odd.0),
        vec![0x61, 0x62, 0xD800, 0x64, 0x2E, 0x74, 0x78, 0x74]
    );
    let hits = cat
        .search(
            store,
            &crate::query::SearchArgs {
                filter: crate::query::Filter {
                    query: Some("b\u{FFFD}D".into()),
                    ..Default::default()
                },
                limit: 10,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(hits.total, 1);
    let plan = cat
        .plan(
            store,
            &crate::query::Selection {
                include: vec![dups.wire()],
                ..Default::default()
            },
            None,
        )
        .unwrap();
    assert_eq!(
        plan.files.len(),
        4,
        "collisions are resolved by recovery (H4)"
    );
}

#[test]
fn hostile_random_records_never_break_the_catalog() {
    let mut rng = hds_io::testing::XorShift::new(0x5EED_CA7A);
    for round in 0..4 {
        let mut lab = base_lab(24, 20);
        for i in 0..300u32 {
            let n = 30 + (rng.next_u64() % 400) as u32;
            let seq = 1 + (rng.next_u64() % 3) as u16;
            let parent = fref(rng.next_u64() % 450, 1 + (rng.next_u64() % 3) as u16);
            let mut r = Rec::new(n).seq(seq).si([t(rng.next_u64() % 30); 4], 0x20);
            if rng.next_u64().is_multiple_of(4) {
                r = r.flags(0x2 | (rng.next_u64() % 2) as u16);
            } else if rng.next_u64().is_multiple_of(5) {
                r = r.flags(0);
            }
            if rng.next_u64().is_multiple_of(6) {
                r = r.base(fref(rng.next_u64() % 450, (rng.next_u64() % 3) as u16));
            }
            if !rng.next_u64().is_multiple_of(3) {
                r = r.name(
                    parent,
                    &format!("n{}", rng.next_u64() % 50),
                    (rng.next_u64() % 4) as u8,
                    if rng.next_u64().is_multiple_of(2) {
                        DIR_FLAG
                    } else {
                        0x20
                    },
                );
            }
            match rng.next_u64() % 4 {
                0 => r = r.resident_data(b"z"),
                1 => {
                    let lcn = if rng.next_u64().is_multiple_of(5) {
                        u64::MAX / 8
                    } else {
                        1000 + rng.next_u64() % 4000
                    };
                    let vcn = if rng.next_u64().is_multiple_of(4) {
                        rng.next_u64() % 9
                    } else {
                        0
                    };
                    r = r.data_extent(
                        vcn,
                        (vcn + 1) as i64,
                        (8192, rng.next_u64() % 100_000, rng.next_u64() % 100_000),
                        &runlist(&[(Some(lcn), 2)]),
                        (rng.next_u64() % 3) as u16 * 0x4000,
                    );
                }
                2 => {
                    r = r.attr_list(&[(
                        0x80,
                        "",
                        rng.next_u64() % 4,
                        fref(rng.next_u64() % 450, 1),
                        0,
                    )])
                }
                _ => {}
            }
            lab.rec(ORIGIN + u64::from(i % 3) * 4 * MIB, &r);
        }
        let run = lab.run(None);
        let store = run.store();
        let cat = catalog(&run);
        // Invariants: a forest in preorder, every chain ends at a root,
        // aggregates add up.
        let mut files = 0u64;
        for id in cat.node_ids() {
            let n = cat.rec(id.0).unwrap();
            if n.parent != NONE {
                assert!(n.parent < id.0, "round {round}");
            }
            let (anc, _) = cat.ancestors(id.0);
            if let Some(&r) = anc.first() {
                assert!(cat.parent(NodeId(r)).is_none());
            }
            if n.kind.is_file() && !n.deleted() {
                files += 1;
            }
            let _ = cat.node_info(store, id.wire()).unwrap();
        }
        let total: u64 = cat
            .instances()
            .iter()
            .map(|i| cat.row(store, i.root, true).unwrap().files)
            .sum();
        assert_eq!(total, files, "round {round}");
        let sel = crate::query::Selection {
            include: cat.instances().iter().map(|i| i.root.wire()).collect(),
            include_deleted: true,
            ..Default::default()
        };
        let plan = cat.plan(store, &sel, None).unwrap();
        assert_eq!(
            plan.files.len() as u64,
            cat.node_ids()
                .filter(|&i| cat.kind(i).is_some_and(|k| k.is_file()))
                .count() as u64
        );
    }
}

/// Scale: ten million records through the scanner, C/D/F, the catalog and
/// the queries (release mode, ignored by default):
///
/// ```text
/// cargo test --release -p hds-core --lib -- --ignored catalog_scale --nocapture
/// HDS_SCALE_RECORDS=N        records (default 10 000 000)
/// HDS_SCALE_CHAIN=1          every directory inside the previous one: a
///                            parent chain N records deep
/// ```
#[test]
#[ignore]
fn catalog_scale() {
    use crate::live::{LiveSnapshot, LiveState};
    use crate::scan::{scan, NoObserver, ScanOptions};
    use crate::session::{Session, SessionMeta, SourceIdentity};
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;

    let total: u64 = std::env::var("HDS_SCALE_RECORDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000_000);
    let chain = std::env::var("HDS_SCALE_CHAIN").is_ok_and(|v| v == "1");
    let source: std::sync::Arc<dyn hds_io::BlockSource> =
        std::sync::Arc::new(scale::Synth { total, chain });
    let range = 0..source.size();
    let ranged = hds_io::RangedSource::new(source.clone(), range.clone()).unwrap();
    let options = ScanOptions {
        collect_carve_hits: false,
        ..ScanOptions::default()
    };
    let meta = SessionMeta::new(SourceIdentity::from(&source.info()), range, options).unwrap();
    let mut session = Session::new(meta);
    let cancel = AtomicBool::new(false);
    let t0 = Instant::now();
    let _ = scan(&ranged, &options, &mut session, &mut NoObserver, &cancel);
    let t_scan = t0.elapsed();
    let live = LiveState {
        scan_time: Some(SCAN),
        snapshot: Some(LiveSnapshot::default()),
    };
    let mut heads = Heads::new();
    heads.attach(Some(ranged.clone()));
    let t1 = Instant::now();
    let layout = {
        let input = crate::instances::AnalysisInput::new(session.store(), session.meta(), &live);
        crate::instances::analyze(&input, &mut heads, &Overrides::new(), &cancel).unwrap()
    };
    let t_an = t1.elapsed();
    let rss_before = scale::status_kib("VmRSS:") / 1024;
    let store = session.store();
    let t2 = Instant::now();
    let cat = Catalog::build(&CatalogInput::new(store, &layout, &heads, &[])).unwrap();
    let t_cat = t2.elapsed();
    let rss_after = scale::status_kib("VmRSS:") / 1024;
    let peak = scale::status_kib("VmHWM:") / 1024;
    eprintln!(
        "CATALOG SCALE{}: {} records, {} nodes; scan {:.1}s, analysis {:.1}s, catalog {:.1}s; \
         RSS {rss_before} -> {rss_after} MiB (catalog +{} B/node), peak {peak} MiB",
        if chain { " (chain)" } else { "" },
        store.record_count(),
        cat.node_count(),
        t_scan.as_secs_f64(),
        t_an.as_secs_f64(),
        t_cat.as_secs_f64(),
        (rss_after.saturating_sub(rss_before)) * 1024 * 1024 / cat.node_count().max(1),
    );
    let time = |what: &str, f: &mut dyn FnMut() -> String| {
        let t = Instant::now();
        let r = f();
        eprintln!(
            "  {what}: {:.1} ms ({r})",
            t.elapsed().as_secs_f64() * 1000.0
        );
    };
    let main = cat.instances()[0].root;
    if chain {
        let deepest = cat
            .node_of_record(
                store
                    .records_by_offset()
                    .iter()
                    .copied()
                    .max_by_key(|&r| store.record(r).unwrap().record_number())
                    .unwrap(),
            )
            .unwrap();
        time("node_info of the deepest node", &mut || {
            let i = cat.node_info(store, deepest.wire()).unwrap();
            assert!(i.path.contains('…') && i.ancestors.len() <= crate::query::MAX_PATH_DEPTH + 1);
            format!(
                "{} ancestors listed, path {} bytes",
                i.ancestors.len(),
                i.path.len()
            )
        });
        time("search the deepest name", &mut || {
            let p = cat
                .search(
                    store,
                    &crate::query::SearchArgs {
                        filter: crate::query::Filter {
                            query: Some(format!("d{}", total + 29)),
                            ..Default::default()
                        },
                        limit: 10,
                        ..Default::default()
                    },
                )
                .unwrap();
            format!("{} hits", p.total)
        });
        time("selection_summary of the root", &mut || {
            let s = cat
                .selection_summary(&crate::query::Selection {
                    include: vec![main.wire()],
                    ..Default::default()
                })
                .unwrap();
            assert_eq!(s.dirs, cat.node_count() - 1);
            format!("{} folders", s.dirs)
        });
        return;
    }
    let big = cat
        .node_of_record(store.record_at(scale::ORIGIN + 30 * 1024).unwrap())
        .unwrap();
    let list = |sort, desc, offset| {
        cat.list_children(
            store,
            &crate::query::ListArgs {
                node: big.wire(),
                offset,
                limit: 200,
                sort,
                desc,
                ..Default::default()
            },
        )
        .unwrap()
    };
    time(
        "list_children of 100 000 entries, first page by name",
        &mut || {
            format!(
                "total {}",
                list(crate::query::SortKey::Name, false, 0).total
            )
        },
    );
    time("... next page", &mut || {
        format!(
            "{} rows",
            list(crate::query::SortKey::Name, false, 200).rows.len()
        )
    });
    time("... page 400", &mut || {
        format!(
            "{} rows",
            list(crate::query::SortKey::Name, false, 80_000).rows.len()
        )
    });
    time("... first page by size, descending", &mut || {
        format!(
            "first {}",
            list(crate::query::SortKey::Size, true, 0).rows[0].size
        )
    });
    time("list_children of the root", &mut || {
        let p = cat
            .list_children(
                store,
                &crate::query::ListArgs {
                    node: main.wire(),
                    limit: 200,
                    ..Default::default()
                },
            )
            .unwrap();
        format!("total {}", p.total)
    });
    let search = |q: Option<&str>, ext: Option<&str>, offset: u64| {
        cat.search(
            store,
            &crate::query::SearchArgs {
                filter: crate::query::Filter {
                    query: q.map(str::to_string),
                    exts: ext.map(|e| vec![e.to_string()]),
                    ..Default::default()
                },
                offset,
                limit: 200,
                ..Default::default()
            },
        )
        .unwrap()
    };
    time("search 'file12345' in all nodes", &mut || {
        format!("{} hits", search(Some("file12345"), None, 0).total)
    });
    time("... next page", &mut || {
        format!("{} rows", search(Some("file12345"), None, 200).rows.len())
    });
    time("search '.txt' (nearly all files), first page", &mut || {
        format!("{} hits", search(Some(".txt"), None, 0).total)
    });
    time("... next page", &mut || {
        format!("{} rows", search(Some(".txt"), None, 200).rows.len())
    });
    time("search by extension jpg, no query", &mut || {
        format!("{} hits", search(None, Some("jpg"), 0).total)
    });
    time("selection_summary of the root", &mut || {
        let s = cat
            .selection_summary(&crate::query::Selection {
                include: vec![main.wire()],
                ..Default::default()
            })
            .unwrap();
        format!("{} files, {} folders", s.files, s.dirs)
    });
    time("plan of the 100 000-entry folder", &mut || {
        let p = cat
            .plan(
                store,
                &crate::query::Selection {
                    include: vec![big.wire()],
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        format!("{} files", p.files.len())
    });
    time("catalog summary", &mut || {
        format!(
            "{} instances",
            cat.summary(&session, &layout).instances.len()
        )
    });
}

/// Records generated when they are read (scale test).
mod scale {
    use super::*;
    use hds_io::{BlockSource, SourceInfo};

    pub const ORIGIN: u64 = 64 << 20;

    pub struct Synth {
        pub total: u64,
        pub chain: bool,
    }

    impl Synth {
        fn record(&self, n: u64) -> Vec<u8> {
            let n32 = n as u32;
            if self.chain {
                let parent = if n == 30 { ROOT } else { fref(n - 1, 1) };
                return dir(n32, 1, parent, &format!("d{n}"), t(10)).build();
            }
            if n == 30 {
                return dir(30, 1, ROOT, "big", t(10)).build();
            }
            if n <= 100_030 {
                // The big folder: JPEGs of different sizes.
                return Rec::new(n32)
                    .si([t(10 + n % 700); 4], 0x20)
                    .name(fref(30, 1), &format!("img{n}.jpg"), 1, 0x20)
                    .data_extent(
                        0,
                        0,
                        (4096, 1 + n % 4000, 1 + n % 4000),
                        &runlist(&[(Some(20_000 + n), 1)]),
                        0,
                    )
                    .build();
            }
            // Folders every 50 records from 100 050 on, 50 per folder;
            // each holds the 49 files after it.
            if n.is_multiple_of(50) && n >= 100_050 {
                let parent = if n >= 102_550 {
                    fref(100_050 + ((n - 100_050) / 2500) * 50, 1)
                } else {
                    ROOT
                };
                return dir(n32, 1, parent, &format!("dir{n}"), t(10)).build();
            }
            let parent = if n < 100_050 {
                ROOT
            } else {
                fref((n / 50) * 50, 1)
            };
            file(n32, 1, parent, &format!("file{n}.txt"), t(10), None).build()
        }
    }

    impl BlockSource for Synth {
        fn size(&self) -> u64 {
            ORIGIN + (self.total + 30) * 1024 + (64 << 20)
        }
        fn sector_size(&self) -> u32 {
            512
        }
        fn physical_sector_size(&self) -> u32 {
            512
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
            buf.fill(0);
            let mut pos = offset.div_ceil(1024) * 1024;
            let end = offset + buf.len() as u64;
            while pos + 1024 <= end {
                if pos >= ORIGIN {
                    let n = (pos - ORIGIN) / 1024;
                    if (30..30 + self.total).contains(&n) {
                        let r = self.record(n);
                        let at = (pos - offset) as usize;
                        buf[at..at + 1024].copy_from_slice(&r);
                    }
                }
                pos += 1024;
            }
            Ok(())
        }
        fn info(&self) -> SourceInfo {
            let mut i = hds_io::testing::MemSource::new(vec![0; 512]).info();
            i.size = self.size();
            i
        }
        fn is_alive(&self) -> bool {
            true
        }
    }

    pub fn status_kib(key: &str) -> u64 {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with(key))
                    .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse().ok()))
            })
            .unwrap_or(0)
    }
}
