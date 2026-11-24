//! PDF (ISO 32000): the last `%%EOF` that belongs to this file.
//!
//! A PDF is a header line (`%PDF-1.x`), numbered objects, a cross-reference
//! section and a trailer that ends with
//!
//! ```text
//! startxref
//! <byte offset of the cross-reference section, from the start of the file>
//! %%EOF
//! ```
//!
//! Three things make "the first `%%EOF`" the wrong answer:
//!
//! * incremental updates: an editor appends new objects, a new
//!   cross-reference section and another `%%EOF`, any number of times;
//! * linearized ("fast web view") files carry a first-page trailer near the
//!   start whose `startxref` is `0`, then the rest of the file;
//! * a PDF embedded uncompressed in another PDF brings its own `%%EOF`.
//!
//! The walker therefore accepts a `%%EOF` only if the `startxref` offset in
//! front of it leads - relative to *this* file's first byte - to a
//! cross-reference table (`xref`) or a cross-reference stream (`n g obj`),
//! which neither an embedded nor a neighbouring PDF satisfies. After an
//! accepted `%%EOF` it looks at what follows: the start of an update section
//! (an object, `xref`, or a comment line) means keep going; anything else
//! (zeros, another file, noise) means this was the end.
//!
//! A linearized file states its total length in the first object (`/L`).
//! When a `%%EOF` ends exactly there and nothing was appended, the result is
//! `Exact` and no scan is needed. Everything else is `Probable`: one end-of-line
//! after the final `%%EOF` is included if present, and nothing in the format
//! proves that no further update followed.
//!
//! When an update was announced (an object, `xref` or a comment right after
//! a `%%EOF`) but no later `%%EOF` completes it before the data ends or goes
//! quiet, the last complete revision is returned - a readable document - but
//! its end is not verified: the file went on, and what followed is missing.
//!
//! The scan reads the file sequentially. It gives up when a long stretch
//! carries no PDF keyword at all (the candidate's tail was overwritten).

use crate::reader::Src;
use crate::util::find_sub;
use crate::{Confidence, Measured};

/// Scanned bytes without any `endobj` or `%%EOF` after which the candidate
/// is abandoned. Larger than any realistic single stream object.
const GIVE_UP: u64 = 128 * 1024 * 1024;

const MARKER: &[u8] = b"%%EOF";
/// `%%EOF` markers examined before giving up. A PDF has one per revision,
/// plus those of embedded PDFs; a file made of nothing else is not a PDF.
const MAX_MARKERS: u32 = 4096;

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\r' | b'\n' | 0x0C)
}

fn skip_space(mut b: &[u8]) -> &[u8] {
    while let Some((&c, rest)) = b.split_first() {
        if !is_space(c) {
            break;
        }
        b = rest;
    }
    b
}

/// Splits leading decimal digits (at most 19, so the value fits) off `b`.
fn digits(b: &[u8]) -> Option<(u64, &[u8])> {
    let n = b.iter().take_while(|c| c.is_ascii_digit()).count();
    if n == 0 || n > 19 {
        return None;
    }
    let (num, rest) = b.split_at(n);
    let v = num.iter().fold(0u64, |v, &c| v * 10 + (c - b'0') as u64);
    Some((v, rest))
}

/// `true` when `b` starts with `<int> <int> obj`.
fn starts_with_object(b: &[u8]) -> bool {
    let Some((_, rest)) = digits(b) else {
        return false;
    };
    let after = skip_space(rest);
    if after.len() == rest.len() {
        return false;
    }
    let Some((_, rest)) = digits(after) else {
        return false;
    };
    let after = skip_space(rest);
    after.len() != rest.len() && after.starts_with(b"obj")
}

/// The `/L` value of a linearization parameter dictionary in `head`.
fn linearized_length(head: &[u8]) -> Option<u64> {
    let at = find_sub(head, b"/Linearized")?;
    let dict_start = head.get(..at)?.windows(2).rposition(|w| w == b"<<")?;
    let dict_len = find_sub(head.get(at..)?, b">>")?;
    let dict = head.get(dict_start..at + dict_len)?;
    let mut from = 0usize;
    while let Some(p) = find_sub(dict.get(from..)?, b"/L") {
        let value = dict.get(from + p + 2..)?;
        // `/L` itself, not `/Linearized` or another name starting with L
        if value
            .first()
            .is_some_and(|&c| is_space(c) || c.is_ascii_digit())
        {
            return digits(skip_space(value)).map(|(v, _)| v);
        }
        from += p + 2;
    }
    None
}

