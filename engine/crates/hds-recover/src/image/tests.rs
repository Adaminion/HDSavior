use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use hds_io::testing::{FaultySource, MemSource};
use hds_io::{open_source, ProtectedReason, SourceKind, Verdict};

use super::*;
use crate::testutil::{names, pattern, TempDir};

const MIB: u64 = 1 << 20;

fn disk(len: u64) -> Arc<FaultySource> {
    Arc::new(FaultySource::new(Arc::new(MemSource::new(pattern(
        len as usize,
    )))))
}

fn ranged(source: Arc<FaultySource>, range: Range<u64>) -> RangedSource {
    RangedSource::new(source, range).unwrap()
}

fn options(resume: bool) -> ImageOptions {
    ImageOptions {
        resume,
        ..ImageOptions::default()
    }
}

#[derive(Default)]
struct Recorder {
    events: Vec<(Instant, ImageProgress)>,
}

impl ImageObserver for Recorder {
    fn progress(&mut self, progress: &ImageProgress) {
        self.events.push((Instant::now(), progress.clone()));
    }
}

fn read_progress(path: &Path) -> Progress {
    Progress::parse(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The image of `range` of a patterned disk with `bad` zero-filled.
fn expected_image(len: u64, range: Range<u64>, bad: &[Range<u64>]) -> Vec<u8> {
    let mut data = pattern(len as usize)[range.start as usize..range.end as usize].to_vec();
    for b in bad {
        data[(b.start - range.start) as usize..(b.end - range.start) as usize].fill(0);
    }
    data
}

#[test]
fn the_range_is_copied_with_unreadable_sectors_zero_filled_and_listed() {
    let tmp = TempDir::new("image");
    let source = disk(32 * MIB);
    source.bad_sectors(5 * MIB..5 * MIB + 1024);
    source.bad_sectors(20 * MIB + 512..20 * MIB + 1024);
    let range = MIB..30 * MIB;
    let dest = tmp.join("images").join("disk.img");
    let mut rec = Recorder::default();
    let summary = image(
        ranged(source, range.clone()),
        &dest,
        &options(false),
        &mut rec,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(
        summary.status,
        JobStatus::Completed,
        "{:?}",
        summary.message
    );
    assert_eq!(summary.bytes_written, 29 * MIB);
    assert_eq!(summary.bad_bytes, 1536);
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        expected_image(
            32 * MIB,
            range.clone(),
            &[5 * MIB..5 * MIB + 1024, 20 * MIB + 512..20 * MIB + 1024]
        )
    );
    let bad_path = tmp.join("images").join("disk.img.bad.txt");
    assert_eq!(summary.bad_map_path, Some(display_path(&bad_path)));
    assert_eq!(
        std::fs::read_to_string(&bad_path).unwrap(),
        format!(
            "# HDSavior: unreadable sectors, recovered as zeros in disk.img\r\n# image_offset length source_offset (bytes)\r\n{} 1024 {}\r\n{} 512 {}\r\n",
            4 * MIB,
            5 * MIB,
            19 * MIB + 512,
            20 * MIB + 512
        )
    );
    let progress = read_progress(&tmp.join("images").join("disk.img.hdsimg"));
    assert_eq!(progress.confirmed, 29 * MIB);
    assert_eq!(progress.range, range);
    assert_eq!(progress.identity.size, 32 * MIB);
    assert!(!progress.samples.is_empty());
    assert_eq!(
        names(&tmp.join("images")),
        ["disk.img", "disk.img.bad.txt", "disk.img.hdsimg"]
    );
    let json = serde_json::to_value(&summary).unwrap();
    let mut keys: Vec<&str> = json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "bad_bytes",
            "bad_map_path",
            "bytes_written",
            "error",
            "path",
            "status"
        ]
    );
}

