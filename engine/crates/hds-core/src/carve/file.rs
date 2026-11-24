//! One carved file: what the catalog shows and what recovery needs.

use std::fmt;

use hds_carve::{Confidence, FileKind, Measured};
use hds_io::out::Timestamps;
use hds_io::RangeSet;
use hds_recover::{
    rate, Flag, FileSource, FlagSet, PlanFile, PlanFolder, Rating, RatingContext, Run,
};

use super::CarveGeometry;

/// Name of the virtual folder that holds the carved files (ALGORITHMS E,
/// I3): `[Carved files]`, below the `main` instance's root.
pub const CARVED_FILES_FOLDER: &str = "[Carved files]";

/// Longest extension a carved file can have (the longest `hds_carve`
/// produces is four characters).
const EXT_CAPACITY: usize = 8;

/// A file extension as [`hds_carve::Measured::ext`] gives it: 1 to 8
/// lower-case ASCII letters and digits, stored inline (carved files are
/// many; none of them allocates).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExtName {
    len: u8,
    bytes: [u8; EXT_CAPACITY],
}

impl ExtName {
    /// The extension `ext`; `None` unless it is 1 to 8 characters of
    /// `a-z` and `0-9`.
    #[must_use]
    pub fn new(ext: &str) -> Option<ExtName> {
        let raw = ext.as_bytes();
        if raw.is_empty()
            || raw.len() > EXT_CAPACITY
            || !raw
                .iter()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        {
            return None;
        }
        let mut bytes = [0_u8; EXT_CAPACITY];
        bytes.get_mut(..raw.len())?.copy_from_slice(raw);
        Some(ExtName {
            len: u8::try_from(raw.len()).ok()?,
            bytes,
        })
    }

    /// The extension, without the dot.
    #[must_use]
    pub fn as_str(&self) -> &str {
        let raw = self.bytes.get(..usize::from(self.len)).unwrap_or(&[]);
        // only ASCII is ever stored
        std::str::from_utf8(raw).unwrap_or("")
    }
}

impl fmt::Debug for ExtName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

impl fmt::Display for ExtName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A file found by carving: a contiguous byte range of the source that
/// starts with a signature and whose end the format's structure gave
/// (ALGORITHMS I3).
///
/// Its node is `[Carved files]/<ext>/f<sector, 10 digits>.<ext>`
/// ([`folder_name`](Self::folder_name), [`name`](Self::name)); its size is
/// [`len`](Self::len); it has no times and no MFT record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CarvedFile {
    offset: u64,
    len: u64,
    kind: FileKind,
    ext: ExtName,
    end_verified: bool,
    exact: bool,
}

impl CarvedFile {
    /// The file `measured` found at absolute offset `offset` as a candidate
    /// of `kind`. `None` when the measurement is empty, does not fit the
    /// address space or names an extension [`ExtName`] does not hold (none
    /// of which `hds_carve` produces).
    #[must_use]
    pub fn from_measured(offset: u64, kind: FileKind, measured: &Measured) -> Option<CarvedFile> {
        CarvedFile::new(
            offset,
            measured.len,
            kind,
            ExtName::new(measured.ext)?,
            measured.end_verified,
            measured.confidence == Confidence::Exact,
        )
    }

    /// A carved file from its parts (the session codec's constructor).
    /// `None` when `len` is 0 or `offset + len` overflows.
    #[must_use]
    pub fn new(
        offset: u64,
        len: u64,
        kind: FileKind,
        ext: ExtName,
        end_verified: bool,
        exact: bool,
    ) -> Option<CarvedFile> {
        (len > 0 && offset.checked_add(len).is_some()).then_some(CarvedFile {
            offset,
            len,
            kind,
            ext,
            end_verified,
            exact,
        })
    }

    /// Absolute byte offset of the first byte on the source.
    #[must_use]
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Length in bytes: the file's size.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Always `false`: a carved file has at least one byte.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Absolute offset one past the last byte.
    #[must_use]
    pub fn end(&self) -> u64 {
        // checked on construction
        self.offset.saturating_add(self.len)
    }

    /// The absolute byte range of the file.
    #[must_use]
    pub fn range(&self) -> std::ops::Range<u64> {
        self.offset..self.end()
    }

    /// The kind its signature had (the carve hit's).
    #[must_use]
    pub fn kind(&self) -> FileKind {
        self.kind
    }

    /// Extension, lower case, no dot: `jpg`, `docx`, `mov`, ...
    #[must_use]
    pub fn ext(&self) -> &str {
        self.ext.as_str()
    }

    /// The extension as stored.
    #[must_use]
    pub fn ext_name(&self) -> ExtName {
        self.ext
    }

    /// Whether the format's structure verified the end
    /// ([`hds_carve::Measured::end_verified`]).
    #[must_use]
    pub fn end_verified(&self) -> bool {
        self.end_verified
    }

