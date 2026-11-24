//! Parameters of a simulated volume.

use serde::{Deserialize, Serialize};

use crate::ondisk::record::BlankForm;
use crate::time;

/// Which formatter and driver a volume imitates. The flavour only selects
/// defaults; every one of them can be overridden in [`VolumeParams`].
///
/// | | `Win2000` | `WinXp` | `Win7` | `Win10` | `Mkntfs` |
/// |---|---|---|---|---|---|
/// | record header | 42 bytes, no number | 48 bytes | 48 bytes | 48 bytes | 48 bytes |
/// | `$MFT` start | 16 KiB | a third / 3 GiB | a third / 3 GiB | a third / 3 GiB | 16 KiB |
/// | `$MFTMirr` | middle | middle | byte 8192 | byte 8192 | middle |
/// | never-used slots | `FILE`, sequence 1 | `FILE`, sequence 0 | `FILE`, sequence 1 | zero bytes | `FILE`, sequence 1; 16–23 in the mkntfs form |
/// | first user record | 24 | 28 | 35 | 38 | 64 |
/// | names | Win32 + DOS | Win32 + DOS | Win32 + DOS | POSIX | POSIX |
/// | user `$STANDARD_INFORMATION` | 48 bytes | 72 bytes | 72 bytes | 72 bytes | 48 bytes |
/// | `$LogFile` sequence numbers | set | set | set | set | 0 |
///
/// Sources: docs/notes/ntfs-on-disk.md sections 3, 10 and 12,
/// docs/notes/quick-format-behaviour.md sections 3 to 5.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Flavor {
    /// Windows 2000 (NTFS 3.0; NT4 wrote the same record header): records
    /// carry no number.
    Win2000,
    /// Windows XP / Server 2003.
    WinXp,
    /// Windows Vista to 8.1 (the "Windows 7-era" sample of the notes).
    Win7,
    /// Windows 10 and 11.
    Win10,
    /// `mkntfs` plus `ntfs-3g`.
    Mkntfs,
}

/// How file names are stored when a scenario does not say.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NamePolicy {
    /// One POSIX name (namespace 0): Windows 10 data volumes with 8.3
    /// names off, and everything ntfs-3g creates.
    Posix,
    /// 8.3 names on: one Win32+DOS name (namespace 3) when the name is a
    /// valid 8.3 name, otherwise a DOS alias (2) followed by the Win32 name
    /// (1), in that order.
    Win32AndDos,
    /// One Win32 name (namespace 1), no alias.
    Win32,
}

/// What a formatter leaves in MFT slots that are initialized but unused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlankSlots {
    /// Zero bytes are written (Windows 10).
    Zeros,
    /// Blank `FILE` records are written.
    Blank(BlankForm),
    /// Nothing is written: whatever was on the disk before stays. Models the
    /// open point of docs/notes/quick-format-behaviour.md section 3 (does
    /// Windows 10 really write the zeros?).
    Untouched,
}

/// One fragment of the `$MFT`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MftFragment {
    /// First cluster; `None` lets the builder choose (the flavour's default
    /// position for the first fragment, free space for the others).
    pub lcn: Option<u64>,
    /// Number of records the fragment holds.
    pub records: u64,
}

/// Form of an `$ATTRIBUTE_LIST`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListForm {
    /// The list is stored in the base record.
    Resident,
    /// The list is stored in clusters; its runlist is in the base record.
    NonResident,
}

