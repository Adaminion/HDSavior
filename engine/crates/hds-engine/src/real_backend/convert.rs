//! Field-by-field conversions between `hds-core` / `hds-recover` values and
//! the protocol types of [`crate::protocol`].
//!
//! The wire names of flags, ratings, kinds and sources are the same on both
//! sides (both follow PROTOCOL.md); the conversions are explicit matches so
//! that a variant added on one side fails to compile here instead of being
//! sent under a wrong name.

use hds_core::catalog::NodeKind as CoreNodeKind;
use hds_core::geometry::{
    ClusterSource as CoreClusterSource, Geometry as CoreGeometry, OffsetSource as CoreOffsetSource,
};
use hds_core::instances::InstanceKind as CoreInstanceKind;
use hds_core::query::{
    CatalogSummary, Filter as CoreFilter, InstanceSummary, NodeDetails, Page, QueryError,
    Row as CoreRow, SearchRow as CoreSearchRow, SelectionSummary as CoreSelectionSummary,
    SortKey as CoreSortKey,
};
use hds_core::scan::{ScanPhase as CorePhase, ScanProgress as CoreScanProgress};
use hds_core::session::SessionError;
use hds_io::{ProtectedReason, SourceInfo, Warning};
use hds_recover::{Flag as CoreFlag, FlagSet, Rating as CoreRating};

use crate::protocol::{
    Ancestor, ByteRange, CatalogInfo, ClusterSource, DataInfo, ErrorCode, Filter, Flag, FlagDetail,
    Geometry, Instance, InstanceKind, LiveVolume, NodeInfo, NodeKind, OffsetSource, ProtectedRange,
    ProtocolError, Rating, RecordInfo, Row, RowsPage, ScanPhase, ScanProgress, SearchPage,
    SearchRow, SelectionSummary, SortKey, VirtualFolders,
};

/// The protocol error code named `code` (an `hds-core` `code()` string);
/// `internal` for a name the protocol does not have.
pub(crate) fn error_code(code: &str) -> ErrorCode {
    serde_json::from_value(serde_json::Value::String(code.to_string()))
        .unwrap_or(ErrorCode::Internal)
}

/// A query error as a protocol error.
pub(crate) fn query_error(error: &QueryError) -> ProtocolError {
    ProtocolError::new(error_code(error.code()), error.to_string())
}

/// A session error as a protocol error; a refusal by the guard carries the
/// check as `data`, as every `destination_refused` does.
pub(crate) fn session_error(error: &SessionError) -> ProtocolError {
    let code = match error {
        SessionError::Io(e) if e.kind() == std::io::ErrorKind::NotFound => ErrorCode::NotFound,
        SessionError::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            ErrorCode::AccessDenied
        }
        _ => error_code(error.code()),
    };
    let mut e = ProtocolError::new(code, error.to_string());
    if let SessionError::DestinationRefused(check) = error {
        if let Ok(data) = serde_json::to_value(check.as_ref()) {
            e = e.with_data(data);
        }
    }
    e
}

pub(crate) fn flag(f: CoreFlag) -> Flag {
    match f {
        CoreFlag::Deleted => Flag::Deleted,
        CoreFlag::TornRecord => Flag::TornRecord,
        CoreFlag::MissingFragments => Flag::MissingFragments,
        CoreFlag::SizeEstimated => Flag::SizeEstimated,
        CoreFlag::OutOfBounds => Flag::OutOfBounds,
        CoreFlag::Overwritten => Flag::Overwritten,
        CoreFlag::Overlap => Flag::Overlap,
        CoreFlag::Compressed => Flag::Compressed,
        CoreFlag::Encrypted => Flag::Encrypted,
        CoreFlag::Placeholder => Flag::Placeholder,
        CoreFlag::Sparse => Flag::Sparse,
        CoreFlag::NoData => Flag::NoData,
        CoreFlag::BadSectors => Flag::BadSectors,
        CoreFlag::OlderGeneration => Flag::OlderGeneration,
        CoreFlag::Embedded => Flag::Embedded,
        CoreFlag::Stray => Flag::Stray,
        CoreFlag::SignatureOk => Flag::SignatureOk,
        CoreFlag::SignatureMismatch => Flag::SignatureMismatch,
    }
}

pub(crate) fn flags(set: FlagSet) -> Vec<Flag> {
    set.iter().map(flag).collect()
}

pub(crate) fn rating(r: CoreRating) -> Rating {
    match r {
        CoreRating::Good => Rating::Good,
        CoreRating::Uncertain => Rating::Uncertain,
        CoreRating::Damaged => Rating::Damaged,
        CoreRating::Unsupported => Rating::Unsupported,
    }
}

pub(crate) fn core_rating(r: Rating) -> CoreRating {
    match r {
        Rating::Good => CoreRating::Good,
        Rating::Uncertain => CoreRating::Uncertain,
        Rating::Damaged => CoreRating::Damaged,
        Rating::Unsupported => CoreRating::Unsupported,
    }
}

