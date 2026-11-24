#![allow(clippy::single_range_in_vec_init)] // lists of protected ranges with one entry

use std::path::Path;
use std::sync::Arc;

use super::*;
use crate::image::ImageFile;
use crate::test_util::{make_dir_link, make_file_link, TempDir};
use crate::testing::MemSource;

const MIB: u64 = 1 << 20;

fn extent(disk: u32, start: u64, len: u64) -> Extent {
    Extent { disk, start, len }
}

#[test]
fn classify_extents_reproduces_the_reference_cases() {
    let scan = vec![MIB..128 * MIB];
    // An empty answer is not "no overlap".
    assert_eq!(classify_extents(&[], 1, &scan), Verdict::UnknownDisk);
    assert_eq!(
        classify_extents(&[extent(0, 0, 500 * MIB)], 1, &scan),
        Verdict::Ok
    );
    assert_eq!(
        classify_extents(&[extent(1, 128 * MIB, 128 * MIB)], 1, &scan),
        Verdict::SameDisk
    );
    assert_eq!(
        classify_extents(&[extent(1, 127 * MIB, 2 * MIB)], 1, &scan),
        Verdict::OverlapsSource
    );
    // A spanned volume: the second extent decides.
    assert_eq!(
        classify_extents(&[extent(0, 0, MIB), extent(1, 64 * MIB, MIB)], 1, &scan),
        Verdict::OverlapsSource
    );
    assert_eq!(
        classify_extents(&[extent(0, 0, MIB), extent(2, 64 * MIB, MIB)], 1, &scan),
        Verdict::Ok
    );
    // The old volume was larger than the scanned partition: the neighbour is protected too.
    let scan_and_old_volume = [MIB..128 * MIB, MIB..256 * MIB];
    assert_eq!(
        classify_extents(&[extent(1, 128 * MIB, 128 * MIB)], 1, &scan_and_old_volume),
        Verdict::OverlapsSource
    );
}

#[test]
fn classify_extents_handles_edges_exactly() {
    let scan = vec![1000..2000u64];
    // Touching is not overlapping.
    assert_eq!(
        classify_extents(&[extent(1, 0, 1000)], 1, &scan),
        Verdict::SameDisk
    );
    assert_eq!(
        classify_extents(&[extent(1, 2000, 500)], 1, &scan),
        Verdict::SameDisk
    );
    assert_eq!(
        classify_extents(&[extent(1, 0, 1001)], 1, &scan),
        Verdict::OverlapsSource
    );
    assert_eq!(
        classify_extents(&[extent(1, 1999, 1)], 1, &scan),
        Verdict::OverlapsSource
    );
    assert_eq!(
        classify_extents(&[extent(1, 0, 10_000)], 1, &scan),
        Verdict::OverlapsSource
    );
    assert_eq!(
        classify_extents(&[extent(1, 1500, 10)], 1, &scan),
        Verdict::OverlapsSource
    );
    // An extent that runs to the end of the offset space does not wrap around.
    assert_eq!(
        classify_extents(&[extent(1, u64::MAX - 5, u64::MAX)], 1, &scan),
        Verdict::SameDisk
    );
    assert_eq!(
        classify_extents(&[extent(1, 500, u64::MAX)], 1, &scan),
        Verdict::OverlapsSource
    );
    // Without protected ranges, or with empty ones, the disk alone decides.
    assert_eq!(
        classify_extents(&[extent(1, 0, 10)], 1, &[]),
        Verdict::SameDisk
    );
    #[allow(clippy::reversed_empty_ranges)]
    let empty = [5..5u64, 9..3];
    assert_eq!(
        classify_extents(&[extent(1, 0, 10)], 1, &empty),
        Verdict::SameDisk
    );
    // A zero-length extent is a malformed answer; it is read conservatively:
    // on the source disk it means "same disk", inside the range "overlap".
    assert_eq!(
        classify_extents(&[extent(1, 5000, 0)], 1, &scan),
        Verdict::SameDisk
    );
    assert_eq!(
        classify_extents(&[extent(1, 1500, 0)], 1, &scan),
        Verdict::OverlapsSource
    );
    assert_eq!(
        classify_extents(&[extent(2, 1500, 0)], 1, &scan),
        Verdict::Ok
    );
}

