//! What the instance-separation scenarios promise, checked on the image and
//! the manifest for several seeds, with ALGORITHMS D2 recomputed by the
//! independent reading of `common`.

mod common;

use std::collections::BTreeMap;

use common::{
    assert_reads_back, build, catalog, changepoint_between, contradictions, expected_path, fragment_records,
    instance_of, meets, must, must_not, shown, warning, SEEDS,
};
use hds_testkit::disk::Built;
use hds_testkit::facts::{RecordFact, RecordRole};
use hds_testkit::manifest::{ContradictionKind, Entry, Flag, FragmentFact, InstanceKind, InstanceRole, Manifest, Presence, RecordClass};

fn each(name: &str, check: impl Fn(&Built)) {
    for seed in SEEDS {
        eprintln!("{name}, seed {seed}");
        check(&build(name, seed));
    }
}

/// The fragment of old records at the origin of the current `$MFT`.
fn anchor_fragment(m: &Manifest) -> &FragmentFact {
    let current = m.instances.iter().find(|i| i.role == InstanceRole::Current).expect("a current volume");
    let origin = current.geometry.mft_extents[0].offset as i64;
    m.fragments.iter().find(|f| f.origin == origin).expect("old records at the current MFT's origin")
}

/// The intact record of physical instance `label` in `slot`.
fn record<'a>(m: &'a Manifest, label: &str, slot: u64) -> &'a RecordFact {
    let id = m.instance(label).id;
    m.records
        .iter()
        .find(|r| r.instance == id && r.slot == slot && matches!(r.role, RecordRole::Base | RecordRole::Extension))
        .unwrap_or_else(|| panic!("no record {slot} of {label}"))
}

fn independent(m: &Manifest, f: &FragmentFact) -> BTreeMap<u64, u64> {
    contradictions(&fragment_records(m, f))
}

#[test]
fn generations_contradicting_through_references_are_cut() {
    each("generations_by_references", |built| {
        let m = &built.manifest;
        let f = anchor_fragment(m);
        let listed: BTreeMap<u64, u64> = f.contradictions.iter().map(|c| (c.hi, c.lo)).collect();
        assert_eq!(listed, independent(m, f));
        // no cluster is mapped by both generations: (B) alone decides
        assert!(f.contradictions.iter().all(|c| !c.kinds.contains(&ContradictionKind::SharedCluster)));
        let of = |lo: u64| f.contradictions.iter().filter(move |c| c.lo == lo);
        // Old folder 0 (slot 300) is a file now; Old folder 1 (301) a folder in its second life
        assert_eq!(of(300).count(), 10);
        assert!(of(300).chain(of(301)).all(|c| c.kinds == [ContradictionKind::ParentSlot]));
        assert_eq!(of(301).count(), 10);
        // the extension record of a base whose slot is a file in its second life
        let base = of(304).collect::<Vec<_>>();
        assert_eq!((base.len(), base[0].hi, base[0].kinds.as_slice()), (1, 700, &[ContradictionKind::BaseSlot][..]));
        assert_eq!(record(m, "gen2", 304).sequence, 2);
        assert_eq!(record(m, "gen1", 700).base.number, 304);
        // the files of Old folder 2 and 3 refer to 2020 folders that happen
        // to have the same number and sequence: no contradiction
        for slot in [302u64, 303] {
            let folder = record(m, "gen2", slot);
            assert!(folder.flags & 3 == 3 && folder.sequence == 1);
            assert_eq!(of(slot).count(), 0);
        }
        // one kept cut at 600, a changepoint, supported by 21 stale records
        assert_eq!(f.cuts.len(), 1);
        assert_eq!((f.cuts[0].position, f.cuts[0].support), (600, 21));
        assert_eq!(changepoint_between(&fragment_records(m, f), 304, 600), Some(600));
        let (main, older) = (catalog(m, "gen2"), catalog(m, "gen1"));
        assert_eq!((main.kind, older.kind), (InstanceKind::Main, InstanceKind::Older));
        assert_eq!(f.segment_instances, [Some(main.id), Some(older.id)]);
        assert_eq!(warning(m, "mixed_generations"), Some(false));
        // the 2017 files stay in their own tree, under nameless lost
        // folders: neither the 2020 folders of the same number nor the
        // 2020 index entries naming them may adopt them
        for k in 0..4 {
            for j in 0..10 {
                let e = m.entry("gen1", &format!("Old folder {k}/kept-{k}-{j}.txt"));
                assert_eq!(instance_of(m, e).id, older.id);
                must(e, Flag::OlderGeneration);
                assert_eq!(expected_path(e), format!("[Lost folders]/Folder #{}/kept-{k}-{j}.txt", 300 + k));
                assert!(e.must_recover_exactly());
                assert_reads_back(built, e);
            }
        }
        for e in m.entries_of("gen2").filter(|e| shown(e)) {
            assert_eq!(instance_of(m, e).id, main.id);
            must_not(e, Flag::OlderGeneration);
        }
        assert_eq!(expected_path(m.entry("gen2", "Same number 302")), "Same number 302");
    });
}