pub(crate) fn core_ratings(list: &Option<Vec<Rating>>) -> Option<Vec<CoreRating>> {
    list.as_ref()
        .map(|l| l.iter().copied().map(core_rating).collect())
}

pub(crate) fn core_sort(s: SortKey) -> CoreSortKey {
    match s {
        SortKey::Name => CoreSortKey::Name,
        SortKey::Size => CoreSortKey::Size,
        SortKey::Modified => CoreSortKey::Modified,
        SortKey::Rating => CoreSortKey::Rating,
        SortKey::Ext => CoreSortKey::Ext,
    }
}

pub(crate) fn core_filter(f: &Filter) -> CoreFilter {
    CoreFilter {
        query: f.query.clone(),
        exts: f.exts.clone(),
        min_size: f.min_size,
        max_size: f.max_size,
        ratings: core_ratings(&f.ratings),
    }
}

pub(crate) fn node_kind(k: CoreNodeKind) -> NodeKind {
    match k {
        CoreNodeKind::Dir => NodeKind::Dir,
        CoreNodeKind::File => NodeKind::File,
        CoreNodeKind::LostDir => NodeKind::LostDir,
        CoreNodeKind::Virtual => NodeKind::Virtual,
        CoreNodeKind::Carved => NodeKind::Carved,
    }
}

pub(crate) fn instance_kind(k: CoreInstanceKind) -> InstanceKind {
    match k {
        CoreInstanceKind::Main => InstanceKind::Main,
        CoreInstanceKind::Older => InstanceKind::Older,
        CoreInstanceKind::Other => InstanceKind::Other,
        CoreInstanceKind::Embedded => InstanceKind::Embedded,
        CoreInstanceKind::Strays => InstanceKind::Strays,
    }
}

fn cluster_source(s: CoreClusterSource) -> ClusterSource {
    match s {
        CoreClusterSource::Attr => ClusterSource::Attr,
        CoreClusterSource::Boot => ClusterSource::Boot,
        CoreClusterSource::Default => ClusterSource::Default,
        CoreClusterSource::Manual => ClusterSource::Manual,
    }
}

fn offset_source(s: CoreOffsetSource) -> OffsetSource {
    match s {
        CoreOffsetSource::Mft => OffsetSource::Mft,
        CoreOffsetSource::Boot => OffsetSource::Boot,
        CoreOffsetSource::BackupBoot => OffsetSource::BackupBoot,
        CoreOffsetSource::Default => OffsetSource::Default,
        CoreOffsetSource::Index => OffsetSource::Index,
        CoreOffsetSource::Signature => OffsetSource::Signature,
        CoreOffsetSource::Inherited => OffsetSource::Inherited,
        CoreOffsetSource::Manual => OffsetSource::Manual,
    }
}

pub(crate) fn geometry(g: &CoreGeometry) -> Geometry {
    Geometry {
        cluster_size: g.cluster_size,
        volume_offset: g.volume_offset,
        cluster_source: cluster_source(g.cluster_source),
        offset_source: offset_source(g.offset_source),
        index_dirs: g.index_dirs,
        index_blocks: g.index_blocks,
        signature_checked: g.signature_checked,
        signature_ok: g.signature_ok,
        validated: g.validated,
        total_clusters: g.total_clusters,
    }
}

pub(crate) fn instance(i: &InstanceSummary) -> Instance {
    Instance {
        id: i.id,
        kind: instance_kind(i.kind),
        manual: i.manual,
        label: i.label.clone(),
        root: i.root.wire(),
        records: i.records,
        files: i.files,
        dirs: i.dirs,
        deleted_files: i.deleted_files,
        lost_dirs: i.lost_dirs,
        bytes: i.bytes,
        time_from: i.time_from,
        time_to: i.time_to,
        embedded_in: i.embedded_in.clone(),
        geometry: geometry(&i.geometry),
        geometry_candidates: i.geometry_candidates.iter().map(geometry).collect(),
        virtual_folders: VirtualFolders {
            lost: i.lost.map(|n| n.wire()),
            orphans: i.orphans.map(|n| n.wire()),
            carved: i.carved.map(|n| n.wire()),
        },
    }
}