fn resolved(placement: Placement) -> Resolved {
    Resolved {
        placement,
        free_bytes: Some(1 << 30),
        fs_name: Some("NTFS".to_string()),
        max_file_bytes: None,
        on_system_volume: false,
        leaf_id: None,
    }
}

#[test]
fn verdicts_for_a_disk_source() {
    let disk = SourceLocation::Disk { number: 1 };
    let range = MIB..128 * MIB;
    let verdict =
        |r: &Resolved, purpose| decide(r, disk, &[Protected::session(range.clone())], purpose).0;

    let other_disk = resolved(Placement::Extents(vec![extent(0, MIB, 500 * MIB)]));
    let same_disk = resolved(Placement::Extents(vec![extent(1, 128 * MIB, 64 * MIB)]));
    let inside = resolved(Placement::Extents(vec![extent(1, 64 * MIB, 64 * MIB)]));
    let no_extents = resolved(Placement::Extents(Vec::new()));
    let unknown = resolved(Placement::Unknown("Storage Spaces".to_string()));
    let network = resolved(Placement::Network);
    // A VHD on another disk whose backing file lies on the source disk.
    let vhd_on_source = resolved(Placement::Extents(vec![
        extent(5, 0, 10 * MIB),
        extent(1, 200 * MIB, 50 * MIB),
    ]));
    let vhd_in_range = resolved(Placement::Extents(vec![
        extent(5, 0, 10 * MIB),
        extent(1, 100 * MIB, 50 * MIB),
    ]));

    for purpose in [
        Purpose::Recover,
        Purpose::Image,
        Purpose::Session,
        Purpose::AppData,
    ] {
        assert_eq!(verdict(&other_disk, purpose), Verdict::Ok);
        assert_eq!(verdict(&same_disk, purpose), Verdict::SameDisk);
        assert_eq!(verdict(&inside, purpose), Verdict::OverlapsSource);
        assert_eq!(verdict(&no_extents, purpose), Verdict::UnknownDisk);
        assert_eq!(verdict(&unknown, purpose), Verdict::UnknownDisk);
        assert_eq!(verdict(&network, purpose), Verdict::Ok);
        assert_eq!(verdict(&vhd_on_source, purpose), Verdict::SameDisk);
        assert_eq!(verdict(&vhd_in_range, purpose), Verdict::OverlapsSource);
    }
    // Messages explain; "ok" has none.
    assert_eq!(
        decide(
            &other_disk,
            disk,
            &[Protected::session(range.clone())],
            Purpose::Recover
        )
        .1,
        ""
    );
    assert!(decide(
        &inside,
        disk,
        &[Protected::session(range.clone())],
        Purpose::Recover
    )
    .1
    .contains("disk 1"));
    assert!(decide(
        &same_disk,
        disk,
        &[Protected::session(range.clone())],
        Purpose::Recover
    )
    .1
    .contains("outside the area known to hold lost data"));
    assert!(decide(
        &unknown,
        disk,
        &[Protected::session(range.clone())],
        Purpose::Recover
    )
    .1
    .contains("Storage Spaces"));
}

#[test]
fn only_session_files_on_the_system_volume_are_exempt_from_same_disk_consent() {
    let disk = SourceLocation::Disk { number: 0 };
    let range = 500 * MIB..900 * MIB;
    let mut on_system = resolved(Placement::Extents(vec![extent(0, MIB, 400 * MIB)]));
    on_system.on_system_volume = true;
    assert_eq!(
        decide(
            &on_system,
            disk,
            &[Protected::session(range.clone())],
            Purpose::Session
        )
        .0,
        Verdict::Ok
    );
    for purpose in [Purpose::Recover, Purpose::Image, Purpose::AppData] {
        assert_eq!(
            decide(
                &on_system,
                disk,
                &[Protected::session(range.clone())],
                purpose
            )
            .0,
            Verdict::SameDisk,
            "{purpose:?}"
        );
    }
    // The exemption never covers an overlap or an unknown location.
    let mut overlapping = resolved(Placement::Extents(vec![extent(0, MIB, 600 * MIB)]));
    overlapping.on_system_volume = true;
    assert_eq!(
        decide(
            &overlapping,
            disk,
            &[Protected::session(range.clone())],
            Purpose::Session
        )
        .0,
        Verdict::OverlapsSource
    );
    let mut unknown = resolved(Placement::Unknown("no extents".to_string()));
    unknown.on_system_volume = true;
    assert_eq!(
        decide(
            &unknown,
            disk,
            &[Protected::session(range.clone())],
            Purpose::Session
        )
        .0,
        Verdict::UnknownDisk
    );
    // Not the system volume: consent is needed for a session file as well.
    let elsewhere = resolved(Placement::Extents(vec![extent(0, MIB, 400 * MIB)]));
    assert_eq!(
        decide(
            &elsewhere,
            disk,
            &[Protected::session(range.clone())],
            Purpose::Session
        )
        .0,
        Verdict::SameDisk
    );
}

