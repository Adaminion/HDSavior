//! What a scenario says about a file or folder it adds to a volume.

use crate::content::{ContentKind, ContentSpec};
use crate::ondisk::attr::reparse_tag;
use crate::ondisk::record::{ATTR_COMPRESSED, ATTR_ENCRYPTED, ATTR_SPARSE};
use crate::ondisk::runlist::Run;
use crate::ondisk::FileRef;

use super::params::ListForm;

/// A file name as UTF-16 code units. Built from `&str`, `String` or raw
/// units (for names that are not valid UTF-16).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Name(pub Vec<u16>);

impl From<&str> for Name {
    fn from(s: &str) -> Self {
        Name(s.encode_utf16().collect())
    }
}

impl From<String> for Name {
    fn from(s: String) -> Self {
        Name(s.encode_utf16().collect())
    }
}

impl From<&String> for Name {
    fn from(s: &String) -> Self {
        Name(s.encode_utf16().collect())
    }
}

impl From<Vec<u16>> for Name {
    fn from(units: Vec<u16>) -> Self {
        Name(units)
    }
}

impl From<&[u16]> for Name {
    fn from(units: &[u16]) -> Self {
        Name(units.to_vec())
    }
}

/// One explicitly specified `$FILE_NAME`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameSpec {
    /// The name.
    pub name: Name,
    /// Namespace: 0 POSIX, 1 Win32, 2 DOS, 3 Win32 + DOS.
    pub namespace: u8,
}

impl NameSpec {
    /// A name in the given namespace.
    pub fn new(name: impl Into<Name>, namespace: u8) -> Self {
        NameSpec { name: name.into(), namespace }
    }
}

/// Where the clusters of a non-resident stream come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Placement {
    /// One contiguous run at the next free place.
    Auto,
    /// This many fragments at the next free places.
    Fragments(u64),
    /// Exactly these runs. Holes are allowed; the allocated runs must be
    /// free unless the spec says `force_runs`.
    Runs(Vec<Run>),
}

/// Whether the unnamed `$DATA` is stored inside the record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageSpec {
    /// Resident when it fits a typical record, as NTFS decides.
    Auto,
    /// Resident (the record must have room).
    Resident,
    /// Non-resident, even for a few bytes.
    NonResident,
}

/// Which record of a multi-record file holds something: 0 is the base
/// record, `k > 0` the k-th extension record.
pub type Holder = usize;

/// One piece of a non-resident attribute that is split over records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Piece {
    /// The record holding this extent.
    pub holder: Holder,
    /// Number of runs in the piece; 0 = all remaining runs.
    pub runs: usize,
}

/// How the attributes of a file or folder are spread over a base record and
/// extension records (ALGORITHMS E1, E2; notes section 8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MultiSpec {
    /// Form of the `$ATTRIBUTE_LIST`.
    pub list: ListForm,
    /// Pieces of the unnamed `$DATA` in VCN order. Empty = one extent in the
    /// base record.
    pub data: Vec<Piece>,
    /// Pieces of the `$INDEX_ALLOCATION` in VCN order. Empty = one extent in
    /// the base record.
    pub index_allocation: Vec<Piece>,
    /// Record holding all `$FILE_NAME` attributes.
    pub names_in: Holder,
    /// Record holding `$INDEX_ROOT` and the index `$BITMAP`.
    pub index_root_in: Holder,
    /// Record holding a resident unnamed `$DATA`.
    pub resident_data_in: Holder,
    /// Record numbers of the extension records, in holder order; `None`
    /// entries (and missing ones) are assigned automatically.
    pub extension_records: Vec<Option<u64>>,
}

impl MultiSpec {
    /// An attribute list and nothing moved yet.
    pub fn new(list: ListForm) -> Self {
        MultiSpec {
            list,
            data: Vec::new(),
            index_allocation: Vec::new(),
            names_in: 0,
            index_root_in: 0,
            resident_data_in: 0,
            extension_records: Vec::new(),
        }
    }

    /// Splits the unnamed `$DATA`: `(holder, number of runs)` per piece.
    pub fn data(mut self, pieces: &[(Holder, usize)]) -> Self {
        self.data = pieces.iter().map(|&(holder, runs)| Piece { holder, runs }).collect();
        self
    }

    /// Splits the `$INDEX_ALLOCATION`.
    pub fn index_allocation(mut self, pieces: &[(Holder, usize)]) -> Self {
        self.index_allocation = pieces.iter().map(|&(holder, runs)| Piece { holder, runs }).collect();
        self
    }

    /// Moves all names to an extension record.
    pub fn names_in(mut self, holder: Holder) -> Self {
        self.names_in = holder;
        self
    }

