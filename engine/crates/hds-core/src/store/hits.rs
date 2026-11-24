//! Boot sectors and file-signature hits found by the scan (ALGORITHMS
//! A3.3 and A3.4).

use hds_carve::FileKind;
use hds_ntfs::{BootSector, MAX_CLUSTER_SIZE};

/// An NTFS boot sector found at some offset of the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BootHit {
    /// Absolute disk offset of the sector.
    pub disk_offset: u64,
    /// The parsed fields; they uphold the guarantees documented at
    /// [`BootSector`] (also when loaded from a session file, which checks
    /// them).
    pub boot: BootSector,
}

/// Whether `boot` upholds the guarantees `hds_ntfs::parse_boot_sector`
/// gives (used for boot sectors read back from a session file).
pub(crate) fn boot_sector_is_sound(boot: &BootSector) -> bool {
    let bps = boot.bytes_per_sector;
    let spc = boot.sectors_per_cluster;
    let sizes_ok = |size: u32| size.is_power_of_two() && (512..=65_536).contains(&size);
    let total_clusters = boot.total_sectors.checked_div(u64::from(spc)).unwrap_or(0);
    bps.is_power_of_two()
        && (512..=4096).contains(&bps)
        && spc.is_power_of_two()
        && bps.checked_mul(spc) == Some(boot.cluster_size)
        && boot.cluster_size <= MAX_CLUSTER_SIZE
        && sizes_ok(boot.mft_record_size)
        && sizes_ok(boot.index_block_size)
        && boot.total_sectors >= 1
        && boot
            .total_sectors
            .checked_mul(u64::from(bps))
            .is_some_and(|bytes| bytes <= i64::MAX.cast_unsigned())
        && boot.mft_lcn < total_clusters
        && boot.mftmirr_lcn < total_clusters
}

/// The stable one-byte code of a file kind, as packed into a [`CarveHit`]
/// and written to session files. Never changes for an existing kind; a new
/// kind of `hds-carve` must get a new code here (the match is exhaustive).
#[must_use]
pub fn carve_kind_code(kind: FileKind) -> u8 {
    match kind {
        FileKind::Jpeg => 1,
        FileKind::Png => 2,
        FileKind::Gif => 3,
        FileKind::Bmp => 4,
        FileKind::Tiff => 5,
        FileKind::Pdf => 6,
        FileKind::Zip => 7,
        FileKind::Mp4 => 8,
        FileKind::Heic => 9,
        FileKind::Cr3 => 10,
        FileKind::Avi => 11,
        FileKind::Wav => 12,
        FileKind::Webp => 13,
        FileKind::Mkv => 14,
        FileKind::Mp3 => 15,
        FileKind::SevenZip => 16,
        FileKind::Rar => 17,
    }
}

/// The kind with the given [`carve_kind_code`]; `None` for any other value.
#[must_use]
pub fn carve_kind_from_code(code: u8) -> Option<FileKind> {
    FileKind::all()
        .iter()
        .copied()
        .find(|&kind| carve_kind_code(kind) == code)
}

/// A file-signature hit: `(offset / 512) << 8 | kind code` in one `u64`
/// (ALGORITHMS A3.4). Sorting the packed values sorts by offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CarveHit(u64);

impl CarveHit {
    /// Packs a hit. `None` when the offset is not a multiple of 512. (Every
    /// sector number of a `u64` offset is below 2^55 and fits.)
    #[must_use]
    pub fn new(disk_offset: u64, kind: FileKind) -> Option<CarveHit> {
        if !disk_offset.is_multiple_of(512) {
            return None;
        }
        let sector = disk_offset / 512;
        Some(CarveHit((sector << 8) | u64::from(carve_kind_code(kind))))
    }

    /// Unpacks a stored value; `None` when the kind code is not valid or
    /// the sector number is too large for a `u64` offset.
    #[must_use]
    pub fn from_raw(raw: u64) -> Option<CarveHit> {
        let code = raw.to_le_bytes()[0];
        if raw >> 8 >= 1 << 55 {
            return None;
        }
        carve_kind_from_code(code).map(|_| CarveHit(raw))
    }

    /// The packed value.
    #[must_use]
    pub fn raw(self) -> u64 {
        self.0
    }

    /// Absolute disk offset of the sector that starts like a file.
    #[must_use]
    pub fn disk_offset(self) -> u64 {
        // The sector number is below 2^55 (checked on construction).
        (self.0 >> 8).saturating_mul(512)
    }

    /// The kind `hds_carve::sniff` reported.
    #[must_use]
    pub fn kind(self) -> FileKind {
        // Every constructor checks the code, so the fallback is never taken.
        carve_kind_from_code(self.0.to_le_bytes()[0]).unwrap_or(FileKind::Jpeg)
    }
}