#[test]
fn verdicts_for_other_kinds_of_source() {
    let range = 0..MIB;
    let anywhere = resolved(Placement::Extents(vec![extent(1, 0, MIB)]));
    let unknown = resolved(Placement::Unknown("?".to_string()));
    // Nothing can overwrite a source that lives in memory.
    assert_eq!(
        decide(
            &anywhere,
            SourceLocation::Detached,
            &[Protected::session(range.clone())],
            Purpose::Recover
        )
        .0,
        Verdict::Ok
    );
    assert_eq!(
        decide(
            &unknown,
            SourceLocation::Detached,
            &[Protected::session(range.clone())],
            Purpose::Recover
        )
        .0,
        Verdict::Ok
    );
    // A source that does not say where it lives protects nothing: never "ok".
    assert_eq!(
        decide(
            &anywhere,
            SourceLocation::Unknown,
            &[Protected::session(range.clone())],
            Purpose::Recover
        )
        .0,
        Verdict::UnknownDisk
    );
    // An image: only the file itself is off limits.
    let id = FileId {
        volume: 7,
        index: 99,
    };
    let image = SourceLocation::File { id };
    assert_eq!(
        decide(
            &anywhere,
            image,
            &[Protected::session(range.clone())],
            Purpose::Recover
        )
        .0,
        Verdict::Ok
    );
    assert_eq!(
        decide(
            &unknown,
            image,
            &[Protected::session(range.clone())],
            Purpose::Image
        )
        .0,
        Verdict::Ok
    );
    let mut the_image_itself = resolved(Placement::Unknown("?".to_string()));
    the_image_itself.leaf_id = Some(id);
    assert_eq!(
        decide(
            &the_image_itself,
            image,
            &[Protected::session(range.clone())],
            Purpose::Image
        )
        .0,
        Verdict::OverlapsSource
    );
    let mut another_file = resolved(Placement::Unknown("?".to_string()));
    another_file.leaf_id = Some(FileId {
        volume: 7,
        index: 100,
    });
    assert_eq!(
        decide(
            &another_file,
            image,
            &[Protected::session(range.clone())],
            Purpose::Image
        )
        .0,
        Verdict::Ok
    );
}