#[test]
fn without_unreadable_sectors_there_is_no_list() {
    let tmp = TempDir::new("image-clean");
    let dest = tmp.join("clean.img");
    let summary = image(
        ranged(disk(4 * MIB), 0..4 * MIB),
        &dest,
        &options(false),
        &mut (),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(summary.status, JobStatus::Completed);
    assert_eq!(summary.bad_map_path, None);
    assert_eq!(summary.bad_bytes, 0);
    assert_eq!(names(tmp.path()), ["clean.img", "clean.img.hdsimg"]);
    assert_eq!(std::fs::read(&dest).unwrap(), pattern(4 * MIB as usize));
}

#[test]
fn a_stopped_image_is_resumed_from_its_confirmed_length() {
    let tmp = TempDir::new("image-resume");
    let source = disk(48 * MIB);
    source.bad_sectors(3 * MIB..3 * MIB + 512); // before the stop
    source.bad_sectors(40 * MIB..40 * MIB + 2048); // after it
    let range = 0..48 * MIB;
    let dest = tmp.join("disk.img");
    // Stop after three chunks: count the reads the first two take, retries included.
    let probe = disk(48 * MIB);
    probe.bad_sectors(3 * MIB..3 * MIB + 512);
    let mut buf = AlignedBuf::new(CHUNK);
    for i in 0..2u64 {
        let _ = read_resilient(&*probe, i * CHUNK as u64, &mut buf, &AtomicBool::new(false));
    }
    let cancel = Arc::new(AtomicBool::new(false));
    source.cancel_after_reads(probe.read_count() + 1, cancel.clone());
    let summary = image(
        ranged(source.clone(), range.clone()),
        &dest,
        &options(false),
        &mut (),
        &cancel,
    )
    .unwrap();
    assert_eq!(summary.status, JobStatus::Cancelled);
    let first = summary.bytes_written;
    assert_eq!(first, 3 * CHUNK as u64);
    assert_eq!(read_progress(&tmp.join("disk.img.hdsimg")).confirmed, first);
    assert_eq!(std::fs::metadata(&dest).unwrap().len(), first);
    // Bytes behind the confirmed part are never trusted: as if they had been
    // written but not flushed before a crash.
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&dest)
            .unwrap();
        file.write_all(&vec![0xEE; 3 * MIB as usize]).unwrap();
    }
    std::fs::write(
        tmp.join("disk.img.bad.txt"),
        format!(
            "# header\r\n{} 512 {}\r\n{} 4096 {}\r\n",
            3 * MIB,
            3 * MIB,
            first + MIB,
            first + MIB
        ),
    )
    .unwrap();
    cancel.store(false, Ordering::SeqCst);
    // Without resume the image is refused.
    match ImageJob::prepare(
        ranged(source.clone(), range.clone()),
        &dest,
        &options(false),
    ) {
        Err(ImageStartError::Exists(path)) => assert!(path.ends_with("disk.img")),
        other => panic!("{other:?}"),
    }
    let job =
        ImageJob::prepare(ranged(source.clone(), range.clone()), &dest, &options(true)).unwrap();
    assert_eq!(job.bytes_confirmed(), first);
    let mut rec = Recorder::default();
    let summary = job.run(&mut rec, &AtomicBool::new(false));
    assert_eq!(
        summary.status,
        JobStatus::Completed,
        "{:?}",
        summary.message
    );
    assert_eq!(summary.bytes_written, 48 * MIB);
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        expected_image(
            48 * MIB,
            range,
            &[3 * MIB..3 * MIB + 512, 40 * MIB..40 * MIB + 2048]
        )
    );
    // The list keeps what lies in the confirmed part and gains the new range.
    let list = std::fs::read_to_string(tmp.join("disk.img.bad.txt")).unwrap();
    let entries: Vec<&str> = list.lines().filter(|l| !l.starts_with('#')).collect();
    assert_eq!(
        entries,
        [
            format!("{} 512 {}", 3 * MIB, 3 * MIB),
            format!("{} 2048 {}", 40 * MIB, 40 * MIB)
        ]
    );
    assert_eq!(summary.bad_bytes, 512 + 2048);
    assert_eq!(
        read_progress(&tmp.join("disk.img.hdsimg")).confirmed,
        48 * MIB
    );
    assert!(rec
        .events
        .first()
        .is_none_or(|(_, p)| p.bytes_done >= first));
}