#[test]
fn a_cut_with_one_stale_record_is_dropped() {
    each("generation_cut_dropped", |built| {
        let m = &built.manifest;
        let f = anchor_fragment(m);
        let listed: BTreeMap<u64, u64> = f.contradictions.iter().map(|c| (c.hi, c.lo)).collect();
        assert_eq!(listed, independent(m, f));
        let (stale, fresh) = (m.entry("gen1", "stale.bin"), m.entry("gen2", "fresh.bin"));
        assert_eq!((stale.record, fresh.record), (512, 300));
        assert_eq!(listed, [(512, 300)].into_iter().collect());
        assert_eq!(f.contradictions[0].kinds, [ContradictionKind::SharedCluster]);
        // the times do separate the generations: a cut is chosen, but with a
        // single stale record it is not kept
        assert_eq!(changepoint_between(&fragment_records(m, f), 300, 512), Some(512));
        assert!(f.cuts.is_empty());
        assert_eq!(f.contradictions[0].explained_by, None);
        assert_eq!(warning(m, "mixed_generations"), Some(true));
        // one main instance holding both file systems
        let main = m.main_instance().unwrap();
        assert_eq!((main.sources.len(), m.catalog.len()), (2, 1));
        assert_eq!(record(m, "gen1", 512).expect.older_generation, Some(true));
        must(stale, Flag::OlderGeneration);
        // the marked record loses the overlap conflict (both readings of G2)
        must(stale, Flag::Overwritten);
        must(fresh, Flag::Overlap);
        must_not(fresh, Flag::Overwritten);
        must_not(fresh, Flag::OlderGeneration);
        assert!(fresh.must_recover_exactly());
        // the other 2017 survivors stay unflagged in the root of main
        for j in 0..20 {
            let e = m.entry("gen1", &format!("left-behind-{j:02}.txt"));
            assert_eq!(instance_of(m, e).id, main.id);
            must_not(e, Flag::OlderGeneration);
            assert_eq!(expected_path(e), e.name());
            assert_reads_back(built, e);
        }
    });
}

