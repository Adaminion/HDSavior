//! Per-file flags and the rating they lead to (ALGORITHMS G1 and G3).
//!
//! The catalog of `hds-core` and the recovery of this crate use the same
//! vocabulary: a [`FlagSet`] of [`Flag`]s per file, plus two conditions that
//! are not flags ([`RatingContext`]), give a [`Rating`] through [`rate`].
//! The wire names of flags and ratings are those of the protocol
//! (`Row.flags`, `Row.rating`).

use std::fmt;

use serde::de::{SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// One fact about a file that matters to whoever wants it back
/// (ALGORITHMS G1). Wire form: the snake-case name, for example
/// `"torn_record"`.
///
/// The order of the variants is the canonical order in which flags are
/// listed (protocol, report).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Flag {
    /// Deleted before the accident: its clusters may have been reused.
    Deleted,
    /// A fixup mismatch in a record of the file: a write of it was torn.
    TornRecord,
    /// Parts of the cluster chain are missing (a hole in the chain, an
    /// overlap, fewer mapped clusters than the size needs, a lost extent).
    MissingFragments,
    /// The size was estimated from the clusters found (the extent that
    /// stores it is lost). Comes with `missing_fragments`.
    SizeEstimated,
    /// A run beyond the end of the volume, or mapped bytes outside the
    /// session range: those bytes read as zeros.
    OutOfBounds,
    /// The clusters have been written to since the file was stored, it lost
    /// an overlap conflict, a carved file was found inside it, or it is the
    /// stale container of an embedded file system.
    Overwritten,
    /// Another file of the same kind of instance claims some of the same
    /// clusters, and this file won the conflict.
    Overlap,
    /// Stored compressed (LZNT1, WOF): not recoverable as plain clusters.
    Compressed,
    /// Encrypted with EFS.
    Encrypted,
    /// A cloud placeholder or a deduplication stub: the content is not on
    /// this disk.
    Placeholder,
    /// The runlist has a hole or the attribute is marked sparse.
    Sparse,
    /// The file has no content stream (0 bytes).
    NoData,
    /// Some of the file's sectors could not be read: they are zeros.
    BadSectors,
    /// The file belongs to a file system from before the previous format.
    OlderGeneration,
    /// The file is inside a disk image stored on the drive (browse only).
    Embedded,
    /// The record is a stray: nothing ties it to a file system.
    Stray,
    /// The first bytes look like a file of the type the extension promises.
    SignatureOk,
    /// The first bytes do not look like a file of the type the extension
    /// promises.
    SignatureMismatch,
}

impl Flag {
    /// Every flag, in canonical order.
    pub const ALL: [Flag; 18] = [
        Flag::Deleted,
        Flag::TornRecord,
        Flag::MissingFragments,
        Flag::SizeEstimated,
        Flag::OutOfBounds,
        Flag::Overwritten,
        Flag::Overlap,
        Flag::Compressed,
        Flag::Encrypted,
        Flag::Placeholder,
        Flag::Sparse,
        Flag::NoData,
        Flag::BadSectors,
        Flag::OlderGeneration,
        Flag::Embedded,
        Flag::Stray,
        Flag::SignatureOk,
        Flag::SignatureMismatch,
    ];