    /// Moves `$INDEX_ROOT` and the index bitmap to an extension record.
    pub fn index_root_in(mut self, holder: Holder) -> Self {
        self.index_root_in = holder;
        self
    }

    /// Moves a resident `$DATA` to an extension record.
    pub fn resident_data_in(mut self, holder: Holder) -> Self {
        self.resident_data_in = holder;
        self
    }

    /// Fixes the record numbers of the extension records.
    pub fn extension_records(mut self, records: &[u64]) -> Self {
        self.extension_records = records.iter().map(|&r| Some(r)).collect();
        self
    }

    pub(crate) fn highest_holder(&self) -> Holder {
        self.data
            .iter()
            .chain(&self.index_allocation)
            .map(|p| p.holder)
            .chain([self.names_in, self.index_root_in, self.resident_data_in])
            .max()
            .unwrap_or(0)
    }
}

/// A named `$DATA` stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamSpec {
    /// Stream name.
    pub name: Name,
    /// Stream content.
    pub content: ContentSpec,
    /// Store it non-resident whatever its size.
    pub non_resident: bool,
}

/// Explicit timestamps of a new file or folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Times {
    /// Creation time.
    pub created: u64,
    /// Last modification time.
    pub modified: u64,
    /// Last MFT record change time.
    pub mft_changed: u64,
    /// Last access time.
    pub accessed: u64,
}

impl Times {
    /// All four times equal.
    pub fn all(t: u64) -> Self {
        Times { created: t, modified: t, mft_changed: t, accessed: t }
    }

    pub(crate) fn array(self) -> [u64; 4] {
        [self.created, self.modified, self.mft_changed, self.accessed]
    }
}

/// Deliberate oddities of the records of one file or folder.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Quirks {
    /// Write the base record (and its extensions) with the pre-3.1 header
    /// that carries no record number.
    pub old_header: bool,
    /// The record number the base record claims (instead of its slot).
    pub claimed_number: Option<u64>,
    /// 512-byte blocks of the base record whose fixup is broken.
    pub torn_blocks: Vec<usize>,
    /// 512-byte blocks whose fixup is broken, per extension record.
    pub torn_extension_blocks: Vec<(usize, Vec<usize>)>,
    /// Store 0 as the highest VCN of the (single) `$DATA` extent, as old
    /// volumes do (notes section 4).
    pub highest_vcn_zero: bool,
    /// Store misleading non-zero sizes in `$DATA` extents that do not start
    /// at VCN 0 (the fields are "not valid" there).
    pub sizes_in_later_extents: bool,
    /// Bytes put directly behind the runlist terminator of `$DATA`.
    pub runlist_tail: Vec<u8>,
    /// Write a 48-byte `$STANDARD_INFORMATION` whatever the flavour says
    /// (`Some(false)`), or a 72-byte one (`Some(true)`).
    pub long_standard_information: Option<bool>,
    /// Leave `$STANDARD_INFORMATION` out altogether.
    pub no_standard_information: bool,
    /// Override the parent reference of every name with this raw value.
    pub raw_parent: Option<FileRef>,
    /// Keep the directory bit out of the record header although the node is
    /// a folder (the `$FILE_NAME` copies still say "directory").
    pub no_directory_flag: bool,
    /// Give the `$FILE_NAME` attributes their attribute ids in reverse
    /// storage order. Attribute ids count creation while Windows keeps the
    /// attributes of one type sorted by value, so a name added later can sit
    /// in front of an older one: the first name in the record is then not
    /// the one with the lowest id (ALGORITHMS E4).
    pub reverse_name_ids: bool,
}

/// A file to add.
#[derive(Clone, Debug)]
pub struct FileSpec {
    /// Logical content; its length is the file size.
    pub content: ContentSpec,
    /// Resident or not.
    pub storage: StorageSpec,
    /// Clusters of a non-resident file.
    pub placement: Placement,
    /// Allow `Placement::Runs` to name clusters that are in use
    /// (cross-linked files).
    pub force_runs: bool,
    /// Clusters allocated beyond what the size needs.
    pub extra_clusters: u64,
    /// Initialized size, if smaller than the size. The clusters behind it
    /// hold stale bytes, and the content reads as zero from there on.
    pub initialized: Option<u64>,
    /// `$DATA` attribute flags.
    pub attr_flags: u16,
    /// Use the 0x48-byte attribute header with a compression unit of 4, as
    /// sparse and compressed files have.
    pub wide_header: bool,
    /// The allocated clusters hold bytes that are not the content
    /// (compressed and encrypted files).
    pub garbage_on_disk: bool,
    /// Record number of the base record.
    pub record: Option<u64>,
    /// Explicit names (all in the given parent); `None` applies the
    /// volume's name policy to the name passed to `add_file`.
    pub names: Option<Vec<NameSpec>>,
    /// Explicit times.
    pub times: Option<Times>,
    /// File attribute flags of `$STANDARD_INFORMATION`.
    pub file_attributes: u32,
    /// Spread over several records.
    pub multi: Option<MultiSpec>,
    /// Named streams.
    pub streams: Vec<StreamSpec>,
    /// Reparse point: tag and data.
    pub reparse: Option<(u32, Vec<u8>)>,
    /// Add the `$EFS` logged utility stream encrypted files carry.
    pub efs_stream: bool,
    /// Leave the unnamed `$DATA` out of the record altogether.
    pub no_data_attribute: bool,
    /// Oddities.
    pub quirks: Quirks,
}

