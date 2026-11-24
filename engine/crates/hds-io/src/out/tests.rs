use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, SystemTime};

use super::*;
use crate::test_util::{make_dir_link, make_file_link, under_wine, TempDir};

/// 2020-01-01T00:00:00Z as FILETIME.
const FT_2020: u64 = 132_223_104_000_000_000;
const FT_DAY: u64 = 864_000_000_000;

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn never(_: &str) -> bool {
    false
}

fn always(_: &str) -> bool {
    true
}

/// Writes `content` to a new part file and finishes it without times.
fn part_with(dir: &OutDir, content: &str) -> PartFile {
    let mut part = dir.create_part().unwrap();
    part.write_all(content.as_bytes()).unwrap();
    part.finish(&Timestamps::default()).unwrap();
    part
}

fn case_insensitive(dir: &TempDir) -> bool {
    std::fs::write(dir.join("CaseProbe"), b"").unwrap();
    let insensitive = dir.join("caseprobe").exists();
    std::fs::remove_file(dir.join("CaseProbe")).unwrap();
    insensitive
}

#[test]
fn create_root_makes_missing_levels_and_returns_the_canonical_directory() {
    let tmp = TempDir::new("out-root");
    let wanted = tmp.join("Rescued").join("2026").join("run 1");
    let root = OutDir::create_root(&wanted).unwrap();
    assert!(wanted.is_dir());
    assert_eq!(root.path(), std::fs::canonicalize(&wanted).unwrap());
    assert_eq!(
        root.path_len_units(),
        root.path().to_str().unwrap().encode_utf16().count()
    );
    assert!(format!("{root:?}").contains("run 1"));
    // Creating it again reuses it; opening works too.
    let again = OutDir::create_root(&wanted).unwrap();
    assert_eq!(again.path(), root.path());
    assert_eq!(OutDir::open_root(&wanted).unwrap().path(), root.path());

    // A file in the way, a missing directory, a relative path.
    std::fs::write(tmp.join("file"), b"x").unwrap();
    assert!(OutDir::create_root(&tmp.join("file").join("below")).is_err());
    assert!(OutDir::create_root(&tmp.join("file")).is_err());
    assert!(OutDir::open_root(&tmp.join("file")).is_err());
    assert_eq!(
        OutDir::open_root(&tmp.join("missing")).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    assert!(OutDir::create_root(Path::new("relative/dir")).is_err());
    assert!(!Path::new("relative").exists());
}

#[test]
fn a_link_on_the_way_to_the_root_is_resolved_once() {
    let tmp = TempDir::new("out-rootlink");
    let real = tmp.join("real");
    std::fs::create_dir(&real).unwrap();
    let link = tmp.join("link");
    if !make_dir_link(&real, &link) {
        eprintln!("SKIPPED: no directory links can be created here");
        return;
    }
    // The user points at the link: the output root is the directory behind it.
    let root = OutDir::create_root(&link.join("sub")).unwrap();
    assert_eq!(
        root.path(),
        std::fs::canonicalize(real.join("sub")).unwrap()
    );
    root.create_new("f.txt").unwrap().write_all(b"x").unwrap();
    assert_eq!(read(&real.join("sub").join("f.txt")), "x");
}

#[test]
fn subdir_creates_one_level_and_reuses_only_plain_directories() {
    let tmp = TempDir::new("out-subdir");
    let root = OutDir::create_root(&tmp.join("dest")).unwrap();
    let (photos, created) = root.subdir("Photos").unwrap();
    assert!(created);
    assert_eq!(photos.path(), root.path().join("Photos"));
    let (again, created) = root.subdir("Photos").unwrap();
    assert!(!created, "an existing plain directory is reused");
    assert_eq!(again.path(), photos.path());
    let (deep, _) = photos.subdir("2019").unwrap().0.subdir("Summer ☀").unwrap();
    deep.create_new("a.txt")
        .unwrap()
        .write_all(b"deep")
        .unwrap();
    assert_eq!(
        read(
            &tmp.join("dest")
                .join("Photos")
                .join("2019")
                .join("Summer ☀")
                .join("a.txt")
        ),
        "deep"
    );

    // A file under the wanted name is a collision, not something to enter.
    std::fs::write(root.path().join("taken"), b"file").unwrap();
    assert_eq!(
        root.subdir("taken").unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(read(&root.path().join("taken")), "file");

    assert!(root.open_subdir("nothing here").unwrap().is_none());
    assert!(root.open_subdir("Photos").unwrap().is_some());
    assert_eq!(
        root.open_subdir("taken").unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(root.entry("Photos").unwrap(), Some(EntryKind::Dir));
    assert_eq!(root.entry("taken").unwrap(), Some(EntryKind::File));
    assert_eq!(root.entry("nothing here").unwrap(), None);
}

#[test]
fn a_linked_directory_inside_the_destination_is_never_entered() {
    let tmp = TempDir::new("out-dirlink");
    let outside = tmp.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("victim.txt"), b"original").unwrap();
    let root = OutDir::create_root(&tmp.join("dest")).unwrap();
    // A junction (here: whatever kind of directory link the system offers) planted
    // under the name of a folder that recovery is about to create.
    if !make_dir_link(&outside, &root.path().join("Photos")) {
        eprintln!("SKIPPED: no directory links can be created here");
        return;
    }
    assert_eq!(root.entry("Photos").unwrap(), Some(EntryKind::Link));
    let err = root.subdir("Photos").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
    assert_eq!(classify_error(&err), OutErrorKind::Exists);
    assert_eq!(
        root.open_subdir("Photos").unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    // Nothing went through the link, and the caller's next name works.
    assert_eq!(names(&outside), vec!["victim.txt"]);
    let (renamed, created) = root.subdir("Photos (2)").unwrap();
    assert!(created);
    renamed
        .create_new("img.jpg")
        .unwrap()
        .write_all(b"jpeg")
        .unwrap();
    assert_eq!(names(&outside), vec!["victim.txt"]);
    assert_eq!(read(&outside.join("victim.txt")), "original");

    // The cleanup of leftover part files does not walk through it either.
    std::fs::write(outside.join("~hds-1.part"), b"not ours to delete").unwrap();
    std::fs::write(root.path().join("~hds-2.part"), b"ours").unwrap();
    let report = root.remove_leftover_parts(true).unwrap();
    assert_eq!(
        report,
        Leftovers {
            removed: 1,
            failed: 0
        }
    );
    assert!(outside.join("~hds-1.part").exists());
}

#[test]
fn create_new_never_truncates_follows_or_replaces() {
    let tmp = TempDir::new("out-createnew");
    let root = OutDir::create_root(&tmp.join("dest")).unwrap();
    let mut file = root.create_new("report.csv").unwrap();
    file.write_all(b"first").unwrap();
    drop(file);
    let err = root.create_new("report.csv").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(classify_error(&err), OutErrorKind::Exists);
    assert_eq!(read(&root.path().join("report.csv")), "first");
    assert_eq!(
        root.create_new("Photos")
            .map(|_| ())
            .and(root.subdir("Photos").map(|_| ()))
            .unwrap_err()
            .kind(),
        io::ErrorKind::AlreadyExists
    );

    // An existing link under the name - even a dangling one - is "exists" too.
    let victim = tmp.join("victim.txt");
    std::fs::write(&victim, b"original").unwrap();
    if make_file_link(&victim, &root.path().join("link.txt")) {
        assert_eq!(
            root.create_new("link.txt").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(read(&victim), "original");
    } else {
        eprintln!("SKIPPED: no file links can be created here");
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(tmp.join("does-not-exist"), root.path().join("dangling.txt"))
            .unwrap();
        assert_eq!(
            root.create_new("dangling.txt").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert!(
            !tmp.join("does-not-exist").exists(),
            "the link's target was not created"
        );
    }
}

#[test]
fn names_are_validated_before_the_file_system_is_asked() {
    let tmp = TempDir::new("out-names");
    let root = OutDir::create_root(&tmp.join("dest")).unwrap();
    for bad in [
        "",
        ".",
        "..",
        "a/b",
        "a\\b",
        "../escape",
        "a:b",
        "x?",
        "trailing.",
        "trailing ",
        "CON",
        "nul.txt",
        "a\0b",
        "tab\t",
    ] {
        let check = |result: io::Result<()>| {
            let err = result.expect_err(bad);
            assert_eq!(
                classify_error(&err),
                OutErrorKind::NameRejected,
                "{bad:?}: {err}"
            );
        };
        check(root.subdir(bad).map(|_| ()));
        check(root.open_subdir(bad).map(|_| ()));
        check(root.create_new(bad).map(|_| ()));
        check(root.open_existing(bad).map(|_| ()));
        check(root.remove_file(bad));
        check(root.entry(bad).map(|_| ()));
        check(root.rename_noreplace(bad, "ok"));
        check(root.rename_noreplace("ok", bad));
        let mut part = root.create_part().unwrap();
        check(part.commit(bad, OnExists::Overwrite, &never).map(|_| ()));
    }
    assert!(
        names(root.path()).is_empty(),
        "nothing was created and no part file is left: {:?}",
        names(root.path())
    );
    assert!(
        names(tmp.path()).len() == 1,
        "nothing escaped the destination"
    );
}

#[test]
fn a_part_file_becomes_the_final_file_with_content_and_times() {
    let tmp = TempDir::new("out-part");
    let root = OutDir::create_root(&tmp.join("dest")).unwrap();
    let mut part = root.create_part().unwrap();
    assert!(is_part_name(part.name()), "{}", part.name());
    assert_eq!(names(root.path()), vec![part.name().to_string()]);
    part.write_all(b"recovered ").unwrap();
    part.write_all(b"content").unwrap();
    part.flush().unwrap();
    part.sync().unwrap();
    let times = Timestamps {
        created: FT_2020,
        modified: FT_2020 + 31 * FT_DAY + 1_234_567,
        accessed: FT_2020 + 60 * FT_DAY,
    };
    let report = part.finish(&times).unwrap();
    assert_eq!(report.modified, TimeOutcome::Applied);
    assert_eq!(report.accessed, TimeOutcome::Applied);
    assert_eq!(
        report.created,
        if cfg!(windows) {
            TimeOutcome::Applied
        } else {
            TimeOutcome::Unsupported
        }
    );
    assert_eq!(report.note(), None);
    assert!(
        part.write_all(b"late").is_err(),
        "nothing can be written after finish"
    );
    assert!(part.finish(&times).is_err());

    assert_eq!(
        part.commit("IMG_0042.JPG", OnExists::Rename, &never)
            .unwrap(),
        CommitOutcome::Committed
    );
    assert_eq!(names(root.path()), vec!["IMG_0042.JPG"]);
    let final_path = root.path().join("IMG_0042.JPG");
    // (Looked at before the content is read: reading moves the access time. On
    // Windows a virus scanner may have read the new file already, so the access
    // time is only compared where nothing of that kind happens.)
    let meta = std::fs::metadata(&final_path).unwrap();
    assert_eq!(
        meta.modified().unwrap(),
        filetime_to_system_time(times.modified).unwrap()
    );
    if cfg!(unix) {
        assert_eq!(
            meta.accessed().unwrap(),
            filetime_to_system_time(times.accessed).unwrap()
        );
    }
    if cfg!(windows) && !under_wine() {
        assert_eq!(
            meta.created().unwrap(),
            filetime_to_system_time(times.created).unwrap()
        );
    }
    assert_eq!(read(&final_path), "recovered content");
    // A committed part file is spent.
    assert!(part.commit("other", OnExists::Rename, &never).is_err());
    drop(part);
    assert_eq!(
        names(root.path()),
        vec!["IMG_0042.JPG"],
        "dropping a committed part deletes nothing"
    );
}

#[test]
fn an_abandoned_part_file_leaves_nothing_behind() {
    let tmp = TempDir::new("out-abandon");
    let root = OutDir::create_root(&tmp.join("dest")).unwrap();
    {
        let mut part = root.create_part().unwrap();
        part.write_all(b"half a file").unwrap();
        // Dropped mid-write: the job failed or was cancelled.
    }
    assert!(names(root.path()).is_empty());
    let mut part = root.create_part().unwrap();
    part.write_all(b"x").unwrap();
    part.set_len(100).unwrap();
    part.discard().unwrap();
    assert!(names(root.path()).is_empty());
    // A finished part that is never committed goes as well.
    drop(part_with(&root, "finished but unnamed"));
    assert!(names(root.path()).is_empty());
    // Part names are not reused while files exist: three parts, three names.
    let (a, b, c) = (
        root.create_part().unwrap(),
        root.create_part().unwrap(),
        root.create_part().unwrap(),
    );
    assert!(a.name() != b.name() && b.name() != c.name() && a.name() != c.name());
    assert_eq!(names(root.path()).len(), 3);
}

#[test]
fn commit_never_replaces_a_file_of_this_run() {
    let tmp = TempDir::new("out-ownfile");
    let root = OutDir::create_root(&tmp.join("dest")).unwrap();
    let mut first = part_with(&root, "first");
    assert_eq!(
        first.commit("a.txt", OnExists::Overwrite, &never).unwrap(),
        CommitOutcome::Committed
    );
    for policy in [OnExists::Rename, OnExists::Skip, OnExists::Overwrite] {
        let mut second = part_with(&root, "second");
        // The caller knows that this run produced "a.txt".
        let seen = std::cell::RefCell::new(Vec::new());
        let produced = |existing: &str| {
            seen.borrow_mut().push(existing.to_string());
            existing == "a.txt"
        };
        assert_eq!(
            second.commit("a.txt", policy, &produced).unwrap(),
            CommitOutcome::NameTaken,
            "{policy:?}"
        );
        assert_eq!(*seen.borrow(), vec!["a.txt"]);
        assert_eq!(read(&root.path().join("a.txt")), "first");
        // The part is still there and takes the next name.
        assert_eq!(
            second.commit("a (2).txt", policy, &produced).unwrap(),
            CommitOutcome::Committed
        );
        assert_eq!(read(&root.path().join("a (2).txt")), "second");
        root.remove_file("a (2).txt").unwrap();
    }
    assert_eq!(names(root.path()), vec!["a.txt"]);
}

#[test]
fn on_exists_decides_for_files_that_were_there_before() {
    let tmp = TempDir::new("out-onexists");
    let root = OutDir::create_root(&tmp.join("dest")).unwrap();
    let existing = root.path().join("old.txt");
    std::fs::write(&existing, b"was here before").unwrap();

    // rename: the old file stays, the caller picks the next name.
    let mut part = part_with(&root, "new");
    assert_eq!(
        part.commit("old.txt", OnExists::Rename, &never).unwrap(),
        CommitOutcome::NameTaken
    );
    assert_eq!(read(&existing), "was here before");
    assert_eq!(
        part.commit("old (2).txt", OnExists::Rename, &never)
            .unwrap(),
        CommitOutcome::Committed
    );

    // skip: the old file stays, the new one is dropped.
    let mut part = part_with(&root, "new");
    let part_name = part.name().to_string();
    assert_eq!(
        part.commit("old.txt", OnExists::Skip, &never).unwrap(),
        CommitOutcome::Skipped
    );
    assert_eq!(read(&existing), "was here before");
    assert!(
        !root.path().join(&part_name).exists(),
        "the skipped part file is deleted"
    );
    assert!(
        part.commit("x.txt", OnExists::Skip, &never).is_err(),
        "a skipped part is spent"
    );

    // overwrite: the old regular file is deleted, then the rename is done.
    let mut part = part_with(&root, "new");
    assert_eq!(
        part.commit("old.txt", OnExists::Overwrite, &never).unwrap(),
        CommitOutcome::Overwritten
    );
    assert_eq!(read(&existing), "new");
    assert_eq!(names(root.path()), vec!["old (2).txt", "old.txt"]);

    // overwrite never touches a directory ...
    root.subdir("folder").unwrap();
    let mut part = part_with(&root, "new");
    assert_eq!(
        part.commit("folder", OnExists::Overwrite, &never).unwrap(),
        CommitOutcome::NameTaken
    );
    assert!(root.path().join("folder").is_dir());
    // ... and a file that this run produced is safe even from "overwrite".
    assert_eq!(
        part.commit("old.txt", OnExists::Overwrite, &always)
            .unwrap(),
        CommitOutcome::NameTaken
    );
    assert_eq!(read(&existing), "new");
}

#[test]
fn overwrite_does_not_write_through_a_linked_file() {
    let tmp = TempDir::new("out-filelink");
    let root = OutDir::create_root(&tmp.join("dest")).unwrap();
    let victim = tmp.join("victim.txt");
    std::fs::write(&victim, b"original").unwrap();
    let link = root.path().join("report.txt");
    if !make_file_link(&victim, &link) {
        eprintln!("SKIPPED: no file links can be created here");
        return;
    }
    assert_eq!(root.entry("report.txt").unwrap(), Some(EntryKind::Link));
    for policy in [OnExists::Rename, OnExists::Skip, OnExists::Overwrite] {
        let mut part = part_with(&root, "recovered");
        let outcome = part.commit("report.txt", policy, &never).unwrap();
        // Whatever the policy, the file behind the link is neither written nor
        // deleted, and the link is not "overwritten" either.
        assert_eq!(read(&victim), "original", "{policy:?}");
        match policy {
            OnExists::Rename | OnExists::Overwrite => {
                assert_eq!(outcome, CommitOutcome::NameTaken, "{policy:?}")
            }
            OnExists::Skip => assert_eq!(outcome, CommitOutcome::Skipped),
        }
        assert!(
            std::fs::symlink_metadata(&link).is_ok(),
            "the link is still there after {policy:?}"
        );
    }
    // Nothing else reaches through the link: not an open for writing, not a deletion.
    assert!(root.open_existing("report.txt").is_err());
    assert_eq!(
        root.remove_file("report.txt").unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert!(std::fs::symlink_metadata(&link).is_ok());
    assert_eq!(read(&victim), "original");
    assert_eq!(
        names(root.path()),
        vec!["report.txt"],
        "no part file is left"
    );
}

#[test]
fn names_that_differ_only_in_case_follow_the_file_system() {
    let tmp = TempDir::new("out-case");
    let insensitive = case_insensitive(&tmp);
    let root = OutDir::create_root(&tmp.join("dest")).unwrap();
    let mut first = part_with(&root, "first");
    assert_eq!(
        first
            .commit("Report.txt", OnExists::Rename, &never)
            .unwrap(),
        CommitOutcome::Committed
    );

    let mut second = part_with(&root, "second");
    let seen = std::cell::RefCell::new(Vec::new());
    let produced = |existing: &str| {
        seen.borrow_mut().push(existing.to_string());
        existing == "Report.txt"
    };
    let outcome = second
        .commit("REPORT.TXT", OnExists::Overwrite, &produced)
        .unwrap();
    if insensitive {
        // The file system says the name exists; the run is told which entry it is,
        // under the name it is stored with, and its own file survives "overwrite".
        assert_eq!(outcome, CommitOutcome::NameTaken);
        assert_eq!(*seen.borrow(), vec!["Report.txt"]);
        assert_eq!(read(&root.path().join("Report.txt")), "first");
        assert_eq!(
            root.create_new("report.TXT").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            root.rename_noreplace(second.name(), "rePORT.txt")
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            second
                .commit("REPORT (2).TXT", OnExists::Overwrite, &produced)
                .unwrap(),
            CommitOutcome::Committed
        );
        assert_eq!(names(root.path()), vec!["REPORT (2).TXT", "Report.txt"]);
    } else {
        // Two different names for this file system: both files exist.
        assert_eq!(outcome, CommitOutcome::Committed);
        assert!(seen.borrow().is_empty());
        assert_eq!(names(root.path()), vec!["REPORT.TXT", "Report.txt"]);
        assert_eq!(read(&root.path().join("Report.txt")), "first");
        assert_eq!(read(&root.path().join("REPORT.TXT")), "second");
    }
}

#[test]
fn rename_noreplace_fails_on_an_existing_name() {
    let tmp = TempDir::new("out-rename");
    let root = OutDir::create_root(&tmp.join("dest")).unwrap();
    root.create_new("from.tmp")
        .unwrap()
        .write_all(b"moving")
        .unwrap();
    root.create_new("taken.txt")
        .unwrap()
        .write_all(b"resident")
        .unwrap();
    root.subdir("taken dir").unwrap();

    for target in ["taken.txt", "taken dir"] {
        let err = root.rename_noreplace("from.tmp", target).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{target}: {err}");
        assert_eq!(classify_error(&err), OutErrorKind::Exists);
    }
    assert_eq!(read(&root.path().join("from.tmp")), "moving");
    assert_eq!(read(&root.path().join("taken.txt")), "resident");
    assert!(root.path().join("taken dir").is_dir());

    root.rename_noreplace("from.tmp", "free.txt").unwrap();
    assert_eq!(read(&root.path().join("free.txt")), "moving");
    assert!(!root.path().join("from.tmp").exists());
    assert_eq!(
        root.rename_noreplace("from.tmp", "other.txt")
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
    // Onto itself: "exists" on Unix, a no-op on Windows; the file is untouched either way.
    let onto_itself = root.rename_noreplace("free.txt", "free.txt");
    if cfg!(unix) {
        assert_eq!(
            onto_itself.unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
    }
    assert_eq!(read(&root.path().join("free.txt")), "moving");
    assert_eq!(
        names(root.path()),
        vec!["free.txt", "taken dir", "taken.txt"]
    );
}

#[test]
fn remove_file_deletes_regular_files_only() {
    let tmp = TempDir::new("out-remove");
    let root = OutDir::create_root(&tmp.join("dest")).unwrap();
    root.create_new("file.bin").unwrap();
    root.remove_file("file.bin").unwrap();
    assert_eq!(root.entry("file.bin").unwrap(), None);
    assert_eq!(
        root.remove_file("file.bin").unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    root.subdir("dir").unwrap();
    assert_eq!(
        root.remove_file("dir").unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert!(root.path().join("dir").is_dir());

    // Links are not followed and not removed: the engine made none of them.
    let outside = tmp.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("keep.txt"), b"keep").unwrap();
    if make_dir_link(&outside, &root.path().join("dirlink")) {
        assert_eq!(
            root.remove_file("dirlink").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(std::fs::symlink_metadata(root.path().join("dirlink")).is_ok());
    }
    if make_file_link(&outside.join("keep.txt"), &root.path().join("filelink")) {
        assert_eq!(
            root.remove_file("filelink").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(std::fs::symlink_metadata(root.path().join("filelink")).is_ok());
    }
    assert_eq!(read(&outside.join("keep.txt")), "keep");
}

#[test]
fn leftover_part_files_are_removed_and_nothing_else() {
    let tmp = TempDir::new("out-leftover");
    let root = OutDir::create_root(&tmp.join("dest")).unwrap();
    let (a, _) = root.subdir("a").unwrap();
    let (b, _) = a.subdir("b").unwrap();
    let (c, _) = root.subdir("c").unwrap();
    // What a killed run leaves behind, at three levels.
    for (dir, name) in [
        (&root, "~hds-1.part"),
        (&a, "~hds-22.part"),
        (&b, "~hds-333.part"),
        (&b, "~hds-4.part"),
    ] {
        std::fs::write(dir.path().join(name), b"half written").unwrap();
    }
    // What must stay: finished files, look-alikes, a directory with a part name.
    let keep = [
        (&root, "photo.jpg"),
        (&root, "~hds-.part"),
        (&root, "~hds-x1.part"),
        (&root, "~hds-1.part.bak"),
        (&a, "x~hds-1.part"),
        (&c, "~HDS-7.PARTIAL"),
        (&c, "_~hds-7.part"),
    ];
    for (dir, name) in keep {
        std::fs::write(dir.path().join(name), b"keep").unwrap();
    }
    std::fs::create_dir(c.path().join("~hds-9.part")).unwrap();
    std::fs::write(c.path().join("~hds-9.part").join("inner.txt"), b"keep").unwrap();

    // Not recursive: only the root's own part file.
    assert_eq!(
        root.remove_leftover_parts(false).unwrap(),
        Leftovers {
            removed: 1,
            failed: 0
        }
    );
    assert!(a.path().join("~hds-22.part").exists());
    // Recursive: the rest.
    assert_eq!(
        root.remove_leftover_parts(true).unwrap(),
        Leftovers {
            removed: 3,
            failed: 0
        }
    );
    assert_eq!(
        root.remove_leftover_parts(true).unwrap(),
        Leftovers::default()
    );
    assert_eq!(
        names(root.path()),
        vec![
            "a",
            "c",
            "photo.jpg",
            "~hds-.part",
            "~hds-1.part.bak",
            "~hds-x1.part"
        ]
    );
    assert_eq!(names(a.path()), vec!["b", "x~hds-1.part"]);
    assert!(names(b.path()).is_empty());
    assert_eq!(
        names(c.path()),
        vec!["_~hds-7.part", "~HDS-7.PARTIAL", "~hds-9.part"]
    );
    assert_eq!(names(&c.path().join("~hds-9.part")), vec!["inner.txt"]);

    // A link that carries a part name is not a part file.
    let victim = tmp.join("victim.txt");
    std::fs::write(&victim, b"original").unwrap();
    let link = root.path().join("~hds-77.part");
    if make_file_link(&victim, &link) {
        assert_eq!(
            root.remove_leftover_parts(true).unwrap(),
            Leftovers::default()
        );
        assert!(std::fs::symlink_metadata(&link).is_ok());
    }
    assert_eq!(read(&victim), "original");
}

#[test]
fn a_wide_and_deep_tree_is_cleaned_with_one_open_directory_per_level() {
    let tmp = TempDir::new("out-leftover-tree");
    let root = OutDir::create_root(&tmp.join("dest")).unwrap();
    // 300 sibling directories (more than a third of the usual descriptor limit
    // if they were all held open) and a chain 40 levels deep.
    for i in 0..300 {
        let (dir, _) = root.subdir(&format!("dir {i:03}")).unwrap();
        std::fs::write(dir.path().join(format!("~hds-{i}.part")), b"x").unwrap();
    }
    let mut deep = root.clone();
    for level in 0..40 {
        deep = deep.subdir(&format!("level {level}")).unwrap().0;
        std::fs::write(deep.path().join("~hds-5.part"), b"x").unwrap();
    }
    drop(deep);
    assert_eq!(
        root.remove_leftover_parts(true).unwrap(),
        Leftovers {
            removed: 340,
            failed: 0
        }
    );
}

#[test]
fn part_names_are_recognised_exactly() {
    for yes in [
        "~hds-1.part",
        "~hds-0.part",
        "~hds-18446744073709551615.part",
    ] {
        assert!(is_part_name(yes), "{yes}");
    }
    for no in [
        "~hds-.part",
        "~hds-1.PART",
        "~HDS-1.part",
        "~hds-1.part ",
        "~hds--1.part",
        "~hds-1a.part",
        "~hds-123456789012345678901.part",
        "hds-1.part",
        "~hds-1.partx",
        "~hds-١.part",
        "",
    ] {
        assert!(!is_part_name(no), "{no}");
    }
}

#[test]
fn times_follow_the_validity_rules_of_h8() {
    assert_eq!(filetime_to_system_time(0), None);
    assert_eq!(filetime_to_system_time(1 << 63), None);
    assert_eq!(filetime_to_system_time(u64::MAX), None);
    assert_eq!(
        filetime_to_system_time(116_444_736_000_000_000),
        Some(SystemTime::UNIX_EPOCH),
        "1970-01-01 is FILETIME 116444736000000000"
    );
    assert_eq!(
        filetime_to_system_time(FT_2020 + 1),
        Some(SystemTime::UNIX_EPOCH + Duration::new(1_577_836_800, 100)),
        "100 ns resolution"
    );
    // The largest value that may be set: year 30828. It converts or is refused, but never panics.
    let _ = filetime_to_system_time((1 << 63) - 1);
    let _ = filetime_to_system_time(1);

    let tmp = TempDir::new("out-times");
    let root = OutDir::create_root(&tmp.join("dest")).unwrap();
    let file = root.create_new("times.bin").unwrap();
    let path = root.path().join("times.bin");
    let before = std::fs::metadata(&path).unwrap().modified().unwrap();

    // Nothing valid: nothing is touched, and the note says so.
    for garbage in [0u64, 1 << 63, u64::MAX, (1 << 63) + 12345] {
        let report = file.set_times(&Timestamps {
            created: garbage,
            modified: garbage,
            accessed: garbage,
        });
        assert_eq!(
            (report.created, report.modified, report.accessed),
            (
                TimeOutcome::Invalid,
                TimeOutcome::Invalid,
                TimeOutcome::Invalid
            )
        );
        assert!(report.note().unwrap().contains("creation time not set"));
    }
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        before
    );

    // One valid value among garbage is applied on its own.
    let report = file.set_times(&Timestamps {
        created: u64::MAX,
        modified: FT_2020,
        accessed: 0,
    });
    assert_eq!(
        (report.created, report.modified, report.accessed),
        (
            TimeOutcome::Invalid,
            TimeOutcome::Applied,
            TimeOutcome::Invalid
        )
    );
    let note = report.note().unwrap();
    assert!(
        note.contains("creation") && note.contains("access") && !note.contains("modification"),
        "{note}"
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        filetime_to_system_time(FT_2020).unwrap()
    );

    // A date before 1970 (a file from 1985). File systems that cannot store it refuse; none may panic.
    let ft_1985 = FT_2020 - 35 * 365 * FT_DAY;
    let report = file.set_times(&Timestamps {
        created: 0,
        modified: ft_1985,
        accessed: ft_1985,
    });
    if report.modified == TimeOutcome::Applied {
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            filetime_to_system_time(ft_1985).unwrap()
        );
    } else {
        assert_eq!(report.modified, TimeOutcome::Failed);
        assert!(report.error.is_some() && report.note().is_some());
    }
    // Year 30828: on Linux tmpfs and ext4 with small inodes cannot store it.
    let report = file.set_times(&Timestamps {
        created: 0,
        modified: (1 << 63) - 1,
        accessed: 0,
    });
    assert!(matches!(
        report.modified,
        TimeOutcome::Applied | TimeOutcome::Failed | TimeOutcome::Invalid
    ));
}

#[test]
fn directories_get_their_times_after_their_content() {
    let tmp = TempDir::new("out-dirtimes");
    let root = OutDir::create_root(&tmp.join("dest")).unwrap();
    let (dir, _) = root.subdir("Photos").unwrap();
    let mut part = part_with(&dir, "x");
    part.commit("a.jpg", OnExists::Rename, &never).unwrap();
    let times = Timestamps {
        created: FT_2020,
        modified: FT_2020 + FT_DAY,
        accessed: FT_2020 + 2 * FT_DAY,
    };
    let report = dir.set_times(&times);
    assert_eq!(report.modified, TimeOutcome::Applied, "{report:?}");
    assert_eq!(report.accessed, TimeOutcome::Applied);
    let meta = std::fs::metadata(dir.path()).unwrap();
    assert_eq!(
        meta.modified().unwrap(),
        filetime_to_system_time(times.modified).unwrap()
    );
    if cfg!(windows) && !under_wine() {
        assert_eq!(report.created, TimeOutcome::Applied);
        assert_eq!(
            meta.created().unwrap(),
            filetime_to_system_time(times.created).unwrap()
        );
    }
    // The root itself can be dated too.
    assert_eq!(root.set_times(&times).modified, TimeOutcome::Applied);
}

#[test]
fn files_for_images_and_sessions_can_be_appended_and_truncated() {
    let tmp = TempDir::new("out-files");
    let root = OutDir::create_root(&tmp.join("dest")).unwrap();
    let mut file = root.create_new("disk.img").unwrap();
    assert!(file.is_empty().unwrap());
    file.write_all(&[0xAB; 4096]).unwrap();
    file.sync().unwrap();
    assert_eq!(file.len().unwrap(), 4096);
    drop(file);

    // Resume: open without truncating, cut to the confirmed length, continue.
    let mut file = root.open_existing("disk.img").unwrap();
    assert_eq!(file.len().unwrap(), 4096);
    file.set_len(1024).unwrap();
    assert_eq!(file.seek(SeekFrom::End(0)).unwrap(), 1024);
    file.write_all(&[0xCD; 512]).unwrap();
    file.flush().unwrap();
    file.seek(SeekFrom::Start(1000)).unwrap();
    let mut back = [0u8; 48];
    file.read_exact(&mut back).unwrap();
    assert_eq!(&back[..24], &[0xAB; 24]);
    assert_eq!(&back[24..], &[0xCD; 24]);
    assert_eq!(file.len().unwrap(), 1536);
    assert_eq!(
        file.set_times(&Timestamps {
            created: 0,
            modified: FT_2020,
            accessed: 0
        })
        .modified,
        TimeOutcome::Applied
    );
    drop(file);
    assert_eq!(
        std::fs::metadata(root.path().join("disk.img"))
            .unwrap()
            .len(),
        1536
    );

    // open_existing opens nothing but existing regular files.
    assert_eq!(
        root.open_existing("missing.img").unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    root.subdir("a folder").unwrap();
    assert!(root.open_existing("a folder").is_err());
    assert_eq!(
        names(root.path()),
        vec!["a folder", "disk.img"],
        "nothing was created by a failed open"
    );
}

#[test]
fn limits_and_free_space_describe_the_destination() {
    let tmp = TempDir::new("out-limits");
    let root = OutDir::create_root(&tmp.join("dest")).unwrap();
    let limits = root.limits().unwrap();
    match limits.max_component {
        ComponentLimit::Utf16Units(n) => assert!(cfg!(windows) && (100..=1024).contains(&n), "{n}"),
        ComponentLimit::Utf8Bytes(n) => assert!(cfg!(unix) && (100..=4096).contains(&n), "{n}"),
    }
    assert!(limits.max_file_bytes.is_none() || limits.max_file_bytes == Some(u64::from(u32::MAX)));
    assert!(root.free_space().unwrap() > 0);

    // A name of exactly the limit can be created, a longer one is rejected as a name.
    let (unit, max) = match limits.max_component {
        ComponentLimit::Utf16Units(n) | ComponentLimit::Utf8Bytes(n) => ("n", n as usize),
    };
    if root.path_len_units() + max + 10 < 4000 {
        let longest = unit.repeat(max - 4) + ".bin";
        root.create_new(&longest).unwrap();
        let too_long = unit.repeat(max - 3) + ".bin";
        let err = root.create_new(&too_long).unwrap_err();
        assert_eq!(classify_error(&err), OutErrorKind::NameRejected, "{err}");
        // The sanitizer's rule 6 produces names that fit.
        let units: Vec<u16> = unit.repeat(max + 50).encode_utf16().collect();
        let fitted =
            String::from_utf16(&crate::fit_component(&units, 7, limits.max_component)).unwrap();
        root.create_new(&fitted).unwrap();
    }
}

#[test]
fn errors_are_classified_for_the_job() {
    assert_eq!(
        classify_error(&io::Error::from(io::ErrorKind::AlreadyExists)),
        OutErrorKind::Exists
    );
    assert_eq!(
        classify_error(&io::Error::from(io::ErrorKind::StorageFull)),
        OutErrorKind::NoSpace
    );
    assert_eq!(
        classify_error(&io::Error::from(io::ErrorKind::QuotaExceeded)),
        OutErrorKind::NoSpace
    );
    assert_eq!(
        classify_error(&io::Error::from(io::ErrorKind::FileTooLarge)),
        OutErrorKind::TooLarge
    );
    assert_eq!(
        classify_error(&io::Error::from(io::ErrorKind::InvalidFilename)),
        OutErrorKind::NameRejected
    );
    assert_eq!(
        classify_error(&io::Error::from(io::ErrorKind::PermissionDenied)),
        OutErrorKind::Other
    );
    assert_eq!(
        classify_error(&io::Error::other("anything")),
        OutErrorKind::Other
    );
    #[cfg(unix)]
    for (errno, kind) in [
        (libc::ENOSPC, OutErrorKind::NoSpace),
        (libc::EDQUOT, OutErrorKind::NoSpace),
        (libc::EFBIG, OutErrorKind::TooLarge),
        (libc::ENAMETOOLONG, OutErrorKind::NameRejected),
        (libc::EILSEQ, OutErrorKind::NameRejected),
        (libc::EEXIST, OutErrorKind::Exists),
        (libc::EIO, OutErrorKind::Other),
        (libc::EACCES, OutErrorKind::Other),
    ] {
        assert_eq!(
            classify_error(&io::Error::from_raw_os_error(errno)),
            kind,
            "errno {errno}"
        );
    }
    #[cfg(windows)]
    for (code, kind) in [
        (112, OutErrorKind::NoSpace),
        (39, OutErrorKind::NoSpace),
        (1295, OutErrorKind::NoSpace),
        (223, OutErrorKind::TooLarge),
        (123, OutErrorKind::NameRejected),
        (206, OutErrorKind::NameRejected),
        (80, OutErrorKind::Exists),
        (183, OutErrorKind::Exists),
        (5, OutErrorKind::Other),
        (32, OutErrorKind::Other),
    ] {
        assert_eq!(
            classify_error(&io::Error::from_raw_os_error(code)),
            kind,
            "error {code}"
        );
    }
}

#[test]
fn split_file_path_separates_directory_and_name() {
    let tmp = TempDir::new("out-split");
    let (dir, name) = split_file_path(&tmp.join("disk1.img")).unwrap();
    assert_eq!((dir.as_path(), name.as_str()), (tmp.path(), "disk1.img"));
    for bad in ["con.img", "trailing.", "a?b"] {
        let err = split_file_path(&tmp.join(bad)).unwrap_err();
        assert_eq!(classify_error(&err), OutErrorKind::NameRejected, "{bad}");
    }
    assert!(split_file_path(Path::new("just-a-name.img")).is_err());
    for no_name in [
        if cfg!(windows) { r"C:\a\.." } else { "/a/.." },
        if cfg!(windows) { r"C:\" } else { "/" },
    ] {
        assert!(split_file_path(Path::new(no_name)).is_err(), "{no_name}");
    }
    // The natural use: an image file next to its side files.
    let (dir, name) = split_file_path(&tmp.join("out").join("disk1.img")).unwrap();
    let dir = OutDir::create_root(&dir).unwrap();
    dir.create_new(&name).unwrap();
    dir.create_new(&format!("{name}.hdsimg")).unwrap();
    assert_eq!(names(dir.path()), vec!["disk1.img", "disk1.img.hdsimg"]);
}

/// S4 seen from the output side: an image that is open as the source cannot
/// be deleted, renamed, opened for writing or overwritten through these
/// primitives - not even by the `overwrite` policy when a recovered file
/// carries its name. Windows refuses with a sharing violation, Unix because
/// the identity of the file is compared with the open sources.
#[test]
fn the_open_source_image_is_out_of_reach() {
    let tmp = TempDir::new("out-source-image");
    let root = OutDir::create_root(&tmp.join("dest")).unwrap();
    let image_path = root.path().join("disk.img");
    let content: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&image_path, &content).unwrap();
    // A second name of the same file, and a file that has nothing to do with it.
    std::fs::hard_link(&image_path, root.path().join("alias.img")).unwrap();
    std::fs::write(root.path().join("other.img"), b"not the source").unwrap();

    let refused = |result: io::Result<()>, what: &str| {
        let err = result.expect_err(what);
        #[cfg(unix)]
        assert_eq!(err.kind(), io::ErrorKind::ResourceBusy, "{what}: {err}");
        #[cfg(windows)]
        assert_eq!(err.raw_os_error(), Some(32), "{what}: {err}"); // ERROR_SHARING_VIOLATION
    };

    let image = crate::ImageFile::open(&image_path, None).unwrap();
    refused(root.remove_file("disk.img"), "delete");
    refused(root.open_existing("disk.img").map(drop), "open for writing");
    refused(root.rename_noreplace("disk.img", "moved.img"), "rename");
    // The file is protected, not the name: its other name is refused as well.
    refused(root.remove_file("alias.img"), "delete by the other name");
    refused(
        root.open_existing("alias.img").map(drop),
        "open for writing by the other name",
    );
    // A recovered file called like the image, with the overwrite policy: the image
    // stays, and the recovered data is still in the part file for another name.
    let mut part = part_with(&root, "a recovered file called disk.img");
    refused(
        part.commit("disk.img", OnExists::Overwrite, &never)
            .map(drop),
        "overwrite",
    );
    assert_eq!(
        part.commit("disk (2).img", OnExists::Overwrite, &never)
            .unwrap(),
        CommitOutcome::Committed
    );
    assert_eq!(
        read(&root.path().join("disk (2).img")),
        "a recovered file called disk.img"
    );
    assert_eq!(std::fs::read(&image_path).unwrap(), content);
    assert_eq!(
        names(root.path()),
        vec!["alias.img", "disk (2).img", "disk.img", "other.img"]
    );
    // Files that are not a source are deleted and overwritten as before.
    root.remove_file("other.img").unwrap();
    let mut part = part_with(&root, "newer");
    assert_eq!(
        part.commit("disk (2).img", OnExists::Overwrite, &never)
            .unwrap(),
        CommitOutcome::Overwritten
    );

    // A second source on the same file keeps it out of reach until the last one is closed.
    let second = crate::ImageFile::open(&image_path, None).unwrap();
    drop(image);
    refused(root.remove_file("disk.img"), "delete while still a source");
    drop(second);
    root.remove_file("alias.img").unwrap();
    root.rename_noreplace("disk.img", "moved.img").unwrap();
    let mut reopened = root.open_existing("moved.img").unwrap();
    let mut back = Vec::new();
    reopened.read_to_end(&mut back).unwrap();
    assert_eq!(back, content);
    drop(reopened);
    root.remove_file("moved.img").unwrap();
    assert_eq!(names(root.path()), vec!["disk (2).img"]);
}

/// Unix: every operation is anchored at the open directory, so swapping the
/// directory for a link after it was verified redirects nothing.
#[cfg(unix)]
#[test]
fn a_directory_swapped_for_a_link_redirects_nothing() {
    let tmp = TempDir::new("out-swap");
    let outside = tmp.join("outside");
    std::fs::create_dir(&outside).unwrap();
    let root = OutDir::create_root(&tmp.join("dest")).unwrap();
    let (photos, _) = root.subdir("Photos").unwrap();
    // Someone moves the verified directory away and plants a link under its name.
    std::fs::rename(root.path().join("Photos"), tmp.join("moved-away")).unwrap();
    std::os::unix::fs::symlink(&outside, root.path().join("Photos")).unwrap();

    let mut part = part_with(&photos, "safe");
    assert_eq!(
        part.commit("a.jpg", OnExists::Rename, &never).unwrap(),
        CommitOutcome::Committed
    );
    photos.subdir("2019").unwrap();
    photos.create_new("b.txt").unwrap();
    assert!(
        names(&outside).is_empty(),
        "nothing went through the link: {:?}",
        names(&outside)
    );
    assert_eq!(
        names(&tmp.join("moved-away")),
        vec!["2019", "a.jpg", "b.txt"]
    );
    // And the planted link is refused the next time the name is asked for.
    assert_eq!(
        root.subdir("Photos").unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
}

/// Windows: a directory that is in use as an output directory cannot be
/// renamed or deleted, so it cannot be swapped for a junction.
#[cfg(windows)]
#[test]
fn a_directory_in_use_cannot_be_moved_away() {
    let tmp = TempDir::new("out-pin");
    let root = OutDir::create_root(&tmp.join("dest")).unwrap();
    let (photos, _) = root.subdir("Photos").unwrap();
    let moved = std::fs::rename(photos.path(), tmp.join("moved-away"));
    let removed = std::fs::remove_dir(photos.path());
    if !under_wine() {
        assert!(
            moved.is_err(),
            "renaming a directory that is held open must fail"
        );
        assert!(
            removed.is_err(),
            "deleting a directory that is held open must fail"
        );
    }
    if moved.is_err() {
        photos.create_new("still here.txt").unwrap();
        assert!(tmp
            .join("dest")
            .join("Photos")
            .join("still here.txt")
            .exists());
    }
    drop(photos);
    drop(root);
}

#[test]
fn a_held_file_is_neither_deleted_nor_overwritten() {
    let tmp = TempDir::new("out-held");
    let dir = OutDir::create_root(&tmp.join("d")).unwrap();
    let mut held = dir.create_new("session.hds").unwrap();
    held.write_all(b"session").unwrap();
    held.hold();
    // Overwrite by a recovered file of the same name: refused, the file stays.
    let mut part = part_with(&dir, "recovered");
    assert!(part
        .commit("session.hds", OnExists::Overwrite, &never)
        .is_err());
    part.discard().unwrap();
    assert!(dir.remove_file("session.hds").is_err());
    assert_eq!(read(&dir.path().join("session.hds")), "session");
    // Once let go of, it is an ordinary file again.
    drop(held);
    dir.remove_file("session.hds").unwrap();
}

/// Mounts a small tmpfs on `dir` (root only); false when that is not possible.
#[cfg(target_os = "linux")]
fn mount_tmpfs(dir: &Path) -> bool {
    std::process::Command::new("mount")
        .args(["-t", "tmpfs", "-o", "size=1m", "tmpfs"])
        .arg(dir)
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(target_os = "linux")]
#[test]
fn a_mount_point_below_the_root_is_never_entered() {
    let tmp = TempDir::new("out-mount");
    let root_path = tmp.join("d");
    std::fs::create_dir_all(root_path.join("mnt")).unwrap();
    if !mount_tmpfs(&root_path.join("mnt")) {
        eprintln!("SKIPPED: cannot mount a tmpfs (needs root)");
        return;
    }
    struct Unmount(std::path::PathBuf);
    impl Drop for Unmount {
        fn drop(&mut self) {
            let _ = std::process::Command::new("umount").arg(&self.0).status();
        }
    }
    let _unmount = Unmount(root_path.join("mnt"));
    std::fs::write(root_path.join("mnt").join("~hds-1.part"), b"x").unwrap();
    let root = OutDir::create_root(&root_path).unwrap();
    assert!(root.subdir("mnt").is_err(), "subdir entered a mount point");
    assert!(
        root.open_subdir("mnt").is_err(),
        "open_subdir entered a mount point"
    );
    let report = root.remove_leftover_parts(true).unwrap();
    assert_eq!(report.removed, 0);
    assert!(root_path.join("mnt").join("~hds-1.part").exists());
    // A plain directory beside it is still entered.
    assert!(root.subdir("plain").is_ok());
}