    /// The wire name (`Row.flags` of the protocol).
    pub const fn name(self) -> &'static str {
        match self {
            Flag::Deleted => "deleted",
            Flag::TornRecord => "torn_record",
            Flag::MissingFragments => "missing_fragments",
            Flag::SizeEstimated => "size_estimated",
            Flag::OutOfBounds => "out_of_bounds",
            Flag::Overwritten => "overwritten",
            Flag::Overlap => "overlap",
            Flag::Compressed => "compressed",
            Flag::Encrypted => "encrypted",
            Flag::Placeholder => "placeholder",
            Flag::Sparse => "sparse",
            Flag::NoData => "no_data",
            Flag::BadSectors => "bad_sectors",
            Flag::OlderGeneration => "older_generation",
            Flag::Embedded => "embedded",
            Flag::Stray => "stray",
            Flag::SignatureOk => "signature_ok",
            Flag::SignatureMismatch => "signature_mismatch",
        }
    }

    /// The flag with this wire name.
    pub fn from_name(name: &str) -> Option<Flag> {
        Flag::ALL.into_iter().find(|flag| flag.name() == name)
    }

    /// The bit of this flag in [`FlagSet::bits`]: `1 << position in ALL`.
    pub const fn bit(self) -> u32 {
        1 << (self as u32)
    }

    /// A plain-language explanation for users, without details that depend
    /// on the file (`node_info.flag_details` can start from it).
    pub const fn explanation(self) -> &'static str {
        match self {
            Flag::Deleted => {
                "The file was deleted before the accident; its clusters may have been reused since."
            }
            Flag::TornRecord => {
                "A write of the file's MFT record was interrupted; parts of the record may be stale."
            }
            Flag::MissingFragments => {
                "Parts of the file's cluster list are missing; those parts are recovered as zeros."
            }
            Flag::SizeEstimated => {
                "The file's size was not found; it was estimated from the clusters that were found."
            }
            Flag::OutOfBounds => {
                "Part of the file lies outside the scanned area or beyond the end of its volume; those bytes are recovered as zeros."
            }
            Flag::Overwritten => {
                "Part of the file's clusters have been written to since the file was stored; its content is probably damaged."
            }
            Flag::Overlap => "Another file claims some of the same clusters.",
            Flag::Compressed => {
                "The file is stored compressed by NTFS; HDSavior cannot recover compressed files."
            }
            Flag::Encrypted => "The file is encrypted (EFS); its content cannot be recovered.",
            Flag::Placeholder => {
                "The file's content is not stored on this disk (cloud placeholder or deduplication)."
            }
            Flag::Sparse => {
                "The file has regions that were never allocated; they read as zeros, which is normal for such files."
            }
            Flag::NoData => "The file has no content stream; it is recovered as an empty file.",
            Flag::BadSectors => {
                "Some sectors of the file could not be read; they are recovered as zeros."
            }
            Flag::OlderGeneration => {
                "The file belongs to a file system from before the previous format; many of its clusters have been reused."
            }
            Flag::Embedded => {
                "The file is inside a disk image stored on the drive: recover the image file and open it as a source."
            }
            Flag::Stray => {
                "The file's record could not be tied to a file system; its content may come from the wrong place."
            }
            Flag::SignatureOk => "The file starts the way a file of its type does.",
            Flag::SignatureMismatch => {
                "The file does not start the way a file of its type does; its content is probably wrong."
            }
        }
    }
}

impl fmt::Display for Flag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A set of [`Flag`]s in one `u32`.
///
/// Serialized as the list of wire names in canonical order (the form of
/// `Row.flags`); [`bits`](FlagSet::bits) and
/// [`from_bits`](FlagSet::from_bits) give a compact form for session files.
///
/// ```
/// use hds_recover::{Flag, FlagSet};
///
/// let flags = FlagSet::from_iter([Flag::SignatureOk, Flag::Deleted]);
/// assert_eq!(serde_json::to_string(&flags).unwrap(), r#"["deleted","signature_ok"]"#);
/// assert_eq!(flags.to_string(), "deleted;signature_ok");
/// ```
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct FlagSet(u32);

impl FlagSet {
    /// The empty set.
    pub const EMPTY: FlagSet = FlagSet(0);

    /// The bits that stand for a flag.
    const VALID_BITS: u32 = (1 << Flag::ALL.len()) - 1;

    /// The empty set.
    pub const fn new() -> FlagSet {
        FlagSet(0)
    }

    /// The set of exactly these flags.
    pub const fn of(flags: &[Flag]) -> FlagSet {
        let mut bits = 0;
        let mut rest = flags;
        while let [first, tail @ ..] = rest {
            bits |= first.bit();
            rest = tail;
        }
        FlagSet(bits)
    }

    /// The set as bits: bit `n` stands for `Flag::ALL[n]`.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// The set with these bits; `None` when a bit stands for no flag (data
    /// from a newer version or a damaged file).
    pub const fn from_bits(bits: u32) -> Option<FlagSet> {
        if bits & !Self::VALID_BITS == 0 {
            Some(FlagSet(bits))
        } else {
            None
        }
    }

    /// Whether `flag` is in the set.
    pub const fn contains(self, flag: Flag) -> bool {
        self.0 & flag.bit() != 0
    }