#[test]
fn result_objects_match_the_protocol() {
    let check = DestinationCheck {
        verdict: Verdict::Ok,
        reason: None,
        message: String::new(),
        free_bytes: Some(500_000_000_000),
        enough_space: Some(true),
        fs: Some("NTFS".to_string()),
        max_file_bytes: None,
        disk_numbers: vec![0],
        source_extents: Vec::new(),
    };
    // The example of PROTOCOL.md, check_destination (the members the guard supplies).
    let expected: serde_json::Value = serde_json::from_str(
        r#"{"verdict": "ok", "reason": null, "message": "", "free_bytes": 500000000000,
            "enough_space": true, "fs": "NTFS", "max_file_bytes": null, "disk_numbers": [0]}"#,
    )
    .unwrap();
    for (reason, text) in [
        (ProtectedReason::SessionRange, "session_range"),
        (ProtectedReason::LostVolume, "lost_volume"),
        (ProtectedReason::SourceImage, "source_image"),
    ] {
        assert_eq!(serde_json::to_value(reason).unwrap(), text);
    }
    assert_eq!(serde_json::to_value(&check).unwrap(), expected);
    assert_eq!(
        serde_json::from_value::<DestinationCheck>(expected).unwrap(),
        check
    );
    for (verdict, text) in [
        (Verdict::Ok, "ok"),
        (Verdict::OverlapsSource, "overlaps_source"),
        (Verdict::SameDisk, "same_disk"),
        (Verdict::UnknownDisk, "unknown_disk"),
        (Verdict::NotFound, "not_found"),
        (Verdict::NotWritable, "not_writable"),
        (Verdict::InvalidPath, "invalid_path"),
    ] {
        assert_eq!(serde_json::to_value(verdict).unwrap(), text);
    }
    for (purpose, text) in [
        (Purpose::Recover, "recover"),
        (Purpose::Image, "image"),
        (Purpose::Session, "session"),
        (Purpose::AppData, "app_data"),
    ] {
        assert_eq!(serde_json::to_value(purpose).unwrap(), text);
        assert_eq!(
            serde_json::from_value::<Purpose>(serde_json::Value::from(text)).unwrap(),
            purpose
        );
    }
}

#[test]
fn consents_are_separate_and_nothing_overrides_an_overlap() {
    let with = |verdict| DestinationCheck {
        verdict,
        ..refused(verdict, String::new())
    };
    assert!(with(Verdict::Ok).is_allowed(false, false));
    assert!(!with(Verdict::SameDisk).is_allowed(false, false));
    assert!(with(Verdict::SameDisk).is_allowed(true, false));
    assert!(
        !with(Verdict::SameDisk).is_allowed(false, true),
        "consent to an unknown disk is not consent to the same disk"
    );
    assert!(
        !with(Verdict::UnknownDisk).is_allowed(true, false),
        "and the other way round"
    );
    assert!(with(Verdict::UnknownDisk).is_allowed(false, true));
    for refused_always in [
        Verdict::OverlapsSource,
        Verdict::NotFound,
        Verdict::NotWritable,
        Verdict::InvalidPath,
    ] {
        assert!(
            !with(refused_always).is_allowed(true, true),
            "{refused_always:?}"
        );
    }
}

fn image_in(dir: &TempDir, name: &str) -> (std::path::PathBuf, ImageFile) {
    let path = dir.join(name);
    std::fs::write(&path, vec![0x11u8; 64 * 512]).unwrap();
    let image = ImageFile::open(&path, None).unwrap();
    (path, image)
}

#[test]
fn the_source_image_itself_is_refused_under_every_name() {
    let tmp = TempDir::new("guard-image");
    let (path, image) = image_in(&tmp, "source.img");
    let range = 0..image.size();
    for purpose in [Purpose::Image, Purpose::Session] {
        let check = destination_check(
            &path,
            purpose,
            &image,
            &[Protected::session(range.clone())],
            None,
        );
        assert_eq!(
            check.verdict,
            Verdict::OverlapsSource,
            "{purpose:?}: {}",
            check.message
        );
        assert!(!check.is_allowed(true, true));
    }
    // Another spelling of the same path.
    let dotted = tmp.path().join(".").join("source.img");
    #[cfg(unix)]
    assert_eq!(
        destination_check(
            &dotted,
            Purpose::Image,
            &image,
            &[Protected::session(range.clone())],
            None
        )
        .verdict,
        Verdict::OverlapsSource
    );
    #[cfg(windows)]
    let _ = dotted;
    // A hard link to it.
    let hard = tmp.join("hardlink.img");
    if std::fs::hard_link(&path, &hard).is_ok() {
        assert_eq!(
            destination_check(
                &hard,
                Purpose::Image,
                &image,
                &[Protected::session(range.clone())],
                None
            )
            .verdict,
            Verdict::OverlapsSource
        );
    }
    // A symbolic link to it, and a path through a linked directory.
    let soft = tmp.join("softlink.img");
    if make_file_link(&path, &soft) {
        assert_eq!(
            destination_check(
                &soft,
                Purpose::Image,
                &image,
                &[Protected::session(range.clone())],
                None
            )
            .verdict,
            Verdict::OverlapsSource
        );
    }
    let dir_link = tmp.join("dirlink");
    if make_dir_link(tmp.path(), &dir_link) {
        let through = dir_link.join("source.img");
        assert_eq!(
            destination_check(
                &through,
                Purpose::Image,
                &image,
                &[Protected::session(range.clone())],
                None
            )
            .verdict,
            Verdict::OverlapsSource
        );
    }
    // The same goes through a RangedSource, which is what the engine holds.
    let shared: Arc<dyn BlockSource> = Arc::new(image);
    let ranged = crate::RangedSource::new(shared, 0..512).unwrap();
    assert_eq!(
        destination_check(
            &path,
            Purpose::Image,
            &ranged,
            &[Protected::session(0..512)],
            None
        )
        .verdict,
        Verdict::OverlapsSource
    );
}

