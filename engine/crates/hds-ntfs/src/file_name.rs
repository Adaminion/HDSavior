//! `$FILE_NAME` values (docs/notes/ntfs-on-disk.md, section 7), shared by
//! the attribute in a record and the key of a directory index entry.

use crate::bytes::{u32_at, u64_at, u8_at};
use crate::name::Utf16Name;
use crate::reference::FileRef;

/// `$FILE_NAME` flag (and index-entry flag): the name belongs to a
/// directory. This, or the directory bit of the record header, is the test
/// for "is a directory" - never the `0x10` bit known from Win32.
pub const FILE_ATTR_DIRECTORY: u32 = 0x1000_0000;
/// `$FILE_NAME` flag: the file has a non-`$I30` ("view") index.
pub const FILE_ATTR_VIEW_INDEX: u32 = 0x2000_0000;
/// File attribute flag: read-only.
pub const FILE_ATTR_READ_ONLY: u32 = 0x0001;
/// File attribute flag: hidden.
pub const FILE_ATTR_HIDDEN: u32 = 0x0002;
/// File attribute flag: system.
pub const FILE_ATTR_SYSTEM: u32 = 0x0004;
/// File attribute flag: archive.
pub const FILE_ATTR_ARCHIVE: u32 = 0x0020;
/// File attribute flag: temporary.
pub const FILE_ATTR_TEMPORARY: u32 = 0x0100;
/// File attribute flag: sparse file.
pub const FILE_ATTR_SPARSE: u32 = 0x0200;
/// File attribute flag: the file is a reparse point.
pub const FILE_ATTR_REPARSE_POINT: u32 = 0x0400;
/// File attribute flag: compressed.
pub const FILE_ATTR_COMPRESSED: u32 = 0x0800;
/// File attribute flag: offline.
pub const FILE_ATTR_OFFLINE: u32 = 0x1000;
/// File attribute flag: not content indexed.
pub const FILE_ATTR_NOT_INDEXED: u32 = 0x2000;
/// File attribute flag: encrypted.
pub const FILE_ATTR_ENCRYPTED: u32 = 0x4000;

/// Smallest `$FILE_NAME` value: the fixed part (0x42 bytes) and one UTF-16
/// code unit.
pub const FILE_NAME_MIN_LEN: usize = 0x44;

/// The four timestamps of `$STANDARD_INFORMATION` and `$FILE_NAME`, as raw
/// FILETIME values (see [`filetime_to_unix_ms`](crate::filetime_to_unix_ms)).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Times {
    /// Creation time.
    pub created: u64,
    /// Last modification of the data.
    pub modified: u64,
    /// Last change of the MFT record. Unlike the creation time it cannot be
    /// back-dated by copy or backup tools.
    pub mft_changed: u64,
    /// Last access.
    pub accessed: u64,
}

impl Times {
    /// Four consecutive FILETIMEs starting at `off`.
    #[inline]
    pub(crate) fn read(b: &[u8], off: usize) -> Option<Times> {
        Some(Times {
            created: u64_at(b, off)?,
            modified: u64_at(b, off.checked_add(8)?)?,
            mft_changed: u64_at(b, off.checked_add(16)?)?,
            accessed: u64_at(b, off.checked_add(24)?)?,
        })
    }
}

/// The namespace of a file name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Namespace {
    /// Case-sensitive name with almost no restrictions. What ntfs-3g writes,
    /// and what Windows 10 writes for every name on volumes with 8.3 names
    /// switched off.
    Posix = 0,
    /// A long name that has a separate DOS alias.
    Win32 = 1,
    /// The 8.3 alias of a [`Win32`](Namespace::Win32) name: a second name
    /// for the same file, never a hard link of its own.
    Dos = 2,
    /// A long name that is a valid 8.3 name at the same time.
    Win32AndDos = 3,
}

impl Namespace {
    /// Decodes the namespace byte; `None` for values above 3.
    #[inline]
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Namespace> {
        match value {
            0 => Some(Namespace::Posix),
            1 => Some(Namespace::Win32),
            2 => Some(Namespace::Dos),
            3 => Some(Namespace::Win32AndDos),
            _ => None,
        }
    }

    /// The namespace byte as stored.
    #[inline]
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// A `$FILE_NAME` value: one name of a file together with a copy of some
/// metadata.
///
/// The sizes and times are copies made when the name was created or the
/// file renamed, and are stale in an MFT record (use `$DATA` and
/// `$STANDARD_INFORMATION`); the copy inside a directory index entry is
/// updated lazily.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileName<'a> {
    /// Reference to the directory that holds this name.
    pub parent: FileRef,
    /// The four timestamps (0x08..0x28).
    pub times: Times,
    /// Allocated size (u64 at 0x28).
    pub allocated_size: u64,
    /// Real size (u64 at 0x30).
    pub real_size: u64,
    /// File attribute flags (u32 at 0x38); see the `FILE_ATTR_*` constants.
    pub flags: u32,
    /// EA size, or the reparse tag when the file is a reparse point
    /// (u32 at 0x3C). Often absent from the record's own `$FILE_NAME`; read
    /// the tag from `$REPARSE_POINT`.
    pub reparse_or_ea: u32,
    /// Namespace (u8 at 0x41).
    pub namespace: Namespace,
    /// The name: 1 to 255 UTF-16 code units.
    pub name: Utf16Name<'a>,
}

impl<'a> FileName<'a> {
    /// Decodes a `$FILE_NAME` value.
    ///
    /// Returns `None` unless the value has the fixed part of 0x42 bytes, a
    /// name length of 1 to 255 code units that fits in the value, and a
    /// namespace of 0 to 3. Bytes behind the name are ignored.
    #[must_use]
    pub fn parse(value: &'a [u8]) -> Option<FileName<'a>> {
        Self::parse_prefix(value).map(|(name, _)| name)
    }

    /// Decodes a `$FILE_NAME` value that must end exactly where its name
    /// ends, as the key of a directory index entry does.
    #[must_use]
    pub fn parse_exact(value: &'a [u8]) -> Option<FileName<'a>> {
        match Self::parse_prefix(value) {
            Some((name, used)) if used == value.len() => Some(name),
            _ => None,
        }
    }

    /// The decoded name and the number of bytes of `value` it occupies.
    #[inline]
    fn parse_prefix(value: &'a [u8]) -> Option<(FileName<'a>, usize)> {
        let name_units = usize::from(u8_at(value, 0x40)?);
        let namespace = Namespace::from_u8(u8_at(value, 0x41)?)?;
        if name_units == 0 {
            return None;
        }
        let end = name_units.checked_mul(2)?.checked_add(0x42)?;
        let name = Utf16Name::from_bytes(value.get(0x42..end)?);
        Some((
            FileName {
                parent: FileRef::from_raw(u64_at(value, 0x00)?),
                times: Times::read(value, 0x08)?,
                allocated_size: u64_at(value, 0x28)?,
                real_size: u64_at(value, 0x30)?,
                flags: u32_at(value, 0x38)?,
                reparse_or_ea: u32_at(value, 0x3C)?,
                namespace,
                name,
            },
            end,
        ))
    }

    /// Whether the name belongs to a directory
    /// ([`FILE_ATTR_DIRECTORY`] is set).
    #[inline]
    #[must_use]
    pub const fn is_directory(&self) -> bool {
        self.flags & FILE_ATTR_DIRECTORY != 0
    }
}
