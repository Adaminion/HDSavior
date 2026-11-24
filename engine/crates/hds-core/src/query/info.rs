//! `catalog_info` (PROTOCOL `CatalogInfo`) and per-instance statistics.

use std::ops::Range;

use hds_io::{Protected, ProtectedReason, Warning};

use crate::catalog::{Catalog, NodeId, NodeKind};
use crate::geometry::Geometry;
use crate::instances::{InstanceKind, Layout, WARN_MIXED_GENERATIONS};
use crate::live::{WARN_LIVE_VOLUME_INCOMPLETE, WARN_WRITES_NOT_DETECTABLE};
use crate::session::Session;

use super::filetime_to_unix_ms;

/// What `post_format_records` means, for the UI (PROTOCOL `CatalogInfo`).
pub const POST_FORMAT_TEXT: &str = "Files that were created and deleted again on the current \
file system after the format. They are not shown, but what they occupied counts as overwritten.";

/// One instance (PROTOCOL `Instance`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceSummary {
    /// Catalog instance id (`instances[0]` is main).
    pub id: u32,
    /// Kind.
    pub kind: InstanceKind,
    /// The kind was set by the user.
    pub manual: bool,
    /// Label (`""` for main).
    pub label: String,
    /// Root node.
    pub root: NodeId,
    /// Records.
    pub records: u64,
    /// Non-deleted files.
    pub files: u64,
    /// Non-deleted folders with a record.
    pub dirs: u64,
    /// Deleted files.
    pub deleted_files: u64,
    /// Lost folders.
    pub lost_dirs: u64,
    /// Bytes of the non-deleted files.
    pub bytes: u64,
    /// Oldest record time, Unix ms.
    pub time_from: Option<i64>,
    /// Newest record time, Unix ms.
    pub time_to: Option<i64>,
    /// Display path of the containing file.
    pub embedded_in: Option<String>,
    /// Geometry in effect.
    pub geometry: Geometry,
    /// Candidates.
    pub geometry_candidates: Vec<Geometry>,
    /// `[Lost folders]`.
    pub lost: Option<NodeId>,
    /// `[Orphaned files]`.
    pub orphans: Option<NodeId>,
    /// `[Carved files]`.
    pub carved: Option<NodeId>,
}

/// A current volume (PROTOCOL `LiveVolume`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveVolumeSummary {
    /// Offset.
    pub offset: u64,
    /// Size in bytes.
    pub size: u64,
    /// Format time, Unix ms.
    pub format_time: Option<i64>,
    /// Snapshot time, Unix ms.
    pub snapshot_time: Option<i64>,
}

/// A protected range with its reason (PROTOCOL `CatalogInfo.protected`).
pub type ProtectedSummary = Protected;

/// `catalog_info` without the two members the engine owns (`epoch`,
/// `source_attached`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogSummary {
    /// Instances, main first.
    pub instances: Vec<InstanceSummary>,
    /// Nodes of all roots.
    pub nodes: u64,
    /// Records of current volumes.
    pub live_records: u64,
    /// Records created and deleted after the format ([`POST_FORMAT_TEXT`]).
    pub post_format_records: u64,
    /// Current volumes.
    pub live_volumes: Vec<LiveVolumeSummary>,
    /// Session range.
    pub range: Range<u64>,
    /// Protected ranges: the session range, then one `lost_volume` per
    /// lost file system with validated geometry (and the anchors').
    pub protected: Vec<ProtectedSummary>,
    /// Bytes scanned.
    pub scanned_bytes: u64,
    /// The whole range was scanned.
    pub scan_complete: bool,
    /// Unreadable bytes.
    pub bad_bytes: u64,
    /// Files with data outside the session range.
    pub out_of_range_files: u64,
    /// Their bytes.
    pub out_of_range_bytes: u64,
    /// Stored signature hits.
    pub carve_hits: u64,
    /// Carved files.
    pub carved: u64,
    /// Warnings.
    pub warnings: Vec<Warning>,
}

fn message(code: &str) -> &'static str {
    match code {
        WARN_MIXED_GENERATIONS => {
            "Records of an older file system could not be separated cleanly; they are marked \
             older_generation and rated at most uncertain."
        }
        WARN_LIVE_VOLUME_INCOMPLETE => {
            "Part of the current file system could not be read; what it overwrote may be \
             incomplete."
        }
        WARN_WRITES_NOT_DETECTABLE => {
            "The range holds a file system that is not NTFS (or one that could not be read): \
             writes since the format cannot be detected."
        }
        "out_of_range_files" => {
            "Some files have data outside the scanned range. Scan the whole disk to reach them."
        }
        _ => "",
    }
}