#[test]
fn other_locations_are_fine_for_an_image_source_and_report_space() {
    let tmp = TempDir::new("guard-ok");
    let (_, image) = image_in(&tmp, "source.img");
    let range = 0..image.size();
    // An existing directory, a directory that does not exist yet, deep below one that does.
    for dest in [
        tmp.path().to_path_buf(),
        tmp.join("Rescued"),
        tmp.join("a").join("b").join("c"),
    ] {
        let check = destination_check(
            &dest,
            Purpose::Recover,
            &image,
            &[Protected::session(range.clone())],
            Some(1),
        );
        assert_eq!(
            check.verdict,
            Verdict::Ok,
            "{}: {}",
            dest.display(),
            check.message
        );
        assert_eq!(check.message, "");
        assert!(check.free_bytes.is_some_and(|free| free > 0), "{check:?}");
        assert_eq!(check.enough_space, Some(true));
        assert!(check.is_allowed(false, false));
    }
    // Asking creates nothing.
    assert!(!tmp.join("Rescued").exists());
    assert!(!tmp.join("a").exists());
    // More space than any disk has.
    let check = destination_check(
        &tmp.join("Rescued"),
        Purpose::Recover,
        &image,
        &[Protected::session(range.clone())],
        Some(u64::MAX),
    );
    assert_eq!(
        (check.verdict, check.enough_space),
        (Verdict::Ok, Some(false))
    );
    let check = destination_check(
        &tmp.join("Rescued"),
        Purpose::Recover,
        &image,
        &[Protected::session(range.clone())],
        None,
    );
    assert_eq!(check.enough_space, None);
    // A new image file next to the source, and an existing other file.
    assert_eq!(
        destination_check(
            &tmp.join("copy.img"),
            Purpose::Image,
            &image,
            &[Protected::session(range.clone())],
            None
        )
        .verdict,
        Verdict::Ok
    );
    std::fs::write(tmp.join("other.img"), b"x").unwrap();
    assert_eq!(
        destination_check(
            &tmp.join("other.img"),
            Purpose::Image,
            &image,
            &[Protected::session(range.clone())],
            None
        )
        .verdict,
        Verdict::Ok
    );
    assert_eq!(
        destination_check(
            &tmp.join("s.hdsc"),
            Purpose::Session,
            &image,
            &[Protected::session(range)],
            None
        )
        .verdict,
        Verdict::Ok
    );
}

