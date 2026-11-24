//! Test support: images with hand-made records, current volumes and index
//! blocks, scanned by the real scanner and analysed (tests only).

#![allow(
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    dead_code,
    clippy::cast_possible_wrap
)]

use std::ops::Range;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use hds_io::testing::MemSource;
use hds_io::{BlockSource, RangedSource};

use crate::geometry::Heads;
use crate::instances::{analyze, AnalysisInput, Layout, Overrides};
use crate::live::{take_snapshot, LiveState};
use crate::scan::{scan, NoObserver, ScanOptions, ScanStatus};
use crate::session::{Session, SessionMeta, SourceIdentity};
pub(crate) use crate::test_build::*;

/// One day in FILETIME ticks.
pub(crate) const DAY: u64 = 864_000_000_000;
/// 2020-01-01 00:00 UTC as FILETIME.
pub(crate) const T0: u64 = 132_223_104_000_000_000;
/// The scan time of the lab: 2025-01-01.
pub(crate) const SCAN: u64 = T0 + 1827 * DAY;
/// Directory flag of `$FILE_NAME`.
pub(crate) const DIR_FLAG: u32 = 0x1000_0000;

/// `T0 + days`.
pub(crate) fn t(days: u64) -> u64 {
    T0 + days * DAY
}

/// An in-use directory `n` (sequence `seq`) in the directory `parent`
/// (a reference: [`fref`]).
pub(crate) fn dir(n: u32, seq: u16, parent: u64, name: &str, changed: u64) -> Rec {
    Rec::new(n)
        .seq(seq)
        .flags(3)
        .si([changed, changed, changed, changed], 0x10)
        .name(parent, name, 1, DIR_FLAG)
}

/// The root directory reference.
pub(crate) const ROOT: u64 = 5 | (5 << 48);

/// An in-use file `n` in the directory `parent` (a reference) with one data run `(lcn, clusters)` of
/// 4 KiB clusters (`None`: resident).
pub(crate) fn file(
    n: u32,
    seq: u16,
    parent: u64,
    name: &str,
    changed: u64,
    run: Option<(u64, u64)>,
) -> Rec {
    file_cs(n, seq, parent, name, changed, run, 4096)
}

/// As [`file`] with cluster size `cs`.
pub(crate) fn file_cs(
    n: u32,
    seq: u16,
    parent: u64,
    name: &str,
    changed: u64,
    run: Option<(u64, u64)>,
    cs: u64,
) -> Rec {
    let r = Rec::new(n)
        .seq(seq)
        .si([changed, changed, changed, changed], 0x20)
        .name(parent, name, 1, 0x20);
    match run {
        Some((lcn, len)) => r.data_extent(
            0,
            len as i64 - 1,
            (len * cs, len * cs, len * cs),
            &runlist(&[(Some(lcn), len)]),
            0,
        ),
        None => r.resident_data(b"hello"),
    }
}

/// A disk image under construction.
pub(crate) struct Lab {
    pub img: Vec<u8>,
}

impl Lab {
    pub fn new(mib: usize) -> Lab {
        Lab {
            img: vec![0; mib << 20],
        }
    }

    pub fn put(&mut self, off: u64, bytes: &[u8]) {
        let o = off as usize;
        self.img[o..o + bytes.len()].copy_from_slice(bytes);
    }

    /// Writes record `r` as number `n` on the grid starting at `origin`.
    pub fn rec(&mut self, origin: u64, r: &Rec) {
        let n = u64::from(r.number.expect("numbered"));
        let b = r.build();
        self.put(origin + n * b.len() as u64, &b);
    }

    /// Writes `r` at an absolute offset.
    pub fn rec_at(&mut self, off: u64, r: &Rec) {
        let b = r.build();
        self.put(off, &b);
    }

