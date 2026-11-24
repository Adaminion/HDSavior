//! File names for recovered files: ALGORITHMS H4, rules 1 to 4 and 6.
//!
//! NTFS names are arbitrary 16-bit units: other drivers store `CON`,
//! `a:b`, names that end in a dot, names with control characters and names
//! with unpaired surrogates. Output paths are created with the `\\?\`
//! prefix, under which Windows checks nothing, so every component must be
//! made safe here or it becomes a file that Explorer cannot open or delete.
//!
//! The rules, in this order:
//!
//! 1. each of `< > : " / \ | ? *` and each unit below 0x20 becomes `_`;
//! 2. an unpaired surrogate becomes U+FFFD;
//! 3. trailing dots and spaces are removed; an empty result becomes `_`;
//! 4. a reserved device name gets a `_` prefix, and so does a name that
//!    begins with `HDSavior-report-` or matches `~hds-*.part` (the engine's
//!    own files);
//! 5. (the caller's job) names that collide inside a folder get ` (2)`,
//!    ` (3)` and so on: [`sanitize_component`] and [`fit_component`] place
//!    such a suffix before the extension;
//! 6. the stem is shortened so that the component fits the destination's
//!    limit, never cutting a surrogate pair and never losing the extension.

const UNDERSCORE: u16 = b'_' as u16;
const DOT: u16 = b'.' as u16;
const SPACE: u16 = b' ' as u16;
const REPLACEMENT: u16 = 0xFFFD;

/// Longest extension (dot included) that is kept intact when a name is
/// shortened or gets a collision suffix. A longer "extension" is treated as
/// part of the stem.
const MAX_EXTENSION_UNITS: usize = 16;

/// Component limits below this are raised to it: the longest suffix and the
/// longest extension need room next to at least a little of the stem.
pub const MIN_COMPONENT_LIMIT: u32 = 96;

/// How long one path component may be on the destination file system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentLimit {
    /// At most this many UTF-16 units (Windows: 255 on NTFS, exFAT, FAT32 and ReFS).
    Utf16Units(u32),
    /// At most this many bytes of UTF-8 (Linux: 255 on most file systems).
    Utf8Bytes(u32),
}

impl ComponentLimit {
    /// The limit used when the destination does not say otherwise: 255 UTF-16 units.
    pub const DEFAULT: ComponentLimit = ComponentLimit::Utf16Units(255);

    fn budget(self) -> usize {
        match self {
            ComponentLimit::Utf16Units(n) | ComponentLimit::Utf8Bytes(n) => {
                n.max(MIN_COMPONENT_LIMIT) as usize
            }
        }
    }

    /// What `name` (well-formed UTF-16) costs against this limit.
    fn cost(self, name: &[u16]) -> usize {
        match self {
            ComponentLimit::Utf16Units(_) => name.len(),
            ComponentLimit::Utf8Bytes(_) => name.iter().map(|&u| utf8_cost(u)).sum(),
        }
    }

    /// True when `name` fits.
    pub fn allows(self, name: &[u16]) -> bool {
        let limit = match self {
            ComponentLimit::Utf16Units(n) | ComponentLimit::Utf8Bytes(n) => n as usize,
        };
        self.cost(name) <= limit
    }
}

/// Bytes of UTF-8 one UTF-16 unit contributes; the two halves of a surrogate
/// pair contribute two each (four for the pair).
fn utf8_cost(unit: u16) -> usize {
    match unit {
        0..=0x7F => 1,
        0x80..=0x7FF => 2,
        0xD800..=0xDFFF => 2,
        _ => 3,
    }
}

fn is_high_surrogate(unit: u16) -> bool {
    (0xD800..=0xDBFF).contains(&unit)
}

fn is_low_surrogate(unit: u16) -> bool {
    (0xDC00..=0xDFFF).contains(&unit)
}

fn is_reserved_char(unit: u16) -> bool {
    unit < 0x20
        || matches!(
            u8::try_from(unit),
            Ok(b'<' | b'>' | b':' | b'"' | b'/' | b'\\' | b'|' | b'?' | b'*')
        )
}

fn ascii_upper(unit: u16) -> u16 {
    if (b'a' as u16..=b'z' as u16).contains(&unit) {
        unit - 32
    } else {
        unit
    }
}

fn eq_ascii_upper(units: &[u16], upper: &str) -> bool {
    units.len() == upper.len()
        && units
            .iter()
            .zip(upper.bytes())
            .all(|(&u, b)| ascii_upper(u) == u16::from(b))
}

fn starts_with_ignore_case(name: &[u16], upper_prefix: &str) -> bool {
    name.len() >= upper_prefix.len() && eq_ascii_upper(&name[..upper_prefix.len()], upper_prefix)
}