#[test]
fn the_record_is_rewritten_after_every_checkpoint() {
    let tmp = TempDir::new("image-checkpoints");
    let source = disk(64 * MIB);
    source.set_delay(Duration::from_millis(100));
    let dest = tmp.join("slow.img");
    let mut job = ImageJob::prepare(ranged(source, 0..64 * MIB), &dest, &options(false)).unwrap();
    job.checkpoint_bytes = 2 * CHUNK as u64;
    struct Watcher {
        side: PathBuf,
        seen: Vec<u64>,
    }
    impl ImageObserver for Watcher {
        fn progress(&mut self, progress: &ImageProgress) {
            let confirmed = read_progress(&self.side).confirmed;
            assert!(confirmed <= progress.bytes_done);
            assert_eq!(
                confirmed % (2 * CHUNK as u64),
                0,
                "only flushed checkpoints"
            );
            self.seen.push(confirmed);
        }
    }
    let mut watcher = Watcher {
        side: tmp.join("slow.img.hdsimg"),
        seen: Vec::new(),
    };
    let summary = job.run(&mut watcher, &AtomicBool::new(false));
    assert_eq!(summary.status, JobStatus::Completed);
    assert!(watcher.seen.windows(2).all(|w| w[0] <= w[1]));
    assert!(
        watcher.seen.iter().any(|&c| c > 0 && c < 64 * MIB),
        "{:?}",
        watcher.seen
    );
}

