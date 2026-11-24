//! Helpers for this crate's unit tests.

#![allow(dead_code)] // not every test module uses every helper

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// A byte that depends on its position (never 0), so that data from the
/// wrong place, or zeros filled in, are caught.
pub fn byte_at(offset: u64) -> u8 {
    let x = offset.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (x >> 56) as u8 | 1
}

/// `len` patterned bytes.
pub fn pattern(len: usize) -> Vec<u8> {
    (0..len as u64).map(byte_at).collect()
}

/// UTF-16 units of `text`.
pub fn utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// A `FILE` record with a resident unnamed `$DATA` holding `value`,
/// protected with update sequence number `usn` as on disk. Written from the
/// layout in docs/notes/ntfs-on-disk.md, not with the parser under test.
/// Returns the record and the offset of the value inside it.
pub fn file_record(record_size: usize, value: &[u8], usn: u16) -> (Vec<u8>, u32) {
    let mut r = vec![0u8; record_size];
    let blocks = record_size / 512;
    r[0..4].copy_from_slice(b"FILE");
    r[0x04..0x06].copy_from_slice(&0x30u16.to_le_bytes());
    r[0x06..0x08].copy_from_slice(&((blocks + 1) as u16).to_le_bytes());
    r[0x10..0x12].copy_from_slice(&1u16.to_le_bytes());
    r[0x12..0x14].copy_from_slice(&1u16.to_le_bytes());
    let attrs = (0x30 + 2 * (blocks + 1) + 7) & !7;
    r[0x14..0x16].copy_from_slice(&(attrs as u16).to_le_bytes());
    r[0x16..0x18].copy_from_slice(&1u16.to_le_bytes());
    r[0x1C..0x20].copy_from_slice(&(record_size as u32).to_le_bytes());
    r[0x2C..0x30].copy_from_slice(&77u32.to_le_bytes());
    let a = attrs;
    let attr_len = (0x18 + value.len() + 7) & !7;
    r[a..a + 4].copy_from_slice(&0x80u32.to_le_bytes());
    r[a + 4..a + 8].copy_from_slice(&(attr_len as u32).to_le_bytes());
    r[a + 0x0A..a + 0x0C].copy_from_slice(&0x18u16.to_le_bytes());
    r[a + 0x10..a + 0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
    r[a + 0x14..a + 0x16].copy_from_slice(&0x18u16.to_le_bytes());
    r[a + 0x18..a + 0x18 + value.len()].copy_from_slice(value);
    let end = a + attr_len;
    r[end..end + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    r[0x18..0x1C].copy_from_slice(&((end + 8) as u32).to_le_bytes());
    r[0x30..0x32].copy_from_slice(&usn.to_le_bytes());
    for block in 0..blocks {
        let tail = block * 512 + 510;
        let saved = [r[tail], r[tail + 1]];
        r[0x32 + 2 * block..0x34 + 2 * block].copy_from_slice(&saved);
        r[tail..tail + 2].copy_from_slice(&usn.to_le_bytes());
    }
    (r, (a + 0x18) as u32)
}

/// A scratch directory that is removed when the value is dropped. Created
/// below `HDS_RECOVER_TEST_DIR` (else `HDS_IO_TEST_DIR`, else the system's
/// temporary directory); under Wine, point it at a `Z:` path so that links
/// can be made.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        let base = std::env::var_os("HDS_RECOVER_TEST_DIR")
            .or_else(|| std::env::var_os("HDS_IO_TEST_DIR"))
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        TempDir::below(&base, tag)
    }

    pub fn below(base: &Path, tag: &str) -> TempDir {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = format!(
            "hds-recover-{tag}-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        );
        let path = base.join(unique);
        std::fs::create_dir_all(&path).expect("create scratch directory");
        let path = std::fs::canonicalize(&path).expect("canonical scratch directory");
        TempDir { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// True when the tests run under Wine (`HDS_IO_WINE` is set).
pub fn under_wine() -> bool {
    std::env::var_os("HDS_IO_WINE").is_some()
}

fn exists_nofollow(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// A link `link` to the directory `target` (a symbolic link on Unix; a
/// directory symbolic link or junction on Windows). False when no link can
/// be made here.
pub fn make_dir_link(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(windows)]
    {
        let _ = std::os::windows::fs::symlink_dir(target, link);
        if exists_nofollow(link) {
            return true;
        }
        let _ = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output();
        if exists_nofollow(link) {
            return true;
        }
        wine_unix_symlink(target, link)
    }
}

/// A symbolic link `link` to the file `target`.
pub fn make_file_link(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(windows)]
    {
        let _ = std::os::windows::fs::symlink_file(target, link);
        if exists_nofollow(link) {
            return true;
        }
        wine_unix_symlink(target, link)
    }
}

/// Wine cannot create links, but shows those of the host: on its `Z:` drive
/// the host's `ln` makes one.
#[cfg(windows)]
fn wine_unix_symlink(target: &Path, link: &Path) -> bool {
    fn unix_path(path: &Path) -> Option<String> {
        let text = path.to_str()?;
        let text = text.strip_prefix(r"\\?\").unwrap_or(text);
        let rest = text
            .strip_prefix("Z:")
            .or_else(|| text.strip_prefix("z:"))?;
        Some(rest.replace('\\', "/"))
    }
    if !under_wine() {
        return false;
    }
    let (Some(target), Some(link_unix)) = (unix_path(target), unix_path(link)) else {
        return false;
    };
    let _ = std::process::Command::new("cmd")
        .args([
            "/c", "start", "/wait", "/unix", "/bin/ln", "-s", &target, &link_unix,
        ])
        .output();
    exists_nofollow(link)
}

/// Names of the entries of `dir`, sorted.
pub fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Every path below `dir` (relative, `/`-separated), sorted; links are not
/// followed.
pub fn tree(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let rel = path
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                out.push(format!("{rel}/"));
                walk(base, &path, out);
            } else {
                out.push(rel);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}