/// The number between `startxref` and the `%%EOF` at `marker`.
fn startxref_before(src: &mut Src, marker: u64) -> Option<u64> {
    let start = marker.saturating_sub(48);
    let (buf, got) = src.side_peek::<48>(start);
    let mut b = buf.get(..got.min((marker - start) as usize))?;
    while let Some((&c, rest)) = b.split_last() {
        if !is_space(c) {
            break;
        }
        b = rest;
    }
    let n = b.iter().rev().take_while(|c| c.is_ascii_digit()).count();
    if n == 0 || n > 19 {
        return None;
    }
    let (mut before, num) = b.split_at(b.len() - n);
    let (value, _) = digits(num)?;
    let unstripped = before.len();
    while let Some((&c, rest)) = before.split_last() {
        if !is_space(c) {
            break;
        }
        before = rest;
    }
    (before.len() != unstripped && before.ends_with(b"startxref")).then_some(value)
}

/// What the cross-reference section a `startxref` names says about the
/// file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Xref {
    /// Neither a table (`xref`) nor a cross-reference stream (an object
    /// whose dictionary has `/Type /XRef`) starts there.
    None,
    /// A section starts there, but something in it, or in a section its
    /// `/Prev` chain leads to, did not check out.
    Unverified,
    /// Every section of the chain is where the offsets say, and every
    /// sampled table entry points at the object it names.
    Verified,
}

/// Sections of a `/Prev` chain followed from one `startxref`.
const MAX_SECTIONS: usize = 16;
/// Subsections of one table that are walked.
const MAX_SUBSECTIONS: usize = 256;
/// Entries per subsection whose offsets are checked.
const SAMPLES_PER_SUBSECTION: u64 = 4;
/// Entries per section whose offsets are checked.
const SAMPLES_PER_SECTION: usize = 16;

/// The integer `<int> <int> obj` at the start of `b` declares, with
/// the bytes after `obj`.
fn object_header(b: &[u8]) -> Option<(u64, u64, &[u8])> {
    let (num, rest) = digits(b)?;
    let after = skip_space(rest);
    if after.len() == rest.len() {
        return None;
    }
    let (gen, rest) = digits(after)?;
    let after = skip_space(rest);
    if after.len() == rest.len() {
        return None;
    }
    let rest = after.strip_prefix(b"obj")?;
    Some((num, gen, rest))
}

/// The value of `/Prev` in the dictionary bytes `dict`, if any.
fn prev_offset(dict: &[u8]) -> Option<u64> {
    let mut from = 0usize;
    while let Some(p) = find_sub(dict.get(from..)?, b"/Prev") {
        let value = dict.get(from + p + 5..)?;
        if value.first().is_some_and(|&c| is_space(c)) {
            return digits(skip_space(value)).map(|(v, _)| v);
        }
        from += p + 5;
    }
    None
}

/// `true` when the name `/XRef` (and not a longer name such as
/// `/XRefStm`) appears in `dict`.
fn has_xref_type(dict: &[u8]) -> bool {
    let mut from = 0usize;
    while let Some(p) = dict.get(from..).and_then(|d| find_sub(d, b"/XRef")) {
        let next = dict.get(from + p + 5).copied();
        if !next.is_some_and(|c| c.is_ascii_alphanumeric()) {
            return true;
        }
        from += p + 5;
    }
    false
}

/// One parsed section: whether it is one, whether its sampled entries
/// matched, and where `/Prev` points.
struct Section {
    ok: bool,
    checked: usize,
    prev: Option<u64>,
}

/// Checks that the object table entry `num`/`gen` at `offset` names: the
/// bytes there must be `num gen obj`, inside the first `end` bytes.
fn object_at(src: &mut Src, offset: u64, num: u64, gen: u64, end: u64) -> bool {
    if offset >= end {
        return false;
    }
    let (buf, got) = src.side_peek::<48>(offset);
    let Some(b) = buf.get(..got.min((end - offset) as usize)) else {
        return false;
    };
    object_header(skip_space(b)).is_some_and(|(n, g, _)| n == num && g == gen)
}

