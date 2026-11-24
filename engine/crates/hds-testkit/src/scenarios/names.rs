//! Names: namespaces, hard links, and everything a Windows path cannot
//! hold (ALGORITHMS E4, H4).

use crate::disk::Built;
use crate::rng::derive;
use crate::volume::spec::{DirSpec, FileSpec, NameSpec, Quirks};

use super::{finish_formatted, stage, MIB};

/// All four namespaces and their combinations.
///
/// **Tests ALGORITHMS E4 (Win32 or Win32+DOS, then POSIX, then DOS) and
/// A4/A5 (every `$FILE_NAME` is kept, namespace included).** Below `Test`:
///
/// * `posix-only.dat` (namespace 0), `win32-only.dat` (1), `DOSONLY.DAT`
///   (2, a file with nothing but a DOS name), `BOTH.DAT` (3);
/// * `long name with alias.dat`: DOS alias `LONGNA~1.DAT` first in the
///   record, as Windows writes it, then the Win32 name;
/// * `win32 first.dat`: the same pair in the other order;
/// * a file with a POSIX name and a Win32 name: the Win32 name wins
///   although it comes second;
/// * `Readme.txt`, `README.TXT`, `readme.txt` side by side (POSIX names
///   that differ only in case; they collide on a Windows destination).
pub fn namespaces(seed: u64) -> Built {
    let (mut disk, old, test) = stage(
        "namespaces",
        seed,
        "Files in every namespace and combination of namespaces.",
        &["E4", "A4", "H4"],
    );
    let v = disk.volume(old);
    let mut add = |name: &str, names: Vec<NameSpec>| {
        v.add_file(
            test,
            name,
            FileSpec::tagged(derive(seed, name), 3000).names(names),
        );
    };
    add("posix-only.dat", vec![NameSpec::new("posix-only.dat", 0)]);
    add("win32-only.dat", vec![NameSpec::new("win32-only.dat", 1)]);
    add("DOSONLY.DAT", vec![NameSpec::new("DOSONLY.DAT", 2)]);
    add("BOTH.DAT", vec![NameSpec::new("BOTH.DAT", 3)]);
    add(
        "long name with alias.dat",
        vec![
            NameSpec::new("LONGNA~1.DAT", 2),
            NameSpec::new("long name with alias.dat", 1),
        ],
    );
    add(
        "win32 first.dat",
        vec![
            NameSpec::new("win32 first.dat", 1),
            NameSpec::new("WIN32F~1.DAT", 2),
        ],
    );
    add(
        "win32 name of a posix file.dat",
        vec![
            NameSpec::new("posix name of the same file.dat", 0),
            NameSpec::new("win32 name of a posix file.dat", 1),
        ],
    );
    for name in ["Readme.txt", "README.TXT", "readme.txt"] {
        add(name, vec![NameSpec::new(name, 0)]);
    }
    // a folder with a DOS alias and children
    let dir = v.mkdir_with(
        test,
        "Folder with alias",
        DirSpec::new().names(vec![
            NameSpec::new("FOLDER~1", 2),
            NameSpec::new("Folder with alias", 1),
        ]),
    );
    v.add_tagged(dir, "child.dat", 1000);
    finish_formatted(disk, old)
}