    /// How much the measured length can be relied on.
    #[must_use]
    pub fn confidence(&self) -> Confidence {
        if self.exact {
            Confidence::Exact
        } else {
            Confidence::Probable
        }
    }

    /// The sector number in the node name: the absolute offset / 512.
    #[must_use]
    pub fn sector(&self) -> u64 {
        self.offset / 512
    }

    /// The node name, `f<sector, at least 10 digits>.<ext>`.
    #[must_use]
    pub fn name(&self) -> String {
        format!("f{:010}.{}", self.sector(), self.ext())
    }

    /// [`name`](Self::name) as UTF-16, as plans and the catalog's name
    /// arena hold names.
    #[must_use]
    pub fn name_utf16(&self) -> Vec<u16> {
        self.name().encode_utf16().collect()
    }

    /// The folder below `[Carved files]` the node lives in: its extension.
    #[must_use]
    pub fn folder_name(&self) -> &str {
        self.ext()
    }

    /// The display path from the instance root:
    /// `[Carved files]/<ext>/f<sector>.<ext>`.
    #[must_use]
    pub fn path(&self) -> String {
        format!("{CARVED_FILES_FOLDER}/{}/{}", self.ext(), self.name())
    }

    /// The cluster the file starts at under `geometry` (`node_info`'s
    /// `first_lcn`); `None` in front of cluster 0.
    #[must_use]
    pub fn first_lcn(&self, geometry: CarveGeometry) -> Option<u64> {
        geometry.lcn(self.offset)
    }

    /// The G1 flags known without reading the file: `overwritten` when its
    /// bytes intersect the overwritten set (C5), `bad_sectors` when they
    /// intersect the scan's bad ranges. Carving drops candidates that meet
    /// unreadable sectors, so the second only appears when the bad ranges
    /// grew after carving. A read (recovery, `read_file`) adds its own
    /// flags, as for every file.
    #[must_use]
    pub fn health_flags(&self, overwritten: &RangeSet, bad_ranges: &RangeSet) -> FlagSet {
        let mut flags = FlagSet::EMPTY;
        if overwritten.intersects(self.range()) {
            flags.insert(Flag::Overwritten);
        }
        if bad_ranges.intersects(self.range()) {
            flags.insert(Flag::BadSectors);
        }
        flags
    }

    /// The rating inputs that are not flags: `carved_end_unverified` unless
    /// the end was verified (I3). A carved file is located by its absolute
    /// offset and its own structure, not through a geometry, so
    /// `geometry_not_validated` is never set.
    #[must_use]
    pub fn rating_context(&self) -> RatingContext {
        RatingContext {
            geometry_not_validated: false,
            carved_end_unverified: !self.end_verified,
        }
    }

    /// The G3 rating with `flags` (from [`health_flags`](Self::health_flags)
    /// and whatever reads added): `good` only with a verified end and no
    /// damage.
    #[must_use]
    pub fn rating(&self, flags: FlagSet) -> Rating {
        rate(flags, self.rating_context())
    }

    /// Where the content is: one run of [`len`](Self::len) bytes at
    /// [`offset`](Self::offset), all of it initialized (I5: carved files
    /// are recovered like any file with one run).
    #[must_use]
    pub fn file_source(&self) -> FileSource {
        FileSource::Extents {
            runs: vec![Run::Data {
                offset: self.offset,
                len: self.len,
            }],
            real_size: self.len,
            initialized_size: self.len,
        }
    }

    /// The recovery plan entry of this file as catalog node `node`, written
    /// into plan folder `folder` (normally the `<ext>` folder made by
    /// [`ext_folder`]) with the pre-read `flags`.
    #[must_use]
    pub fn plan_file(&self, node: u64, folder: Option<u64>, flags: FlagSet) -> PlanFile {
        PlanFile {
            node,
            folder,
            name: self.name_utf16(),
            ext: self.ext().to_string(),
            source: self.file_source(),
            size: self.len,
            times: Timestamps::default(),
            flags,
            context: self.rating_context(),
            in_use: true,
            record_number: None,
            record_offset: None,
            first_data_offset: Some(self.offset),
            original_path: self.path(),
        }
    }
}

/// The plan folder `[Carved files]` with id `id` inside `parent` (the
/// `main` instance's root folder of the plan, or `None` for the
/// destination itself).
#[must_use]
pub fn carved_files_folder(id: u64, parent: Option<u64>) -> PlanFolder {
    PlanFolder {
        id,
        parent,
        name: CARVED_FILES_FOLDER.encode_utf16().collect(),
        times: Timestamps::default(),
        in_use: true,
        record_number: None,
    }
}

/// The plan folder `<ext>` with id `id` inside the `[Carved files]` folder
/// `parent`.
#[must_use]
pub fn ext_folder(id: u64, parent: u64, ext: &str) -> PlanFolder {
    PlanFolder {
        id,
        parent: Some(parent),
        name: ext.encode_utf16().collect(),
        times: Timestamps::default(),
        in_use: true,
        record_number: None,
    }
}
