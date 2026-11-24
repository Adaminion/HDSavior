//! Helpers for this crate's own tests: scratch directories and links.
//! Compiled only for tests (and included by the integration tests).

#![allow(dead_code)] // not every test binary uses every helper

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// A scratch directory that is removed when the value is dropped.
///
/// Created below `HDS_IO_TEST_DIR` if that variable is set, else below the
/// system's temporary directory.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let base = std::env::var_os("HDS_IO_TEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let unique = format!(
            "hds-io-{tag}-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        );
        let path = base.join(unique);
        std::fs::create_dir_all(&path).expect("create scratch directory");
        // Tests compare against canonical paths.
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
        // Does not follow links: a link inside the directory is removed, not its target.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// True when the tests run under Wine (the runner sets `HDS_IO_WINE=1`):
/// a few things Wine does not emulate are then not asserted.
pub fn under_wine() -> bool {
    std::env::var_os("HDS_IO_WINE").is_some()
}

fn exists_nofollow(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Creates a link `link` that leads to the directory `target`: a symbolic
/// link on Unix; on Windows a directory symbolic link or, failing that, a
/// junction. Returns false when no kind of link can be made here (the test
/// then skips what needs one).
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

/// Creates a symbolic link `link` that leads to the file `target`.
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

/// Wine 9 does not implement the calls that create links, but it shows the
/// symbolic links of the host file system. On its `Z:` drive (the host's
/// root) a link can be made by the host's `ln`.
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

// ---------------------------------------------------------------------------------------------
// Disk layouts for partition-table tests, written from the format descriptions
// (not from the parser under test)
// ---------------------------------------------------------------------------------------------

/// Bit-by-bit CRC-32, independent of the table-driven one under test.
pub fn crc32_bitwise(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

pub fn guid_bytes(text: &str) -> [u8; 16] {
    let hex: Vec<u8> = text
        .bytes()
        .filter(|b| *b != b'-')
        .collect::<Vec<u8>>()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect();
    let mut raw = [0u8; 16];
    raw.copy_from_slice(&hex);
    // The first three fields are stored little-endian.
    raw[0..4].reverse();
    raw[4..6].reverse();
    raw[6..8].reverse();
    raw
}

pub fn ntfs_boot_sector(
    bytes_per_sector: u16,
    sectors_per_cluster: u8,
    total_sectors: u64,
    serial: u64,
) -> [u8; 512] {
    let mut s = [0u8; 512];
    s[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
    s[3..11].copy_from_slice(b"NTFS    ");
    s[0x0B..0x0D].copy_from_slice(&bytes_per_sector.to_le_bytes());
    s[0x0D] = sectors_per_cluster;
    s[0x15] = 0xF8;
    s[0x28..0x30].copy_from_slice(&total_sectors.to_le_bytes());
    s[0x30..0x38].copy_from_slice(&4u64.to_le_bytes());
    s[0x38..0x40].copy_from_slice(
        &(total_sectors / 2 / u64::from(sectors_per_cluster.max(1))).to_le_bytes(),
    );
    s[0x40] = 0xF6; // 1024-byte records
    s[0x44] = 0x01; // one cluster per index block
    s[0x48..0x50].copy_from_slice(&serial.to_le_bytes());
    // Windows puts boot code and message text where an MBR has its partition table.
    for (i, byte) in s[0x54..510].iter_mut().enumerate() {
        *byte = (i as u8).wrapping_mul(37).wrapping_add(11);
    }
    s[510] = 0x55;
    s[511] = 0xAA;
    s
}

pub fn fat32_boot_sector() -> [u8; 512] {
    let mut s = [0u8; 512];
    s[0..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
    s[3..11].copy_from_slice(b"MSDOS5.0");
    s[11..13].copy_from_slice(&512u16.to_le_bytes());
    s[13] = 8;
    s[14..16].copy_from_slice(&32u16.to_le_bytes());
    s[16] = 2;
    s[21] = 0xF8;
    s[32..36].copy_from_slice(&1_000_000u32.to_le_bytes());
    s[82..90].copy_from_slice(b"FAT32   ");
    s[510] = 0x55;
    s[511] = 0xAA;
    s
}

pub fn fat16_boot_sector() -> [u8; 512] {
    let mut s = [0u8; 512];
    s[0..3].copy_from_slice(&[0xEB, 0x3C, 0x90]);
    s[3..11].copy_from_slice(b"MSDOS5.0");
    s[11..13].copy_from_slice(&512u16.to_le_bytes());
    s[13] = 4;
    s[14..16].copy_from_slice(&1u16.to_le_bytes());
    s[16] = 2;
    s[19..21].copy_from_slice(&40_000u16.to_le_bytes());
    s[21] = 0xF8;
    s[54..62].copy_from_slice(b"FAT16   ");
    s[510] = 0x55;
    s[511] = 0xAA;
    s
}

pub fn exfat_boot_sector() -> [u8; 512] {
    let mut s = [0u8; 512];
    s[0..3].copy_from_slice(&[0xEB, 0x76, 0x90]);
    s[3..11].copy_from_slice(b"EXFAT   ");
    s[64..72].copy_from_slice(&2048u64.to_le_bytes());
    s[510] = 0x55;
    s[511] = 0xAA;
    s
}

pub fn refs_boot_sector() -> [u8; 512] {
    let mut s = [0u8; 512];
    s[3..7].copy_from_slice(b"ReFS");
    s[16..20].copy_from_slice(b"FSRS");
    s[510] = 0x55;
    s[511] = 0xAA;
    s
}

pub fn bitlocker_boot_sector() -> [u8; 512] {
    let mut s = [0u8; 512];
    s[0..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
    s[3..11].copy_from_slice(b"-FVE-FS-");
    s[11..13].copy_from_slice(&512u16.to_le_bytes());
    s[510] = 0x55;
    s[511] = 0xAA;
    s
}

/// (status, type, first LBA, sector count)
pub type Entry = (u8, u8, u32, u32);

pub fn boot_record(entries: &[Entry]) -> [u8; 512] {
    let mut s = [0u8; 512];
    s[0..4].copy_from_slice(&[0x33, 0xC0, 0x8E, 0xD0]); // start of the Windows MBR code
    for (i, &(status, kind, start, sectors)) in entries.iter().enumerate() {
        let e = &mut s[446 + 16 * i..446 + 16 * (i + 1)];
        e[0] = status;
        e[1..4].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
        e[4] = kind;
        e[5..8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
        e[8..12].copy_from_slice(&start.to_le_bytes());
        e[12..16].copy_from_slice(&sectors.to_le_bytes());
    }
    s[510] = 0x55;
    s[511] = 0xAA;
    s
}

pub struct Disk {
    pub data: Vec<u8>,
    pub sector: usize,
}

impl Disk {
    pub fn new(sectors: usize, sector: usize) -> Disk {
        Disk {
            data: vec![0u8; sectors * sector],
            sector,
        }
    }

    pub fn sectors(&self) -> u64 {
        (self.data.len() / self.sector) as u64
    }

    pub fn put(&mut self, lba: u64, bytes: &[u8]) {
        let at = lba as usize * self.sector;
        self.data[at..at + bytes.len()].copy_from_slice(bytes);
    }

    /// Writes protective MBR, primary and backup GPT for `parts` (type, first, last LBA).
    pub fn put_gpt(&mut self, parts: &[(&str, u64, u64)]) {
        let last_lba = self.sectors() - 1;
        self.put(
            0,
            &boot_record(&[(0, 0xEE, 1, u32::try_from(last_lba).unwrap_or(u32::MAX))]),
        );
        let mut entries = vec![0u8; 128 * 128];
        for (i, &(kind, first, last)) in parts.iter().enumerate() {
            let e = &mut entries[i * 128..(i + 1) * 128];
            e[0..16].copy_from_slice(&guid_bytes(kind));
            e[16..32].copy_from_slice(&[0x11 + i as u8; 16]);
            e[32..40].copy_from_slice(&first.to_le_bytes());
            e[40..48].copy_from_slice(&last.to_le_bytes());
            for (j, unit) in "part".encode_utf16().enumerate() {
                e[56 + 2 * j..58 + 2 * j].copy_from_slice(&unit.to_le_bytes());
            }
        }
        let entry_sectors = (entries.len() / self.sector) as u64;
        let backup_entries_lba = last_lba - entry_sectors;
        self.put(2, &entries);
        self.put(backup_entries_lba, &entries);
        let primary = gpt_header_bytes(1, last_lba, 2, &entries, self.sector);
        let backup = gpt_header_bytes(last_lba, 1, backup_entries_lba, &entries, self.sector);
        self.put(1, &primary);
        self.put(last_lba, &backup);
    }
}

pub fn gpt_header_bytes(
    my_lba: u64,
    alternate_lba: u64,
    entries_lba: u64,
    entries: &[u8],
    sector: usize,
) -> Vec<u8> {
    let mut h = vec![0u8; sector];
    h[0..8].copy_from_slice(b"EFI PART");
    h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
    h[12..16].copy_from_slice(&92u32.to_le_bytes());
    h[24..32].copy_from_slice(&my_lba.to_le_bytes());
    h[32..40].copy_from_slice(&alternate_lba.to_le_bytes());
    h[40..48].copy_from_slice(&34u64.to_le_bytes());
    h[56..72].copy_from_slice(&[0xD1; 16]);
    h[72..80].copy_from_slice(&entries_lba.to_le_bytes());
    h[80..84].copy_from_slice(&128u32.to_le_bytes());
    h[84..88].copy_from_slice(&128u32.to_le_bytes());
    h[88..92].copy_from_slice(&crc32_bitwise(entries).to_le_bytes());
    let crc = crc32_bitwise(&h[..92]);
    h[16..20].copy_from_slice(&crc.to_le_bytes());
    h
}