/// A classic cross-reference table whose `xref` keyword is at `at`.
fn classic_section(src: &mut Src, at: u64, end: u64) -> Section {
    let bad = Section {
        ok: false,
        checked: 0,
        prev: None,
    };
    let mut pos = at + 4;
    let mut checked = 0usize;
    for _ in 0..MAX_SUBSECTIONS {
        // a subsection header `first count`, or the trailer
        let (buf, got) = src.side_peek::<64>(pos);
        let Some(b) = buf.get(..got) else {
            return bad;
        };
        let head = skip_space(b);
        let lead = (b.len() - head.len()) as u64;
        if head.starts_with(b"trailer") {
            let start = pos + lead + 7;
            let (buf, got) = src.side_peek::<1024>(start);
            let t = buf.get(..got.min(end.saturating_sub(start) as usize)).unwrap_or(&[]);
            // the dictionary ends before `startxref` (if it is in view)
            let t = t.get(..find_sub(t, b"startxref").unwrap_or(t.len())).unwrap_or(t);
            return Section {
                ok: skip_space(t).starts_with(b"<<"),
                checked,
                prev: prev_offset(t),
            };
        }
        let Some((first, rest)) = digits(head) else {
            return bad;
        };
        let after = rest
            .iter()
            .take_while(|&&c| c == b' ')
            .count();
        let Some((count, rest)) = rest.get(after..).filter(|_| after > 0).and_then(digits) else {
            return bad;
        };
        // the line ends; entries follow
        let body = skip_space(rest);
        if body.len() == rest.len() {
            return bad;
        }
        let entries = pos + (b.len() - body.len()) as u64;
        // each entry is `oooooooooo ggggg n` and a two-byte end of line;
        // a few writers end it with one byte
        let (e, got) = src.side_peek::<20>(entries);
        let entry_len: u64 = if count == 0 {
            20
        } else {
            match e.get(..got) {
                Some(e) if e.len() == 20 && e.get(18..20).is_some_and(|eol| {
                    matches!(eol, b" \n" | b" \r" | b"\r\n")
                }) => 20,
                Some(e) if e.len() >= 19 && matches!(e.get(18), Some(b'\n' | b'\r')) => 19,
                _ => return bad,
            }
        };
        let Some(span) = count.checked_mul(entry_len) else {
            return bad;
        };
        if entries.saturating_add(span) > end {
            return bad;
        }
        // a few entries spread over the subsection
        let picks = count.min(SAMPLES_PER_SUBSECTION);
        let mut last = None;
        for k in 0..picks {
            let i = if picks <= 1 { 0 } else { k * (count - 1) / (picks - 1) };
            if last == Some(i) || checked >= SAMPLES_PER_SECTION {
                continue;
            }
            last = Some(i);
            let (e, got) = src.side_peek::<18>(entries + i * entry_len);
            let Some(e) = e.get(..got).filter(|e| e.len() == 18) else {
                return bad;
            };
            let Some((offset, r1)) = digits(e) else {
                return bad;
            };
            let well_formed = e.len() - r1.len() == 10
                && r1.first() == Some(&b' ')
                && r1.get(1..6).is_some_and(|g| g.iter().all(u8::is_ascii_digit))
                && r1.get(6) == Some(&b' ');
            if !well_formed {
                return bad;
            }
            let gen = r1
                .get(1..6)
                .and_then(digits)
                .map_or(u64::MAX, |(g, _)| g);
            match r1.get(7) {
                Some(b'n') => {
                    checked += 1;
                    let num = first.saturating_add(i);
                    if !object_at(src, offset, num, gen, end) {
                        return bad;
                    }
                }
                Some(b'f') => {}
                _ => return bad,
            }
        }
        pos = entries + span;
    }
    bad
}

