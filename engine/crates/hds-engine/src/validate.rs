//! Argument checks that do not depend on the engine's state.
//!
//! Every function answers `bad_request` with a message that names the
//! offending member.

use std::collections::HashSet;
use std::ops::Range;

use hds_carve::FileKind;

use crate::protocol::{Filter, ProtocolError, MAX_PAGE, MAX_READ_LENGTH};

/// Largest cluster size `geometry_set` accepts (2 MiB).
pub const MAX_CLUSTER_SIZE: u64 = 2 << 20;

/// A byte range for `scan_start` / `image_start`: `start < end <= size`,
/// both multiples of `sector_size`.
pub fn byte_range(
    start: u64,
    end: Option<u64>,
    size: u64,
    sector_size: u32,
) -> Result<Range<u64>, ProtocolError> {
    let end = end.unwrap_or(size);
    let sector = u64::from(sector_size.max(1));
    if !start.is_multiple_of(sector) || !end.is_multiple_of(sector) {
        return Err(ProtocolError::bad_request(format!(
            "start ({start}) and end ({end}) must be multiples of the sector size ({sector} bytes)"
        )));
    }
    if end > size {
        return Err(ProtocolError::bad_request(format!(
            "end ({end}) lies beyond the end of the source ({size} bytes)"
        )));
    }
    if start >= end {
        return Err(ProtocolError::bad_request(format!(
            "the range {start}..{end} is empty"
        )));
    }
    Ok(start..end)
}

/// A page size: at most [`MAX_PAGE`].
pub fn limit(limit: u32) -> Result<(), ProtocolError> {
    if limit > MAX_PAGE {
        return Err(ProtocolError::bad_request(format!(
            "limit ({limit}) must not exceed {MAX_PAGE}"
        )));
    }
    Ok(())
}

/// A `read_file` length: at most [`MAX_READ_LENGTH`].
pub fn read_length(length: u64) -> Result<(), ProtocolError> {
    if length > MAX_READ_LENGTH {
        return Err(ProtocolError::bad_request(format!(
            "length ({length}) must not exceed {MAX_READ_LENGTH}"
        )));
    }
    Ok(())
}

/// A list filter: `null` means any, an empty list is refused.
pub fn non_empty<T>(member: &str, list: &Option<Vec<T>>) -> Result<(), ProtocolError> {
    match list {
        Some(items) if items.is_empty() => Err(ProtocolError::bad_request(format!(
            "{member} must be null (any) or a non-empty list"
        ))),
        _ => Ok(()),
    }
}

/// Extensions of a filter: lower case without the dot (`""` for none).
/// Upper-case ASCII letters are folded; an entry containing a dot or a path
/// separator can never match and is refused.
pub fn exts(member: &str, exts: &mut Option<Vec<String>>) -> Result<(), ProtocolError> {
    non_empty(member, exts)?;
    if let Some(items) = exts {
        for ext in items.iter_mut() {
            if ext.contains(['.', '/', '\\']) {
                return Err(ProtocolError::bad_request(format!(
                    "{member}: \"{ext}\" is not an extension (lower case, without the dot)"
                )));
            }
            ext.make_ascii_lowercase();
        }
    }
    Ok(())
}

/// A `Filter`: non-empty lists, extensions as by [`exts`], `min_size <= max_size`.
pub fn filter(member: &str, filter: &mut Filter) -> Result<(), ProtocolError> {
    exts(&format!("{member}.exts"), &mut filter.exts)?;
    non_empty(&format!("{member}.ratings"), &filter.ratings)?;
    if let (Some(min), Some(max)) = (filter.min_size, filter.max_size) {
        if min > max {
            return Err(ProtocolError::bad_request(format!(
                "{member}.min_size ({min}) exceeds {member}.max_size ({max})"
            )));
        }
    }
    Ok(())
}

/// `include` / `exclude` of a selection: a node may not be in both lists.
/// With `require_include`, `include` must not be empty (an empty selection).
pub fn selection(
    include: &[u64],
    exclude: &[u64],
    require_include: bool,
) -> Result<(), ProtocolError> {
    if require_include && include.is_empty() {
        return Err(ProtocolError::bad_request(
            "the selection is empty: include names no node",
        ));
    }
    let included: HashSet<u64> = include.iter().copied().collect();
    if let Some(both) = exclude.iter().find(|id| included.contains(id)) {
        return Err(ProtocolError::bad_request(format!(
            "node {both} is in both include and exclude"
        )));
    }
    Ok(())
}

/// Carve kinds: `null` = every kind; otherwise a non-empty list of names of
/// `hello.carve_kinds`, duplicates removed, in the given order.
pub fn carve_kinds(kinds: &Option<Vec<String>>) -> Result<Vec<FileKind>, ProtocolError> {
    let Some(names) = kinds else {
        return Ok(FileKind::all().to_vec());
    };
    non_empty("kinds", kinds)?;
    let mut out: Vec<FileKind> = Vec::new();
    for name in names {
        let kind = FileKind::from_name(name).ok_or_else(|| {
            ProtocolError::bad_request(format!("kinds: \"{name}\" is not a carve kind"))
        })?;
        if !out.contains(&kind) {
            out.push(kind);
        }
    }
    Ok(out)
}