/// Everything needed to format a volume.
#[derive(Clone, Debug)]
pub struct VolumeParams {
    /// Name of the volume inside the scenario ("old", "new", …).
    pub label: String,
    /// The `$VOLUME_NAME`.
    pub volume_name: String,
    /// Byte offset of the volume (= of its partition) in the image.
    pub offset: u64,
    /// Sectors of the partition. The volume has one sector less; the last
    /// sector of the partition holds the backup boot sector.
    pub partition_sectors: u64,
    /// Bytes per sector: 512 or 4096.
    pub bytes_per_sector: u32,
    /// Cluster size in bytes.
    pub cluster_size: u32,
    /// MFT record size: 1024 or 4096.
    pub record_size: u32,
    /// Index block size (4096 on every real volume).
    pub index_block_size: u32,
    /// "Hidden sectors" of the boot sector; `None` = `offset / bytes_per_sector`.
    pub hidden_sectors: Option<u32>,
    /// Volume serial number; `None` derives one from the seed.
    pub serial: Option<u64>,
    /// Formatter / driver imitated.
    pub flavor: Flavor,
    /// Time of the format: creation time of the system files.
    pub format_time: u64,
    /// The fragments of the `$MFT` (at least one). Their record counts add
    /// up to the capacity of the volume: the MFT does not grow.
    pub mft: Vec<MftFragment>,
    /// Records the `$MFT`'s initialized size covers; `None` = all. Slots
    /// beyond it are allocated but never written.
    pub mft_initialized_records: Option<u64>,
    /// Give `$MFT` an `$ATTRIBUTE_LIST`: its first fragment stays in record
    /// 0, all further ones go to record 15 (notes section 8).
    pub mft_attribute_list: Option<ListForm>,
    /// First cluster of `$MFTMirr`; `None` = the flavour's place.
    pub mftmirr_lcn: Option<u64>,
    /// Size of `$LogFile` in bytes.
    pub log_bytes: u64,
    /// Create the files a Windows of this flavour adds at the first mount
    /// (`$Extend\$RmMetadata` with the TxF logs, `System Volume
    /// Information`, …).
    pub first_mount_files: bool,
    /// Size of each of the two TxF log containers created at first mount.
    /// Windows uses 10 MiB (2 MiB on tiny volumes).
    pub txf_log_bytes: u64,
    /// What unused MFT slots hold; `None` = the flavour's form.
    pub blank_slots: Option<BlankSlots>,
    /// Name storage policy; `None` = the flavour's.
    pub name_policy: Option<NamePolicy>,
    /// First record number handed to user files; `None` = the flavour's.
    pub first_user_record: Option<u64>,
    /// Reuse the lowest freed record number for new files, as NTFS does.
    /// Off by default so that deleted files keep their records unless a
    /// scenario reuses them on purpose.
    pub reuse_records: bool,
    /// Write the backup boot sector into the last sector of the partition.
    pub backup_boot: bool,
    /// Write `$MFTMirr`.
    pub write_mftmirr: bool,
}

impl VolumeParams {
    /// A Windows 10 volume with 512-byte sectors, 4 KiB clusters and 1 KiB
    /// records that fills `partition_bytes` at `offset`, with room for 1024
    /// MFT records in one fragment.
    pub fn new(offset: u64, partition_bytes: u64) -> Self {
        assert!(offset.is_multiple_of(512), "volume offset must be sector aligned");
        assert!(partition_bytes.is_multiple_of(512), "partition size must be whole sectors");
        VolumeParams {
            label: "volume".into(),
            volume_name: "Data".into(),
            offset,
            partition_sectors: partition_bytes / 512,
            bytes_per_sector: 512,
            cluster_size: 4096,
            record_size: 1024,
            index_block_size: 4096,
            hidden_sectors: None,
            serial: None,
            flavor: Flavor::Win10,
            format_time: time::ymd(2021, 3, 14),
            mft: vec![MftFragment { lcn: None, records: 1024 }],
            mft_initialized_records: None,
            mft_attribute_list: None,
            mftmirr_lcn: None,
            log_bytes: 2 << 20,
            first_mount_files: true,
            txf_log_bytes: 256 << 10,
            blank_slots: None,
            name_policy: None,
            first_user_record: None,
            reuse_records: false,
            backup_boot: true,
            write_mftmirr: true,
        }
    }

    /// Sets the scenario label.
    pub fn label(mut self, label: &str) -> Self {
        self.label = label.into();
        self
    }

    /// Sets the volume name.
    pub fn volume_name(mut self, name: &str) -> Self {
        self.volume_name = name.into();
        self
    }

    /// Sets the sector size, keeping the partition's byte size.
    pub fn sector_size(mut self, bytes: u32) -> Self {
        let partition_bytes = self.partition_sectors * self.bytes_per_sector as u64;
        assert!(bytes == 512 || bytes == 4096, "sector size must be 512 or 4096");
        assert!(partition_bytes.is_multiple_of(bytes as u64), "partition is not whole {bytes}-byte sectors");
        self.bytes_per_sector = bytes;
        self.partition_sectors = partition_bytes / bytes as u64;
        self
    }

    /// Sets the cluster size.
    pub fn cluster_size(mut self, bytes: u32) -> Self {
        self.cluster_size = bytes;
        self
    }

    /// Sets the MFT record size.
    pub fn record_size(mut self, bytes: u32) -> Self {
        self.record_size = bytes;
        self
    }

    /// Sets the flavour.
    pub fn flavor(mut self, flavor: Flavor) -> Self {
        self.flavor = flavor;
        self
    }

    /// Sets the time of the format.
    pub fn formatted_at(mut self, filetime: u64) -> Self {
        self.format_time = filetime;
        self
    }

