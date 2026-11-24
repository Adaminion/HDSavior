//! The reasons a structure is rejected as a whole.

use core::fmt;

/// Why a boot sector, `FILE` record or `INDX` block was rejected.
///
/// Damage *inside* a structure that still has a usable header is not an
/// error: it is reported next to the parsed data ([`FixupStatus`],
/// [`AttrChain`], [`AttrBody::Malformed`], [`RunlistStatus`], [`WalkEnd`]),
/// so that everything that can be read is read.
///
/// [`FixupStatus`]: crate::FixupStatus
/// [`AttrChain`]: crate::AttrChain
/// [`AttrBody::Malformed`]: crate::AttrBody::Malformed
/// [`RunlistStatus`]: crate::RunlistStatus
/// [`WalkEnd`]: crate::WalkEnd
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Error {
    /// The buffer is shorter than the structure (or than the part of it
    /// that had to be read to tell).
    TooShort,
    /// The signature is missing: `FILE`, `INDX`, or for a boot sector the
    /// OEM id `"NTFS    "` at 0x03 together with `55 AA` at 0x1FE.
    BadMagic,
    /// The update sequence array does not lie where it can: it must start
    /// behind the fixed header, end before the last two bytes of the first
    /// 512-byte block, and have one entry per 512-byte block plus one.
    BadUsa,
    /// The size of a `FILE` record is not a power of two in the accepted
    /// range; or the boot sector's record-size field decodes to no usable
    /// size.
    BadRecordSize,
    /// The size of an `INDX` block (0x18 + the allocated size of its node
    /// header) is not a power of two in 512..=65536; or the boot sector's
    /// index-block-size field decodes to no usable size.
    BadIndexBlockSize,
    /// The offset of the first attribute lies inside the header or the
    /// update sequence array, outside the record, or is not a multiple of 8.
    BadAttrsOffset,
    /// The used size of a `FILE` record is larger than the record or leaves
    /// no room for the first attribute.
    BadUsedSize,
    /// The node header of an `INDX` block contradicts the block: entries
    /// starting inside the header, inside the update sequence array or
    /// behind the used size, or a used size beyond the allocated size.
    BadIndexHeader,
    /// Boot sector: bytes per sector is not a power of two in 512..=4096.
    BadBytesPerSector,
    /// Boot sector: the sectors-per-cluster byte is not a power of two in
    /// 1..=0x80 and not one of the shift codes 0xF4..=0xFF.
    BadSectorsPerCluster,
    /// Boot sector: the cluster would be larger than 2 MiB.
    BadClusterSize,
    /// Boot sector: the sector count is zero or negative, or the volume
    /// would be larger than 2^63 - 1 bytes.
    BadTotalSectors,
    /// Boot sector: the `$MFT` or `$MFTMirr` cluster number is negative or
    /// outside the volume.
    BadMftLcn,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::TooShort => "buffer too short for the structure",
            Error::BadMagic => "signature missing",
            Error::BadUsa => "update sequence array out of place",
            Error::BadRecordSize => "invalid FILE record size",
            Error::BadIndexBlockSize => "invalid index block size",
            Error::BadAttrsOffset => "invalid offset of the first attribute",
            Error::BadUsedSize => "invalid used size",
            Error::BadIndexHeader => "inconsistent index node header",
            Error::BadBytesPerSector => "invalid bytes per sector",
            Error::BadSectorsPerCluster => "invalid sectors per cluster",
            Error::BadClusterSize => "cluster larger than 2 MiB",
            Error::BadTotalSectors => "invalid sector count",
            Error::BadMftLcn => "MFT location outside the volume",
        })
    }
}

impl std::error::Error for Error {}
