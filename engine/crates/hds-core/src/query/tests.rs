//! Tests of the queries and plans (PROTOCOL catalog commands, H2).

#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::single_range_in_vec_init
)]

use std::sync::atomic::AtomicBool;

use hds_recover::{FileSource, Flag, Rating};

use super::*;
use crate::catalog::tests::support::*;
use crate::catalog::{instance_set, CarvedEntry, Catalog, NodeId, NodeKind};
use crate::instances::testlab::*;
use crate::instances::{InstanceKind, KindChoice, Overrides};

/// A volume with a folder of mixed children.
fn browse_lab() -> Run {
    let mut lab = base_lab(32, 24);
    lab.rec(ORIGIN, &dir(100, 1, ROOT, "Mixed", t(10)));
    let p = fref(100, 1);
    lab.rec(ORIGIN, &file(101, 1, p, "b.TXT", t(12), Some((3000, 3))));
    lab.rec(ORIGIN, &file(102, 1, p, "a.jpg", t(11), Some((3010, 1))));
    lab.put(
        3010 * 4096,
        &[
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0,
        ],
    );
    lab.rec(ORIGIN, &file(103, 1, p, "C.txt", t(13), None));
    lab.rec(
        ORIGIN,
        &freed(file(104, 1, p, "deleted.txt", t(14), Some((3020, 2)))),
    );
    lab.rec(ORIGIN, &dir(105, 1, p, "zsub", t(10)));
    lab.rec(ORIGIN, &dir(106, 1, p, "Asub", t(10)));
    lab.rec(
        ORIGIN,
        &file(107, 1, fref(106, 1), "inner.png", t(10), Some((3030, 1))),
    );
    lab.put(3030 * 4096, &png_head());
    // Freeing a record bumps its sequence: (108, 1) resolves to it.
    lab.rec(ORIGIN, &freed(dir(108, 2, p, "deadsub", t(10))));
    lab.rec(
        ORIGIN,
        &freed(file(109, 1, fref(108, 1), "dead.txt", t(10), None)),
    );
    lab.run(None)
}

fn names(rows: &[Row]) -> Vec<&str> {
    rows.iter().map(|r| r.name.as_str()).collect()
}

fn list(cat: &Catalog, run: &Run, node: NodeId, f: impl FnOnce(&mut ListArgs)) -> Page<Row> {
    let mut a = ListArgs {
        node: node.wire(),
        limit: 200,
        ..ListArgs::default()
    };
    f(&mut a);
    cat.list_children(run.store(), &a).unwrap()
}

#[test]
fn list_children_sorts_filters_and_pages() {
    let run = browse_lab();
    let store = run.store();
    let cat = catalog(&run);
    let mixed = at(&cat, store, "Mixed");
    let p = list(&cat, &run, mixed, |_| {});
    // Folders first, case-insensitive names; deleted files and the deleted
    // folder without a non-deleted file are hidden.
    assert_eq!(names(&p.rows), ["Asub", "zsub", "a.jpg", "b.TXT", "C.txt"]);
    assert_eq!(p.total, 5);
    let p = list(&cat, &run, mixed, |a| a.show_deleted = true);
    assert_eq!(
        names(&p.rows),
        [
            "Asub",
            "deadsub",
            "zsub",
            "a.jpg",
            "b.TXT",
            "C.txt",
            "deleted.txt"
        ]
    );
    let p = list(&cat, &run, mixed, |a| {
        a.sort = SortKey::Size;
        a.desc = true;
    });
    // Folders still first (by size: Asub holds 4096, zsub nothing).
    assert_eq!(names(&p.rows), ["Asub", "zsub", "b.TXT", "a.jpg", "C.txt"]);
    let p = list(&cat, &run, mixed, |a| a.sort = SortKey::Modified);
    assert_eq!(names(&p.rows)[2..], ["a.jpg", "b.TXT", "C.txt"]);
    let p = list(&cat, &run, mixed, |a| a.sort = SortKey::Ext);
    assert_eq!(names(&p.rows)[2..], ["a.jpg", "b.TXT", "C.txt"]);
    // Filters apply to files only.
    let p = list(&cat, &run, mixed, |a| a.exts = Some(vec!["txt".into()]));
    assert_eq!(names(&p.rows), ["Asub", "zsub", "b.TXT", "C.txt"]);
    let p = list(&cat, &run, mixed, |a| a.dirs_only = true);
    assert_eq!(names(&p.rows), ["Asub", "zsub"]);
    assert!(!p.rows[0].has_children && !p.rows[1].has_children);
    let p = list(&cat, &run, mixed, |a| {
        a.ratings = Some(vec![Rating::Uncertain])
    });
    assert_eq!(names(&p.rows), ["Asub", "zsub"]);
    // Paging.
    let p = list(&cat, &run, mixed, |a| {
        a.offset = 1;
        a.limit = 2;
    });
    assert_eq!((p.total, names(&p.rows)), (5, vec!["zsub", "a.jpg"]));
    let p = list(&cat, &run, mixed, |a| a.offset = 99);
    assert!(p.rows.is_empty() && p.total == 5);
    // Rows.
    let row = &list(&cat, &run, mixed, |_| {}).rows[3];
    assert_eq!(
        (row.kind, row.ext.as_str(), row.size, row.deleted),
        (NodeKind::File, "txt", 3 * 4096, false)
    );
    assert_eq!(row.modified, filetime_to_unix_ms(t(12)));
    assert_eq!(row.rating, Some(Rating::Good));
    let folder = cat.row(store, mixed, false).unwrap();
    assert_eq!(
        (folder.files, folder.deleted_files, folder.rating),
        (4, 2, None)
    );
    assert!(folder.has_children);
    // has_children follows show_deleted: deadsub is only visible with it.
    let root = cat.instances()[0].root;
    let mixed_row = list(&cat, &run, root, |_| {})
        .rows
        .into_iter()
        .find(|r| r.name == "Mixed")
        .unwrap();
    assert!(mixed_row.has_children);
    // Errors.
    assert_eq!(
        cat.list_children(
            store,
            &ListArgs {
                node: 1 << 33,
                ..ListArgs::default()
            }
        ),
        Err(QueryError::NotFound(1 << 33))
    );
    let file = at(&cat, store, "Mixed/a.jpg");
    assert!(matches!(
        cat.list_children(
            store,
            &ListArgs {
                node: file.wire(),
                ..ListArgs::default()
            }
        ),
        Err(QueryError::BadRequest(_))
    ));
}