/// Hard links in different folders.
///
/// **Tests ALGORITHMS E4 ("one whose parent reference resolves to a
/// surviving directory, then the lowest attribute id"; other links only in
/// `node_info`), E10 (a file is counted once).**
///
/// * `two-homes.dat` — names in `Test/A` and `Test/B`, both alive: the
///   first name (lowest attribute id) decides, the file appears once, in A.
/// * `second-home-survives.dat` — its first name points into a folder
///   whose record was destroyed and that nothing names any more; the
///   second into `Test/B`: the file belongs into B under its second name.
/// * `three-links.dat` — three names, two of them in the same folder.
/// * `root-and-folder.dat` — first name in the root directory (reference
///   5, which always resolves), second in `Test/A`.
/// * `stored first, created last.dat` in `Test/A` with a second name
///   `stored last, created first.dat` in `Test/B` — the name that comes
///   first in the record has the *higher* attribute id (the file was created
///   in B and linked into A later; Windows keeps same-type attributes sorted
///   by value, not by age). Both folders are alive: the lowest attribute id
///   decides and the file belongs into B.
pub fn hard_links(seed: u64) -> Built {
    let mut disk = crate::disk::Disk::new("hard_links", seed, 64 * MIB);
    disk.describe(
        "Files with several names in different folders.",
        &["E4", "E10"],
    );
    let old = disk.format(crate::volume::params::VolumeParams::new(0, 64 * MIB).label("old"));
    let v = disk.volume(old);
    let root = v.root();
    // record 38: destroyed by the format; its single index entry is in the
    // root's first index block, which is destroyed too
    let gone = v.mkdir(root, "Destroyed folder");
    let first = v.mkdir(root, "First things");
    while v.reference(first).number + v.children(first).len() as u64 + 1 < 256 {
        let i = v.children(first).len();
        v.add_tagged(first, format!("early-{i:03}.dat").as_str(), 64);
    }
    let test = v.mkdir(root, "Test");
    assert_eq!(v.reference(test).number, 256);
    let a = v.mkdir(test, "A");
    let b = v.mkdir(test, "B");
    let two = v.add_file(
        a,
        "two-homes.dat",
        FileSpec::tagged(derive(seed, "two"), 5000).names(vec![NameSpec::new("two-homes.dat", 1)]),
    );
    v.hard_link(two, b, "second name of two-homes.dat", 1);
    let second = v.add_file(
        gone,
        "name-in-destroyed-folder.dat",
        FileSpec::tagged(derive(seed, "second"), 6000)
            .names(vec![NameSpec::new("name-in-destroyed-folder.dat", 1)]),
    );
    v.hard_link(second, b, "second-home-survives.dat", 1);
    let three = v.add_file(
        a,
        "three-links.dat",
        FileSpec::tagged(derive(seed, "three"), 7000)
            .names(vec![NameSpec::new("three-links.dat", 1)]),
    );
    v.hard_link(three, a, "three-links (2).dat", 1);
    v.hard_link(three, b, "three-links in B.dat", 1);
    let rooted = v.add_file(
        root,
        "root-and-folder.dat",
        FileSpec::tagged(derive(seed, "root"), 8000)
            .names(vec![NameSpec::new("root-and-folder.dat", 1)]),
    );
    v.hard_link(rooted, a, "also in A.dat", 1);
    let ordered = v.add_file(
        a,
        "stored first, created last.dat",
        FileSpec::tagged(derive(seed, "ids"), 4000)
            .names(vec![NameSpec::new("stored first, created last.dat", 1)])
            .quirks(Quirks {
                reverse_name_ids: true,
                ..Quirks::default()
            }),
    );
    v.hard_link(ordered, b, "stored last, created first.dat", 1);
    finish_formatted(disk, old)
}