#[test]
fn a_lost_source_does_not_extend_the_image() {
    let tmp = TempDir::new("image-lost");
    let source = disk(40 * MIB);
    source.lose_after_reads(3);
    let dest = tmp.join("lost.img");
    let summary = image(
        ranged(source, 0..40 * MIB),
        &dest,
        &options(false),
        &mut (),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(summary.status, JobStatus::Failed);
    assert_eq!(summary.error, Some(JobError::SourceLost));
    assert_eq!(summary.bytes_written, 3 * CHUNK as u64);
    assert_eq!(std::fs::metadata(&dest).unwrap().len(), 3 * CHUNK as u64);
    assert_eq!(
        read_progress(&tmp.join("lost.img.hdsimg")).confirmed,
        3 * CHUNK as u64
    );
    assert_eq!(summary.bad_map_path, None, "nothing is recorded as bad");
    assert_eq!(std::fs::read(&dest).unwrap(), pattern(3 * CHUNK));
}

#[test]
fn resume_refuses_another_source_or_range() {
    let tmp = TempDir::new("image-mismatch");
    let source = disk(24 * MIB);
    let cancel = Arc::new(AtomicBool::new(false));
    source.cancel_after_reads(2, cancel.clone());
    let dest = tmp.join("a.img");
    let summary = image(
        ranged(source, 0..24 * MIB),
        &dest,
        &options(false),
        &mut (),
        &cancel,
    )
    .unwrap();
    assert_eq!(summary.status, JobStatus::Cancelled);
    let before = std::fs::read(&dest).unwrap();

    // Same size, other content.
    let mut other = pattern(24 * MIB as usize);
    other[100] ^= 0xFF;
    let other = Arc::new(MemSource::new(other));
    match ImageJob::prepare(
        RangedSource::new(other, 0..24 * MIB).unwrap(),
        &dest,
        &options(true),
    ) {
        Err(ImageStartError::SourceMismatch(checks)) => assert_eq!(checks, ["content"]),
        other => panic!("{other:?}"),
    }
    // Another size.
    let bigger = disk(32 * MIB);
    match ImageJob::prepare(ranged(bigger.clone(), 0..24 * MIB), &dest, &options(true)) {
        Err(ImageStartError::SourceMismatch(checks)) => assert_eq!(checks, ["size"]),
        other => panic!("{other:?}"),
    }
    // Another range of the same source.
    let same = disk(24 * MIB);
    match ImageJob::prepare(ranged(same.clone(), MIB..24 * MIB), &dest, &options(true)) {
        Err(ImageStartError::SourceMismatch(checks)) => assert_eq!(checks, ["range"]),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        before,
        "the image is untouched"
    );
    // The right source resumes.
    let summary = image(
        ranged(same, 0..24 * MIB),
        &dest,
        &options(true),
        &mut (),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(summary.status, JobStatus::Completed);
    assert_eq!(std::fs::read(&dest).unwrap(), pattern(24 * MIB as usize));
}

#[test]
fn files_in_the_way_are_refused_and_records_are_required_for_resume() {
    let tmp = TempDir::new("image-exists");
    let dest = tmp.join("x.img");
    let src = || ranged(disk(MIB), 0..MIB);
    // A stale record without its image.
    std::fs::write(tmp.join("x.img.hdsimg"), b"stale").unwrap();
    for resume in [false, true] {
        match ImageJob::prepare(src(), &dest, &options(resume)) {
            Err(ImageStartError::Exists(path)) => assert!(path.ends_with("x.img.hdsimg")),
            other => panic!("{other:?}"),
        }
    }
    std::fs::remove_file(tmp.join("x.img.hdsimg")).unwrap();
    // An image without a record cannot be continued, and is not overwritten.
    std::fs::write(&dest, b"someone's file").unwrap();
    match ImageJob::prepare(src(), &dest, &options(true)) {
        Err(ImageStartError::CannotResume(why)) => {
            assert!(why.contains("no progress record"), "{why}")
        }
        other => panic!("{other:?}"),
    }
    match ImageJob::prepare(src(), &dest, &options(false)) {
        Err(ImageStartError::Exists(_)) => {}
        other => panic!("{other:?}"),
    }
    // A record that is not one.
    std::fs::write(
        tmp.join("x.img.hdsimg"),
        b"HDSavior image progress 1\nkind disk\n",
    )
    .unwrap();
    match ImageJob::prepare(src(), &dest, &options(true)) {
        Err(ImageStartError::CannotResume(_)) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(std::fs::read(&dest).unwrap(), b"someone's file");
}

#[test]
fn an_image_shorter_than_its_record_cannot_be_continued() {
    let tmp = TempDir::new("image-short");
    let source = disk(24 * MIB);
    let cancel = Arc::new(AtomicBool::new(false));
    source.cancel_after_reads(2, cancel.clone());
    let dest = tmp.join("s.img");
    image(
        ranged(source.clone(), 0..24 * MIB),
        &dest,
        &options(false),
        &mut (),
        &cancel,
    )
    .unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&dest)
        .unwrap()
        .set_len(MIB)
        .unwrap();
    match ImageJob::prepare(ranged(disk(24 * MIB), 0..24 * MIB), &dest, &options(true)) {
        Err(ImageStartError::CannotResume(why)) => assert!(why.contains("fewer"), "{why}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_guard_protects_the_imaged_range_and_the_source_image() {
    let tmp = TempDir::new("image-guard");
    let image_path = tmp.join("source.img");
    std::fs::write(&image_path, pattern(2 * MIB as usize)).unwrap();
    let source = open_source(SourceKind::Image, &image_path, None).unwrap();
    let session = RangedSource::new(source, 0..2 * MIB).unwrap();
    match ImageJob::prepare(session.clone(), &image_path, &options(false)) {
        Err(ImageStartError::DestinationRefused(check)) => {
            assert_eq!(check.verdict, Verdict::OverlapsSource);
            assert_eq!(check.reason, Some(ProtectedReason::SourceImage));
        }
        other => panic!("{other:?}"),
    }
    // Elsewhere it is fine, and the source image is left as it was.
    let summary = image(
        session,
        &tmp.join("copy.img"),
        &options(false),
        &mut (),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(summary.status, JobStatus::Completed);
    assert_eq!(
        std::fs::read(tmp.join("copy.img")).unwrap(),
        pattern(2 * MIB as usize)
    );
    assert_eq!(
        std::fs::read(&image_path).unwrap(),
        pattern(2 * MIB as usize)
    );
    // A disk source cannot be placed on this system: unknown_disk needs consent.
    if !cfg!(windows) {
        let on_disk = Arc::new(
            MemSource::new(pattern(MIB as usize))
                .at_location(hds_io::SourceLocation::Disk { number: 2 }),
        );
        let src = RangedSource::new(on_disk, 0..MIB).unwrap();
        match ImageJob::prepare(src.clone(), &tmp.join("d.img"), &options(false)) {
            Err(ImageStartError::DestinationRefused(check)) => {
                assert_eq!(check.verdict, Verdict::UnknownDisk)
            }
            other => panic!("{other:?}"),
        }
        let consent = ImageOptions {
            allow_unknown_disk: true,
            ..options(false)
        };
        assert!(ImageJob::prepare(src, &tmp.join("d.img"), &consent).is_ok());
    }
}

#[test]
fn an_image_too_large_for_the_file_system_is_refused_before_writing() {
    let tmp = TempDir::new("image-too-large");
    let dest = tmp.join("big.img");
    match ImageJob::prepare_with(
        ranged(disk(8 * MIB), 0..8 * MIB),
        &dest,
        &options(false),
        Some(4 * MIB),
    ) {
        Err(ImageStartError::TooLarge {
            size,
            max_file_bytes,
        }) => {
            assert_eq!((size, max_file_bytes), (8 * MIB, 4 * MIB));
        }
        other => panic!("{other:?}"),
    }
    assert!(names(tmp.path()).is_empty(), "nothing was written");
}

#[test]
fn too_little_free_space_is_refused_before_writing() {
    let Some(base) = std::env::var_os("HDS_RECOVER_NOSPACE_DIR") else {
        eprintln!("SKIPPED: set HDS_RECOVER_NOSPACE_DIR to a directory on a small file system (about 1 MiB)");
        return;
    };
    let tmp = TempDir::below(Path::new(&base), "image-nospace");
    if OutDir::open_root(tmp.path()).unwrap().free_space().is_err() {
        eprintln!("SKIPPED: the file system at HDS_RECOVER_NOSPACE_DIR does not report its free space here");
        return;
    }
    match ImageJob::prepare(
        ranged(disk(64 * MIB), 0..64 * MIB),
        &tmp.join("x.img"),
        &options(false),
    ) {
        Err(ImageStartError::NoSpace {
            free_bytes,
            need_bytes,
        }) => {
            assert_eq!(need_bytes, 64 * MIB);
            assert!(free_bytes < need_bytes);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn progress_events_are_spaced_and_have_the_wire_form() {
    let tmp = TempDir::new("image-progress");
    let source = disk(48 * MIB);
    source.set_delay(Duration::from_millis(100));
    let mut rec = Recorder::default();
    image(
        ranged(source, 0..48 * MIB),
        &tmp.join("p.img"),
        &options(false),
        &mut rec,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(rec.events.len() >= 2);
    for pair in rec.events.windows(2) {
        assert!(pair[1].0.duration_since(pair[0].0) >= Duration::from_millis(249));
    }
    let json = serde_json::to_value(&rec.events[0].1).unwrap();
    let mut keys: Vec<&str> = json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "bad_bytes",
            "bytes_done",
            "bytes_total",
            "elapsed_seconds",
            "eta_seconds",
            "speed_bps"
        ]
    );
}

#[test]
fn the_record_round_trips_and_garbage_is_refused() {
    let progress = Progress {
        identity: Identity {
            kind: SourceKind::Disk,
            path: r"\\.\PhysicalDrive1".into(),
            size: 4_000_787_030_016,
            sector_size: 512,
            model: Some("WDC WD40EZRZ".into()),
            serial: None,
        },
        range: 1_048_576..4_000_786_153_472,
        confirmed: 1 << 30,
        samples: vec![(1_048_576, 0xDEAD_BEEF), (1 << 30, 7)],
    };
    let text = progress.to_text();
    assert!(text.starts_with("HDSavior image progress 1\nkind disk\n"));
    assert_eq!(Progress::parse(&text).unwrap(), progress);
    assert_eq!(one_line("a\nb\tc"), "a?b?c");
    for bad in [
        "",
        "HDSavior image progress 2\n",
        "HDSavior image progress 1\nkind tape\n",
        "HDSavior image progress 1\nkind disk\nsize x\n",
        "HDSavior image progress 1\nkind disk\nsize 1\nsector_size 512\nrange 0\nconfirmed 0\n",
    ] {
        assert!(Progress::parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn the_bad_sector_list_is_clipped_to_the_confirmed_part() {
    let text = "# comment\r\n0 512 4096\r\n1000 600 5096\r\n2048 512 6144\r\n\r\n";
    assert_eq!(
        parse_bad_map(text, 1024).unwrap(),
        [(0, 512, 4096), (1000, 24, 5096)]
    );
    assert!(parse_bad_map("1 2\r\n", 100).is_err());
    assert!(parse_bad_map("1 2 3 4\r\n", 100).is_err());
}

#[test]
fn a_destination_that_fills_up_ends_the_job_with_only_whole_chunks() {
    let Some(base) = std::env::var_os("HDS_RECOVER_NOSPACE_DIR") else {
        eprintln!("SKIPPED: set HDS_RECOVER_NOSPACE_DIR to a directory on a small file system (about 1 MiB)");
        return;
    };
    let tmp = TempDir::below(Path::new(&base), "image-fills");
    let Ok(free) = OutDir::open_root(tmp.path()).unwrap().free_space() else {
        eprintln!("SKIPPED: the file system at HDS_RECOVER_NOSPACE_DIR does not report its free space here");
        return;
    };
    assert!(
        free < 64 * MIB,
        "HDS_RECOVER_NOSPACE_DIR is not small: {free} bytes free"
    );
    // An image that fits at the start, written in 64 KiB chunks.
    let total = (free / 2) / 65_536 * 65_536;
    let dest = tmp.join("full.img");
    let mut job = ImageJob::prepare(ranged(disk(total), 0..total), &dest, &options(false)).unwrap();
    job.chunk_bytes = 65_536;
    // Someone else fills the destination after the first chunks.
    struct Filler {
        path: PathBuf,
        done: bool,
    }
    impl ImageObserver for Filler {
        fn progress(&mut self, _: &ImageProgress) {
            if !self.done {
                self.done = true;
                let mut file = std::fs::File::create(&self.path).unwrap();
                let block = vec![0x55u8; 4096];
                while file.write_all(&block).is_ok() {}
            }
        }
    }
    let mut filler = Filler {
        path: tmp.join("filler"),
        done: false,
    };
    let summary = job.run(&mut filler, &AtomicBool::new(false));
    assert_eq!(summary.status, JobStatus::Failed, "{summary:?}");
    assert_eq!(summary.error, Some(JobError::NoSpace));
    let written = summary.bytes_written;
    assert!(written > 0 && written < total, "{written}");
    assert_eq!(written % 65_536, 0, "only whole chunks count");
    let image = std::fs::read(&dest).unwrap();
    assert_eq!(
        image.len() as u64,
        written,
        "the partial chunk was cut off again"
    );
    assert_eq!(image, pattern(written as usize));
    // The record never claims more than the image holds.
    let recorded = read_progress(&tmp.join("full.img.hdsimg")).confirmed;
    assert!(recorded <= written, "{recorded} > {written}");
}