#[test]
fn node_info_describes_files_and_folders() {
    let run = browse_lab();
    let store = run.store();
    let cat = catalog(&run);
    let id = at(&cat, store, "Mixed/Asub/inner.png");
    let info = cat.node_info(store, id.wire()).unwrap();
    assert_eq!(info.path, "Mixed/Asub/inner.png");
    let anc: Vec<&str> = info.ancestors.iter().map(|(_, n)| n.as_str()).collect();
    assert_eq!(anc, ["", "Mixed", "Asub"]);
    assert_eq!(info.instance, 0);
    let rec = info.record.unwrap();
    assert_eq!(
        (rec.number, rec.seq, rec.disk_offset, rec.extension_records),
        (Some(107), 1, off(107), 0)
    );
    let data = info.data.unwrap();
    assert_eq!(
        (
            data.resident,
            data.real_size,
            data.runs,
            data.first_lcn,
            data.first_offset
        ),
        (false, 4096, 1, Some(3030), Some(3030 * 4096))
    );
    assert!(info
        .flag_details
        .iter()
        .any(|(f, text)| *f == Flag::SignatureOk && !text.is_empty()));
    let folder = cat
        .node_info(store, at(&cat, store, "Mixed").wire())
        .unwrap();
    assert!(folder.data.is_none() && folder.record.is_some());
    let resident = cat
        .node_info(store, at(&cat, store, "Mixed/C.txt").wire())
        .unwrap();
    let d = resident.data.unwrap();
    assert!(d.resident && d.real_size == 5 && d.first_offset.unwrap() > off(103));
    let root = cat
        .node_info(store, cat.instances()[0].root.wire())
        .unwrap();
    assert!(root.record.is_none() && root.ancestors.is_empty() && root.path.is_empty());
    assert_eq!(
        cat.node_info(store, 1 << 40),
        Err(QueryError::NotFound(1 << 40))
    );
}