/// A `geometry_set` geometry: cluster size a power of two from 512 to
/// 2 MiB, volume offset a multiple of 512 inside the source.
pub fn geometry(
    cluster_size: u64,
    volume_offset: u64,
    source_size: u64,
) -> Result<(), ProtocolError> {
    if !cluster_size.is_power_of_two() || !(512..=MAX_CLUSTER_SIZE).contains(&cluster_size) {
        return Err(ProtocolError::bad_request(format!(
            "cluster_size ({cluster_size}) must be a power of two from 512 to {MAX_CLUSTER_SIZE}"
        )));
    }
    if !volume_offset.is_multiple_of(512) || volume_offset >= source_size {
        return Err(ProtocolError::bad_request(format!(
            "volume_offset ({volume_offset}) must be a multiple of 512 inside the source ({source_size} bytes)"
        )));
    }
    Ok(())
}

/// A path member: not empty.
pub fn path(member: &str, path: &str) -> Result<(), ProtocolError> {
    if path.is_empty() {
        return Err(ProtocolError::bad_request(format!("{member} is empty")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ErrorCode, Rating};

    fn is_bad<T: std::fmt::Debug>(r: Result<T, ProtocolError>) -> bool {
        matches!(
            r,
            Err(ProtocolError {
                code: ErrorCode::BadRequest,
                ..
            })
        )
    }

    #[test]
    fn ranges() {
        assert_eq!(byte_range(0, None, 4096, 512).unwrap(), 0..4096);
        assert_eq!(byte_range(512, Some(1024), 4096, 512).unwrap(), 512..1024);
        assert!(is_bad(byte_range(1, None, 4096, 512)));
        assert!(is_bad(byte_range(0, Some(1000), 4096, 512)));
        assert!(is_bad(byte_range(512, Some(4096), 4096, 4096)));
        assert!(is_bad(byte_range(0, Some(8192), 4096, 512)));
        assert!(is_bad(byte_range(1024, Some(1024), 4096, 512)));
        assert!(is_bad(byte_range(2048, Some(1024), 4096, 512)));
        assert!(is_bad(byte_range(u64::MAX - 511, None, 4096, 512)));
    }

    #[test]
    fn filters() {
        let mut f = Filter {
            exts: Some(vec!["JPG".into(), "".into()]),
            ..Filter::default()
        };
        filter("filter", &mut f).unwrap();
        assert_eq!(f.exts, Some(vec!["jpg".to_string(), String::new()]));
        for mut bad in [
            Filter {
                exts: Some(vec![]),
                ..Filter::default()
            },
            Filter {
                exts: Some(vec![".jpg".into()]),
                ..Filter::default()
            },
            Filter {
                ratings: Some(vec![]),
                ..Filter::default()
            },
            Filter {
                min_size: Some(2),
                max_size: Some(1),
                ..Filter::default()
            },
        ] {
            assert!(is_bad(filter("filter", &mut bad)), "{bad:?}");
        }
        let mut ok = Filter {
            ratings: Some(vec![Rating::Good]),
            min_size: Some(1),
            max_size: Some(1),
            ..Filter::default()
        };
        filter("filter", &mut ok).unwrap();
    }

    #[test]
    fn selections_and_kinds() {
        selection(&[1, 2], &[3], true).unwrap();
        selection(&[], &[], false).unwrap();
        assert!(is_bad(selection(&[], &[], true)));
        assert!(is_bad(selection(&[1, 2], &[2], false)));
        assert_eq!(carve_kinds(&None).unwrap().len(), FileKind::all().len());
        assert_eq!(
            carve_kinds(&Some(vec!["jpeg".into(), "jpeg".into(), "7z".into()])).unwrap(),
            vec![FileKind::Jpeg, FileKind::SevenZip]
        );
        assert!(is_bad(carve_kinds(&Some(vec![]))));
        assert!(is_bad(carve_kinds(&Some(vec!["JPEG".into()]))));
    }

    #[test]
    fn geometries_and_limits() {
        geometry(512, 0, 4096).unwrap();
        geometry(MAX_CLUSTER_SIZE, 3584, 4096).unwrap();
        assert!(is_bad(geometry(256, 0, 4096)));
        assert!(is_bad(geometry(3000, 0, 4096)));
        assert!(is_bad(geometry(MAX_CLUSTER_SIZE * 2, 0, 4096)));
        assert!(is_bad(geometry(4096, 100, 4096)));
        assert!(is_bad(geometry(4096, 4096, 4096)));
        limit(1000).unwrap();
        assert!(is_bad(limit(1001)));
        read_length(MAX_READ_LENGTH).unwrap();
        assert!(is_bad(read_length(MAX_READ_LENGTH + 1)));
        assert!(is_bad(path("dest", "")));
    }
}
