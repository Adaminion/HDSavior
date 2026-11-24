//! NTFS boot sector (notes section 1; `layout.h` `NTFS_BOOT_SECTOR`).

use super::{put_u16, put_u32, put_u64};

/// The fields of a boot sector that matter to anybody reading the volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootParams {
    /// Bytes per sector (0x0B): 512 or 4096 on real volumes.
    pub bytes_per_sector: u32,
    /// Cluster size in bytes; a power-of-two multiple of the sector size.
    pub cluster_size: u32,
    /// Sectors in the volume (0x28): one less than the partition holds.
    pub total_sectors: u64,
    /// First cluster of `$MFT` (0x30).
    pub mft_lcn: u64,
    /// First cluster of `$MFTMirr` (0x38).
    pub mftmirr_lcn: u64,
    /// MFT record size in bytes (encoded at 0x40).
    pub record_size: u32,
    /// Index block size in bytes (encoded at 0x44).
    pub index_block_size: u32,
    /// Sector number of the partition start at format time (0x1C).
    pub hidden_sectors: u32,
    /// Volume serial number (0x48).
    pub serial: u64,
}

/// Encodes "sectors per cluster" (0x0D): the count itself up to 0x80,
/// `256 - log2(count)` above (notes section 1, "Encodings and rules").
pub fn sectors_per_cluster_byte(sectors: u32) -> u8 {
    assert!(sectors.is_power_of_two(), "sectors per cluster must be a power of two");
    if sectors <= 0x80 {
        sectors as u8
    } else {
        (256 - sectors.trailing_zeros()) as u8
    }
}

/// Encodes a record or index block size (0x40 / 0x44): a positive count of
/// clusters when the structure is at least one cluster, otherwise
/// `-log2(bytes)`.
pub fn size_byte(size: u32, cluster_size: u32) -> u8 {
    assert!(size.is_power_of_two(), "structure size must be a power of two");
    if size >= cluster_size {
        let clusters = size / cluster_size;
        assert!(clusters <= 0x7F, "structure of {clusters} clusters cannot be encoded");
        clusters as u8
    } else {
        (-(size.trailing_zeros() as i32)) as i8 as u8
    }
}

/// The 512 bytes of a boot sector. Only this first block is meaningful,
/// also on 4096-byte-sector volumes.
pub fn encode_boot_sector(p: &BootParams) -> [u8; 512] {
    assert!(p.cluster_size.is_multiple_of(p.bytes_per_sector), "cluster must be whole sectors");
    let mut b = [0u8; 512];
    b[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
    b[3..11].copy_from_slice(b"NTFS    ");
    put_u16(&mut b, 0x0B, p.bytes_per_sector as u16);
    b[0x0D] = sectors_per_cluster_byte(p.cluster_size / p.bytes_per_sector);
    b[0x15] = 0xF8; // media descriptor: hard disk
    put_u16(&mut b, 0x18, 63); // sectors per track
    put_u16(&mut b, 0x1A, 255); // heads
    put_u32(&mut b, 0x1C, p.hidden_sectors);
    b[0x24] = 0x80; // physical drive
    b[0x26] = 0x80; // extended boot signature
    put_u64(&mut b, 0x28, p.total_sectors);
    put_u64(&mut b, 0x30, p.mft_lcn);
    put_u64(&mut b, 0x38, p.mftmirr_lcn);
    b[0x40] = size_byte(p.record_size, p.cluster_size);
    b[0x44] = size_byte(p.index_block_size, p.cluster_size);
    put_u64(&mut b, 0x48, p.serial);
    // 0x54..0x1FE is boot code, which nobody but a BIOS reads. A short text
    // makes the sector easy to spot in a hex dump.
    let text = b"hds-testkit synthetic NTFS volume; not bootable";
    b[0x54..0x54 + text.len()].copy_from_slice(text);
    b[0x1FE] = 0x55;
    b[0x1FF] = 0xAA;
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sectors_per_cluster_encoding_matches_the_values_seen_on_real_volumes() {
        // notes section 1: 01 02 04 08 10 80 direct, F8 = 128 KiB at 512,
        // F5 = 1 MiB, F4 = 2 MiB at 512, F7 = 2 MiB at 4096, 20 = 128 KiB at 4096.
        assert_eq!(sectors_per_cluster_byte(1), 0x01);
        assert_eq!(sectors_per_cluster_byte(8), 0x08);
        assert_eq!(sectors_per_cluster_byte(128), 0x80);
        assert_eq!(sectors_per_cluster_byte(256), 0xF8);
        assert_eq!(sectors_per_cluster_byte(2048), 0xF5);
        assert_eq!(sectors_per_cluster_byte(4096), 0xF4);
        assert_eq!(sectors_per_cluster_byte(2 * 1024 * 1024 / 4096), 0xF7);
        assert_eq!(sectors_per_cluster_byte(128 * 1024 / 4096), 0x20);
    }

    #[test]
    fn record_and_index_size_encoding_matches_windows() {
        // notes section 1: 02/08 at 512-byte clusters, 01/04 at 1 KiB,
        // F6/01 at 4 KiB; F4 for 4096-byte records below the cluster size.
        assert_eq!(size_byte(1024, 512), 0x02);
        assert_eq!(size_byte(4096, 512), 0x08);
        assert_eq!(size_byte(1024, 1024), 0x01);
        assert_eq!(size_byte(4096, 1024), 0x04);
        assert_eq!(size_byte(1024, 4096), 0xF6);
        assert_eq!(size_byte(4096, 4096), 0x01);
        assert_eq!(size_byte(4096, 65536), 0xF4);
        assert_eq!(size_byte(1024, 2 * 1024 * 1024), 0xF6);
    }

    #[test]
    fn boot_sector_bytes_equal_the_windows_10_sample_fields() {
        // The Windows 10 sample of the notes (ntfs-fixed.vhd): 4 KiB
        // clusters, 6015 sectors, MFT at LCN 250, mirror at LCN 2, partition
        // at sector 128, serial E0A4EF9BA4EF7288.
        let b = encode_boot_sector(&BootParams {
            bytes_per_sector: 512,
            cluster_size: 4096,
            total_sectors: 6015,
            mft_lcn: 250,
            mftmirr_lcn: 2,
            record_size: 1024,
            index_block_size: 4096,
            hidden_sectors: 128,
            serial: 0xE0A4_EF9B_A4EF_7288,
        });
        assert_eq!(&b[..0x0B], b"\xEB\x52\x90NTFS    ");
        assert_eq!(&b[0x0B..0x0E], &[0x00, 0x02, 0x08]);
        assert_eq!(&b[0x0E..0x15], &[0; 7], "reserved sectors, FATs, root entries, sectors: zero");
        assert_eq!(b[0x15], 0xF8);
        assert_eq!(&b[0x1C..0x20], &128u32.to_le_bytes());
        assert_eq!(&b[0x28..0x30], &6015u64.to_le_bytes());
        assert_eq!(&b[0x30..0x38], &250u64.to_le_bytes());
        assert_eq!(&b[0x38..0x40], &2u64.to_le_bytes());
        assert_eq!(&b[0x40..0x44], &[0xF6, 0, 0, 0]);
        assert_eq!(&b[0x44..0x48], &[0x01, 0, 0, 0]);
        assert_eq!(&b[0x48..0x50], &[0x88, 0x72, 0xEF, 0xA4, 0x9B, 0xEF, 0xA4, 0xE0]);
        assert_eq!(&b[0x1FE..], &[0x55, 0xAA]);
    }
}
