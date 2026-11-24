//! Output names (ALGORITHMS H4): which name each entry of an output
//! directory gets, before any data is read.
//!
//! Rules 1 to 4 and 6 are `hds_io`'s sanitizer ([`sanitize_name`],
//! [`fit_component`]); this module adds rule 5, the order in which entries
//! claim names, and the bookkeeping that keeps names unique for the whole
//! run: a name taken by an entry of this run is never handed out again in
//! that directory, compared without regard to case.

use std::collections::{HashMap, HashSet};

use hds_io::{fit_component, sanitize_name, ComponentLimit};
use hds_ntfs::fnv1a64;

/// Key of the destination directory itself in name hashes.
pub(crate) const ROOT_KEY: u64 = u64::MAX;

/// How many collision suffixes are tried for one name before giving up.
pub(crate) const MAX_NAME_ATTEMPTS: u32 = 10_000;

/// One entry that needs a name in a directory.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Claim<'a> {
    /// The name as NTFS stored it.
    pub raw: &'a [u16],
    /// In-use entries claim before deleted ones.
    pub in_use: bool,
    /// Then lower record numbers first; entries without one come last.
    pub record: Option<u64>,
}

/// The name an entry got: the final component and the collision suffix it
/// carries (1 = none), so that a later retry continues from there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Assigned {
    pub name: Box<str>,
    pub suffix: u32,
}

/// Unicode upper case of one UTF-16 unit's character, as far as it maps to
/// exactly one character of the Basic Multilingual Plane: close to the
/// `$UpCase` table NTFS compares names with. (The destination has the last
/// word: a name it still finds taken is retried with the next suffix.)
fn upcase(c: char) -> char {
    if u32::from(c) > 0xFFFF {
        return c;
    }
    let mut upper = c.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(u), None) if u32::from(u) <= 0xFFFF => u,
        _ => c,
    }
}

/// `name` folded for case-insensitive comparison, as UTF-16 units.
pub(crate) fn fold(name: &str) -> Vec<u16> {
    let mut out = Vec::with_capacity(name.len());
    let mut buf = [0u16; 2];
    for c in name.chars() {
        out.extend_from_slice(upcase(c).encode_utf16(&mut buf));
    }
    out
}

fn hash_parts(domain: u8, dir: u64, bytes: impl Iterator<Item = u8>) -> u64 {
    let mut data: Vec<u8> = Vec::with_capacity(64);
    data.push(domain);
    data.extend_from_slice(&dir.to_le_bytes());
    data.extend(bytes);
    fnv1a64(&data)
}

/// Hash of a name in directory `dir`, without regard to case: the key of
/// the set of names this run has claimed.
pub(crate) fn folded_key(dir: u64, name: &str) -> u64 {
    hash_parts(b'F', dir, fold(name).into_iter().flat_map(u16::to_le_bytes))
}

/// Hash of a name in directory `dir`, exactly as given: the key of the set
/// of entries this run has created. (A collision between two hashes only
/// ever costs an extra ` (n)`: it can make a name look taken, never free.)
pub(crate) fn exact_key(dir: u64, name: &str) -> u64 {
    hash_parts(b'E', dir, name.bytes())
}

/// The suffix after `suffix` (1 = none, then 2, 3, ...); `None` when the
/// numbers run out.
pub(crate) fn next_suffix(suffix: u32) -> Option<u32> {
    if suffix < 2 {
        Some(2)
    } else {
        suffix.checked_add(1)
    }
}

/// Length of `name` in UTF-16 units.
pub(crate) fn units(name: &str) -> usize {
    name.encode_utf16().count()
}

/// The first variant of `base` (already sanitized, rules 1 to 4) from
/// suffix `suffix` on whose name is not claimed in `dir` yet; it is claimed.
/// `None` after [`MAX_NAME_ATTEMPTS`] taken names.
pub(crate) fn claim_from(
    claimed: &mut HashSet<u64>,
    dir: u64,
    base: &[u16],
    mut suffix: u32,
    limit: ComponentLimit,
) -> Option<Assigned> {
    for _ in 0..MAX_NAME_ATTEMPTS {
        let name = String::from_utf16_lossy(&fit_component(base, suffix, limit));
        if claimed.insert(folded_key(dir, &name)) {
            return Some(Assigned {
                name: name.into_boxed_str(),
                suffix,
            });
        }
        suffix = next_suffix(suffix)?;
    }
    None
}

/// Names for all entries of one directory (rules 1 to 6, ALGORITHMS H4):
/// in-use entries first, then by record number (entries without one last),
/// then in the given order. Returns the names in the order of `claims`.
///
/// Every name is claimed in `claimed` under `dir`. An entry that finds no
/// free name within [`MAX_NAME_ATTEMPTS`] suffixes keeps its last candidate;
/// the destination then refuses it and the job reports the entry.
pub(crate) fn assign(
    claimed: &mut HashSet<u64>,
    dir: u64,
    claims: &[Claim<'_>],
    limit: ComponentLimit,
) -> Vec<Assigned> {
    let mut order: Vec<usize> = (0..claims.len()).collect();
    order.sort_by_key(|&i| {
        claims
            .get(i)
            .map(|c| (!c.in_use, c.record.unwrap_or(u64::MAX), i))
    });
    // Next suffix to try per folded base name, so that many equal names cost
    // one step each instead of a scan.
    let mut next: HashMap<Vec<u16>, u32> = HashMap::new();
    let mut out: Vec<Option<Assigned>> = vec![None; claims.len()];
    for i in order {
        let Some(claim) = claims.get(i) else { continue };
        let base = sanitize_name(claim.raw);
        let folded_base = fold(&String::from_utf16_lossy(&base));
        let start = next.get(&folded_base).copied().unwrap_or(1);
        let assigned = claim_from(claimed, dir, &base, start, limit).unwrap_or_else(|| Assigned {
            name: String::from_utf16_lossy(&fit_component(&base, start, limit)).into_boxed_str(),
            suffix: start,
        });
        if let Some(after) = next_suffix(assigned.suffix) {
            next.insert(folded_base, after);
        }
        if let Some(slot) = out.get_mut(i) {
            *slot = Some(assigned);
        }
    }
    out.into_iter()
        .map(|a| {
            a.unwrap_or(Assigned {
                name: "_".into(),
                suffix: 1,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;