#[test]
fn search_finds_by_substring_filter_and_subtree() {
    let run = browse_lab();
    let store = run.store();
    let cat = catalog(&run);
    let search = |f: &dyn Fn(&mut SearchArgs)| {
        let mut a = SearchArgs {
            limit: 100,
            ..SearchArgs::default()
        };
        f(&mut a);
        cat.search(store, &a).unwrap()
    };
    let p = search(&|a| a.filter.query = Some("TXT".into()));
    assert_eq!(
        names(&p.rows.iter().map(|r| r.row.clone()).collect::<Vec<_>>()),
        ["b.TXT", "C.txt"]
    );
    let r = &p.rows[0];
    assert_eq!(r.path, "Mixed/b.TXT");
    assert_eq!(
        r.ancestor_ids,
        vec![cat.instances()[0].root, at(&cat, store, "Mixed")]
    );
    let p = search(&|a| {
        a.filter.query = Some(".txt".into());
        a.show_deleted = true;
    });
    assert_eq!(p.total, 4, "deleted.txt and dead.txt too");
    let p = search(&|a| {
        a.under = Some(at(&cat, store, "Mixed/Asub").wire());
    });
    assert_eq!(p.total, 1);
    let p = search(&|a| {
        a.filter.exts = Some(vec!["png".into()]);
        a.filter.min_size = Some(4096);
    });
    assert_eq!(p.total, 25, "24 PNGs in the root and inner.png");
    let p = search(&|a| a.filter.max_size = Some(5));
    assert_eq!(
        names(&p.rows.iter().map(|r| r.row.clone()).collect::<Vec<_>>()),
        ["C.txt"]
    );
    let p = search(&|a| {
        a.filter.query = Some("p".into());
        a.sort = SortKey::Name;
        a.desc = true;
        a.offset = 2;
        a.limit = 3;
    });
    assert_eq!(
        names(&p.rows.iter().map(|r| r.row.clone()).collect::<Vec<_>>()),
        ["p7.png", "p6.png", "p5.png"]
    );
    // Folders are never results.
    let p = search(&|a| a.filter.query = Some("sub".into()));
    assert_eq!(p.total, 0);
}

#[test]
fn selections_and_plans() {
    let run = browse_lab();
    let store = run.store();
    let cat = catalog(&run);
    let mixed = at(&cat, store, "Mixed");
    let asub = at(&cat, store, "Mixed/Asub");
    let inner = at(&cat, store, "Mixed/Asub/inner.png");
    let sel = Selection {
        include: vec![mixed.wire()],
        exclude: vec![asub.wire()],
        ..Selection::default()
    };
    let s = cat.selection_summary(&sel).unwrap();
    // a.jpg, b.TXT, C.txt; folders Mixed, zsub (deadsub is deleted).
    assert_eq!((s.files, s.dirs, s.good, s.bytes), (3, 2, 3, 4 * 4096 + 5));
    let s2 = cat
        .selection_summary(&Selection {
            include: vec![mixed.wire(), inner.wire()],
            exclude: vec![asub.wire()],
            include_deleted: true,
            ..Selection::default()
        })
        .unwrap();
    // + deleted.txt, dead.txt, inner.png; folders Mixed, zsub, deadsub and
    // Asub (excluded, but the folder of the included inner.png).
    assert_eq!((s2.files, s2.dirs), (6, 4));
    assert_eq!(s2.uncertain, 2, "deleted files are uncertain");
    let plan = cat
        .plan(
            store,
            &Selection {
                include: vec![inner.wire()],
                ..Selection::default()
            },
            None,
        )
        .unwrap();
    // The full path from the instance root: Mixed/Asub/inner.png.
    assert_eq!(plan.files.len(), 1);
    let f = &plan.files[0];
    assert_eq!(f.original_path, "Mixed/Asub/inner.png");
    assert_eq!(f.folder, Some(u64::from(asub.0)));
    assert_eq!(String::from_utf16(&f.name).unwrap(), "inner.png");
    assert_eq!(f.ext, "png");
    assert_eq!(
        f.source,
        FileSource::Extents {
            runs: vec![hds_recover::Run::Data {
                offset: 3030 * 4096,
                len: 4096
            }],
            real_size: 4096,
            initialized_size: 4096
        }
    );
    assert_eq!(f.record_offset, Some(off(107)));
    assert_eq!(f.times.modified, t(10));
    assert!(!f.context.geometry_not_validated);
    let folders: Vec<(u64, Option<u64>, String)> = plan
        .folders
        .iter()
        .map(|d| (d.id, d.parent, String::from_utf16(&d.name).unwrap()))
        .collect();
    assert_eq!(
        folders,
        vec![
            (u64::from(mixed.0), None, "Mixed".to_string()),
            (
                u64::from(asub.0),
                Some(u64::from(mixed.0)),
                "Asub".to_string()
            )
        ]
    );
    // An empty selected folder is created; a selection without files is
    // still a plan (the engine refuses an empty one).
    let plan = cat
        .plan(
            store,
            &Selection {
                include: vec![at(&cat, store, "Mixed/zsub").wire()],
                ..Selection::default()
            },
            None,
        )
        .unwrap();
    assert_eq!((plan.files.len(), plan.folders.len()), (0, 2));
    // Errors.
    assert!(matches!(
        cat.selection_summary(&Selection {
            include: vec![mixed.wire()],
            exclude: vec![mixed.wire()],
            ..Selection::default()
        }),
        Err(QueryError::BadRequest(_))
    ));
    assert_eq!(
        cat.selection_summary(&Selection {
            include: vec![1 << 35],
            ..Selection::default()
        }),
        Err(QueryError::NotFound(1 << 35))
    );
    // skip_damaged leaves damaged bytes out of `bytes`, not out of `files`.
    cat.observe(
        at(&cat, store, "Mixed/a.jpg"),
        hds_recover::FlagSet::of(&[Flag::BadSectors]),
    );
    let s3 = cat
        .selection_summary(&Selection {
            skip_damaged: true,
            ..sel.clone()
        })
        .unwrap();
    assert_eq!((s3.files, s3.damaged, s3.bytes), (3, 1, 3 * 4096 + 5));
    // Filters.
    let s4 = cat
        .selection_summary(&Selection {
            filter: Some(Filter {
                query: Some("b.t".into()),
                ..Filter::default()
            }),
            ..sel.clone()
        })
        .unwrap();
    assert_eq!(s4.files, 1);
    // H1: a refreshed overwritten set marks before the plan is made.
    let mut fresh = hds_io::RangeSet::new();
    fresh.insert(3000 * 4096..3001 * 4096);
    let plan = cat.plan(store, &sel, Some(&fresh)).unwrap();
    let b = plan
        .files
        .iter()
        .find(|f| f.original_path == "Mixed/b.TXT")
        .unwrap();
    assert!(b.flags.contains(Flag::Overwritten));
    // check_destination: files in a byte range of the source.
    assert_eq!(
        cat.files_in_extents(&[3000 * 4096..3011 * 4096]),
        (2, 4 * 4096)
    );
    assert_eq!(cat.files_in_extents(&[0..4096]), (0, 0));
}