#[test]
fn paths_that_cannot_be_used_are_told_apart() {
    let tmp = TempDir::new("guard-refuse");
    let (image_path, image) = image_in(&tmp, "source.img");
    let range = 0..image.size();
    let verdict = |path: &Path, purpose| {
        destination_check(
            path,
            purpose,
            &image,
            &[Protected::session(range.clone())],
            None,
        )
        .verdict
    };

    // Not absolute.
    for relative in ["Rescued", "./Rescued", "../x", ""] {
        let check = destination_check(
            Path::new(relative),
            Purpose::Recover,
            &image,
            &[Protected::session(range.clone())],
            Some(5),
        );
        assert_eq!(check.verdict, Verdict::InvalidPath, "{relative:?}");
        assert!(!check.message.is_empty());
        assert_eq!(
            (
                check.free_bytes,
                check.enough_space,
                check.fs,
                check.disk_numbers.len()
            ),
            (None, None, None, 0)
        );
    }
    // A file where a directory is needed, and below a file.
    std::fs::write(tmp.join("file.txt"), b"x").unwrap();
    assert_eq!(
        verdict(&tmp.join("file.txt"), Purpose::Recover),
        Verdict::NotWritable
    );
    assert_eq!(
        verdict(&tmp.join("file.txt"), Purpose::AppData),
        Verdict::NotWritable
    );
    assert_eq!(
        verdict(&tmp.join("file.txt").join("below"), Purpose::Recover),
        Verdict::NotWritable
    );
    assert_eq!(
        verdict(&tmp.join("file.txt").join("below.img"), Purpose::Image),
        Verdict::NotWritable
    );
    assert_eq!(
        verdict(&image_path.join("inside the image"), Purpose::Recover),
        Verdict::NotWritable
    );
    // A directory where a file is needed.
    std::fs::create_dir(tmp.join("dir")).unwrap();
    assert_eq!(
        verdict(&tmp.join("dir"), Purpose::Image),
        Verdict::NotWritable
    );
    assert_eq!(
        verdict(&tmp.join("dir"), Purpose::Session),
        Verdict::NotWritable
    );
    assert_eq!(verdict(&tmp.join("dir"), Purpose::Recover), Verdict::Ok);
}

#[cfg(unix)]
#[test]
fn unix_cannot_place_a_disk_source_and_says_so() {
    let tmp = TempDir::new("guard-unix-disk");
    let disk = MemSource::new(vec![0; 4096]).at_location(SourceLocation::Disk { number: 1 });
    let check = destination_check(
        &tmp.join("out"),
        Purpose::Recover,
        &disk,
        &[Protected::session(0..4096)],
        None,
    );
    assert_eq!(check.verdict, Verdict::UnknownDisk);
    assert!(check.message.contains("cannot find out"));
    assert!(check.free_bytes.is_some(), "space is reported all the same");
    assert!(check.disk_numbers.is_empty());
    // A source that lives nowhere, and one that does not say.
    let detached = MemSource::new(vec![0; 4096]);
    assert_eq!(
        destination_check(
            &tmp.join("out"),
            Purpose::Recover,
            &detached,
            &[Protected::session(0..4096)],
            None
        )
        .verdict,
        Verdict::Ok
    );
    let unknown = MemSource::new(vec![0; 4096]).at_location(SourceLocation::Unknown);
    assert_eq!(
        destination_check(
            &tmp.join("out"),
            Purpose::Recover,
            &unknown,
            &[Protected::session(0..4096)],
            None
        )
        .verdict,
        Verdict::UnknownDisk
    );
    // A location on a file system that is mounted read-only, if this machine has one.
    let read_only_mount = std::fs::read_to_string("/proc/self/mounts")
        .ok()
        .and_then(|mounts| {
            mounts.lines().find_map(|line| {
                let mut fields = line.split(' ');
                let (mount_point, options) = (fields.nth(1)?, fields.nth(1)?);
                (options.split(',').any(|o| o == "ro") && !mount_point.contains('\\'))
                    .then(|| mount_point.to_string())
            })
        });
    if let Some(mount_point) = read_only_mount {
        let dest = Path::new(&mount_point).join("hds-io-test");
        let check = destination_check(
            &dest,
            Purpose::Recover,
            &detached,
            &[Protected::session(0..4096)],
            None,
        );
        assert_eq!(
            check.verdict,
            Verdict::NotWritable,
            "{mount_point}: {}",
            check.message
        );
    }
    // The file-system name is reported where it is known.
    let check = destination_check(
        tmp.path(),
        Purpose::Recover,
        &detached,
        &[Protected::session(0..4096)],
        None,
    );
    if let Some(fs) = &check.fs {
        assert!(!fs.is_empty());
    }
    assert!(check.max_file_bytes.is_none() || check.fs.as_deref() == Some("FAT"));
}

