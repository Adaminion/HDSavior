//! Partition tables: a classic MBR and a GPT (UEFI specification 2.x,
//! chapter 5), enough for the scenarios that put a volume somewhere else
//! than at byte 0 of the image.

use crate::disk::Disk;
use crate::hash::crc32;
use crate::manifest::PartitionFact;
use crate::rng::derive_n;

/// One partition to write: where it starts and how long it is, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Partition {
    /// Byte offset of the partition.
    pub offset: u64,
    /// Length in bytes.
    pub size: u64,
}

const NTFS_TYPE: u8 = 0x07;
/// "Microsoft basic data", `EBD0A0A2-B9E5-4433-87C0-68B6B72699C7`.
const BASIC_DATA: [u8; 16] = [
    0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7,
];

fn mbr_entry(entry: &mut [u8], kind: u8, first_sector: u64, sectors: u64) {
    entry[0] = 0x00; // not bootable
    entry[1..4].copy_from_slice(&[0xFE, 0xFF, 0xFF]); // CHS: use the LBA fields
    entry[4] = kind;
    entry[5..8].copy_from_slice(&[0xFE, 0xFF, 0xFF]);
    entry[8..12].copy_from_slice(&(first_sector.min(u32::MAX as u64) as u32).to_le_bytes());
    entry[12..16].copy_from_slice(&(sectors.min(u32::MAX as u64) as u32).to_le_bytes());
}

/// Writes an MBR with up to four primary NTFS partitions (type 0x07) into
/// sector 0 and records them in the manifest. `sector_size` is the logical
/// sector size the LBA fields count in.
pub fn write_mbr(disk: &mut Disk, sector_size: u32, partitions: &[Partition]) {
    assert!(partitions.len() <= 4, "an MBR has four primary partitions");
    let mut sector = [0u8; 512];
    sector[0x1B8..0x1BC]
        .copy_from_slice(&(derive_n(disk.seed(), 0x4D42_5200) as u32).to_le_bytes());
    for (i, p) in partitions.iter().enumerate() {
        assert!(
            p.offset.is_multiple_of(sector_size as u64)
                && p.size.is_multiple_of(sector_size as u64)
        );
        assert!(
            p.offset >= sector_size as u64 && p.offset + p.size <= disk.size(),
            "partition outside the disk"
        );
        mbr_entry(
            &mut sector[0x1BE + 16 * i..0x1CE + 16 * i],
            NTFS_TYPE,
            p.offset / sector_size as u64,
            p.size / sector_size as u64,
        );
    }
    sector[510] = 0x55;
    sector[511] = 0xAA;
    disk.write(0, &sector);
    record(disk, partitions, "mbr", |_| format!("{NTFS_TYPE:02x}"));
}

fn record(
    disk: &mut Disk,
    partitions: &[Partition],
    table: &str,
    type_id: impl Fn(usize) -> String,
) {
    let mut sorted: Vec<(usize, &Partition)> = partitions.iter().enumerate().collect();
    sorted.sort_by_key(|(_, p)| p.offset);
    for (index, (i, p)) in sorted.into_iter().enumerate() {
        disk.add_partition(PartitionFact {
            index: index as u32 + 1,
            offset: p.offset,
            size: p.size,
            table: table.to_string(),
            type_id: type_id(i),
        });
    }
}

fn guid(seed: u64, n: u64) -> [u8; 16] {
    let mut g = [0u8; 16];
    g[..8].copy_from_slice(&derive_n(seed, 2 * n).to_le_bytes());
    g[8..].copy_from_slice(&derive_n(seed, 2 * n + 1).to_le_bytes());
    g[7] = (g[7] & 0x0F) | 0x40; // version 4
    g[8] = (g[8] & 0x3F) | 0x80; // variant 1
    g
}