#[test]
fn plans_of_other_instances_and_carved_files() {
    let mut lab = base_lab(32, 30);
    let older = 8 * MIB;
    for k in 0..20u32 {
        lab.rec(
            older,
            &file(
                300 + k,
                1,
                ROOT,
                &format!("o{k}.txt"),
                t(1),
                Some((5000 + u64::from(k), 1)),
            ),
        );
    }
    let run = lab.run(None);
    let store = run.store();
    let cat = catalog(&run);
    let cat = cat
        .with_carved(
            store,
            &[CarvedEntry {
                offset: 6000 * 4096,
                len: 100,
                ext: "png".into(),
                end_verified: true,
            }],
        )
        .unwrap();
    let o = cat
        .instances()
        .iter()
        .find(|i| i.kind == InstanceKind::Older)
        .unwrap();
    let o3 = at(&cat, store, "[Older file system]/o3.txt");
    let main_root = cat.instances()[0].root;
    let plan = cat
        .plan(
            store,
            &Selection {
                include: vec![o3.wire(), main_root.wire()],
                ..Selection::default()
            },
            None,
        )
        .unwrap();
    let label = plan
        .folders
        .iter()
        .find(|d| d.id == u64::from(o.root.0))
        .unwrap();
    assert_eq!(
        (label.parent, String::from_utf16(&label.name).unwrap()),
        (None, "[Older file system]".to_string())
    );
    let of = plan
        .files
        .iter()
        .find(|f| f.node == u64::from(o3.0))
        .unwrap();
    assert_eq!(of.folder, Some(u64::from(o.root.0)));
    assert!(of.flags.contains(Flag::OlderGeneration));
    let cf = plan
        .files
        .iter()
        .find(|f| f.original_path == "[Carved files]/png/f0000048000.png")
        .unwrap();
    assert_eq!(
        cf.source,
        FileSource::Extents {
            runs: vec![hds_recover::Run::Data {
                offset: 6000 * 4096,
                len: 100
            }],
            real_size: 100,
            initialized_size: 100
        }
    );
    let ext_folder = plan
        .folders
        .iter()
        .find(|d| Some(d.id) == cf.folder)
        .unwrap();
    assert_eq!(String::from_utf16(&ext_folder.name).unwrap(), "png");
    let carved_root = plan
        .folders
        .iter()
        .find(|d| Some(d.id) == ext_folder.parent)
        .unwrap();
    assert_eq!(
        (
            carved_root.parent,
            String::from_utf16(&carved_root.name).unwrap()
        ),
        (None, "[Carved files]".to_string())
    );
    // Search under the main root reaches the carved files.
    let p = cat
        .search(
            store,
            &SearchArgs {
                filter: Filter {
                    query: Some("f00000".into()),
                    ..Filter::default()
                },
                limit: 10,
                ..SearchArgs::default()
            },
        )
        .unwrap();
    assert_eq!(p.total, 1);
    assert_eq!(p.rows[0].row.kind, NodeKind::Carved);
}

