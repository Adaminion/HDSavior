//! The NTFS boot sector (docs/notes/ntfs-on-disk.md, section 1).

use crate::error::Error;

/// Number of bytes of a boot sector that carry information. Only the first
/// 512 bytes are used, also on volumes with 4096-byte sectors.
pub const BOOT_SECTOR_SIZE: usize = 512;

/// The OEM id at offset 0x03 of an NTFS boot sector.
pub const NTFS_OEM_ID: [u8; 8] = *b"NTFS    ";

/// Largest cluster NTFS supports: 2 MiB.
pub const MAX_CLUSTER_SIZE: u32 = 2 * 1024 * 1024;

/// The fields of an NTFS boot sector, decoded and validated.
///
/// A value returned by [`parse_boot_sector`] upholds these guarantees, so
/// that consumers can do the usual arithmetic without overflow:
///
/// * `bytes_per_sector` is 512, 1024, 2048 or 4096;
/// * `cluster_size == bytes_per_sector * sectors_per_cluster`, a power of
///   two of at most 2 MiB;
/// * `mft_record_size` and `index_block_size` are powers of two in
///   512..=65536;
/// * `total_sectors >= 1` and `total_sectors * bytes_per_sector < 2^63`;
/// * `mft_lcn` and `mftmirr_lcn` are below [`total_clusters`], so
///   `lcn * cluster_size` is below the volume size.
///
/// [`total_clusters`]: BootSector::total_clusters
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BootSector {
    /// Bytes per sector (u16 at 0x0B).
    pub bytes_per_sector: u32,
    /// Sectors per cluster, decoded from the byte at 0x0D.
    pub sectors_per_cluster: u32,
    /// Bytes per cluster.
    pub cluster_size: u32,
    /// Sectors of the volume (i64 at 0x28). The partition has one more: its
    /// last sector holds the backup boot sector.
    pub total_sectors: u64,
    /// "Hidden sectors" (u32 at 0x1C): the LBA of the partition start at
    /// format time, 0 when the formatter did not know it.
    pub hidden_sectors: u32,
    /// First cluster of `$MFT` (i64 at 0x30).
    pub mft_lcn: u64,
    /// First cluster of `$MFTMirr` (i64 at 0x38).
    pub mftmirr_lcn: u64,
    /// Bytes per MFT record, decoded from the signed byte at 0x40.
    pub mft_record_size: u32,
    /// Bytes per index block, decoded from the signed byte at 0x44.
    pub index_block_size: u32,
    /// Volume serial number (u64 at 0x48).
    pub serial: u64,
}

impl BootSector {
    /// Size of the volume in bytes: `total_sectors * bytes_per_sector`.
    ///
    /// This is also the distance from the start of the volume to its backup
    /// boot sector, which is the sector directly behind the volume. A boot
    /// sector found at byte `X` is therefore either the primary of a volume
    /// starting at `X` or the backup of one starting at `X - volume_size()`.
    #[inline]
    #[must_use]
    pub fn volume_size(&self) -> u64 {
        self.total_sectors
            .saturating_mul(u64::from(self.bytes_per_sector))
    }

    /// Number of whole clusters in the volume.
    #[inline]
    #[must_use]
    pub fn total_clusters(&self) -> u64 {
        self.total_sectors
            .checked_div(u64::from(self.sectors_per_cluster))
            .unwrap_or(0)
    }

    /// Byte offset of `$MFT` from the start of the volume.
    #[inline]
    #[must_use]
    pub fn mft_offset(&self) -> u64 {
        self.mft_lcn.saturating_mul(u64::from(self.cluster_size))
    }

    /// Byte offset of `$MFTMirr` from the start of the volume.
    #[inline]
    #[must_use]
    pub fn mftmirr_offset(&self) -> u64 {
        self.mftmirr_lcn
            .saturating_mul(u64::from(self.cluster_size))
    }
}

/// Whether a sector carries the two signatures of an NTFS boot sector: the
/// OEM id `"NTFS    "` at 0x03 and `55 AA` at 0x1FE. Cheap enough to ask of
/// every sector; [`parse_boot_sector`] does the validation.
#[inline]
#[must_use]
pub fn is_boot_sector_candidate(sector: &[u8]) -> bool {
    match sector.first_chunk::<BOOT_SECTOR_SIZE>() {
        Some(s) => s[0x03..0x0B] == NTFS_OEM_ID && s[0x1FE] == 0x55 && s[0x1FF] == 0xAA,
        None => false,
    }
}