/// A cross-reference stream: an object at `at` whose dictionary says
/// `/Type /XRef`. Its entries are compressed and not sampled.
fn stream_section(src: &mut Src, at: u64, end: u64) -> Option<Section> {
    let (buf, got) = src.side_peek::<1024>(at);
    let b = buf.get(..got.min(end.saturating_sub(at) as usize))?;
    let (_, _, rest) = object_header(skip_space(b))?;
    let dict = rest.get(..find_sub(rest, b"stream").unwrap_or(rest.len()))?;
    if !skip_space(dict).starts_with(b"<<") || !has_xref_type(dict) {
        return None;
    }
    Some(Section {
        ok: true,
        checked: 1,
        prev: prev_offset(dict),
    })
}

/// Checks the cross-reference section at `off` and the chain of older
/// sections its `/Prev` entries lead to, all inside the first `end` bytes.
fn check_xref(src: &mut Src, off: u64, end: u64) -> Xref {
    let mut at = off;
    let mut seen: Vec<u64> = Vec::new();
    let mut checked = 0usize;
    for depth in 0..MAX_SECTIONS {
        let (buf, got) = src.side_peek::<40>(at);
        let Some(b) = buf.get(..got) else {
            return Xref::None;
        };
        let body = skip_space(b);
        let start = at + (b.len() - body.len()) as u64;
        let section = if body.starts_with(b"xref") {
            Some(classic_section(src, start, end))
        } else if starts_with_object(body) {
            stream_section(src, start, end)
        } else {
            None
        };
        let Some(section) = section else {
            // the section `startxref` names must be one; one that a
            // `/Prev` names may be damaged without making this one foreign
            return if depth == 0 { Xref::None } else { Xref::Unverified };
        };
        if !section.ok {
            return Xref::Unverified;
        }
        checked += section.checked;
        seen.push(at);
        match section.prev {
            None => {
                return if checked > 0 {
                    Xref::Verified
                } else {
                    Xref::Unverified
                };
            }
            Some(p) if p >= end || seen.contains(&p) => return Xref::Unverified,
            Some(p) => at = p,
        }
    }
    Xref::Unverified
}

/// What the bytes after an accepted end look like.
fn update_follows(src: &mut Src, end: u64) -> bool {
    let (buf, got) = src.side_peek::<64>(end);
    let Some(b) = buf.get(..got) else {
        return false;
    };
    let b = skip_space(b);
    if b.starts_with(b"%PDF-") {
        return false;
    }
    b.starts_with(b"%") || b.starts_with(b"xref") || starts_with_object(b)
}

/// Examines the `%%EOF` at `marker`: the end offset it implies, if it is a
/// trailer of this file, and whether its cross-reference chain checked out.
fn end_at_marker(src: &mut Src, marker: u64) -> Option<(u64, bool)> {
    let xref = startxref_before(src, marker)?;
    // 0 is the dummy value in the first-page trailer of a linearized file
    if xref == 0 || xref >= marker {
        return None;
    }
    let after_marker = marker + MARKER.len() as u64;
    let (buf, got) = src.side_peek::<32>(after_marker);
    let after = buf.get(..got)?;
    let eol = match after {
        [b'\r', b'\n', ..] => 2,
        [b'\r' | b'\n', ..] => 1,
        _ => 0,
    };
    // inside a stream of an enclosing PDF: not an end of anything on disk
    let rest = skip_space(after.get(eol..)?);
    if rest.starts_with(b"endstream") || rest.starts_with(b"endobj") {
        return None;
    }
    let end = after_marker + eol as u64;
    match check_xref(src, xref, end) {
        Xref::None => None,
        Xref::Unverified => Some((end, false)),
        Xref::Verified => Some((end, true)),
    }
}

/// The `%%EOF` in the 32 bytes before `end`, when only white space
/// follows it there.
fn marker_before(src: &mut Src, end: u64) -> Option<u64> {
    let start = end.checked_sub(32)?;
    let tail = src.side::<32>(start)?;
    let p = tail.windows(MARKER.len()).rposition(|w| w == MARKER)?;
    tail.get(p + MARKER.len()..)
        .is_some_and(|rest| rest.iter().all(|&c| is_space(c)))
        .then_some(start + p as u64)
}

