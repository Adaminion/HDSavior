//! Partition tables (MBR, EBR chains, GPT) and file-system sniffing.
//!
//! Everything here is pure: it reads sectors from a [`BlockSource`] and asks
//! the operating system nothing, so it works the same on a disk and on an
//! image of one. On-disk values are never trusted: all arithmetic is
//! checked, chains are bounded and nothing is allocated by a size taken from
//! the disk without a cap.

use std::ops::Range;

use serde::{Deserialize, Serialize};

use crate::buf::AlignedBuf;
use crate::source::BlockSource;

/// Most logical drives followed in one extended partition.
const MAX_EBR_LINKS: usize = 256;

/// Largest GPT partition entry array that is read (Windows writes 16 KiB).
const GPT_MAX_ENTRY_BYTES: u64 = 1 << 20;

/// Smallest size of a GPT partition entry.
const GPT_MIN_ENTRY_SIZE: u32 = 128;

/// Kind of partition table. Wire form: `"mbr"`, `"gpt"`, `"none"`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum TableKind {
    /// Master boot record, possibly with logical drives in an extended partition.
    Mbr,
    /// GUID partition table.
    Gpt,
    /// No partition table: an unpartitioned disk, a partition image, or nothing readable.
    None,
}

/// File system found in the first sector of a partition. Wire form:
/// `"ntfs"`, `"fat"`, `"exfat"`, `"refs"`, `"bitlocker"`, `"other"`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum FsKind {
    /// NTFS with a boot sector that parses.
    Ntfs,
    /// FAT12, FAT16 or FAT32.
    Fat,
    /// exFAT.
    Exfat,
    /// ReFS.
    Refs,
    /// A BitLocker-encrypted volume.
    Bitlocker,
    /// Something else that is not all zeros.
    Other,
}

/// Geometry of an NTFS volume as its boot sector states it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct NtfsSummary {
    /// Cluster size in bytes.
    pub cluster_size: u32,
    /// Sectors of the volume as recorded in the boot sector: one less than
    /// the partition has (the last sector holds the backup boot sector).
    pub total_sectors: u64,
    /// Volume serial number, 16 upper-case hexadecimal digits.
    pub serial: String,
}

/// One partition: the `Partition` object of the protocol.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PartitionInfo {
    /// 1-based position in offset order; 0 for the pseudo-partition of a
    /// source without partition table.
    pub index: u32,
    /// Absolute byte offset of the partition on the source.
    pub offset: u64,
    /// Size in bytes (clipped to the end of the source).
    pub size: u64,
    /// Human-readable partition type, e.g. `"Basic data"`; empty when unknown.
    #[serde(rename = "type")]
    pub type_name: String,
    /// MBR type byte as two hexadecimal digits, or the GPT type GUID; empty
    /// when unknown.
    pub type_id: String,
    /// File system sniffed from the first sector; `None` when that sector is
    /// unreadable or all zeros.
    pub fs: Option<FsKind>,
    /// NTFS geometry; `Some` exactly when `fs` is [`FsKind::Ntfs`] and the
    /// boot sector was read by this crate.
    pub ntfs: Option<NtfsSummary>,
    /// Drive letters (`"E:"`) of the volume mounted there; filled by
    /// [`annotate_partitions`](crate::annotate_partitions) and
    /// [`list_disks`](crate::list_disks), always empty for images.
    pub letters: Vec<String>,
    /// Label of the mounted volume, likewise.
    pub label: Option<String>,
}

impl PartitionInfo {
    /// The byte range `[offset, offset + size)` of the partition.
    pub fn range(&self) -> Range<u64> {
        self.offset..self.offset.saturating_add(self.size)
    }
}