fn ends_with_ignore_case(name: &[u16], upper_suffix: &str) -> bool {
    name.len() >= upper_suffix.len()
        && eq_ascii_upper(&name[name.len() - upper_suffix.len()..], upper_suffix)
}

/// True for the names Windows reserves for devices: `CON`, `PRN`, `AUX`,
/// `NUL`, `CONIN$`, `CONOUT$`, `COM0`-`COM9`, `LPT0`-`LPT9` and `COM` / `LPT`
/// followed by one of the superscript digits `¹ ² ³`.
///
/// Compared without regard to ASCII case against the part of the name before
/// the **first** dot with trailing spaces removed, as Windows (before 11)
/// does: `nul`, `NUL.txt`, `nul .tar.gz` and `Com¹.log` are all reserved.
pub fn is_reserved_device_name(name: &[u16]) -> bool {
    let base_end = name.iter().position(|&u| u == DOT).unwrap_or(name.len());
    let mut base = &name[..base_end];
    while let [rest @ .., last] = base {
        if *last != SPACE {
            break;
        }
        base = rest;
    }
    if ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"]
        .iter()
        .any(|reserved| eq_ascii_upper(base, reserved))
    {
        return true;
    }
    if base.len() == 4 && (eq_ascii_upper(&base[..3], "COM") || eq_ascii_upper(&base[..3], "LPT")) {
        let digit = base[3];
        return (b'0' as u16..=b'9' as u16).contains(&digit) || matches!(digit, 0xB9 | 0xB2 | 0xB3);
    }
    false
}

/// True for names the engine uses for its own files in an output tree:
/// reports (`HDSavior-report-...`) and part files (`~hds-*.part`), without
/// regard to ASCII case.
pub fn is_engine_file_name(name: &[u16]) -> bool {
    starts_with_ignore_case(name, "HDSAVIOR-REPORT-")
        || (name.len() >= "~hds-.part".len()
            && starts_with_ignore_case(name, "~HDS-")
            && ends_with_ignore_case(name, ".PART"))
}

fn trim_trailing_dots_and_spaces(name: &mut Vec<u16>) {
    while matches!(name.last(), Some(&u) if u == DOT || u == SPACE) {
        name.pop();
    }
}

/// Rules 1 to 4: turns any sequence of UTF-16 units into a name that Windows
/// programs can create, open and delete. The result is never empty, is
/// well-formed UTF-16 and is a fixed point: sanitizing it again changes
/// nothing. Its length is not limited; see [`fit_component`].
pub fn sanitize_name(raw: &[u16]) -> Vec<u16> {
    // Rules 1 and 2.
    let mut name: Vec<u16> = Vec::with_capacity(raw.len() + 1);
    let mut i = 0;
    while i < raw.len() {
        let unit = raw[i];
        if is_reserved_char(unit) {
            name.push(UNDERSCORE);
        } else if is_high_surrogate(unit)
            && raw.get(i + 1).is_some_and(|&next| is_low_surrogate(next))
        {
            name.push(unit);
            name.push(raw[i + 1]);
            i += 1;
        } else if is_high_surrogate(unit) || is_low_surrogate(unit) {
            name.push(REPLACEMENT);
        } else {
            name.push(unit);
        }
        i += 1;
    }
    // Rule 3.
    trim_trailing_dots_and_spaces(&mut name);
    if name.is_empty() {
        name.push(UNDERSCORE);
    }
    // Rule 4.
    if is_reserved_device_name(&name) || is_engine_file_name(&name) {
        name.insert(0, UNDERSCORE);
    }
    name
}

/// True when `name` is a single path component that needs no sanitizing as
/// far as the file system is concerned: not empty, well-formed UTF-16, no
/// reserved or control character, no trailing dot or space, not a reserved
/// device name. (Names of the engine's own files pass: this is the test the
/// output primitives apply to every name they are given.)
pub fn is_valid_component(name: &[u16]) -> bool {
    if name.is_empty() || matches!(name.last(), Some(&u) if u == DOT || u == SPACE) {
        return false;
    }
    let mut i = 0;
    while i < name.len() {
        let unit = name[i];
        if is_reserved_char(unit) {
            return false;
        }
        if is_high_surrogate(unit) {
            if !name.get(i + 1).is_some_and(|&next| is_low_surrogate(next)) {
                return false;
            }
            i += 1;
        } else if is_low_surrogate(unit) {
            return false;
        }
        i += 1;
    }
    !is_reserved_device_name(name)
}