pub(crate) fn measure(src: &mut Src) -> Option<Measured> {
    src.scan_mode();
    let head = src.peek(0, 1024).to_vec();
    if !head.starts_with(b"%PDF-") {
        return None;
    }
    // every end ends with `%%EOF` whose `startxref` leads to a
    // cross-reference section of this file; it is verified unless what
    // follows announced an update that never completed
    let done = |len, confidence, end_verified| {
        Some(Measured {
            len,
            confidence,
            ext: "pdf",
            end_verified,
        })
    };

    let mut best: Option<(u64, bool)> = None;
    let mut pos = 0u64;
    if let Some(l) = linearized_length(&head) {
        // the `%%EOF` that ends the stated length, with a cross-reference
        // chain of this file in front of it
        let at = (l >= 64 && l <= src.limit())
            .then(|| marker_before(src, l))
            .flatten();
        if let Some((_, verified)) = at.and_then(|m| end_at_marker(src, m)) {
            if !update_follows(src, l) {
                return done(l, Confidence::Exact, verified);
            }
            best = Some((l, verified));
            pos = l;
        }
    }

    let mut barren = 0u64;
    let mut examined = 0u32;
    let mut hits: Vec<u64> = Vec::new();
    loop {
        let w = src.window_min(pos, MARKER.len());
        if w.is_empty() {
            break;
        }
        let wlen = w.len();
        hits.clear();
        let mut from = 0usize;
        while let Some(p) = w.get(from..).and_then(|s| find_sub(s, MARKER)) {
            hits.push(pos + (from + p) as u64);
            from += p + MARKER.len();
        }
        let keyword = !hits.is_empty() || find_sub(w, b"endobj").is_some();

        for &marker in &hits {
            // sections are appended: an end before the best one is stale
            if best.is_some_and(|(b, _)| marker < b) {
                continue;
            }
            examined += 1;
            if examined > MAX_MARKERS {
                return None;
            }
            if let Some((end, verified)) = end_at_marker(src, marker) {
                if !update_follows(src, end) {
                    return done(end, Confidence::Probable, verified);
                }
                best = Some((end, verified));
            }
        }

        if keyword {
            barren = 0;
        } else {
            barren += wlen as u64;
            if barren > GIVE_UP {
                break;
            }
        }
        // keep an overlap so that a marker split across two refills is found
        pos += if wlen >= MARKER.len() {
            (wlen - (MARKER.len() - 1)) as u64
        } else {
            wlen as u64
        };
    }
    // The data ended (or went quiet) after a section that announced an
    // update which never completed: the last complete revision stands, but
    // the file evidently went on - into a bad sector, another file, or
    // the next fragment - so this is not where it ends.
    done(best?.0, Confidence::Probable, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linearized_dictionary() {
        assert_eq!(
            linearized_length(b"%PDF-1.3\n5 0 obj\n<< /Linearized 1 /L 2416 /H [ 1 2 ] >>"),
            Some(2416)
        );
        assert_eq!(
            linearized_length(b"%PDF-1.5\n1 0 obj<</Linearized 1/L 123456/O 3>>endobj"),
            Some(123456)
        );
        assert_eq!(
            linearized_length(b"1 0 obj<</L 77/Linearized 1>>"),
            Some(77)
        );
        assert_eq!(
            linearized_length(b"1 0 obj<</Linearized 1/Length 9>>"),
            None
        );
        assert_eq!(linearized_length(b"1 0 obj<</Length 9/L 5>>"), None);
        assert_eq!(linearized_length(b"/Linearized 1 /L 5 >>"), None);
    }

    #[test]
    fn object_headers() {
        assert!(starts_with_object(b"12 0 obj\n<<"));
        assert!(starts_with_object(b"1\n0\nobj"));
        assert!(!starts_with_object(b"12 0 object-ish"[..7].as_ref()));
        assert!(!starts_with_object(b"12 obj"));
        assert!(!starts_with_object(b"120obj"));
        assert!(!starts_with_object(b"x 0 obj"));
        assert!(!starts_with_object(b""));
    }

    #[test]
    fn number_parsing_is_bounded() {
        assert_eq!(digits(b"0007x").map(|(v, r)| (v, r.len())), Some((7, 1)));
        assert!(digits(b"99999999999999999999999").is_none());
        assert!(digits(b"x").is_none());
    }
}