    /// One MFT fragment at `lcn` with room for `records`.
    pub fn mft_at(mut self, lcn: u64, records: u64) -> Self {
        self.mft = vec![MftFragment { lcn: Some(lcn), records }];
        self
    }

    /// One MFT fragment at the flavour's default place with room for
    /// `records`.
    pub fn mft_capacity(mut self, records: u64) -> Self {
        self.mft = vec![MftFragment { lcn: self.mft[0].lcn, records }];
        self
    }

    /// Several MFT fragments.
    pub fn mft_fragments(mut self, fragments: Vec<MftFragment>) -> Self {
        assert!(!fragments.is_empty(), "the MFT needs at least one fragment");
        self.mft = fragments;
        self
    }

    /// Sets the serial number.
    pub fn serial(mut self, serial: u64) -> Self {
        self.serial = Some(serial);
        self
    }

    /// Sets what unused MFT slots hold.
    pub fn blank_slots(mut self, slots: BlankSlots) -> Self {
        self.blank_slots = Some(slots);
        self
    }

    /// Sets the name policy.
    pub fn name_policy(mut self, policy: NamePolicy) -> Self {
        self.name_policy = Some(policy);
        self
    }

    /// Leaves out the first-mount files.
    pub fn without_first_mount_files(mut self) -> Self {
        self.first_mount_files = false;
        self
    }

    /// Sets the `$LogFile` size.
    pub fn log_bytes(mut self, bytes: u64) -> Self {
        self.log_bytes = bytes;
        self
    }

    /// Size of the partition in bytes.
    pub fn partition_bytes(&self) -> u64 {
        self.partition_sectors * self.bytes_per_sector as u64
    }

    /// Sectors of the volume (one less than the partition).
    pub fn total_sectors(&self) -> u64 {
        self.partition_sectors - 1
    }

    /// Whole clusters of the volume.
    pub fn total_clusters(&self) -> u64 {
        self.total_sectors() * self.bytes_per_sector as u64 / self.cluster_size as u64
    }

    /// Total number of MFT records.
    pub fn mft_records(&self) -> u64 {
        self.mft.iter().map(|f| f.records).sum()
    }

    pub(crate) fn effective_blank_slots(&self) -> BlankSlots {
        self.blank_slots.unwrap_or(match self.flavor {
            Flavor::Win10 => BlankSlots::Zeros,
            Flavor::WinXp => BlankSlots::Blank(BlankForm::Empty { sequence: 0 }),
            Flavor::Win2000 | Flavor::Win7 | Flavor::Mkntfs => BlankSlots::Blank(BlankForm::Empty { sequence: 1 }),
        })
    }

    pub(crate) fn effective_name_policy(&self) -> NamePolicy {
        self.name_policy.unwrap_or(match self.flavor {
            Flavor::Win10 | Flavor::Mkntfs => NamePolicy::Posix,
            Flavor::Win2000 | Flavor::WinXp | Flavor::Win7 => NamePolicy::Win32AndDos,
        })
    }

    /// Checks the combination of sizes; panics with a description of what is
    /// wrong. (Hostile geometry belongs into a boot sector written with
    /// [`crate::ondisk::boot`], not into a simulated volume.)
    pub(crate) fn validate(&self) {
        let bps = self.bytes_per_sector;
        assert!(bps == 512 || bps == 4096, "sector size must be 512 or 4096");
        assert!(
            self.cluster_size.is_power_of_two() && self.cluster_size >= bps && self.cluster_size <= 2 << 20,
            "cluster size {} must be a power of two from the sector size to 2 MiB",
            self.cluster_size
        );
        assert!(
            self.record_size == 1024 || self.record_size == 4096,
            "record size must be 1024 or 4096"
        );
        assert!(self.record_size >= bps, "a record cannot be smaller than a sector");
        assert!(self.index_block_size == 4096, "index block size must be 4096");
        assert!(self.offset.is_multiple_of(512), "volume offset must be a multiple of 512");
        assert!(!self.mft.is_empty(), "the MFT needs at least one fragment");
        assert!(self.mft[0].records >= 64, "the first MFT fragment must hold at least 64 records");
        assert!(self.total_clusters() >= 1024, "volume too small");
        assert!(self.log_bytes.is_multiple_of(4096) && self.log_bytes >= 64 << 10, "bad $LogFile size");
        for fragment in &self.mft {
            assert!(
                (fragment.records * self.record_size as u64).is_multiple_of(self.cluster_size as u64),
                "an MFT fragment of {} records is not a whole number of clusters",
                fragment.records
            );
        }
    }
}