    /// Whether the two sets have a flag in common.
    pub const fn intersects(self, other: FlagSet) -> bool {
        self.0 & other.0 != 0
    }

    /// Adds `flag`.
    pub fn insert(&mut self, flag: Flag) {
        self.0 |= flag.bit();
    }

    /// Removes `flag`.
    pub fn remove(&mut self, flag: Flag) {
        self.0 &= !flag.bit();
    }

    /// The set with `flag` added.
    #[must_use]
    pub const fn with(self, flag: Flag) -> FlagSet {
        FlagSet(self.0 | flag.bit())
    }

    /// The set without `flag`.
    #[must_use]
    pub const fn without(self, flag: Flag) -> FlagSet {
        FlagSet(self.0 & !flag.bit())
    }

    /// The flags in either set.
    #[must_use]
    pub const fn union(self, other: FlagSet) -> FlagSet {
        FlagSet(self.0 | other.0)
    }

    /// The flags in both sets.
    #[must_use]
    pub const fn intersection(self, other: FlagSet) -> FlagSet {
        FlagSet(self.0 & other.0)
    }

    /// The flags of `self` that are not in `other`.
    #[must_use]
    pub const fn difference(self, other: FlagSet) -> FlagSet {
        FlagSet(self.0 & !other.0)
    }

    /// Whether the set is empty.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Number of flags in the set.
    pub const fn len(self) -> usize {
        self.0.count_ones() as usize
    }

    /// The flags in canonical order.
    pub fn iter(self) -> impl Iterator<Item = Flag> {
        Flag::ALL
            .into_iter()
            .filter(move |flag| self.contains(*flag))
    }

    /// The rating these flags lead to in `context` (ALGORITHMS G3).
    pub fn rating(self, context: RatingContext) -> Rating {
        rate(self, context)
    }
}

impl From<Flag> for FlagSet {
    fn from(flag: Flag) -> FlagSet {
        FlagSet(flag.bit())
    }
}

impl FromIterator<Flag> for FlagSet {
    fn from_iter<I: IntoIterator<Item = Flag>>(iter: I) -> FlagSet {
        let mut set = FlagSet::new();
        set.extend(iter);
        set
    }
}

impl Extend<Flag> for FlagSet {
    fn extend<I: IntoIterator<Item = Flag>>(&mut self, iter: I) {
        for flag in iter {
            self.insert(flag);
        }
    }
}

impl std::ops::BitOr for FlagSet {
    type Output = FlagSet;

    fn bitor(self, other: FlagSet) -> FlagSet {
        self.union(other)
    }
}

impl std::ops::BitOrAssign for FlagSet {
    fn bitor_assign(&mut self, other: FlagSet) {
        *self = self.union(other);
    }
}

impl fmt::Debug for FlagSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

/// The wire names joined with `;`, as in the recovery report.
impl fmt::Display for FlagSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, flag) in self.iter().enumerate() {
            if i > 0 {
                f.write_str(";")?;
            }
            f.write_str(flag.name())?;
        }
        Ok(())
    }
}

impl Serialize for FlagSet {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(self.len()))?;
        for flag in self.iter() {
            seq.serialize_element(&flag)?;
        }
        seq.end()
    }
}

impl<'de> Deserialize<'de> for FlagSet {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<FlagSet, D::Error> {
        struct FlagsVisitor;

        impl<'de> Visitor<'de> for FlagsVisitor {
            type Value = FlagSet;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a list of flag names")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<FlagSet, A::Error> {
                let mut set = FlagSet::new();
                while let Some(flag) = seq.next_element::<Flag>()? {
                    set.insert(flag);
                }
                Ok(set)
            }
        }

        deserializer.deserialize_seq(FlagsVisitor)
    }
}

/// How far a file can be trusted (ALGORITHMS G3). Wire form: `"good"`,
/// `"uncertain"`, `"damaged"`, `"unsupported"`.
///
/// Ordered from best to worst, which is the order of `sort: "rating"`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Rating {
    /// Nothing speaks against the file.
    Good,
    /// Something casts doubt on the file, nothing shows that it is damaged.
    Uncertain,
    /// The content is known to be incomplete or wrong in places.
    Damaged,
    /// The content is not stored in a form HDSavior can recover; never
    /// written.
    Unsupported,
}