/// `CatalogInfo` from a catalog summary. The epoch is the engine's; it is
/// left 0 here.
pub(crate) fn catalog_info(
    s: &CatalogSummary,
    source_attached: bool,
    extra: &[Warning],
) -> CatalogInfo {
    let mut warnings = s.warnings.clone();
    for w in extra {
        if !warnings.iter().any(|x| x.code == w.code) {
            warnings.push(w.clone());
        }
    }
    CatalogInfo {
        epoch: 0,
        instances: s.instances.iter().map(instance).collect(),
        nodes: s.nodes,
        live_records: s.live_records,
        post_format_records: s.post_format_records,
        live_volumes: s
            .live_volumes
            .iter()
            .map(|v| LiveVolume {
                offset: v.offset,
                size: v.size,
                format_time: v.format_time,
                snapshot_time: v.snapshot_time,
            })
            .collect(),
        range: ByteRange::from(s.range.clone()),
        protected: s
            .protected
            .iter()
            .filter(|p| p.range.start < p.range.end || p.reason == ProtectedReason::SessionRange)
            .map(|p| ProtectedRange {
                start: p.range.start,
                end: p.range.end,
                reason: p.reason,
            })
            .collect(),
        scanned_bytes: s.scanned_bytes,
        scan_complete: s.scan_complete,
        bad_bytes: s.bad_bytes,
        out_of_range_files: s.out_of_range_files,
        out_of_range_bytes: s.out_of_range_bytes,
        carve_hits: s.carve_hits,
        carved: s.carved,
        source_attached,
        warnings,
    }
}

pub(crate) fn row(r: &CoreRow) -> Row {
    Row {
        id: r.id.wire(),
        parent: r.parent.map(|p| p.wire()),
        name: r.name.clone(),
        kind: node_kind(r.kind),
        ext: r.ext.clone(),
        size: r.size,
        modified: r.modified,
        created: r.created,
        deleted: r.deleted,
        rating: r.rating.map(rating),
        flags: flags(r.flags),
        files: r.files,
        deleted_files: r.deleted_files,
        deleted_size: r.deleted_size,
        has_children: r.has_children,
    }
}

pub(crate) fn rows_page(p: &Page<CoreRow>) -> RowsPage {
    RowsPage {
        total: p.total,
        rows: p.rows.iter().map(row).collect(),
    }
}

pub(crate) fn search_page(p: &Page<CoreSearchRow>) -> SearchPage {
    SearchPage {
        total: p.total,
        rows: p
            .rows
            .iter()
            .map(|r| SearchRow {
                row: row(&r.row),
                path: r.path.clone(),
                ancestor_ids: r.ancestor_ids.iter().map(|a| a.wire()).collect(),
            })
            .collect(),
    }
}

pub(crate) fn node_info(d: &NodeDetails) -> NodeInfo {
    NodeInfo {
        row: row(&d.row),
        ancestors: d
            .ancestors
            .iter()
            .map(|(id, name)| Ancestor {
                id: id.wire(),
                name: name.clone(),
            })
            .collect(),
        path: d.path.clone(),
        instance: d.instance,
        accessed: d.accessed,
        other_names: d.other_names.clone(),
        record: d.record.as_ref().map(|r| RecordInfo {
            number: r.number,
            seq: r.seq,
            disk_offset: r.disk_offset,
            extension_records: r.extension_records,
        }),
        data: d.data.as_ref().map(|x| DataInfo {
            resident: x.resident,
            real_size: x.real_size,
            initialized_size: x.initialized_size,
            allocated_size: x.allocated_size,
            runs: x.runs,
            first_lcn: x.first_lcn,
            first_offset: x.first_offset,
        }),
        flag_details: d
            .flag_details
            .iter()
            .map(|(f, text)| FlagDetail {
                flag: flag(*f),
                text: (*text).to_string(),
            })
            .collect(),
    }
}

pub(crate) fn selection_summary(s: &CoreSelectionSummary) -> SelectionSummary {
    SelectionSummary {
        files: s.files,
        bytes: s.bytes,
        dirs: s.dirs,
        good: s.good,
        uncertain: s.uncertain,
        damaged: s.damaged,
        unsupported: s.unsupported,
    }
}

pub(crate) fn scan_progress(p: &CoreScanProgress) -> ScanProgress {
    ScanProgress {
        phase: match p.phase {
            CorePhase::MftProbe => ScanPhase::MftProbe,
            CorePhase::Sweep => ScanPhase::Sweep,
        },
        bytes_done: p.bytes_done,
        bytes_total: p.bytes_total,
        position: p.position,
        speed_bps: p.speed_bps,
        eta_seconds: p.eta_seconds,
        elapsed_seconds: p.elapsed_seconds,
        records: p.records,
        files: p.files,
        dirs: p.dirs,
        index_blocks: p.index_blocks,
        carve_hits: p.carve_hits,
        bad_bytes: p.bad_bytes,
        warnings: p.warnings.clone(),
    }
}

/// The `SourceInfo` of a session's recorded source (not attached).
pub(crate) fn recorded_source(id: &hds_core::session::SourceIdentity) -> SourceInfo {
    SourceInfo {
        kind: id.kind,
        path: id.path.clone(),
        size: id.size,
        sector_size: id.sector_size,
        physical_sector_size: id.physical_sector_size,
        model: id.model.clone(),
        serial: id.serial.clone(),
        disk_number: None,
    }
}

/// A path for display: without the `\\?\` prefix Windows' canonical paths
/// carry (`\\?\UNC\server\share` becomes `\\server\share`).
pub fn display_path(path: &std::path::Path) -> String {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
}