impl FileSpec {
    /// A file with the given content, stored as NTFS would store it.
    pub fn new(content: ContentSpec) -> Self {
        FileSpec {
            content,
            storage: StorageSpec::Auto,
            placement: Placement::Auto,
            force_runs: false,
            extra_clusters: 0,
            initialized: None,
            attr_flags: 0,
            wide_header: false,
            garbage_on_disk: false,
            record: None,
            names: None,
            times: None,
            file_attributes: 0x20,
            multi: None,
            streams: Vec::new(),
            reparse: None,
            efs_stream: false,
            no_data_attribute: false,
            quirks: Quirks::default(),
        }
    }

    /// `len` bytes of tagged content.
    pub fn tagged(seed: u64, len: u64) -> Self {
        Self::new(ContentSpec::tagged(seed, len))
    }

    /// A valid media file of at least `min_len` bytes.
    pub fn media(kind: ContentKind, seed: u64, min_len: u64) -> Self {
        Self::new(ContentSpec::media(kind, seed, min_len))
    }

    /// Forces resident storage.
    pub fn resident(mut self) -> Self {
        self.storage = StorageSpec::Resident;
        self
    }

    /// Forces non-resident storage.
    pub fn non_resident(mut self) -> Self {
        self.storage = StorageSpec::NonResident;
        self
    }

    /// Non-resident in this many fragments.
    pub fn fragments(mut self, pieces: u64) -> Self {
        self.storage = StorageSpec::NonResident;
        self.placement = Placement::Fragments(pieces);
        self
    }

    /// Non-resident at exactly these runs (holes allowed).
    pub fn runs(mut self, runs: Vec<Run>) -> Self {
        self.storage = StorageSpec::NonResident;
        self.placement = Placement::Runs(runs);
        self
    }

    /// Like [`FileSpec::runs`] but onto clusters that may be in use.
    pub fn forced_runs(mut self, runs: Vec<Run>) -> Self {
        self.force_runs = true;
        self.runs(runs)
    }

    /// Sets the initialized size.
    pub fn initialized(mut self, bytes: u64) -> Self {
        self.storage = StorageSpec::NonResident;
        self.initialized = Some(bytes);
        self
    }

    /// Marks the `$DATA` sparse (flag, wide header); holes come from the
    /// runs given with [`FileSpec::runs`].
    pub fn sparse(mut self) -> Self {
        self.attr_flags |= ATTR_SPARSE;
        self.wide_header = true;
        self.file_attributes |= 0x200;
        self
    }

    /// An LZNT1-compressed file: compression flag, wide header, and
    /// pseudo-compressed bytes on disk (the logical content never is).
    pub fn compressed(mut self) -> Self {
        self.storage = StorageSpec::NonResident;
        self.attr_flags |= ATTR_COMPRESSED;
        self.wide_header = true;
        self.garbage_on_disk = true;
        self.file_attributes |= 0x800;
        self
    }

    /// An EFS-encrypted file: encryption flag, `$EFS` stream, and bytes on
    /// disk that are not the content.
    pub fn encrypted(mut self) -> Self {
        self.storage = StorageSpec::NonResident;
        self.attr_flags |= ATTR_ENCRYPTED;
        self.garbage_on_disk = true;
        self.efs_stream = true;
        self.file_attributes |= 0x4000;
        self
    }

    /// Sets the base record number.
    pub fn record(mut self, number: u64) -> Self {
        self.record = Some(number);
        self
    }

    /// Sets explicit names.
    pub fn names(mut self, names: Vec<NameSpec>) -> Self {
        self.names = Some(names);
        self
    }

    /// Sets explicit times.
    pub fn times(mut self, times: Times) -> Self {
        self.times = Some(times);
        self
    }

    /// Spreads the file over several records.
    pub fn multi(mut self, multi: MultiSpec) -> Self {
        self.multi = Some(multi);
        self
    }

    /// Adds a named stream.
    pub fn stream(mut self, name: impl Into<Name>, content: ContentSpec) -> Self {
        self.streams.push(StreamSpec { name: name.into(), content, non_resident: false });
        self
    }