impl Catalog {
    /// Per catalog instance: (files, dirs, deleted files, lost dirs,
    /// bytes).
    fn instance_counts(&self, ci: usize) -> [u64; 5] {
        let mut c = [0u64; 5];
        let Some(inst) = self.instances.get(ci) else {
            return c;
        };
        for r in self.subtree(inst.root.0) {
            for id in r {
                let Some(n) = self.rec(id) else { continue };
                let deleted = n.deleted();
                let slot = match (n.kind, deleted) {
                    (NodeKind::File | NodeKind::Carved, false) => {
                        c[4] = c[4].saturating_add(self.numbers(id).0);
                        0
                    }
                    (NodeKind::Dir, false) => 1,
                    (NodeKind::File | NodeKind::Carved, true) => 2,
                    (NodeKind::LostDir, _) => 3,
                    _ => continue,
                };
                if let Some(x) = c.get_mut(slot) {
                    *x = x.saturating_add(1);
                }
            }
        }
        c
    }

    /// The `Instance` objects.
    pub fn instance_summaries(&self) -> Vec<InstanceSummary> {
        self.instances
            .iter()
            .enumerate()
            .map(|(k, i)| {
                let [files, dirs, deleted_files, lost_dirs, bytes] = self.instance_counts(k);
                InstanceSummary {
                    id: i.id,
                    kind: i.kind,
                    manual: i.manual,
                    label: i.label.clone(),
                    root: i.root,
                    records: i.records,
                    files,
                    dirs,
                    deleted_files,
                    lost_dirs,
                    bytes,
                    time_from: i.times.and_then(|t| filetime_to_unix_ms(t.0)),
                    time_to: i.times.and_then(|t| filetime_to_unix_ms(t.1)),
                    embedded_in: i.embedded_in.clone(),
                    geometry: i.geometry.clone(),
                    geometry_candidates: i.candidates.clone(),
                    lost: i.lost,
                    orphans: i.orphans,
                    carved: i.carved,
                }
            })
            .collect()
    }

    /// `catalog_info` (without `epoch` and `source_attached`).
    pub fn summary(&self, session: &Session, layout: &Layout) -> CatalogSummary {
        let store = session.store();
        let live = layout.live();
        let mut warnings: Vec<Warning> = store.warnings().to_vec();
        let mut add = |code: &str| {
            if !warnings.iter().any(|w| w.code == code) {
                warnings.push(Warning {
                    code: code.to_string(),
                    message: message(code).to_string(),
                });
            }
        };
        for w in layout.warnings() {
            add(w);
        }
        let (oor_files, oor_bytes) = self.health.out_of_range;
        if oor_files > 0 {
            add("out_of_range_files");
        }
        let snapshot = layout.snapshot_time().and_then(filetime_to_unix_ms);
        let mut protected: Vec<Protected> = layout.protected().to_vec();
        if protected.first().map(|p| p.reason) != Some(ProtectedReason::SessionRange) {
            protected.insert(0, Protected::session(layout.range().clone()));
        }
        CatalogSummary {
            instances: self.instance_summaries(),
            nodes: self.node_count(),
            live_records: live.live_records(),
            post_format_records: live.post_format_records(),
            live_volumes: layout
                .live_volumes()
                .iter()
                .map(|v| LiveVolumeSummary {
                    offset: v.offset,
                    size: v.size,
                    format_time: filetime_to_unix_ms(v.format_time),
                    snapshot_time: snapshot,
                })
                .collect(),
            range: layout.range().clone(),
            protected,
            scanned_bytes: store.done().total_len(),
            scan_complete: session.is_complete(),
            bad_bytes: store.bad_ranges().total_len(),
            out_of_range_files: oor_files,
            out_of_range_bytes: oor_bytes,
            carve_hits: u64::try_from(store.carve_hit_count()).unwrap_or(u64::MAX),
            carved: u64::try_from(self.carved.entries.iter().flatten().count()).unwrap_or(0),
            warnings,
        }
    }
}