/// Names a Windows destination cannot take as they are.
///
/// **Tests ALGORITHMS H4 (sanitizing, in its stated order) and PROTOCOL
/// ("an unpaired UTF-16 surrogate is sent as U+FFFD").** All below
/// `Test/names`, in the POSIX namespace, which allows them; each entry's
/// `tags` say what is special:
///
/// * lone surrogates (a high one in the middle, a low one first, a high one
///   last), and valid surrogate pairs next to them;
/// * names of exactly 255 UTF-16 units: ASCII, CJK, and 127 emoji pairs
///   plus one unit (shortening must not cut a pair);
/// * every character of `<>:"/\|?*`, and control characters 0x01, 0x09,
///   0x1F;
/// * trailing dots and spaces, names made only of dots or spaces;
/// * reserved device names: `CON`, `nul.txt`, `COM1`, `LPT9.tar.gz`,
///   `AUX`, `CONIN$`, `CONOUT$`, `COM¹`, `con .txt` (trailing space before
///   the dot), and `CONSOLE.txt` and `COM10` which are *not* reserved;
/// * names the engine uses itself: `HDSavior-report-20260101-000000.csv`,
///   `~hds-17.part`;
/// * a folder called `PRN` with a file in it, and a folder whose name ends
///   in a dot.
pub fn odd_names(seed: u64) -> Built {
    let (mut disk, old, test) = stage(
        "odd_names",
        seed,
        "Names with lone surrogates, 255 units, illegal characters and reserved words.",
        &["H4"],
    );
    let v = disk.volume(old);
    let names = v.mkdir(test, "names");
    let u = |s: &str| -> Vec<u16> { s.encode_utf16().collect() };
    let mut cases: Vec<(Vec<u16>, &str)> = Vec::new();
    // surrogates
    let mut lone_middle = u("lone-");
    lone_middle.push(0xD83D);
    lone_middle.extend(u("-high.dat"));
    cases.push((lone_middle, "lone-surrogate"));
    let mut lone_first = vec![0xDE00];
    lone_first.extend(u("low-first.dat"));
    cases.push((lone_first, "lone-surrogate"));
    let mut lone_last = u("high-last");
    lone_last.push(0xD800);
    cases.push((lone_last, "lone-surrogate"));
    cases.push((u("pair-\u{1F600}-ok.dat"), "surrogate-pair"));
    // 255 units
    cases.push((u(&format!("{}.dat", "a".repeat(251))), "255-units"));
    cases.push((u(&"\u{6F22}".repeat(255)), "255-units"));
    let mut emoji = u(&"\u{1F4BE}".repeat(127));
    emoji.push(b'x' as u16);
    cases.push((emoji, "255-units"));
    // illegal characters
    for c in ['<', '>', ':', '"', '/', '\\', '|', '?', '*'] {
        cases.push((u(&format!("illegal {c} char.dat")), "illegal-char"));
    }
    cases.push((u("all<>:\"/\\|?*.dat"), "illegal-char"));
    for c in [0x01u16, 0x09, 0x1F] {
        let mut name = u("control-");
        name.push(c);
        name.extend(u(".dat"));
        cases.push((name, "control-char"));
    }
    // dots and spaces
    for name in [
        "trailing dot.",
        "trailing space ",
        "trailing both . .",
        "...",
        "   ",
        " leading space.dat",
        ".leading dot",
    ] {
        cases.push((u(name), "dots-and-spaces"));
    }
    // device names
    for name in [
        "CON",
        "nul.txt",
        "COM1",
        "LPT9.tar.gz",
        "AUX",
        "CONIN$",
        "CONOUT$",
        "COM\u{B9}",
        "con .txt",
        "Lpt\u{B2}.dat",
    ] {
        cases.push((u(name), "reserved-name"));
    }
    for name in ["CONSOLE.txt", "COM10", "AUXILIARY", "nul"] {
        cases.push((
            u(name),
            if name == "nul" {
                "reserved-name"
            } else {
                "not-reserved"
            },
        ));
    }
    // names of the engine's own files
    cases.push((u("HDSavior-report-20260101-000000.csv"), "engine-name"));
    cases.push((u("~hds-17.part"), "engine-name"));
    cases.push((u("~hds-.part.txt"), "not-reserved"));
    for (i, (name, tag)) in cases.into_iter().enumerate() {
        let node = v.add_file(
            names,
            name,
            FileSpec::tagged(derive(seed, "odd") + i as u64, 100 + 10 * i as u64),
        );
        v.tag(node, tag);
    }
    let prn = v.mkdir(names, "PRN");
    v.tag(prn, "reserved-name");
    v.add_tagged(prn, "inside a device name.dat", 500);
    let dotted = v.mkdir(names, "folder ending in a dot.");
    v.tag(dotted, "dots-and-spaces");
    v.add_tagged(dotted, "inside.dat", 500);
    finish_formatted(disk, old)
}