    /// Makes the file a reparse point.
    pub fn reparse(mut self, tag: u32, data: Vec<u8>) -> Self {
        self.reparse = Some((tag, data));
        self.file_attributes |= 0x400;
        self
    }

    /// Sets the oddities.
    pub fn quirks(mut self, quirks: Quirks) -> Self {
        self.quirks = quirks;
        self
    }

    /// A cloud-files placeholder (OneDrive): reparse tag `tag`, a sparse
    /// unnamed stream of `len` bytes without a single allocated cluster
    /// (notes section 11). `content` is what the cloud holds; nothing of it
    /// is on the disk.
    pub fn cloud_placeholder(seed: u64, len: u64, tag: u32) -> Self {
        let mut spec = Self::tagged(seed, len).sparse();
        spec.storage = StorageSpec::NonResident;
        spec.placement = Placement::Runs(Vec::new());
        // offline | recall on data access
        spec.file_attributes |= 0x1000 | 0x0040_0000;
        spec.reparse(tag, vec![0x02, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00])
    }

    /// A Data Deduplication stub: reparse tag `0x80000013`, sparse stream
    /// without clusters.
    pub fn dedup_stub(seed: u64, len: u64) -> Self {
        let mut spec = Self::tagged(seed, len).sparse();
        spec.storage = StorageSpec::NonResident;
        spec.placement = Placement::Runs(Vec::new());
        spec.reparse(reparse_tag::DEDUP, vec![0; 16])
    }

    /// A WOF-compressed file ("compact /exe"): reparse tag `0x80000017`, a
    /// sparse unnamed stream without clusters, and the compressed bytes in a
    /// named stream `WofCompressedData`.
    pub fn wof_compressed(seed: u64, len: u64) -> Self {
        let mut spec = Self::tagged(seed, len).sparse();
        spec.storage = StorageSpec::NonResident;
        spec.placement = Placement::Runs(Vec::new());
        let packed = ContentSpec::tagged(crate::rng::derive_n(seed, 0x57_4F46), (len / 2).max(4096));
        spec.streams.push(StreamSpec { name: "WofCompressedData".into(), content: packed, non_resident: true });
        spec.reparse(reparse_tag::WOF, vec![1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0])
    }
}

/// A folder to add.
#[derive(Clone, Debug)]
pub struct DirSpec {
    /// Record number of the base record.
    pub record: Option<u64>,
    /// Explicit names.
    pub names: Option<Vec<NameSpec>>,
    /// Explicit times.
    pub times: Option<Times>,
    /// File attribute flags of `$STANDARD_INFORMATION`.
    pub file_attributes: u32,
    /// Keep the index in `INDX` blocks even when it would fit the record.
    pub force_index_blocks: bool,
    /// How full index blocks are packed (percent).
    pub index_fill_percent: u32,
    /// Where the index blocks go: explicit runs used first, in order;
    /// further blocks are allocated automatically.
    pub index_runs: Vec<Run>,
    /// Allocate every automatically placed index block as a fragment of its
    /// own (many small runs).
    pub fragment_index: bool,
    /// Spread over several records.
    pub multi: Option<MultiSpec>,
    /// Oddities.
    pub quirks: Quirks,
}

impl Default for DirSpec {
    fn default() -> Self {
        DirSpec {
            record: None,
            names: None,
            times: None,
            file_attributes: 0,
            force_index_blocks: false,
            index_fill_percent: 100,
            index_runs: Vec::new(),
            fragment_index: false,
            multi: None,
            quirks: Quirks::default(),
        }
    }
}

impl DirSpec {
    /// A folder as NTFS would create it.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the base record number.
    pub fn record(mut self, number: u64) -> Self {
        self.record = Some(number);
        self
    }

    /// Sets explicit names.
    pub fn names(mut self, names: Vec<NameSpec>) -> Self {
        self.names = Some(names);
        self
    }

    /// Sets explicit times.
    pub fn times(mut self, times: Times) -> Self {
        self.times = Some(times);
        self
    }

    /// Keeps the index in `INDX` blocks whatever its size.
    pub fn force_index_blocks(mut self) -> Self {
        self.force_index_blocks = true;
        self
    }

    /// Puts the first index blocks at these runs.
    pub fn index_runs(mut self, runs: Vec<Run>) -> Self {
        self.index_runs = runs;
        self
    }

    /// Spreads the folder over several records.
    pub fn multi(mut self, multi: MultiSpec) -> Self {
        self.multi = Some(multi);
        self
    }

    /// Sets the oddities.
    pub fn quirks(mut self, quirks: Quirks) -> Self {
        self.quirks = quirks;
        self
    }
}
