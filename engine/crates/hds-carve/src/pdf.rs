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
//! The scan reads the file sequentially. It gives up when a long stretch
//! carries no PDF keyword at all (the candidate's tail was overwritten).

use crate::reader::Src;
use crate::util::find_sub;
use crate::{Confidence, Measured};

/// Scanned bytes without any `endobj` or `%%EOF` after which the candidate
/// is abandoned. Larger than any realistic single stream object.
const GIVE_UP: u64 = 128 * 1024 * 1024;

const MARKER: &[u8] = b"%%EOF";

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
    let Some((_, rest)) = digits(b) else { return false };
    let after = skip_space(rest);
    if after.len() == rest.len() {
        return false;
    }
    let Some((_, rest)) = digits(after) else { return false };
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
        if value.first().is_some_and(|&c| is_space(c) || c.is_ascii_digit()) {
            return digits(skip_space(value)).map(|(v, _)| v);
        }
        from += p + 2;
    }
    None
}

/// `true` when the bytes just before `end` are `%%EOF` plus optional
/// trailing white space.
fn ends_with_marker(src: &mut Src, end: u64) -> bool {
    let Some(start) = end.checked_sub(32) else { return false };
    let Some(tail) = src.side::<32>(start) else { return false };
    let Some(p) = tail.windows(MARKER.len()).rposition(|w| w == MARKER) else {
        return false;
    };
    tail.get(p + MARKER.len()..).is_some_and(|rest| rest.iter().all(|&c| is_space(c)))
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

/// `true` when a cross-reference table or stream starts at `off`.
fn xref_at(src: &mut Src, off: u64) -> bool {
    let (buf, got) = src.side_peek::<40>(off);
    let Some(b) = buf.get(..got) else { return false };
    let b = skip_space(b);
    b.starts_with(b"xref") || starts_with_object(b)
}

/// What the bytes after an accepted end look like.
fn update_follows(src: &mut Src, end: u64) -> bool {
    let (buf, got) = src.side_peek::<64>(end);
    let Some(b) = buf.get(..got) else { return false };
    let b = skip_space(b);
    if b.starts_with(b"%PDF-") {
        return false;
    }
    b.starts_with(b"%") || b.starts_with(b"xref") || starts_with_object(b)
}

/// Examines the `%%EOF` at `marker`: the end offset it implies, if it is a
/// trailer of this file.
fn end_at_marker(src: &mut Src, marker: u64) -> Option<u64> {
    let xref = startxref_before(src, marker)?;
    // 0 is the dummy value in the first-page trailer of a linearized file
    if xref == 0 || xref >= marker || !xref_at(src, xref) {
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
    Some(after_marker + eol as u64)
}

pub(crate) fn measure(src: &mut Src) -> Option<Measured> {
    src.scan_mode();
    let head = src.peek(0, 1024).to_vec();
    if !head.starts_with(b"%PDF-") {
        return None;
    }
    let done = |len, confidence| Some(Measured { len, confidence, ext: "pdf" });

    let mut best: Option<u64> = None;
    let mut pos = 0u64;
    if let Some(l) = linearized_length(&head) {
        if l >= 64 && l <= src.limit() && ends_with_marker(src, l) {
            if !update_follows(src, l) {
                return done(l, Confidence::Exact);
            }
            best = Some(l);
            pos = l;
        }
    }

    let mut barren = 0u64;
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
            if best.is_some_and(|b| marker < b) {
                continue;
            }
            if let Some(end) = end_at_marker(src, marker) {
                if !update_follows(src, end) {
                    return done(end, Confidence::Probable);
                }
                best = Some(end);
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
        pos += if wlen >= MARKER.len() { (wlen - (MARKER.len() - 1)) as u64 } else { wlen as u64 };
    }
    // The data ended (or went quiet) after a section that announced an
    // update which never completed: the last complete revision stands.
    done(best?, Confidence::Probable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linearized_dictionary() {
        assert_eq!(linearized_length(b"%PDF-1.3\n5 0 obj\n<< /Linearized 1 /L 2416 /H [ 1 2 ] >>"), Some(2416));
        assert_eq!(linearized_length(b"%PDF-1.5\n1 0 obj<</Linearized 1/L 123456/O 3>>endobj"), Some(123456));
        assert_eq!(linearized_length(b"1 0 obj<</L 77/Linearized 1>>"), Some(77));
        assert_eq!(linearized_length(b"1 0 obj<</Linearized 1/Length 9>>"), None);
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