/// Writes a GPT: protective MBR, primary header and entry array at the
/// start, backup entry array and header at the end of the disk, all CRCs
/// valid. The partitions are "Microsoft basic data" partitions.
pub fn write_gpt(disk: &mut Disk, sector_size: u32, partitions: &[Partition]) {
    const ENTRIES: usize = 128;
    const ENTRY_SIZE: usize = 128;
    let ss = sector_size as u64;
    let total_sectors = disk.size() / ss;
    let array_sectors = ((ENTRIES * ENTRY_SIZE) as u64).div_ceil(ss);
    let first_usable = 2 + array_sectors;
    let last_usable = total_sectors - 2 - array_sectors;
    let seed = disk.seed();

    let mut array = vec![0u8; ENTRIES * ENTRY_SIZE];
    for (i, p) in partitions.iter().enumerate() {
        assert!(
            p.offset.is_multiple_of(ss) && p.size.is_multiple_of(ss),
            "partition not sector aligned"
        );
        let (first, last) = (p.offset / ss, (p.offset + p.size) / ss - 1);
        assert!(
            first >= first_usable && last <= last_usable,
            "partition {i} is outside the usable area"
        );
        let e = &mut array[i * ENTRY_SIZE..(i + 1) * ENTRY_SIZE];
        e[..16].copy_from_slice(&BASIC_DATA);
        e[16..32].copy_from_slice(&guid(seed, 100 + i as u64));
        e[32..40].copy_from_slice(&first.to_le_bytes());
        e[40..48].copy_from_slice(&last.to_le_bytes());
        for (k, unit) in "Basic data partition".encode_utf16().enumerate() {
            e[56 + 2 * k..58 + 2 * k].copy_from_slice(&unit.to_le_bytes());
        }
    }
    let array_crc = crc32(&array);
    let header = |current: u64, other: u64, array_lba: u64| -> Vec<u8> {
        let mut h = vec![0u8; sector_size as usize];
        h[..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&current.to_le_bytes());
        h[32..40].copy_from_slice(&other.to_le_bytes());
        h[40..48].copy_from_slice(&first_usable.to_le_bytes());
        h[48..56].copy_from_slice(&last_usable.to_le_bytes());
        h[56..72].copy_from_slice(&guid(seed, 99));
        h[72..80].copy_from_slice(&array_lba.to_le_bytes());
        h[80..84].copy_from_slice(&(ENTRIES as u32).to_le_bytes());
        h[84..88].copy_from_slice(&(ENTRY_SIZE as u32).to_le_bytes());
        h[88..92].copy_from_slice(&array_crc.to_le_bytes());
        let crc = crc32(&h[..92]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        h
    };

    let mut protective = [0u8; 512];
    mbr_entry(&mut protective[0x1BE..0x1CE], 0xEE, 1, total_sectors - 1);
    protective[1] = 0x00;
    protective[510] = 0x55;
    protective[511] = 0xAA;
    disk.write(0, &protective);
    disk.write(ss, &header(1, total_sectors - 1, 2));
    disk.write(2 * ss, &array);
    disk.write((last_usable + 1) * ss, &array);
    disk.write(
        (total_sectors - 1) * ss,
        &header(total_sectors - 1, 1, last_usable + 1),
    );
    record(disk, partitions, "gpt", |_| {
        "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7".to_string()
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mbr_layout() {
        let mut disk = Disk::new("t", 1, 64 << 20);
        write_mbr(
            &mut disk,
            512,
            &[
                Partition {
                    offset: 1 << 20,
                    size: 32 << 20,
                },
                Partition {
                    offset: 63 * 512 + (40 << 20),
                    size: 8 << 20,
                },
            ],
        );
        let s = disk.image().read_vec(0, 512);
        assert_eq!(&s[510..], &[0x55, 0xAA]);
        assert_eq!(s[0x1BE + 4], 0x07);
        assert_eq!(&s[0x1BE + 8..0x1BE + 12], &2048u32.to_le_bytes());
        assert_eq!(&s[0x1BE + 12..0x1BE + 16], &65536u32.to_le_bytes());
        assert_eq!(&s[0x1CE + 8..0x1CE + 12], &(63 + 81920u32).to_le_bytes());
        assert_eq!(&s[0x1DE..0x1FE], &[0u8; 32], "unused entries are zero");
        // it must not look like an NTFS boot sector
        assert_ne!(&s[3..11], b"NTFS    ");
    }

    #[test]
    fn gpt_checksums_and_backup() {
        let mut disk = Disk::new("t", 1, 64 << 20);
        write_gpt(
            &mut disk,
            512,
            &[Partition {
                offset: 16 << 20,
                size: 32 << 20,
            }],
        );
        let image = disk.image();
        let primary = image.read_vec(512, 512);
        assert_eq!(&primary[..8], b"EFI PART");
        let mut copy = primary.clone();
        copy[16..20].fill(0);
        assert_eq!(
            crc32(&copy[..92]),
            u32::from_le_bytes(primary[16..20].try_into().unwrap())
        );
        let array = image.read_vec(1024, 128 * 128);
        assert_eq!(
            crc32(&array),
            u32::from_le_bytes(primary[88..92].try_into().unwrap())
        );
        assert_eq!(&array[..16], &BASIC_DATA);
        assert_eq!(
            u64::from_le_bytes(array[32..40].try_into().unwrap()),
            (16 << 20) / 512
        );
        assert_eq!(
            u64::from_le_bytes(array[40..48].try_into().unwrap()),
            (48 << 20) / 512 - 1
        );
        // backup header in the last sector points back at the primary
        let total = (64u64 << 20) / 512;
        let backup = image.read_vec((total - 1) * 512, 512);
        assert_eq!(&backup[..8], b"EFI PART");
        assert_eq!(
            u64::from_le_bytes(backup[24..32].try_into().unwrap()),
            total - 1
        );
        assert_eq!(u64::from_le_bytes(backup[32..40].try_into().unwrap()), 1);
        assert_eq!(image.read_vec((total - 33) * 512, 128 * 128), array);
        // protective MBR
        assert_eq!(image.read_vec(0, 512)[0x1BE + 4], 0xEE);
    }
}
