//! The safety invariants S1 and S6 as properties of this crate's source text.
//!
//! A reviewer must be able to verify the invariants by reading the code;
//! this test keeps the code in the shape that makes that possible. It fails
//! when the crate gains write access to devices, a control code outside the
//! one wrapper, a second place that opens devices, or code outside
//! `hds_io::out` that creates, writes, renames or deletes files.
//!
//! The rules look at code only: comments and the contents of string
//! literals are removed first, so documentation may name what the code must
//! not use. Files that are compiled for tests only (`tests.rs`,
//! `test_util.rs`) are exempt, and the test checks that they really are
//! test-only.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Removes comments and the contents of string and character literals.
fn strip(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '/' if next == Some('/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if next == Some('*') => {
                let mut depth = 1;
                i += 2;
                while i < chars.len() && depth > 0 {
                    if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                        depth += 1;
                        i += 2;
                    } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        if chars[i] == '\n' {
                            out.push('\n');
                        }
                        i += 1;
                    }
                }
            }
            'r' if matches!(next, Some('"' | '#')) && !prev_is_ident(&chars, i) => {
                // Raw string: r"..." or r#"..."# with any number of hashes.
                let mut j = i + 1;
                let mut hashes = 0;
                while chars.get(j) == Some(&'#') {
                    hashes += 1;
                    j += 1;
                }
                if chars.get(j) != Some(&'"') {
                    out.push(c);
                    i += 1;
                    continue;
                }
                j += 1;
                loop {
                    match chars.get(j) {
                        None => break,
                        Some('"') if (1..=hashes).all(|k| chars.get(j + k) == Some(&'#')) => {
                            j += 1 + hashes;
                            break;
                        }
                        Some('\n') => {
                            out.push('\n');
                            j += 1;
                        }
                        Some(_) => j += 1,
                    }
                }
                out.push_str("\"\"");
                i = j;
            }
            '"' => {
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' {
                        i += 1;
                    }
                    if chars.get(i) == Some(&'\n') {
                        out.push('\n');
                    }
                    i += 1;
                }
                i += 1;
                out.push_str("\"\"");
            }
            '\'' => {
                // A character literal ('x', '\n', '\'') or a lifetime ('a).
                if next == Some('\\') {
                    i += 2;
                    while i < chars.len() && chars[i] != '\'' {
                        i += 1;
                    }
                    i += 1;
                    out.push_str("' '");
                } else if chars.get(i + 2) == Some(&'\'') {
                    i += 3;
                    out.push_str("' '");
                } else {
                    out.push(c);
                    i += 1;
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn prev_is_ident(chars: &[char], i: usize) -> bool {
    i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_')
}

#[test]
fn the_stripper_removes_comments_and_literal_contents() {
    let source = r##"
        let a = "GENERIC_WRITE // not a comment"; // DeviceIoControl here
        /* block FSCTL_LOCK_VOLUME /* nested */ still */ let b = r"\\.\PhysicalDrive0";
        let c = r#"quote " inside"#; let d = '"'; let e = '\''; fn f<'a>(x: &'a str) {}
        let real = GENERIC_READ;
    "##;
    let code = strip(source);
    for gone in [
        "GENERIC_WRITE",
        "DeviceIoControl",
        "FSCTL",
        "PhysicalDrive",
        "quote",
        "nested",
    ] {
        assert!(!code.contains(gone), "{gone} survived: {code}");
    }
    for kept in [
        "let a = \"\";",
        "let b = \"\";",
        "let c = \"\";",
        "fn f<'a>(x: &'a str) {}",
        "let real = GENERIC_READ;",
    ] {
        assert!(code.contains(kept), "{kept} is missing: {code}");
    }
    assert_eq!(
        code.lines().count(),
        source.lines().count(),
        "line numbers are preserved"
    );
}

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// Path relative to the crate, with forward slashes.
fn relative(path: &Path) -> String {
    path.strip_prefix(crate_dir())
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
}

fn is_test_only(relative_path: &str) -> bool {
    relative_path.ends_with("/tests.rs") || relative_path == "src/test_util.rs"
}

/// Stripped code of every file of `dir` that goes into the library (or of
/// the examples), keyed by relative path.
fn production_code(dir: &str) -> BTreeMap<String, String> {
    let mut files = Vec::new();
    rust_files(&crate_dir().join(dir), &mut files);
    files
        .iter()
        .map(|path| (relative(path), path))
        .filter(|(name, _)| !is_test_only(name))
        .map(|(name, path)| (name, strip(&std::fs::read_to_string(path).unwrap())))
        .collect()
}

/// Occurrences of `token` as a whole identifier-like word.
fn count_word(code: &str, token: &str) -> usize {
    code.match_indices(token)
        .filter(|(at, _)| {
            let before = code[..*at].chars().next_back();
            let after = code[at + token.len()..].chars().next();
            let is_ident = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
            !is_ident(before) && !is_ident(after)
        })
        .count()
}

/// `text` without any white space: what is compared when an expectation spans
/// several tokens, so that it does not depend on how the code is laid out.
fn compact(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// True when `code` contains `expected`, white space aside.
fn has(code: &str, expected: &str) -> bool {
    compact(code).contains(&compact(expected))
}

fn files_with(code: &BTreeMap<String, String>, token: &str) -> Vec<String> {
    code.iter()
        .filter(|(_, text)| text.contains(token))
        .map(|(name, _)| name.clone())
        .collect()
}

#[test]
fn test_only_files_are_compiled_for_tests_only() {
    let mut files = Vec::new();
    rust_files(&crate_dir().join("src"), &mut files);
    let mut declarations = 0;
    for path in &files {
        let text = std::fs::read_to_string(path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let line = line.trim();
            if line == "mod tests;" || line == "mod test_util;" {
                declarations += 1;
                let previous = lines[..i]
                    .iter()
                    .rev()
                    .map(|l| l.trim())
                    .find(|l| !l.is_empty());
                assert_eq!(
                    previous,
                    Some("#[cfg(test)]"),
                    "{}: `{line}` must be test-only",
                    relative(path)
                );
            }
        }
    }
    let exempt = files.iter().filter(|p| is_test_only(&relative(p))).count();
    assert_eq!(
        declarations, exempt,
        "every exempt file is declared exactly once, as a test-only module"
    );
    assert!(exempt >= 10, "the test modules were found");
}

#[test]
fn s1_no_write_access_to_devices_and_no_state_changing_calls() {
    // Access rights, flags and functions that have no business in a crate that
    // only ever reads devices and only writes files through std.
    const FORBIDDEN: &[&str] = &[
        "GENERIC_WRITE",
        "FILE_GENERIC_WRITE",
        "GENERIC_ALL",
        "FILE_ALL_ACCESS",
        "MAXIMUM_ALLOWED",
        "FILE_WRITE_DATA",
        "FILE_APPEND_DATA",
        "FILE_ADD_FILE",
        "FILE_FLAG_OVERLAPPED",
        "FILE_FLAG_DELETE_ON_CLOSE",
        "FILE_FLAG_NO_BUFFERING",
        "CreateFileW",
        "CreateFileA",
        "CreateFile2",
        "NtCreateFile",
        "NtOpenFile",
        "WriteFile",
        "FSCTL_",
        "IOCTL_DISK_SET",
        "IOCTL_DISK_DELETE",
        "IOCTL_DISK_CREATE",
        "IOCTL_DISK_UPDATE",
        "IOCTL_DISK_GROW",
        "IOCTL_VOLUME_OFFLINE",
        "IOCTL_VOLUME_ONLINE",
        "IOCTL_STORAGE_SET",
        "IOCTL_STORAGE_EJECT",
        "IOCTL_STORAGE_MEDIA_REMOVAL",
        "ReplaceFile",
        "CopyFile",
        "SetFileAttributes",
        "CreateSymbolicLink",
        "CreateHardLink",
        "SetVolumeMountPoint",
        "DeleteVolumeMountPoint",
        "DefineDosDevice",
    ];
    for dir in ["src", "examples"] {
        let code = production_code(dir);
        assert!(!code.is_empty());
        for token in FORBIDDEN {
            assert_eq!(
                files_with(&code, token),
                Vec::<String>::new(),
                "`{token}` must not occur in {dir}/"
            );
        }
    }
}

#[test]
fn s1_control_codes_go_through_one_wrapper_with_a_closed_list() {
    const ALLOWED: [&str; 6] = [
        "IOCTL_DISK_GET_LENGTH_INFO",
        "IOCTL_DISK_GET_DRIVE_GEOMETRY_EX",
        "IOCTL_STORAGE_QUERY_PROPERTY",
        "IOCTL_STORAGE_GET_DEVICE_NUMBER",
        "IOCTL_STORAGE_CHECK_VERIFY2",
        "IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS",
    ];
    let code = production_code("src");
    // The system call exists once: imported and called in the wrapper.
    assert_eq!(files_with(&code, "DeviceIoControl"), ["src/win/device.rs"]);
    let device = &code["src/win/device.rs"];
    assert_eq!(
        count_word(device, "DeviceIoControl"),
        2,
        "one import and one call"
    );
    assert_eq!(device.matches("fn control(").count(), 1);
    assert!(
        has(device, "if !ALLOWED_CONTROL_CODES.contains(&code)"),
        "the wrapper checks the list first"
    );

    // Every control code named anywhere is on the list, and is named in that file only.
    for (name, text) in &code {
        let mut rest = text.as_str();
        while let Some(at) = rest.find("IOCTL_") {
            let word: String = rest[at..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            assert!(
                ALLOWED.contains(&word.as_str()),
                "{name}: control code {word} is not on the closed list"
            );
            assert_eq!(
                name, "src/win/device.rs",
                "{word} is used outside the wrapper's file"
            );
            rest = &rest[at + word.len()..];
        }
    }
    for allowed in ALLOWED {
        assert!(
            count_word(device, allowed) >= 2,
            "{allowed} is on the list and used"
        );
    }
    // The list itself holds exactly the six.
    let list_start = device
        .find("const ALLOWED_CONTROL_CODES: [u32; 6] = [")
        .expect("the closed list");
    let list = &device[list_start..list_start + device[list_start..].find("];").unwrap()];
    assert_eq!(list.matches("IOCTL_").count(), 6);
    // Nothing in the examples talks to devices directly.
    let examples = production_code("examples");
    for token in ["DeviceIoControl", "IOCTL_", "windows_sys"] {
        assert_eq!(
            files_with(&examples, token),
            Vec::<String>::new(),
            "`{token}` in examples/"
        );
    }
}

/// The arguments of every `.access_mode(...)` call in `text`.
fn access_modes(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(".access_mode(") {
        let args = &rest[at + ".access_mode(".len()..];
        let end = args.find(')').expect("closing parenthesis");
        found.push(args[..end].split_whitespace().collect::<Vec<_>>().join(" "));
        rest = &args[end..];
    }
    found
}

#[test]
fn s1_devices_are_opened_by_one_function_for_query_or_read() {
    let code = production_code("src");
    // Read access to devices is named in one file ...
    assert_eq!(files_with(&code, "GENERIC_READ"), ["src/win/device.rs"]);
    let device = &code["src/win/device.rs"];
    // ... which opens exactly once, with one of two access levels and full sharing.
    assert_eq!(
        device.matches(".open(").count(),
        1,
        "open_device is the only open in the device module"
    );
    assert_eq!(access_modes(device), ["desired_access"]);
    assert!(has(device, "Access::Query => 0,"));
    assert!(has(device, "Access::Read => GENERIC_READ,"));
    assert!(has(
        device,
        ".access_mode(desired_access).share_mode(SHARE_ALL).open(path)"
    ));
    assert!(
        !device.contains("custom_flags"),
        "no flags on device handles"
    );

    // Every other explicit access mask in the crate, by file. Zero is "query".
    let mut masks: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (name, text) in &code {
        let found = access_modes(text);
        if !found.is_empty() && name != "src/win/device.rs" {
            masks.insert(name.as_str(), found);
        }
    }
    let expected: BTreeMap<&str, Vec<String>> = [
        // Directories of the output tree: list + change times; fallbacks without
        // rights; and a handle without rights that compares a name with the open
        // source images before it is deleted.
        (
            "src/out/windows.rs",
            vec![
                "FILE_LIST_DIRECTORY | FILE_WRITE_ATTRIBUTES".to_string(),
                "0".to_string(),
                "0".to_string(),
                "0".to_string(),
            ],
        ),
        // The deepest existing ancestor of a destination.
        ("src/win/volumes.rs", vec!["0".to_string()]),
    ]
    .into_iter()
    .collect();
    assert_eq!(masks, expected);

    // A device path is never handed to anything but open_device: the only places
    // that build one call it (or only compare strings).
    for (name, text) in &code {
        if text.contains("physical_drive_path(") && !name.ends_with("winpath.rs") {
            assert!(
                text.contains("open_device(") || name == "src/source.rs",
                "{name} builds a disk path without going through open_device"
            );
        }
    }
    // The image file source: read-only, and nobody else may write (S4).
    let image = &code["src/image.rs"];
    assert!(has(image, ".read(true).share_mode(FILE_SHARE_READ).open("));
    assert_eq!(image.matches("share_mode(").count(), 1);
}

#[test]
fn s1_the_block_source_trait_cannot_write() {
    let code = production_code("src");
    let source = &code["src/source.rs"];
    let start = source.find("pub trait BlockSource").expect("the trait");
    let body = &source[start..start + source[start..].find("\n}\n").expect("end of the trait")];
    let methods: Vec<&str> = body
        .split("fn ")
        .skip(1)
        .map(|rest| rest.split('(').next().unwrap())
        .collect();
    assert_eq!(
        methods,
        [
            "size",
            "sector_size",
            "physical_sector_size",
            "read_at",
            "info",
            "is_alive",
            "location"
        ]
    );
    assert!(!body.contains("&mut self"));
    assert_eq!(
        body.matches("&mut [u8]").count(),
        1,
        "the only buffer the trait takes is the one it fills"
    );
}

#[test]
fn s6_only_the_out_module_changes_the_file_system() {
    // Everything through which a file or directory can be created, written,
    // renamed, truncated, linked or deleted.
    const WRITERS: &[&str] = &[
        ".write(true)",
        ".append(true)",
        ".create(true)",
        ".create_new(true)",
        "File::create",
        "File::options",
        "fs::write",
        "fs::rename",
        "fs::remove_file",
        "fs::remove_dir",
        "fs::create_dir",
        "fs::copy",
        "fs::hard_link",
        "fs::set_permissions",
        "symlink",
        "set_len(",
        "set_times(",
        "set_modified(",
        "MoveFileExW",
        "DeleteFileW",
        "RemoveDirectoryW",
        "CreateDirectoryW",
        "libc::open(",
        "libc::openat(",
        "libc::mkdirat(",
        "libc::mkdir(",
        "libc::unlinkat(",
        "libc::unlink(",
        "libc::rename",
        "libc::linkat(",
        "libc::link(",
        "libc::ftruncate",
        "libc::truncate",
        "libc::write",
        "SYS_renameat2",
        "O_WRONLY",
        "O_RDWR",
        "O_CREAT",
        "O_APPEND",
    ];
    const OUT_MODULE: [&str; 3] = ["src/out.rs", "src/out/unix.rs", "src/out/windows.rs"];
    let code = production_code("src");
    for token in WRITERS {
        for file in files_with(&code, token) {
            assert!(
                OUT_MODULE.contains(&file.as_str()),
                "`{token}` occurs in {file}, outside hds_io::out"
            );
        }
    }
    // And even there: nothing that truncates on open, replaces on rename, creates
    // several levels at once or deletes a tree (ALGORITHMS H6).
    const NEVER: &[&str] = &[
        "create_dir_all",
        "remove_dir_all",
        ".truncate(true)",
        "File::create",
        "fs::write",
        "fs::rename",
        "fs::copy",
        "O_TRUNC",
        "MOVEFILE_",
        "RENAME_EXCHANGE",
        "libc::rename(",
        "libc::renameat(",
    ];
    for token in NEVER {
        assert_eq!(
            files_with(&code, token),
            Vec::<String>::new(),
            "`{token}` must not occur in src/"
        );
    }
    // The primitives are what the module documentation says they are.
    let unix = &code["src/out/unix.rs"];
    assert!(
        has(unix, "libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW"),
        "create-new, no-follow"
    );
    assert!(unix.contains("libc::RENAME_NOREPLACE"));
    assert_eq!(
        unix.matches("O_NOFOLLOW").count(),
        3,
        "every open below the root refuses links"
    );
    let windows = &code["src/out/windows.rs"];
    assert!(windows.contains(".create_new(true)"));
    assert!(
        has(windows, "MoveFileExW(from.as_ptr(), to.as_ptr(), 0)"),
        "rename without any flag"
    );
    assert_eq!(
        windows.matches(".write(true)").count(),
        2,
        "create_new and open_existing"
    );
}

#[test]
fn unsafe_code_stays_in_the_platform_modules() {
    const MAY_BE_UNSAFE: [&str; 8] = [
        "src/buf.rs",
        "src/elevation.rs",
        "src/guard/unix.rs",
        "src/out/unix.rs",
        "src/out/windows.rs",
        "src/win/device.rs",
        "src/win/mod.rs",
        "src/win/volumes.rs",
    ];
    let code = production_code("src");
    for (name, text) in &code {
        if count_word(text, "unsafe") > 0 {
            assert!(
                MAY_BE_UNSAFE.contains(&name.as_str()),
                "{name} contains unsafe code"
            );
        }
    }
    assert_eq!(
        files_with(&production_code("examples"), "unsafe"),
        Vec::<String>::new()
    );
}