#[cfg(windows)]
#[test]
fn a_disk_source_is_compared_with_the_disks_of_the_destination() {
    use crate::test_util::under_wine;

    let tmp = TempDir::new("guard-win-disk");
    let dest = tmp.join("Rescued").join("deeper");
    let disk_source =
        |number| MemSource::new(vec![0; 4096]).at_location(SourceLocation::Disk { number });
    // A disk this machine does not have.
    let check = destination_check(
        &dest,
        Purpose::Recover,
        &disk_source(9_999),
        &[Protected::session(0..4096)],
        Some(1),
    );
    assert!(!tmp.join("Rescued").exists(), "the check creates nothing");
    if check.verdict == Verdict::UnknownDisk {
        // Wine reports no disk extents for its volumes: that is "unknown", never "ok".
        assert!(under_wine(), "{check:?}");
        assert!(check.disk_numbers.is_empty());
        assert!(check.is_allowed(false, true) && !check.is_allowed(true, false));
        return;
    }
    assert_eq!(check.verdict, Verdict::Ok, "{check:?}");
    assert!(!check.disk_numbers.is_empty());
    assert!(check.free_bytes.is_some_and(|free| free > 0) && check.fs.is_some());
    assert_eq!(check.enough_space, Some(true));

    // Every disk the destination is stored on (several for a spanned volume or a
    // virtual disk on top of a physical one).
    for &disk in &check.disk_numbers {
        // The whole disk is being rescued: the destination lies inside the range.
        let inside = destination_check(
            &dest,
            Purpose::Recover,
            &disk_source(disk),
            &[Protected::session(0..u64::MAX)],
            None,
        );
        assert_eq!(inside.verdict, Verdict::OverlapsSource, "{inside:?}");
        assert!(
            !inside.is_allowed(true, true),
            "nothing overrides an overlap"
        );
        assert!(inside.message.contains(&format!("disk {disk}")));
        // Only the first sector is (where the partition table is, not the volume).
        let elsewhere = destination_check(
            &dest,
            Purpose::Recover,
            &disk_source(disk),
            &[Protected::session(0..512)],
            None,
        );
        assert_eq!(elsewhere.verdict, Verdict::SameDisk, "{elsewhere:?}");
        assert!(elsewhere.is_allowed(true, false) && !elsewhere.is_allowed(false, true));
        assert_eq!(elsewhere.disk_numbers, check.disk_numbers);
        // A session file there needs no consent exactly when that is the system volume.
        let session_path = tmp.join("scan.hdsc");
        let resolved = sys::resolve(&session_path, Purpose::Session)
            .ok()
            .expect("the scratch directory resolves");
        let session = destination_check(
            &session_path,
            Purpose::Session,
            &disk_source(disk),
            &[Protected::session(0..512)],
            None,
        );
        let expected = if resolved.on_system_volume {
            Verdict::Ok
        } else {
            Verdict::SameDisk
        };
        assert_eq!(session.verdict, expected, "{session:?}");
    }
}