    /// A current volume at `off` (4 KiB clusters, 1 KiB records): boot
    /// sector, `$MFT` at `mft_lcn` with `slots` slots, records 0
    /// (`$MFT`), 1 (`$MFTMirr` at `mirr_lcn`), 3 (`$Volume`, created at
    /// `format_time`), 6 (`$Bitmap` at `bitmap_lcn`). `in_use` slots are set
    /// in the `$MFT` bitmap besides 0..16; `allocated` cluster ranges in
    /// `$Bitmap` besides the system ones.
    #[allow(clippy::too_many_arguments)]
    pub fn current_volume(
        &mut self,
        off: u64,
        total_clusters: u64,
        mft_lcn: u64,
        slots: u64,
        format_time: u64,
        in_use: &[u64],
        allocated: &[Range<u64>],
    ) -> CurrentLayout {
        let cs = 4096u64;
        let mft_clusters = slots * 1024 / cs;
        let mirr_lcn = mft_lcn + mft_clusters + 2;
        let bitmap_lcn = mirr_lcn + 1;
        let bitmap_bytes = total_clusters.div_ceil(8);
        let bitmap_clusters = bitmap_bytes.div_ceil(cs);
        let boot = boot_sector(
            512,
            8,
            total_clusters * 8 - 1,
            mft_lcn,
            mirr_lcn,
            -10,
            1,
            (off / 512) as u32,
        );
        self.put(off, &boot);
        let origin = off + mft_lcn * cs;
        let mut mbits = vec![0u8; (slots as usize).div_ceil(8)];
        for s in (0..16).chain(in_use.iter().copied()) {
            mbits[(s / 8) as usize] |= 1 << (s % 8);
        }
        let ft = [format_time; 4];
        let r0 = Rec::new(0)
            .si(ft, 6)
            .name(fref(5, 5), "$MFT", 3, 6)
            .data_extent(
                0,
                mft_clusters as i64 - 1,
                (mft_clusters * cs, slots * 1024, slots * 1024),
                &runlist(&[(Some(mft_lcn), mft_clusters)]),
                0,
            )
            .raw_attr(resident_attr(0xB0, "", 9, 0, &mbits));
        self.rec(origin, &r0);
        let r1 = Rec::new(1)
            .si(ft, 6)
            .name(fref(5, 5), "$MFTMirr", 3, 6)
            .data_extent(0, 0, (cs, cs, cs), &runlist(&[(Some(mirr_lcn), 1)]), 0);
        self.rec(origin, &r1);
        let r3 = Rec::new(3).si(ft, 6).name(fref(5, 5), "$Volume", 3, 6);
        self.rec(origin, &r3);
        let r6 = Rec::new(6)
            .si(ft, 6)
            .name(fref(5, 5), "$Bitmap", 3, 6)
            .data_extent(
                0,
                bitmap_clusters as i64 - 1,
                (bitmap_clusters * cs, bitmap_bytes, bitmap_bytes),
                &runlist(&[(Some(bitmap_lcn), bitmap_clusters)]),
                0,
            );
        self.rec(origin, &r6);
        // Cluster bitmap: boot, $MFT, $MFTMirr, $Bitmap and `allocated`.
        let mut bits = vec![0u8; bitmap_bytes as usize];
        let mut set = |a: u64, b: u64| {
            for c in a..b.min(total_clusters) {
                bits[(c / 8) as usize] |= 1 << (c % 8);
            }
        };
        set(0, 1);
        set(mft_lcn, mft_lcn + mft_clusters);
        set(mirr_lcn, mirr_lcn + 1);
        set(bitmap_lcn, bitmap_lcn + bitmap_clusters);
        for r in allocated {
            set(r.start, r.end);
        }
        self.put(off + bitmap_lcn * cs, &bits);
        CurrentLayout {
            origin,
            mirror: off + mirr_lcn * cs,
        }
    }

    pub fn source(&self) -> Arc<dyn BlockSource> {
        Arc::new(MemSource::new(self.img.clone()))
    }

    /// Scans `range` (the whole image by default) and analyses it.
    pub fn run(&self, range: Option<Range<u64>>) -> Run {
        self.run_with(range, &Overrides::new())
    }

    pub fn run_with(&self, range: Option<Range<u64>>, overrides: &Overrides) -> Run {
        let source = self.source();
        let range = range.unwrap_or(0..source.size());
        let ranged = RangedSource::new(source.clone(), range.clone()).unwrap();
        let options = ScanOptions::default();
        let meta = SessionMeta::new(SourceIdentity::from(&source.info()), range, options).unwrap();
        let mut session = Session::new(meta);
        let cancel = AtomicBool::new(false);
        let summary = scan(&ranged, &options, &mut session, &mut NoObserver, &cancel);
        assert_eq!(summary.status, ScanStatus::Completed);
        let snapshot = take_snapshot(&ranged, SCAN, &cancel).unwrap();
        let live = LiveState {
            scan_time: Some(SCAN),
            snapshot: Some(snapshot),
        };
        let mut heads = Heads::new();
        heads.attach(Some(ranged.clone()));
        let layout = {
            let input = AnalysisInput::new(session.store(), session.meta(), &live);
            analyze(&input, &mut heads, overrides, &cancel).unwrap()
        };
        Run {
            session,
            live,
            heads,
            layout,
            ranged,
        }
    }
}

/// Where [`Lab::current_volume`] put things.
pub(crate) struct CurrentLayout {
    /// Disk offset of the `$MFT`.
    pub origin: u64,
    /// Disk offset of the `$MFTMirr`.
    pub mirror: u64,
}

/// A scanned and analysed lab image.
pub(crate) struct Run {
    pub session: Session,
    pub live: LiveState,
    pub heads: Heads,
    pub layout: Layout,
    pub ranged: RangedSource,
}

impl Run {
    pub fn store(&self) -> &crate::store::ScanStore {
        self.session.store()
    }

    /// The record id at a disk offset.
    pub fn id(&self, off: u64) -> crate::store::RecordId {
        self.store()
            .record_at(off)
            .unwrap_or_else(|| panic!("no record at {off}"))
    }

    /// The instance of the record at `off`.
    pub fn inst(&self, off: u64) -> Option<crate::instances::InstanceId> {
        self.layout.instance_of(self.id(off))
    }

    /// The kind of the instance of the record at `off`.
    pub fn kind(&self, off: u64) -> Option<crate::instances::InstanceKind> {
        self.inst(off)
            .and_then(|i| self.layout.instance(i))
            .map(|i| i.kind)
    }

    /// Re-analyses from the stored state (no source), as after a load.
    pub fn reanalyze(&self, overrides: &Overrides) -> Layout {
        let mut heads = Heads::new();
        let payload = self.heads.to_payload();
        heads.merge_payload(&payload).unwrap();
        let input = AnalysisInput::new(self.session.store(), self.session.meta(), &self.live);
        analyze(&input, &mut heads, overrides, &AtomicBool::new(false)).unwrap()
    }
}