/// Something noteworthy that happened while a partition table was read.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TableNote {
    /// Sector 0 could not be read.
    Sector0Unreadable,
    /// Sector 0 ends in `55 AA` but its four entries are not a partition table.
    MbrImplausible,
    /// The primary GPT header or its entries are damaged; the backup copy at
    /// the end of the disk was used.
    GptBackupUsed,
    /// A protective MBR announces a GPT, but neither copy of it is valid.
    GptInvalid,
    /// A valid GPT was found although sector 0 holds no protective MBR.
    ProtectiveMbrMissing,
    /// A GPT header announces more partition entries than this crate reads.
    GptTooLarge,
    /// The chain of logical drives leads back to a sector it already visited.
    EbrLoop,
    /// A link in the chain of logical drives could not be read or is no
    /// extended boot record; the drives behind it are not listed.
    EbrBroken,
    /// The chain of logical drives is longer than 256 links and was cut.
    EbrChainTooLong,
    /// A partition reaches beyond the end of the source and was clipped, or
    /// starts beyond it and was dropped.
    PartitionBeyondEnd,
}

/// Result of [`parse_partition_table`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PartitionTable {
    /// Kind of table found.
    pub table: TableKind,
    /// Partitions in offset order.
    pub partitions: Vec<PartitionInfo>,
    /// Irregularities, for the log. Not part of the protocol's `open_source`
    /// result (its clients ignore members they do not know).
    pub notes: Vec<TableNote>,
}

/// A partition as the table states it, before clipping and sniffing.
struct RawPartition {
    offset: u64,
    size: u64,
    type_name: String,
    type_id: String,
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(raw)
}

/// Reads `len` bytes at `offset` (both sector multiples); `None` when the
/// range is outside the source or cannot be read.
fn read_sectors(src: &dyn BlockSource, offset: u64, len: u64) -> Option<AlignedBuf> {
    let end = offset.checked_add(len)?;
    if len == 0 || end > src.size() {
        return None;
    }
    let mut buf = AlignedBuf::new(usize::try_from(len).ok()?);
    src.read_at(offset, &mut buf).ok()?;
    Some(buf)
}

// ---------------------------------------------------------------------------------------------
// CRC-32 (IEEE 802.3), as used by GPT
// ---------------------------------------------------------------------------------------------

const fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            bit += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC32_TABLE: [u32; 256] = crc32_table();

fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    for &byte in data {
        crc = CRC32_TABLE[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

/// CRC-32 with the polynomial of IEEE 802.3 (zlib, GPT).
pub(crate) fn crc32(data: &[u8]) -> u32 {
    !crc32_update(!0, data)
}

// ---------------------------------------------------------------------------------------------
// File-system sniffing
// ---------------------------------------------------------------------------------------------

fn has_boot_signature(sector: &[u8]) -> bool {
    sector.len() >= 512 && sector[510] == 0x55 && sector[511] == 0xAA
}

/// Size field of the NTFS boot sector for MFT records and index blocks:
/// positive = clusters, negative = 2^(-v) bytes with 9 <= -v <= 16.
fn ntfs_size_field_is_valid(raw: u8, cluster_size: u64) -> bool {
    let v = raw as i8;
    if v > 0 {
        cluster_size.checked_mul(v as u64).is_some()
    } else {
        (9..=16).contains(&-(v as i32))
    }
}

/// Parses the first 512 bytes of a volume as an NTFS boot sector, with the
/// rules of ARCHITECTURE 5.1, and returns the geometry summary.
fn ntfs_summary(sector: &[u8]) -> Option<NtfsSummary> {
    if !has_boot_signature(sector) || &sector[3..11] != b"NTFS    " {
        return None;
    }
    let bytes_per_sector = u32::from(u16_at(sector, 0x0B));
    if !bytes_per_sector.is_power_of_two() || !(512..=4096).contains(&bytes_per_sector) {
        return None;
    }
    let sectors_per_cluster: u32 = match sector[0x0D] {
        v @ 1..=0x80 if v.is_power_of_two() => u32::from(v),
        v @ 0xF4..=0xFF => 1u32 << (256 - u32::from(v)),
        _ => return None,
    };
    let cluster_size = bytes_per_sector.checked_mul(sectors_per_cluster)?;
    if cluster_size > 2 * 1024 * 1024 {
        return None;
    }
    let total_sectors = u64_at(sector, 0x28);
    if total_sectors == 0 || total_sectors > i64::MAX as u64 {
        return None;
    }
    if !ntfs_size_field_is_valid(sector[0x40], u64::from(cluster_size))
        || !ntfs_size_field_is_valid(sector[0x44], u64::from(cluster_size))
    {
        return None;
    }
    Some(NtfsSummary {
        cluster_size,
        total_sectors,
        serial: format!("{:016X}", u64_at(sector, 0x48)),
    })
}

fn is_fat_boot_sector(sector: &[u8]) -> bool {
    if !has_boot_signature(sector) {
        return false;
    }
    let jump_ok = (sector[0] == 0xEB && sector[2] == 0x90) || sector[0] == 0xE9;
    let bytes_per_sector = u32::from(u16_at(sector, 11));
    let sectors_per_cluster = sector[13];
    let reserved = u16_at(sector, 14);
    let fats = sector[16];
    let media = sector[21];
    let total = if u16_at(sector, 19) != 0 {
        u32::from(u16_at(sector, 19))
    } else {
        u32_at(sector, 32)
    };
    // The type strings are informational in the specification, but every formatter
    // writes them, and they are what tells a FAT boot sector from boot code with a
    // parameter block in front (a master boot record written by GRUB, for one).
    let named = &sector[54..57] == b"FAT" || &sector[82..87] == b"FAT32";
    jump_ok
        && bytes_per_sector.is_power_of_two()
        && (512..=4096).contains(&bytes_per_sector)
        && sectors_per_cluster.is_power_of_two()
        && reserved >= 1
        && (1..=2).contains(&fats)
        && (media == 0xF0 || media >= 0xF8)
        && total != 0
        && named
}

fn is_exfat_boot_sector(sector: &[u8]) -> bool {
    has_boot_signature(sector)
        && &sector[3..11] == b"EXFAT   "
        && sector[11..64].iter().all(|&b| b == 0)
}

fn is_refs_boot_sector(sector: &[u8]) -> bool {
    sector.len() >= 512 && &sector[3..11] == b"ReFS\0\0\0\0" && &sector[16..20] == b"FSRS"
}

fn is_bitlocker_boot_sector(sector: &[u8]) -> bool {
    sector.len() >= 512 && &sector[3..11] == b"-FVE-FS-"
}

/// Recognises the boot sector of a file system in the first sector of a
/// volume (only the first 512 bytes are looked at). Returns `None` when the
/// sector is none of NTFS, exFAT, ReFS, BitLocker or FAT; the NTFS summary is
/// present exactly for [`FsKind::Ntfs`].
pub fn sniff_boot_sector(sector: &[u8]) -> Option<(FsKind, Option<NtfsSummary>)> {
    if sector.len() < 512 {
        return None;
    }
    if let Some(summary) = ntfs_summary(sector) {
        return Some((FsKind::Ntfs, Some(summary)));
    }
    if is_exfat_boot_sector(sector) {
        return Some((FsKind::Exfat, None));
    }
    if is_refs_boot_sector(sector) {
        return Some((FsKind::Refs, None));
    }
    if is_bitlocker_boot_sector(sector) {
        return Some((FsKind::Bitlocker, None));
    }
    if is_fat_boot_sector(sector) {
        return Some((FsKind::Fat, None));
    }
    None
}

/// `fs` and `ntfs` of a partition from its first sector (`None`: unreadable).
fn sniff_partition(first_sector: Option<&[u8]>) -> (Option<FsKind>, Option<NtfsSummary>) {
    match first_sector {
        None => (None, None),
        Some(sector) if sector.iter().all(|&b| b == 0) => (None, None),
        Some(sector) => match sniff_boot_sector(sector) {
            Some((kind, summary)) => (Some(kind), summary),
            None => (Some(FsKind::Other), None),
        },
    }
}

// ---------------------------------------------------------------------------------------------
// MBR and EBR
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct MbrEntry {
    status: u8,
    kind: u8,
    start: u32,
    sectors: u32,
}

impl MbrEntry {
    fn is_used(&self) -> bool {
        self.kind != 0 && self.sectors != 0
    }

    fn is_extended(&self) -> bool {
        matches!(self.kind, 0x05 | 0x0F | 0x85)
    }
}

/// The four entries of a boot record, if the sector carries the `55 AA` signature.
fn mbr_entries(sector: &[u8]) -> Option<[MbrEntry; 4]> {
    if !has_boot_signature(sector) {
        return None;
    }
    let mut entries = [MbrEntry {
        status: 0,
        kind: 0,
        start: 0,
        sectors: 0,
    }; 4];
    for (i, entry) in entries.iter_mut().enumerate() {
        let raw = &sector[446 + 16 * i..446 + 16 * (i + 1)];
        *entry = MbrEntry {
            status: raw[0],
            kind: raw[4],
            start: u32_at(raw, 8),
            sectors: u32_at(raw, 12),
        };
    }
    Some(entries)
}

fn mbr_type_name(kind: u8) -> String {
    let name = match kind {
        0x01 => "FAT12",
        0x04 | 0x06 | 0x0E => "FAT16",
        0x05 | 0x0F | 0x85 => "Extended",
        0x07 => "NTFS / exFAT",
        0x0B | 0x0C => "FAT32",
        0x11 | 0x14 | 0x16 | 0x1E => "Hidden FAT16",
        0x12 => "OEM service",
        0x17 => "Hidden NTFS",
        0x1B | 0x1C => "Hidden FAT32",
        0x27 => "Windows recovery",
        0x42 => "Dynamic disk (LDM)",
        0x82 => "Linux swap",
        0x83 => "Linux",
        0x8E => "Linux LVM",
        0xA5 => "FreeBSD",
        0xA8 | 0xAB | 0xAF => "Apple",
        0xDE => "Dell utility",
        0xE7 => "Storage Spaces",
        0xEE => "GPT protective",
        0xEF => "EFI system",
        0xFD => "Linux RAID",
        _ => return format!("Type {kind:02X}"),
    };
    name.to_string()
}

fn mbr_partition(start_lba: u64, sectors: u64, kind: u8, sector_size: u64) -> Option<RawPartition> {
    Some(RawPartition {
        offset: start_lba.checked_mul(sector_size)?,
        size: sectors.checked_mul(sector_size)?,
        type_name: mbr_type_name(kind),
        type_id: format!("{kind:02X}"),
    })
}

/// Follows the chain of extended boot records that starts at `ext_start`
/// (LBA of the extended partition) and appends the logical drives to `out`.
fn walk_ebr_chain(
    src: &dyn BlockSource,
    sector_size: u64,
    ext_start: u64,
    out: &mut Vec<RawPartition>,
    notes: &mut Vec<TableNote>,
) {
    let mut visited: Vec<u64> = Vec::new();
    let mut ebr_lba = ext_start;
    loop {
        if visited.contains(&ebr_lba) {
            notes.push(TableNote::EbrLoop);
            return;
        }
        if visited.len() >= MAX_EBR_LINKS {
            notes.push(TableNote::EbrChainTooLong);
            return;
        }
        visited.push(ebr_lba);
        let entries = ebr_lba
            .checked_mul(sector_size)
            .and_then(|offset| read_sectors(src, offset, sector_size))
            .and_then(|sector| mbr_entries(&sector));
        let Some(entries) = entries else {
            notes.push(TableNote::EbrBroken);
            return;
        };
        // Entry 0: the logical drive, relative to this boot record.
        let drive = entries[0];
        if drive.is_used() && !drive.is_extended() {
            let start = ebr_lba.checked_add(u64::from(drive.start));
            if let Some(p) = start
                .and_then(|s| mbr_partition(s, u64::from(drive.sectors), drive.kind, sector_size))
            {
                out.push(p);
            }
        }
        // Entry 1: the next boot record, relative to the start of the extended partition.
        let link = entries[1];
        if !(link.is_used() && link.is_extended()) {
            return;
        }
        match ext_start.checked_add(u64::from(link.start)) {
            Some(next) => ebr_lba = next,
            None => {
                notes.push(TableNote::EbrBroken);
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// GPT
// ---------------------------------------------------------------------------------------------

struct GptHeader {
    alternate_lba: u64,
    entries_lba: u64,
    entry_count: u32,
    entry_size: u32,
    entries_crc: u32,
}

/// Validates a GPT header found at `lba`: signature, size, CRC and its own
/// idea of where it is.
fn gpt_header(sector: &[u8], lba: u64) -> Option<GptHeader> {
    if sector.len() < 512 || &sector[..8] != b"EFI PART" {
        return None;
    }
    let header_size = u32_at(sector, 12) as usize;
    if !(92..=sector.len()).contains(&header_size) {
        return None;
    }
    let stored_crc = u32_at(sector, 16);
    let mut crc = crc32_update(!0, &sector[..16]);
    crc = crc32_update(crc, &[0, 0, 0, 0]);
    crc = !crc32_update(crc, &sector[20..header_size]);
    if crc != stored_crc || u64_at(sector, 24) != lba {
        return None;
    }
    let entry_size = u32_at(sector, 84);
    if entry_size < GPT_MIN_ENTRY_SIZE || !entry_size.is_multiple_of(8) {
        return None;
    }
    Some(GptHeader {
        alternate_lba: u64_at(sector, 32),
        entries_lba: u64_at(sector, 72),
        entry_count: u32_at(sector, 80),
        entry_size,
        entries_crc: u32_at(sector, 88),
    })
}

/// True when `sector`, read from LBA `lba`, is a valid GPT header. Used to
/// detect the sector size of an image file.
pub(crate) fn is_gpt_header(sector: &[u8], lba: u64) -> bool {
    gpt_header(sector, lba).is_some()
}

/// Formats a GUID as stored on disk (first three fields little-endian) in the
/// usual upper-case text form.
fn guid_text(raw: &[u8]) -> String {
    format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        u32_at(raw, 0),
        u16_at(raw, 4),
        u16_at(raw, 6),
        raw[8],
        raw[9],
        raw[10],
        raw[11],
        raw[12],
        raw[13],
        raw[14],
        raw[15]
    )
}

fn gpt_type_name(guid: &str) -> &'static str {
    match guid {
        "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7" => "Basic data",
        "E3C9E316-0B5C-4DB8-817D-F92DF00215AE" => "Microsoft reserved",
        "C12A7328-F81F-11D2-BA4B-00A0C93EC93B" => "EFI system",
        "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC" => "Windows recovery",
        "5808C8AA-7E8F-42E0-85D2-E1E90434CFB3" => "Dynamic disk (LDM metadata)",
        "AF9B60A0-1431-4F62-BC68-3311714A69AD" => "Dynamic disk (LDM data)",
        "E75CAF8F-F680-4CEE-AFA3-B001E56EFC2D" => "Storage Spaces",
        "21686148-6449-6E6F-744E-656564454649" => "BIOS boot",
        "0FC63DAF-8483-4772-8E79-3D69D8477DE4" => "Linux",
        "0657FD6D-A4AB-43C4-84E5-0933C84B4F4F" => "Linux swap",
        "E6D6D379-F507-44C2-A23C-238F2A3DF928" => "Linux LVM",
        "A19D880F-05FC-4D3B-A006-743F0F84911E" => "Linux RAID",
        "48465300-0000-11AA-AA11-00306543ECAC" => "Apple HFS+",
        "7C3457EF-0000-11AA-AA11-00306543ECAC" => "Apple APFS",
        _ => "Unknown",
    }
}

/// Reads and validates the partition entry array a header points to.
fn gpt_entries(
    src: &dyn BlockSource,
    sector_size: u64,
    header: &GptHeader,
    notes: &mut Vec<TableNote>,
) -> Option<Vec<RawPartition>> {
    let entry_size = u64::from(header.entry_size);
    let total = u64::from(header.entry_count).checked_mul(entry_size)?;
    if total > GPT_MAX_ENTRY_BYTES {
        notes.push(TableNote::GptTooLarge);
        return None;
    }
    if total == 0 {
        return (header.entries_crc == crc32(&[])).then(Vec::new);
    }
    let offset = header.entries_lba.checked_mul(sector_size)?;
    let array = read_sectors(
        src,
        offset,
        total.div_ceil(sector_size).checked_mul(sector_size)?,
    )?;
    let array = &array[..total as usize];
    if crc32(array) != header.entries_crc {
        return None;
    }
    let mut partitions = Vec::new();
    for entry in array.chunks_exact(header.entry_size as usize) {
        if entry[..16].iter().all(|&b| b == 0) {
            continue;
        }
        let (first, last) = (u64_at(entry, 32), u64_at(entry, 40));
        if first > last {
            continue;
        }
        let type_id = guid_text(&entry[..16]);
        let size = (last - first)
            .checked_add(1)
            .and_then(|sectors| sectors.checked_mul(sector_size));
        if let (Some(offset), Some(size)) = (first.checked_mul(sector_size), size) {
            partitions.push(RawPartition {
                offset,
                size,
                type_name: gpt_type_name(&type_id).to_string(),
                type_id,
            });
        }
    }
    Some(partitions)
}

/// The partitions of a valid GPT: from the primary header at LBA 1, else from
/// the backup header (where the primary says it is, and in the last sector).
fn parse_gpt(
    src: &dyn BlockSource,
    sector_size: u64,
    notes: &mut Vec<TableNote>,
) -> Option<Vec<RawPartition>> {
    let primary =
        read_sectors(src, sector_size, sector_size).and_then(|sector| gpt_header(&sector, 1));
    if let Some(header) = &primary {
        if let Some(partitions) = gpt_entries(src, sector_size, header, notes) {
            return Some(partitions);
        }
    }
    let last_lba = (src.size() / sector_size).checked_sub(1)?;
    let mut candidates = Vec::with_capacity(2);
    if let Some(header) = &primary {
        candidates.push(header.alternate_lba);
    }
    if !candidates.contains(&last_lba) {
        candidates.push(last_lba);
    }
    for lba in candidates {
        // LBA 0 and 1 belong to the protective MBR and the primary header.
        if lba < 2 {
            continue;
        }
        let header = lba
            .checked_mul(sector_size)
            .and_then(|offset| read_sectors(src, offset, sector_size))
            .and_then(|sector| gpt_header(&sector, lba));
        if let Some(partitions) = header.and_then(|h| gpt_entries(src, sector_size, &h, notes)) {
            notes.push(TableNote::GptBackupUsed);
            return Some(partitions);
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// The whole table
// ---------------------------------------------------------------------------------------------

/// Clips the partitions to the source, orders them, numbers them from 1 and
/// sniffs their file systems.
fn finish(
    src: &dyn BlockSource,
    sector_size: u64,
    table: TableKind,
    mut raw: Vec<RawPartition>,
    mut notes: Vec<TableNote>,
) -> PartitionTable {
    let size = src.size();
    let mut beyond_end = false;
    raw.retain_mut(|p| {
        if p.size == 0 || p.offset >= size {
            beyond_end |= p.size != 0;
            return false;
        }
        if p.size > size - p.offset {
            p.size = size - p.offset;
            beyond_end = true;
        }
        true
    });
    if beyond_end {
        notes.push(TableNote::PartitionBeyondEnd);
    }
    raw.sort_by_key(|p| (p.offset, p.size));
    let partitions = raw
        .into_iter()
        .enumerate()
        .map(|(i, p)| {
            // A clipped partition can be shorter than a sector; then nothing is sniffed.
            let first = read_sectors(src, p.offset, sector_size.min(p.size));
            let (fs, ntfs) = sniff_partition(first.as_deref());
            PartitionInfo {
                index: u32::try_from(i + 1).unwrap_or(u32::MAX),
                offset: p.offset,
                size: p.size,
                type_name: p.type_name,
                type_id: p.type_id,
                fs,
                ntfs,
                letters: Vec::new(),
                label: None,
            }
        })
        .collect();
    notes.dedup();
    PartitionTable {
        table,
        partitions,
        notes,
    }
}

/// Reads the partition table of a source.
///
/// * A file-system boot sector in sector 0 is tested **before** the MBR
///   signature (an NTFS or FAT boot sector ends in `55 AA` too): the result is
///   table `none` with one pseudo-partition of index 0 that covers the source.
/// * MBR: the primary partitions and the logical drives of every extended
///   partition (the chain is followed for at most 256 links and never twice
///   through the same sector). The extended container itself is not listed.
/// * GPT: when the MBR is protective (type `EE`) or lists nothing. The primary
///   header and its entry array must pass their CRCs; otherwise the backup
///   header is used. LBAs are multiplied by the source's sector size, so a
///   4096-byte-sector disk needs a source that reports 4096.
/// * Every partition gets `fs` (and `ntfs`) from its first sector, is clipped
///   to the source and numbered from 1 in offset order.
///
/// The function never fails: what cannot be read or does not validate is
/// left out and mentioned in [`PartitionTable::notes`].
pub fn parse_partition_table(src: &dyn BlockSource) -> PartitionTable {
    let sector_size = u64::from(src.sector_size());
    let mut notes = Vec::new();
    // (The upper bound keeps a source with a nonsensical sector size from making
    // this function allocate by it.)
    if !(512..=65536).contains(&sector_size) || src.size() < sector_size {
        return PartitionTable {
            table: TableKind::None,
            partitions: Vec::new(),
            notes,
        };
    }
    let sector0 = read_sectors(src, 0, sector_size);
    if sector0.is_none() {
        notes.push(TableNote::Sector0Unreadable);
    }
    if let Some((kind, ntfs)) = sector0.as_deref().and_then(sniff_boot_sector) {
        let whole = PartitionInfo {
            index: 0,
            offset: 0,
            size: src.size(),
            type_name: String::new(),
            type_id: String::new(),
            fs: Some(kind),
            ntfs,
            letters: Vec::new(),
            label: None,
        };
        return PartitionTable {
            table: TableKind::None,
            partitions: vec![whole],
            notes,
        };
    }

    let entries = sector0.as_deref().and_then(mbr_entries);
    let used: Vec<MbrEntry> = entries
        .iter()
        .flatten()
        .copied()
        .filter(MbrEntry::is_used)
        .collect();
    // Boot code that happens to end in 55 AA is not a partition table.
    let plausible = used.iter().all(|e| e.status == 0x00 || e.status == 0x80);
    if !plausible {
        notes.push(TableNote::MbrImplausible);
    }
    let protective = plausible && used.iter().any(|e| e.kind == 0xEE);
    if protective || used.is_empty() || !plausible {
        if let Some(partitions) = parse_gpt(src, sector_size, &mut notes) {
            if !protective {
                notes.push(TableNote::ProtectiveMbrMissing);
            }
            return finish(src, sector_size, TableKind::Gpt, partitions, notes);
        }
        if protective {
            notes.push(TableNote::GptInvalid);
        }
    }
    if used.is_empty() || !plausible {
        notes.dedup();
        return PartitionTable {
            table: TableKind::None,
            partitions: Vec::new(),
            notes,
        };
    }

    let mut raw = Vec::new();
    for entry in &used {
        if entry.is_extended() {
            walk_ebr_chain(
                src,
                sector_size,
                u64::from(entry.start),
                &mut raw,
                &mut notes,
            );
        } else if let Some(p) = mbr_partition(
            u64::from(entry.start),
            u64::from(entry.sectors),
            entry.kind,
            sector_size,
        ) {
            raw.push(p);
        }
    }
    finish(src, sector_size, TableKind::Mbr, raw, notes)
}

#[cfg(test)]
mod tests;