#[cfg(windows)]
#[test]
fn windows_paths_must_be_absolute_drive_or_unc_paths() {
    let tmp = TempDir::new("guard-win-paths");
    let source = MemSource::new(vec![0; 4096]);
    let verdict = |path: &str| {
        destination_check(
            Path::new(path),
            Purpose::Recover,
            &source,
            &[Protected::session(0..4096)],
            None,
        )
        .verdict
    };
    for bad in [
        "C:relative",
        r"\rooted\path",
        r"\\.\C:\x",
        r"\\.\PhysicalDrive0",
        r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\x",
        r"\\?\C:",
        r"\\server",
        "NUL",
        "CON",
        "a\0b",
    ] {
        assert_eq!(verdict(bad), Verdict::InvalidPath, "{bad:?}");
    }
    // The scratch directory in its verbatim form (canonical paths are verbatim) ...
    let verbatim = tmp.path().to_str().unwrap().to_string();
    assert!(verbatim.starts_with(r"\\?\"), "{verbatim}");
    assert_eq!(verdict(&verbatim), Verdict::Ok);
    assert_eq!(verdict(&format!(r"{verbatim}\new\folders")), Verdict::Ok);
    // ... in its ordinary form, with forward slashes, and with dots to fold.
    let plain = verbatim.trim_start_matches(r"\\?\").to_string();
    assert_eq!(verdict(&plain), Verdict::Ok);
    assert_eq!(verdict(&plain.replace('\\', "/")), Verdict::Ok);
    assert_eq!(verdict(&format!(r"{plain}\a\..\b\.")), Verdict::Ok);
    // A drive letter nobody has.
    if let Some(letter) = ('D'..='Y')
        .rev()
        .find(|l| !Path::new(&format!(r"{l}:\")).exists())
    {
        assert_eq!(
            verdict(&format!(r"{letter}:\Rescued")),
            Verdict::NotFound,
            "drive {letter}:"
        );
    }
}

#[cfg(windows)]
#[test]
fn a_share_of_this_machine_is_never_simply_ok() {
    let disk = MemSource::new(vec![0; 4096]).at_location(SourceLocation::Disk { number: 0 });
    for path in [
        r"\\localhost\C$\hds-io-test",
        r"\\127.0.0.1\C$\hds-io-test",
        r"\\.\C$\x",
    ] {
        let check = destination_check(
            Path::new(path),
            Purpose::Recover,
            &disk,
            &[Protected::session(0..4096)],
            None,
        );
        // Reachable: unknown (it leads back to a local volume). Not reachable: not found.
        assert!(
            matches!(
                check.verdict,
                Verdict::UnknownDisk
                    | Verdict::NotFound
                    | Verdict::NotWritable
                    | Verdict::InvalidPath
            ),
            "{path}: {check:?}"
        );
    }
}

#[test]
fn the_first_protected_range_that_is_hit_names_the_reason() {
    let disk = SourceLocation::Disk { number: 3 };
    let at = |start: u64, len: u64| {
        resolved(Placement::Extents(vec![Extent {
            disk: 3,
            start,
            len,
        }]))
    };
    // The session range is the first partition; the lost volume reaches into the second.
    let protected = [
        Protected::session(1_000..5_000),
        Protected::lost_volume(1_000..9_000),
    ];
    // Inside both: the session range, listed first, is named.
    let (verdict, message, reason) = decide(&at(2_000, 100), disk, &protected, Purpose::Recover);
    assert_eq!(verdict, Verdict::OverlapsSource);
    assert_eq!(reason, Some(ProtectedReason::SessionRange));
    assert!(message.contains("being rescued"), "{message}");
    // Only inside the lost volume: refused all the same, with its own reason and text.
    let (verdict, message, reason) = decide(&at(6_000, 100), disk, &protected, Purpose::Recover);
    assert_eq!(verdict, Verdict::OverlapsSource);
    assert_eq!(reason, Some(ProtectedReason::LostVolume));
    assert!(message.contains("lost volume"), "{message}");
    // The session exemption on the system volume never applies to an overlap.
    let mut system = at(6_000, 100);
    system.on_system_volume = true;
    assert_eq!(
        decide(&system, disk, &protected, Purpose::Session).0,
        Verdict::OverlapsSource
    );
    // Behind every protected range: same disk, consent, no reason.
    let (verdict, _, reason) = decide(&at(9_000, 100), disk, &protected, Purpose::Recover);
    assert_eq!(verdict, Verdict::SameDisk);
    assert_eq!(reason, None);
    // An empty or inverted protected range protects nothing.
    #[allow(clippy::reversed_empty_ranges)]
    let hollow = [
        Protected::lost_volume(7_000..7_000),
        Protected::lost_volume(9..3),
    ];
    assert_eq!(
        decide(&at(6_000, 2_000), disk, &hollow, Purpose::Recover).0,
        Verdict::SameDisk
    );
    // No protected range at all still means consent on the source disk.
    assert_eq!(
        decide(&at(0, 10), disk, &[], Purpose::Recover).0,
        Verdict::SameDisk
    );
    // The image source reports its own reason.
    let image = SourceLocation::File {
        id: FileId {
            volume: 7,
            index: 99,
        },
    };
    let mut itself = resolved(Placement::Unknown("?".to_string()));
    itself.leaf_id = Some(FileId {
        volume: 7,
        index: 99,
    });
    assert_eq!(
        decide(&itself, image, &protected, Purpose::Image).2,
        Some(ProtectedReason::SourceImage)
    );
}