impl Rating {
    /// Every rating, best first.
    pub const ALL: [Rating; 4] = [
        Rating::Good,
        Rating::Uncertain,
        Rating::Damaged,
        Rating::Unsupported,
    ];

    /// The wire name.
    pub const fn name(self) -> &'static str {
        match self {
            Rating::Good => "good",
            Rating::Uncertain => "uncertain",
            Rating::Damaged => "damaged",
            Rating::Unsupported => "unsupported",
        }
    }

    /// The rating with this wire name.
    pub fn from_name(name: &str) -> Option<Rating> {
        Rating::ALL.into_iter().find(|rating| rating.name() == name)
    }
}

impl fmt::Display for Rating {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The two inputs of the rating that are not flags (ALGORITHMS G3).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct RatingContext {
    /// The geometry of the file's instance is not validated (ALGORITHMS F6):
    /// its runs may point to the wrong clusters.
    pub geometry_not_validated: bool,
    /// The file was carved and its end was not verified by the format's
    /// structure (ALGORITHMS I3).
    pub carved_end_unverified: bool,
}

impl RatingContext {
    /// Names of the conditions that hold, as listed after `warning:` in a
    /// recovery status: `geometry_not_validated`, `carved_end_unverified`.
    pub fn names(self) -> impl Iterator<Item = &'static str> {
        [
            (self.geometry_not_validated, "geometry_not_validated"),
            (self.carved_end_unverified, "carved_end_unverified"),
        ]
        .into_iter()
        .filter_map(|(holds, name)| holds.then_some(name))
    }
}

/// Flags that make a file `unsupported`: its content is not stored as plain
/// clusters on this disk, or it is browse-only.
pub const UNSUPPORTED_FLAGS: FlagSet = FlagSet::of(&[
    Flag::Compressed,
    Flag::Encrypted,
    Flag::Placeholder,
    Flag::Embedded,
]);

/// Flags that make a file `damaged` (unless it is unsupported).
pub const DAMAGED_FLAGS: FlagSet = FlagSet::of(&[
    Flag::MissingFragments,
    Flag::OutOfBounds,
    Flag::Overwritten,
    Flag::BadSectors,
    Flag::SignatureMismatch,
]);

/// Flags that make a file `uncertain` (unless something worse applies).
pub const UNCERTAIN_FLAGS: FlagSet = FlagSet::of(&[
    Flag::Deleted,
    Flag::Overlap,
    Flag::TornRecord,
    Flag::NoData,
    Flag::OlderGeneration,
    Flag::Stray,
]);

/// The rating of a file (ALGORITHMS G3, first match wins):
///
/// | rating | condition |
/// |---|---|
/// | `unsupported` | `compressed`, `encrypted`, `placeholder` or `embedded` |
/// | `damaged` | `missing_fragments`, `out_of_bounds`, `overwritten`, `bad_sectors` or `signature_mismatch` |
/// | `uncertain` | `deleted`, `overlap`, `torn_record`, `no_data`, `older_generation`, `stray`, geometry not validated, or carved without a verified end |
/// | `good` | otherwise |
///
/// `sparse`, `size_estimated` and `signature_ok` never change a rating by
/// themselves.
pub fn rate(flags: FlagSet, context: RatingContext) -> Rating {
    if flags.intersects(UNSUPPORTED_FLAGS) {
        Rating::Unsupported
    } else if flags.intersects(DAMAGED_FLAGS) {
        Rating::Damaged
    } else if flags.intersects(UNCERTAIN_FLAGS)
        || context.geometry_not_validated
        || context.carved_end_unverified
    {
        Rating::Uncertain
    } else {
        Rating::Good
    }
}

/// What keeps a file from `good`: the flags of the rating table that are
/// set, in canonical order, then the names of the conditions of `context`
/// that hold. Empty exactly when [`rate`] says `good`.
pub fn rating_reasons(flags: FlagSet, context: RatingContext) -> Vec<&'static str> {
    let relevant = flags.intersection(
        UNSUPPORTED_FLAGS
            .union(DAMAGED_FLAGS)
            .union(UNCERTAIN_FLAGS),
    );
    relevant
        .iter()
        .map(Flag::name)
        .chain(context.names())
        .collect()
}

#[cfg(test)]
mod tests;