#[test]
fn instance_set_stores_the_choice_and_rebuilds() {
    let mut lab = base_lab(32, 30);
    let older = 8 * MIB;
    for k in 0..20u32 {
        lab.rec(
            older,
            &file(
                300 + k,
                1,
                ROOT,
                &format!("o{k}.txt"),
                t(1),
                Some((5000 + u64::from(k), 1)),
            ),
        );
    }
    let mut run = lab.run(None);
    let cat = catalog(&run);
    let older_id = cat
        .instances()
        .iter()
        .find(|i| i.kind == InstanceKind::Older)
        .unwrap()
        .id;
    let mut overrides = Overrides::new();
    let cancel = AtomicBool::new(false);
    let c2 = instance_set(
        &mut run.session,
        &mut run.layout,
        &mut overrides,
        &mut run.heads,
        &cat,
        older_id,
        Some(KindChoice::Main),
        &cancel,
    )
    .unwrap();
    assert_eq!(c2.instances()[0].kind, InstanceKind::Main);
    assert!(c2.instances()[0].manual);
    assert_eq!(c2.instances()[0].records, 20);
    assert!(c2
        .instances()
        .iter()
        .any(|i| i.kind == InstanceKind::Other && i.records > 20));
    let store = run.session.store();
    at(&c2, store, "o0.txt");
    assert!(!c2
        .flags(at(&c2, store, "o0.txt"))
        .contains(Flag::OlderGeneration));
    // Stored in the session: a reload gives the same catalog.
    let loaded = Overrides::load(&run.session).unwrap();
    let layout = run.reanalyze(&loaded);
    let c3 = Catalog::build(&crate::catalog::CatalogInput::new(
        store,
        &layout,
        &run.heads,
        &[],
    ))
    .unwrap();
    assert_eq!(c3.instances()[0].records, 20);
    // Auto withdraws; strays cannot be changed.
    let c4 = instance_set(
        &mut run.session,
        &mut run.layout,
        &mut overrides,
        &mut run.heads,
        &c2,
        0,
        None,
        &cancel,
    )
    .unwrap();
    assert_eq!(c4.instances()[0].records, 30);
    assert!(c4.instances().iter().any(|i| i.kind == InstanceKind::Older));
    assert!(matches!(
        instance_set(
            &mut run.session,
            &mut run.layout,
            &mut overrides,
            &mut run.heads,
            &c4,
            99,
            None,
            &cancel
        ),
        Err(crate::catalog::InstanceSetError::NotFound)
    ));
}

#[test]
fn summary_reports_instances_and_ranges() {
    let run = browse_lab();
    let cat = catalog(&run);
    let s = cat.summary(&run.session, &run.layout);
    assert_eq!(s.instances.len(), 1);
    let i = &s.instances[0];
    assert_eq!(
        (i.kind, i.label.as_str(), i.records),
        (InstanceKind::Main, "", 34)
    );
    // 24 PNGs + a.jpg, b.TXT, C.txt, inner.png; Mixed, zsub, Asub.
    assert_eq!((i.files, i.deleted_files, i.dirs), (28, 2, 3));
    assert!(i.geometry.validated);
    assert_eq!(s.range, 0..32 * MIB);
    assert_eq!(s.protected[0], hds_io::Protected::session(0..32 * MIB));
    assert!(s
        .protected
        .iter()
        .any(|p| p.reason == hds_io::ProtectedReason::LostVolume));
    assert_eq!(s.nodes, cat.node_count());
    assert!(s.scan_complete);
    assert_eq!(filetime_to_unix_ms(116_444_736_000_000_000), Some(0));
    assert_eq!(filetime_to_unix_ms(0), None);
    assert_eq!(filetime_to_unix_ms(u64::MAX), None);
    assert_eq!(filetime_to_unix_ms(1), Some(-11_644_473_600_000));
}