/// Splits a name into stem and extension (with its dot). The part after the
/// last dot is an extension only when it is short (at most 16 units with
/// the dot) and the dot is not the first unit, so `.gitignore` and
/// `notes.a very long tail` have none.
pub fn split_extension(name: &[u16]) -> (&[u16], &[u16]) {
    match name.iter().rposition(|&u| u == DOT) {
        Some(dot) if dot > 0 && name.len() - dot <= MAX_EXTENSION_UNITS => name.split_at(dot),
        _ => (name, &[]),
    }
}

/// Shortens `stem` until it costs at most `budget`, without cutting a
/// surrogate pair in half.
fn truncate_to_cost(stem: &mut Vec<u16>, budget: usize, limit: ComponentLimit) {
    let mut cost = 0usize;
    let mut keep = 0usize;
    while keep < stem.len() {
        let step = if is_high_surrogate(stem[keep])
            && stem.get(keep + 1).is_some_and(|&u| is_low_surrogate(u))
        {
            2
        } else {
            1
        };
        let step_cost = limit.cost(&stem[keep..keep + step]);
        if cost + step_cost > budget {
            break;
        }
        cost += step_cost;
        keep += step;
    }
    stem.truncate(keep);
}

/// Joins `stem`, `tail` and `ext`, shortening the stem so that the whole fits
/// `limit` (rule 6).
fn assemble(stem: &[u16], tail: &[u16], ext: &[u16], limit: ComponentLimit) -> Vec<u16> {
    let mut stem = stem.to_vec();
    let fixed = limit.cost(tail) + limit.cost(ext);
    let budget = limit.budget().saturating_sub(fixed).max(1);
    if limit.cost(&stem) > budget {
        truncate_to_cost(&mut stem, budget, limit);
        if tail.is_empty() && ext.is_empty() {
            // The cut may have exposed a dot or a space at the end.
            trim_trailing_dots_and_spaces(&mut stem);
        }
        if stem.is_empty() {
            stem.push(UNDERSCORE);
        }
    }
    let mut out = stem;
    out.extend_from_slice(tail);
    out.extend_from_slice(ext);
    out
}

/// Rule 6 for a name that already went through [`sanitize_name`]: appends
/// the collision suffix ` (n)` before the extension when `suffix >= 2`
/// (0 and 1 mean "no suffix") and shortens the stem so that the component
/// fits `limit`. The extension survives and no surrogate pair is cut.
///
/// A limit below [`MIN_COMPONENT_LIMIT`] is treated as that minimum.
pub fn fit_component(name: &[u16], suffix: u32, limit: ComponentLimit) -> Vec<u16> {
    let tail: Vec<u16> = if suffix >= 2 {
        format!(" ({suffix})").encode_utf16().collect()
    } else {
        Vec::new()
    };
    let (stem, ext) = split_extension(name);
    assemble(stem, &tail, ext, limit)
}

/// One name of the rescued tree turned into an output name: rules 1 to 4,
/// then the collision suffix and rule 6 as in [`fit_component`].
///
/// ```
/// use hds_io::{sanitize_component, ComponentLimit};
///
/// let fix = |name: &str, suffix| {
///     let units: Vec<u16> = name.encode_utf16().collect();
///     String::from_utf16(&sanitize_component(&units, suffix, ComponentLimit::DEFAULT)).unwrap()
/// };
/// assert_eq!(fix("report.txt", 0), "report.txt");
/// assert_eq!(fix("report.txt", 2), "report (2).txt");
/// assert_eq!(fix("a:b?.txt ", 0), "a_b_.txt");
/// assert_eq!(fix("NUL.tar.gz", 0), "_NUL.tar.gz");
/// assert_eq!(fix("...", 0), "_");
/// ```
pub fn sanitize_component(raw: &[u16], suffix: u32, limit: ComponentLimit) -> Vec<u16> {
    fit_component(&sanitize_name(raw), suffix, limit)
}

/// The name to try when the destination file system rejects a sanitized
/// name: `<shortened stem>~<record number><extension>`, with the stem cut to
/// at most 24 units (H4, last sentence). `name` must have gone through
/// [`sanitize_name`].
pub fn fallback_component(name: &[u16], record_number: u64, limit: ComponentLimit) -> Vec<u16> {
    const FALLBACK_STEM_UNITS: usize = 24;
    let (stem, ext) = split_extension(name);
    let mut short = stem.to_vec();
    truncate_to_cost(
        &mut short,
        FALLBACK_STEM_UNITS,
        ComponentLimit::Utf16Units(u32::MAX),
    );
    let tail: Vec<u16> = format!("~{record_number}").encode_utf16().collect();
    assemble(&short, &tail, ext, limit)
}

#[cfg(test)]
mod tests;
