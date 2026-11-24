//! Where the content of one file is: [`FileSource`] and its [`Run`]s.

use serde::{Deserialize, Serialize};

/// One piece of a file's content, in file order.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Run {
    /// `len` bytes stored at absolute byte `offset` of the source.
    ///
    /// Any value is accepted: whatever part of the run lies outside the
    /// session range (or beyond the end of the address space) reads as
    /// zeros and is reported as `out_of_bounds`.
    Data {
        /// Absolute byte offset on the source (not on the volume).
        offset: u64,
        /// Length in bytes.
        len: u64,
    },
    /// `len` bytes that read as zeros: a sparse run, or clusters missing
    /// from the chain (for which the catalog sets `missing_fragments`).
    Hole {
        /// Length in bytes.
        len: u64,
    },
}

impl Run {
    /// Length of the run in bytes.
    pub const fn len(&self) -> u64 {
        match *self {
            Run::Data { len, .. } | Run::Hole { len } => len,
        }
    }

    /// Whether the run is empty.
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The run of `clusters` clusters at `lcn` (`None`: a sparse run) of a
    /// volume whose cluster 0 is at absolute byte `volume_offset`.
    ///
    /// Values from damaged records can be anything: an offset or a length
    /// that does not fit 64 bits saturates, which puts the run beyond every
    /// source, so it reads as zeros and is reported `out_of_bounds`.
    pub const fn from_clusters(
        lcn: Option<u64>,
        clusters: u64,
        cluster_size: u64,
        volume_offset: u64,
    ) -> Run {
        let len = clusters.saturating_mul(cluster_size);
        match lcn {
            Some(lcn) => Run::Data {
                offset: volume_offset.saturating_add(lcn.saturating_mul(cluster_size)),
                len,
            },
            None => Run::Hole { len },
        }
    }
}

/// Where the content of one file is stored (ARCHITECTURE 5.4).
///
/// `hds-core` builds these from the catalog; the content is read with
/// [`ContentReader`](crate::ContentReader) by the rules of ALGORITHMS H7.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum FileSource {
    /// The value of a resident `$DATA` attribute, inside the MFT record at
    /// `record_offset` (the base record or an extension record).
    Resident {
        /// Absolute byte offset of the record on the source.
        record_offset: u64,
        /// Size of the record in bytes (its allocated size: a multiple of
        /// 512 from 1024 to 65536).
        record_size: u32,
        /// Offset of the attribute value inside the record.
        value_offset: u32,
        /// Length of the attribute value in bytes: the file's size.
        value_len: u32,
        /// FNV-1a 64 of the record's bytes **as read by the scan**, before
        /// fixups ([`hds_ntfs::fnv1a64`]).
        hash: u64,
    },
    /// Non-resident content: runs on the source, in file order.
    Extents {
        /// The runs. Bytes of the file beyond the last run read as zeros
        /// and are reported as `missing_fragments`.
        runs: Vec<Run>,
        /// Size of the file in bytes; nothing beyond it is returned.
        real_size: u64,
        /// Bytes from here on read as zeros whatever the clusters hold
        /// (the NTFS "valid data length").
        initialized_size: u64,
    },
}

impl FileSource {
    /// Size of the content in bytes.
    pub fn size(&self) -> u64 {
        match self {
            FileSource::Resident { value_len, .. } => u64::from(*value_len),
            FileSource::Extents { real_size, .. } => *real_size,
        }
    }

    /// The content of a file of `real_size` bytes, of which the first
    /// `initialized_size` are valid, made of `runs` given in clusters as in
    /// a runlist (`(Some(lcn), clusters)` or `(None, clusters)` for a sparse
    /// run), on a volume with the given geometry. See
    /// [`Run::from_clusters`].
    pub fn from_clusters(
        runs: impl IntoIterator<Item = (Option<u64>, u64)>,
        cluster_size: u64,
        volume_offset: u64,
        real_size: u64,
        initialized_size: u64,
    ) -> FileSource {
        FileSource::Extents {
            runs: runs
                .into_iter()
                .map(|(lcn, clusters)| {
                    Run::from_clusters(lcn, clusters, cluster_size, volume_offset)
                })
                .collect(),
            real_size,
            initialized_size,
        }
    }

    /// The absolute offset of the first byte of content on the source: the
    /// first data run, or the record of resident content. Recovery
    /// processes files in ascending order of this offset (ALGORITHMS H5).
    /// `None` when no byte is stored anywhere (empty, or only holes).
    pub fn first_offset(&self) -> Option<u64> {
        match self {
            FileSource::Resident { record_offset, .. } => Some(*record_offset),
            FileSource::Extents { runs, .. } => runs.iter().find_map(|run| match *run {
                Run::Data { offset, len } if len > 0 => Some(offset),
                _ => None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_from_clusters_use_the_geometry_and_saturate() {
        assert_eq!(
            Run::from_clusters(Some(10), 3, 4096, 1 << 20),
            Run::Data {
                offset: (1 << 20) + 40960,
                len: 12288
            }
        );
        assert_eq!(
            Run::from_clusters(None, 3, 4096, 1 << 20),
            Run::Hole { len: 12288 }
        );
        assert_eq!(
            Run::from_clusters(Some(u64::MAX / 2), u64::MAX, 4096, 512),
            Run::Data {
                offset: u64::MAX,
                len: u64::MAX
            }
        );
        assert!(Run::Hole { len: 0 }.is_empty());
    }

    #[test]
    fn sizes_and_first_offsets() {
        let resident = FileSource::Resident {
            record_offset: 4096,
            record_size: 1024,
            value_offset: 0x98,
            value_len: 600,
            hash: 7,
        };
        assert_eq!(resident.size(), 600);
        assert_eq!(resident.first_offset(), Some(4096));
        let sparse = FileSource::from_clusters([(None, 4), (Some(2), 1)], 512, 0, 2560, 2560);
        assert_eq!(sparse.size(), 2560);
        assert_eq!(sparse.first_offset(), Some(1024));
        let holes = FileSource::from_clusters([(None, 4)], 512, 0, 2048, 0);
        assert_eq!(holes.first_offset(), None);
        let json = serde_json::to_string(&sparse).unwrap();
        assert_eq!(
            json,
            r#"{"extents":{"runs":[{"hole":{"len":2048}},{"data":{"offset":1024,"len":512}}],"real_size":2560,"initialized_size":2560}}"#
        );
        assert_eq!(serde_json::from_str::<FileSource>(&json).unwrap(), sparse);
    }
}