#[test]
fn paging_through_partial_sorts_equals_one_full_sort() {
    let mut lab = base_lab(32, 60);
    for k in 0..40u32 {
        lab.rec(
            ORIGIN,
            &file(
                200 + k,
                1,
                ROOT,
                &format!("Q{}.txt", (k * 7) % 40),
                t(10 + u64::from(k % 5)),
                Some((5000 + 4 * u64::from(k), 1 + u64::from(k % 3))),
            ),
        );
    }
    let run = lab.run(None);
    let store = run.store();
    let cat = catalog(&run);
    for sort in [
        SortKey::Name,
        SortKey::Size,
        SortKey::Modified,
        SortKey::Ext,
        SortKey::Rating,
    ] {
        for desc in [false, true] {
            let args = |offset, limit| SearchArgs {
                sort,
                desc,
                offset,
                limit,
                ..SearchArgs::default()
            };
            let all: Vec<NodeId> = cat
                .search(store, &args(0, 1000))
                .unwrap()
                .rows
                .iter()
                .map(|r| r.row.id)
                .collect();
            assert_eq!(all.len(), 100);
            // A fresh catalog (empty cache), paged in steps of 3.
            let fresh = catalog(&run);
            fresh.set_sort_tuning(16, 4);
            let mut paged = Vec::new();
            let mut off = 0;
            loop {
                let p = fresh.search(store, &args(off, 3)).unwrap();
                if p.rows.is_empty() {
                    break;
                }
                paged.extend(p.rows.iter().map(|r| r.row.id));
                off += 3;
            }
            assert_eq!(paged, all, "{sort:?} desc={desc}");
            // The order is the documented one.
            let vals: Vec<Row> = all
                .iter()
                .map(|&i| cat.row(store, i, false).unwrap())
                .collect();
            for w in vals.windows(2) {
                let (a, b) = (&w[0], &w[1]);
                let ord = match sort {
                    SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                    SortKey::Size => a.size.cmp(&b.size),
                    SortKey::Modified => a.modified.cmp(&b.modified),
                    SortKey::Ext => a.ext.cmp(&b.ext),
                    SortKey::Rating => a.rating.cmp(&b.rating),
                };
                let ord = if desc { ord.reverse() } else { ord };
                assert_ne!(
                    ord,
                    std::cmp::Ordering::Greater,
                    "{sort:?} {} {}",
                    a.name,
                    b.name
                );
            }
        }
    }
}

/// A main volume with a directory chain `depth` levels deep below the
/// root (`d0/d1/...`), a file in each of the folders listed in `at`
/// (`f<k>.txt` in `d<k>`) and `flat` more files in the deepest folder.
fn chain_lab(depth: u32, at: &[u32], flat: u32) -> Run {
    let records = depth + at.len() as u32 + flat;
    let mib = ((u64::from(records + 200) * 1024 + ORIGIN) / MIB + 2) as usize;
    let mut lab = Lab::new(mib);
    let mut parent = ROOT;
    let mut parents = Vec::new();
    for k in 0..depth {
        let n = 100 + k;
        lab.rec(ORIGIN, &dir(n, 1, parent, &format!("d{k}"), t(10)));
        parent = fref(u64::from(n), 1);
        parents.push(parent);
    }
    let mut n = 100 + depth;
    for &k in at {
        let p = parents[k as usize];
        lab.rec(ORIGIN, &file(n, 1, p, &format!("f{k}.txt"), t(10), None));
        n += 1;
    }
    for j in 0..flat {
        lab.rec(
            ORIGIN,
            &file(n, 1, parent, &format!("leaf{j}.txt"), t(10), None),
        );
        n += 1;
    }
    lab.run(None)
}

/// The display path by a full walk to the root (the reference for the
/// bounded walk): the root, `…` when more than [`MAX_PATH_DEPTH`] levels
/// lie between, the nearest levels and the node.
fn walked_path(cat: &Catalog, store: &crate::store::ScanStore, id: NodeId) -> String {
    let mut chain = Vec::new();
    let mut x = cat.parent(id);
    while let Some(p) = x {
        chain.push(p);
        x = cat.parent(p);
    }
    chain.reverse(); // root first
    let mut parts = vec![cat.name(store, chain[0])];
    let near = &chain[1..];
    if near.len() > MAX_PATH_DEPTH {
        parts.push("…".to_string());
    }
    let keep = near.len().min(MAX_PATH_DEPTH);
    for &a in &near[near.len() - keep..] {
        parts.push(cat.name(store, a));
    }
    parts.push(cat.name(store, id));
    parts.retain(|p| !p.is_empty());
    parts.join("/")
}