#[test]
fn copies_of_mft_blocks_are_strays() {
    each("copies_of_mft_blocks", |built| {
        let m = &built.manifest;
        let copies: Vec<&RecordFact> = m.records.iter().filter(|r| r.role == RecordRole::StaleCopy).collect();
        assert_eq!(copies.len(), 96);
        let mut slots: Vec<u64> = copies.iter().map(|r| r.slot).collect();
        slots.sort_unstable();
        assert_eq!(slots, (300..364).chain(400..432).collect::<Vec<_>>());
        let main = m.main_instance().unwrap();
        let strays = catalog(m, "strays");
        assert_eq!(strays.kind, InstanceKind::Strays);
        assert!(m.catalog_of_kind(InstanceKind::Other).next().is_none(), "no second tree");
        let old = m.instance("old").id;
        for r in &copies {
            assert_eq!((r.expect.class, r.expect.instance), (Some(RecordClass::Old), Some(strays.id)));
            // a copy set: every copy repeats number and sequence of a record of main
            let original = m
                .records
                .iter()
                .find(|o| o.instance == old && o.role == RecordRole::Base && o.slot == r.slot)
                .expect("the original survives");
            assert_eq!((original.number, original.sequence, original.fnv1a64), (r.number, r.sequence, r.fnv1a64));
            assert_eq!(original.expect.instance, Some(main.id));
        }
        // two fragments of their own, more than 16 records each
        let origins: Vec<i64> = copies.iter().map(|r| r.offset as i64 - (r.number.unwrap() * 1024) as i64).collect();
        let mut distinct = origins.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), 2);
        for origin in distinct {
            assert!(origins.iter().filter(|&&o| o == origin).count() > 16);
        }
        // 32 copies lie in a deleted file: no container for D7
        let container = m.entry("old", "defrag.tmp");
        assert!(container.deleted && container.truth.record_intact && container.truth.content_intact == Some(true));
        let inside = |offset: u64| container.extents.iter().any(|x| x.offset.is_some_and(|o| (o..o + x.bytes).contains(&offset)));
        assert_eq!(copies.iter().filter(|r| inside(r.offset)).count(), 32);
        assert!(m.catalog_of_kind(InstanceKind::Embedded).next().is_none());
    });
}

/// The record a manifest entry's extension record `number` was written to.
fn extension<'a>(m: &'a Manifest, e: &Entry, number: u64) -> &'a RecordFact {
    let &(_, at) = e.extension_records.iter().find(|x| x.0 == number).expect("an extension record");
    m.records.iter().find(|r| r.offset == at).expect("in the image")
}

#[test]
fn extension_records_in_free_slots_of_the_current_mft() {
    each("extensions_in_free_slots", |built| {
        let m = &built.manifest;
        let new = m.instance("new");
        assert_eq!(record(m, "new", 38).expect.class, Some(RecordClass::Live));
        // in use: old, whatever its base
        let alive = m.entry("old", "alive-in-free-slots.bin");
        let ext = extension(m, alive, 132);
        assert!(ext.flags & 1 == 1 && ext.standard_information.is_none());
        assert_eq!(ext.expect.class, Some(RecordClass::Old));
        assert!(shown(alive) && alive.must_recover_exactly());
        must_not(alive, Flag::MissingFragments);
        assert_reads_back(built, alive);
        // free, base resolves to an old (deleted) record: old
        let deleted = m.entry("old", "deleted-in-free-slots.bin");
        let ext = extension(m, deleted, 131);
        assert!(ext.flags & 1 == 0 && ext.standard_information.is_none());
        assert_eq!(ext.expect.class, Some(RecordClass::Old));
        assert_eq!(record(m, "old", 40).expect.class, Some(RecordClass::Old));
        assert!(shown(deleted) && deleted.must_recover_exactly());
        must(deleted, Flag::Deleted);
        must_not(deleted, Flag::MissingFragments);
        assert_reads_back(built, deleted);
        // free, base slot now holds the new volume's record: post_format
        let orphaned = m.entry("old", "orphaned-by-the-new-volume.bin");
        let ext = extension(m, orphaned, 130);
        assert!(ext.flags & 1 == 0 && ext.base.number == 38);
        assert_eq!(ext.expect.class, Some(RecordClass::PostFormat));
        assert_eq!(orphaned.expect.presence, Presence::Gone);
        for x in &orphaned.extents {
            assert!(meets(&m.overwritten_set, x.offset.unwrap(), x.bytes), "its clusters join the overwritten set");
            assert!(!meets(&new.live_allocated, x.offset.unwrap(), x.bytes), "they are free in the new $Bitmap");
        }
        // the conservative side of C5: intact bytes, but overwritten by the rules
        let reuser = m.entry("old", "reused-the-clusters.bin");
        assert_eq!(record(m, "old", 42).expect.class, Some(RecordClass::Old));
        assert!(shown(reuser) && reuser.truth.content_intact == Some(true));
        must(reuser, Flag::Overwritten);
        must(reuser, Flag::Deleted);
        assert_eq!(reuser.extents.iter().map(|x| x.offset).collect::<Vec<_>>(), orphaned.extents.iter().map(|x| x.offset).collect::<Vec<_>>());
    });
}