/// Decodes and validates an NTFS boot sector from the first 512 bytes of
/// `sector`.
///
/// Rules (ARCHITECTURE.md 5.1):
///
/// * OEM id `"NTFS    "` at 0x03 and `55 AA` at 0x1FE, else
///   [`Error::BadMagic`];
/// * bytes per sector: a power of two in 512..=4096;
/// * sectors per cluster (u8 at 0x0D): `1..=0x80` is a sector count and
///   must be a power of two; `0xF4..=0xFF` means `2^(256 - v)` sectors;
///   everything else is invalid; the cluster is at most 2 MiB;
/// * MFT record size and index block size (i8 at 0x40 and 0x44): a positive
///   value counts clusters, a negative value `v` means `2^(-v)` bytes with
///   `9 <= -v <= 16`. Either way the result must be a power of two in
///   512..=65536 (for the positive form this bound is an addition to the
///   contract text, which leaves it open: nothing larger can be a record or
///   an index block);
/// * the sector count must be positive and the volume smaller than 2^63
///   bytes; `$MFT` and `$MFTMirr` must start inside the volume. No NTFS
///   driver mounts a volume that fails these, and they are what makes the
///   arithmetic guarantees of [`BootSector`] hold.
///
/// # Errors
///
/// [`Error::TooShort`] for fewer than 512 bytes, otherwise the variant
/// naming the first rule that failed, in the order above.
pub fn parse_boot_sector(sector: &[u8]) -> Result<BootSector, Error> {
    let s = sector
        .first_chunk::<BOOT_SECTOR_SIZE>()
        .ok_or(Error::TooShort)?;
    if !is_boot_sector_candidate(s) {
        return Err(Error::BadMagic);
    }

    let bytes_per_sector = u32::from(u16::from_le_bytes([s[0x0B], s[0x0C]]));
    if !bytes_per_sector.is_power_of_two() || !(512..=4096).contains(&bytes_per_sector) {
        return Err(Error::BadBytesPerSector);
    }

    let sectors_per_cluster =
        decode_sectors_per_cluster(s[0x0D]).ok_or(Error::BadSectorsPerCluster)?;
    let cluster_size = bytes_per_sector
        .checked_mul(sectors_per_cluster)
        .filter(|size| *size <= MAX_CLUSTER_SIZE)
        .ok_or(Error::BadClusterSize)?;

    let mft_record_size = decode_size(s[0x40], cluster_size).ok_or(Error::BadRecordSize)?;
    let index_block_size = decode_size(s[0x44], cluster_size).ok_or(Error::BadIndexBlockSize)?;

    let total_sectors = i64::from_le_bytes([
        s[0x28], s[0x29], s[0x2A], s[0x2B], s[0x2C], s[0x2D], s[0x2E], s[0x2F],
    ]);
    if total_sectors <= 0
        || total_sectors
            .checked_mul(i64::from(bytes_per_sector))
            .is_none()
    {
        return Err(Error::BadTotalSectors);
    }
    let total_sectors = total_sectors.cast_unsigned();
    let total_clusters = total_sectors
        .checked_div(u64::from(sectors_per_cluster))
        .unwrap_or(0);

    let mft_lcn = u64::from_le_bytes([
        s[0x30], s[0x31], s[0x32], s[0x33], s[0x34], s[0x35], s[0x36], s[0x37],
    ]);
    let mftmirr_lcn = u64::from_le_bytes([
        s[0x38], s[0x39], s[0x3A], s[0x3B], s[0x3C], s[0x3D], s[0x3E], s[0x3F],
    ]);
    // A negative (signed) cluster number is a huge unsigned one and fails
    // the same comparison.
    if mft_lcn >= total_clusters || mftmirr_lcn >= total_clusters {
        return Err(Error::BadMftLcn);
    }

    Ok(BootSector {
        bytes_per_sector,
        sectors_per_cluster,
        cluster_size,
        total_sectors,
        hidden_sectors: u32::from_le_bytes([s[0x1C], s[0x1D], s[0x1E], s[0x1F]]),
        mft_lcn,
        mftmirr_lcn,
        mft_record_size,
        index_block_size,
        serial: u64::from_le_bytes([
            s[0x48], s[0x49], s[0x4A], s[0x4B], s[0x4C], s[0x4D], s[0x4E], s[0x4F],
        ]),
    })
}

/// Sectors per cluster from the byte at 0x0D.
fn decode_sectors_per_cluster(raw: u8) -> Option<u32> {
    match raw {
        1..=0x80 if raw.is_power_of_two() => Some(u32::from(raw)),
        // 2^(256 - raw): the two's complement of the byte is the shift,
        // 12 for 0xF4 down to 1 for 0xFF.
        0xF4..=0xFF => 1_u32.checked_shl(u32::from(raw.wrapping_neg())),
        _ => None,
    }
}

/// MFT record size or index block size from its signed byte.
fn decode_size(raw: u8, cluster_size: u32) -> Option<u32> {
    let value = raw.cast_signed();
    if value > 0 {
        // A number of clusters; the product must still be a size that a
        // record or an index block can have.
        let size = cluster_size.checked_mul(u32::from(raw))?;
        (size.is_power_of_two() && (512..=65_536).contains(&size)).then_some(size)
    } else {
        // 2^(-value) bytes, from 2^9 to 2^16.
        let shift = u32::from(value.unsigned_abs());
        if (9..=16).contains(&shift) {
            1_u32.checked_shl(shift)
        } else {
            None
        }
    }
}