#[test]
fn deep_chains_paths_ancestors_and_plan_paths_around_the_depth_cap() {
    let cap = MAX_PATH_DEPTH as u32;
    let at: Vec<u32> = (cap - 3..cap + 4).chain([0, 1, 7]).collect();
    let run = chain_lab(cap + 6, &at, 5);
    let store = run.store();
    let cat = catalog(&run);
    let files: Vec<NodeId> = cat
        .node_ids()
        .filter(|&i| cat.kind(i) == Some(NodeKind::File))
        .collect();
    assert_eq!(files.len(), at.len() + 5);
    let plan = cat
        .plan(
            store,
            &Selection {
                include: vec![cat.instances()[0].root.wire()],
                ..Default::default()
            },
            None,
        )
        .unwrap();
    assert_eq!(plan.files.len(), files.len());
    let mut elided = 0;
    for f in &plan.files {
        let id = NodeId(f.node as u32);
        let want = walked_path(&cat, store, id);
        assert_eq!(f.original_path, want, "plan path of {id:?}");
        assert_eq!(cat.path(store, id), want);
        let info = cat.node_info(store, id.wire()).unwrap();
        assert_eq!(info.path, want);
        // Root first, then at most MAX_PATH_DEPTH nearest levels.
        assert_eq!(info.ancestors[0].0, cat.instances()[0].root);
        assert!(info.ancestors.len() <= MAX_PATH_DEPTH + 1);
        assert_eq!(info.ancestors.last().unwrap().0, cat.parent(id).unwrap());
        if want.contains('…') {
            elided += 1;
            assert_eq!(info.ancestors.len(), MAX_PATH_DEPTH + 1);
        }
    }
    // f<k> has k + 1 folders between it and the root: f1021..f1023 show
    // every level, f1024..f1027 and the leaves do not.
    assert_eq!(elided, 4 + 5);
    // Search rows carry the same path and ancestors.
    let hits = cat
        .search(
            store,
            &SearchArgs {
                filter: Filter {
                    query: Some(".txt".into()),
                    ..Default::default()
                },
                limit: 1000,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(hits.rows.len(), at.len() + 5);
    for r in &hits.rows {
        let info = cat.node_info(store, r.row.id.wire()).unwrap();
        assert_eq!(r.path, info.path);
        let ids: Vec<NodeId> = info.ancestors.iter().map(|a| a.0).collect();
        assert_eq!(r.ancestor_ids, ids);
    }
}

/// Measures a search page and a plan below a deep chain:
/// `HDS_CHAIN_DEPTH=N cargo test --release -p hds-core --lib deep_chain_cost
/// -- --ignored --nocapture`.
#[test]
#[ignore]
fn deep_chain_cost() {
    let depth: u32 = std::env::var("HDS_CHAIN_DEPTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let run = chain_lab(depth, &[], 1000);
    let store = run.store();
    let cat = catalog(&run);
    let t0 = std::time::Instant::now();
    let page = cat
        .search(
            store,
            &SearchArgs {
                filter: Filter {
                    query: Some("leaf".into()),
                    ..Default::default()
                },
                limit: 1000,
                ..Default::default()
            },
        )
        .unwrap();
    let search = t0.elapsed();
    let t1 = std::time::Instant::now();
    let plan = cat
        .plan(
            store,
            &Selection {
                include: vec![cat.instances()[0].root.wire()],
                ..Default::default()
            },
            None,
        )
        .unwrap();
    let planned = t1.elapsed();
    eprintln!(
        "depth {depth}: search page of {} rows {:.3}s; plan of {} files + {} folders {:.3}s",
        page.rows.len(),
        search.as_secs_f64(),
        plan.files.len(),
        plan.folders.len(),
        planned.as_secs_f64()
    );
}

/// Random directories, files, deleted records, `$INDEX_ROOT` entries and
/// loose INDX blocks naming random children and parents (lost-folder
/// chains and cycles): no panic, a forest in preorder, aggregates that
/// add up, the same catalog twice and after a resume, plan paths equal to
/// the walked ones. (From the independent review.)
#[test]
fn hostile_index_entries_and_lost_folder_chains() {
    let mut rng = hds_io::testing::XorShift::new(0xBADC_0FFE);
    for round in 0..6 {
        let mut lab = base_lab(24, 20);
        let times = |d: u64| [t(d); 4];
        for i in 0..200u32 {
            let n = 40 + (rng.next_u64() % 200) as u32;
            let parent = fref(rng.next_u64() % 300, 1 + (rng.next_u64() % 2) as u16);
            let mut r = if rng.next_u64().is_multiple_of(2) {
                dir(
                    n,
                    1 + (rng.next_u64() % 2) as u16,
                    parent,
                    &format!("d{}", rng.next_u64() % 9),
                    t(rng.next_u64() % 20),
                )
            } else {
                file(
                    n,
                    1,
                    parent,
                    &format!("f{}.txt", rng.next_u64() % 9),
                    t(rng.next_u64() % 20),
                    None,
                )
            };
            if rng.next_u64().is_multiple_of(5) {
                r = freed(r);
            }
            if rng.next_u64().is_multiple_of(3) {
                let mut es = Vec::new();
                for _ in 0..(rng.next_u64() % 5) {
                    es.push(index_entry(
                        fref(rng.next_u64() % 300, 1 + (rng.next_u64() % 2) as u16),
                        fref(rng.next_u64() % 300, 1 + (rng.next_u64() % 2) as u16),
                        &format!("L{}", rng.next_u64() % 7),
                        (rng.next_u64() % 4) as u8,
                        DIR_FLAG,
                        times(rng.next_u64() % 40),
                    ));
                }
                r = r.index_root(&es);
            }
            lab.rec(ORIGIN + u64::from(i % 2) * 4 * MIB, &r);
        }
        // Loose INDX blocks naming random children with random parents.
        for b in 0..40u64 {
            let mut es = Vec::new();
            for _ in 0..(1 + rng.next_u64() % 6) {
                es.push(index_entry(
                    fref(rng.next_u64() % 300, 1 + (rng.next_u64() % 2) as u16),
                    fref(rng.next_u64() % 300, 1 + (rng.next_u64() % 2) as u16),
                    &format!("B{}", rng.next_u64() % 7),
                    (rng.next_u64() % 4) as u8,
                    DIR_FLAG,
                    times(rng.next_u64() % 40),
                ));
            }
            lab.put(
                16 * MIB + b * 4096,
                &indx_block(b, rng.next_u64() % 9, 4096, &es),
            );
        }
        let run = lab.run(None);
        let store = run.store();
        let cat = catalog(&run);
        let cat2 = catalog(&run);
        let mut files = 0u64;
        for id in cat.node_ids() {
            let n = cat.rec(id.0).unwrap();
            if n.parent != crate::catalog::NONE {
                assert!(n.parent < id.0, "round {round}");
            }
            assert_eq!(cat.path(store, id), cat2.path(store, id), "determinism");
            if n.kind.is_file() && !n.deleted() {
                files += 1;
            }
            let _ = cat.node_info(store, id.wire()).unwrap();
            if n.kind.is_folder() {
                let _ = cat
                    .list_children(
                        store,
                        &crate::query::ListArgs {
                            node: id.wire(),
                            limit: 1000,
                            show_deleted: true,
                            ..Default::default()
                        },
                    )
                    .unwrap();
            }
        }
        let total: u64 = cat
            .instances()
            .iter()
            .map(|i| cat.row(store, i.root, true).unwrap().files)
            .sum();
        assert_eq!(total, files, "round {round}");
        // Resume: analysis from stored state gives the same catalog.
        let layout2 = run.reanalyze(&Overrides::new());
        let mut heads2 = crate::geometry::Heads::new();
        heads2.merge_payload(&run.heads.to_payload()).unwrap();
        let cat3 = crate::catalog::Catalog::build(&crate::catalog::CatalogInput::new(
            store,
            &layout2,
            &heads2,
            &[],
        ))
        .unwrap();
        assert_eq!(cat.node_count(), cat3.node_count());
        for id in cat.node_ids() {
            assert_eq!(cat.path(store, id), cat3.path(store, id), "resume");
            assert_eq!(cat.flags(id), cat3.flags(id), "resume flags");
        }
        // Plan paths (built from the parent's path) equal the walked ones.
        let plan = cat
            .plan(
                store,
                &Selection {
                    include: cat.instances().iter().map(|i| i.root.wire()).collect(),
                    include_deleted: true,
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        assert_eq!(
            plan.files.len() as u64,
            cat.node_ids()
                .filter(|&i| cat.kind(i).is_some_and(|k| k.is_file()))
                .count() as u64
        );
        for f in &plan.files {
            assert_eq!(
                f.original_path,
                cat.path(store, NodeId(f.node as u32)),
                "round {round}"
            );
        }
        let lost = cat
            .node_ids()
            .filter(|&i| cat.kind(i) == Some(crate::catalog::NodeKind::LostDir))
            .count();
        let named = cat.node_ids().filter(|&i| cat.lost_folder_named(i)).count();
        eprintln!(
            "round {round}: {} nodes, {lost} lost folders, {named} named",
            cat.node_count()
        );
    }
}
