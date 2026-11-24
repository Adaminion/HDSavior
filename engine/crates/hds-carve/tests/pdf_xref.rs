//! PDF ends checked against the cross-reference sections: minimal PDFs
//! written here by hand (classic tables, 19-byte entries, cross-reference
//! streams, incremental updates), then cut, damaged, or given a foreign
//! trailer, to hold `measure` to the rule of the crate documentation:
//! `end_verified` only when the `startxref` chain leads to sections of this
//! file whose sampled entries point at the objects they name.

use hds_carve::{measure, Confidence, FileKind, Measured};

/// A PDF under construction: objects are appended, their offsets kept.
struct Pdf {
    data: Vec<u8>,
    /// `(object number, offset)` of the objects of the current revision.
    objects: Vec<(u32, usize)>,
    /// Offset of the previous revision's cross-reference section.
    prev: Option<usize>,
    /// End of line after each table entry: `" \n"` (20-byte entries) or
    /// `"\n"` (19 bytes, written by a few generators).
    entry_eol: &'static str,
}

impl Pdf {
    fn new() -> Pdf {
        Pdf {
            data: b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n".to_vec(),
            objects: Vec::new(),
            prev: None,
            entry_eol: " \n",
        }
    }

    fn object(&mut self, num: u32, body: &str) -> &mut Pdf {
        self.objects.push((num, self.data.len()));
        self.data
            .extend_from_slice(format!("{num} 0 obj\n{body}\nendobj\n").as_bytes());
        self
    }

    /// The usual first revision: catalog, page tree, one page, content.
    fn basic() -> Pdf {
        let mut p = Pdf::new();
        p.object(1, "<< /Type /Catalog /Pages 2 0 R >>")
            .object(2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>")
            .object(
                3,
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R >>",
            )
            .object(
                4,
                "<< /Length 21 >>\nstream\n0 0 m 100 100 l S\n\nendstream",
            );
        p
    }

    /// Ends the revision with a classic table (one subsection per run of
    /// consecutive numbers), a trailer and `%%EOF`; returns the table's
    /// offset.
    fn classic_end(&mut self, size: u32) -> usize {
        let xref = self.data.len();
        let mut objects = std::mem::take(&mut self.objects);
        objects.sort();
        let mut table = String::from("xref\n");
        if self.prev.is_none() {
            table.push_str(&format!("0 1\n0000000000 65535 f{}", self.entry_eol));
        }
        let mut i = 0;
        while i < objects.len() {
            let mut j = i;
            while j + 1 < objects.len() && objects[j + 1].0 == objects[j].0 + 1 {
                j += 1;
            }
            table.push_str(&format!("{} {}\n", objects[i].0, j - i + 1));
            for &(_, off) in &objects[i..=j] {
                table.push_str(&format!("{off:010} 00000 n{}", self.entry_eol));
            }
            i = j + 1;
        }
        let prev = self.prev.map_or(String::new(), |p| format!(" /Prev {p}"));
        table.push_str(&format!(
            "trailer\n<< /Size {size} /Root 1 0 R{prev} >>\nstartxref\n{xref}\n%%EOF\n"
        ));
        self.data.extend_from_slice(table.as_bytes());
        self.prev = Some(xref);
        xref
    }

    /// Ends the revision with a cross-reference stream (its data is not
    /// decoded by the walker; a plausible uncompressed one is written).
    fn stream_end(&mut self, num: u32, size: u32) -> usize {
        self.stream_end_with(num, size, "")
    }

    /// [`Pdf::stream_end`] with `extra` written into the dictionary before
    /// `/Root` (a long `/Index` array, as a large update writes).
    fn stream_end_with(&mut self, num: u32, size: u32, extra: &str) -> usize {
        let xref = self.data.len();
        let mut objects = std::mem::take(&mut self.objects);
        objects.push((num, xref));
        objects.sort();
        let mut rows = vec![0u8, 0, 0, 0xFF];
        for &(_, off) in &objects {
            rows.extend_from_slice(&[1, (off >> 8) as u8, off as u8, 0]);
        }
        let prev = self.prev.map_or(String::new(), |p| format!(" /Prev {p}"));
        let first = objects[0].0;
        let mut obj = format!(
            "{num} 0 obj\n<< /Type /XRef /Size {size} /W [1 2 1] /Index [0 1 {first} {}] \
             {extra}/Root 1 0 R{prev} /Length {} >>\nstream\n",
            objects.len(),
            rows.len()
        )
        .into_bytes();
        obj.extend_from_slice(&rows);
        obj.extend_from_slice(
            format!("\nendstream\nendobj\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
        );
        self.data.extend_from_slice(&obj);
        self.prev = Some(xref);
        xref
    }
}

/// Measures `data` followed by free space (zeros).
fn run(data: &[u8]) -> Option<Measured> {
    let mut disk = data.to_vec();
    disk.resize(data.len() + 4096, 0);
    measure(FileKind::Pdf, &disk, disk.len() as u64)
}

fn find(data: &[u8], needle: &[u8]) -> usize {
    data.windows(needle.len())
        .position(|w| w == needle)
        .unwrap_or_else(|| panic!("{:?} not found", String::from_utf8_lossy(needle)))
}

/// Offset of the table entry (in the table at `xref`) that holds
/// `offset`.
fn entry_of(data: &[u8], xref: usize, offset: usize) -> usize {
    let text = format!("{offset:010} 00000 n");
    xref + find(&data[xref..], text.as_bytes())
}

fn verified(len: usize) -> Option<(u64, bool)> {
    Some((len as u64, true))
}

fn unverified(len: usize) -> Option<(u64, bool)> {
    Some((len as u64, false))
}

fn summary(m: Option<Measured>) -> Option<(u64, bool)> {
    m.map(|m| {
        assert_eq!((m.confidence, m.ext), (Confidence::Probable, "pdf"));
        (m.len, m.end_verified)
    })
}

#[test]
fn classic_table_is_verified() {
    let mut p = Pdf::basic();
    p.classic_end(5);
    assert_eq!(summary(run(&p.data)), verified(p.data.len()));

    // 19-byte entries (one-byte end of line) are read as well
    let mut p = Pdf::basic();
    p.entry_eol = "\n";
    p.classic_end(5);
    assert_eq!(summary(run(&p.data)), verified(p.data.len()));
}

#[test]
fn entries_that_miss_their_objects_are_not_verified() {
    let mut p = Pdf::basic();
    let xref = p.classic_end(5);
    // the entry of object 3 points 1 byte into its header
    let third = find(&p.data, b"3 0 obj");
    let at = entry_of(&p.data, xref, third);
    let mut bad = p.data.clone();
    bad[at..at + 10].copy_from_slice(format!("{:010}", third + 1).as_bytes());
    // the same length - the trailer is intact - but not a good file
    assert_eq!(summary(run(&bad)), unverified(p.data.len()));

    // an entry naming another object's offset
    let mut bad = p.data.clone();
    let second = find(&p.data, b"2 0 obj");
    bad[at..at + 10].copy_from_slice(format!("{second:010}").as_bytes());
    assert_eq!(summary(run(&bad)), unverified(p.data.len()));

    // an entry beyond the end of the file
    let mut bad = p.data.clone();
    bad[at..at + 10].copy_from_slice(format!("{:010}", p.data.len() + 100).as_bytes());
    assert_eq!(summary(run(&bad)), unverified(p.data.len()));

    // a table whose entries are not in the fixed format
    let mut bad = p.data.clone();
    bad[at + 10] = b'x';
    assert_eq!(summary(run(&bad)), unverified(p.data.len()));

    // a subsection that claims more entries than the file holds
    let mut bad = p.data.clone();
    let sub = xref + find(&p.data[xref..], b"\n1 4\n") + 3;
    bad[sub] = b'9';
    assert_eq!(summary(run(&bad)), unverified(p.data.len()));
}

#[test]
fn incremental_updates_follow_the_prev_chain() {
    let mut p = Pdf::basic();
    let first = p.classic_end(5);
    let end1 = p.data.len();
    p.object(
        4,
        "<< /Length 21 >>\nstream\n0 0 m 150 150 l S\n\nendstream",
    );
    let second = p.classic_end(5);
    let end2 = p.data.len();
    p.object(5, "<< /Producer (hand) >>").object(
        3,
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R >>",
    );
    p.classic_end(6);
    let end3 = p.data.len();
    assert_eq!(summary(run(&p.data)), verified(end3));

    // every revision on its own, free space behind: verified
    assert_eq!(summary(run(&p.data[..end1])), verified(end1));
    assert_eq!(summary(run(&p.data[..end2])), verified(end2));

    // a /Prev that leads nowhere: the end is found, but not verified
    let mut bad = p.data.clone();
    let text = format!("/Prev {second}");
    let at = end2 + find(&p.data[end2..], text.as_bytes()) + 6;
    let wrong = (second + 7).to_string();
    assert_eq!(wrong.len(), second.to_string().len());
    bad[at..at + wrong.len()].copy_from_slice(wrong.as_bytes());
    assert_eq!(summary(run(&bad)), unverified(end3));

    // an older table damaged: the newest section is fine, the chain not
    let mut bad = p.data.clone();
    let one = find(&p.data, b"1 0 obj");
    let at = entry_of(&p.data, first, one);
    bad[at..at + 10].copy_from_slice(format!("{:010}", one + 2).as_bytes());
    assert_eq!(summary(run(&bad)), unverified(end3));

    // the last update cut short (its `%%EOF` missing): the revision before
    // it is complete, and not verified - the file went on
    for cut in [end2 + 10, end3 - 40, end3 - 7] {
        assert_eq!(
            summary(run(&p.data[..cut])),
            unverified(end2),
            "cut at {cut}"
        );
    }
    // cut before the first `%%EOF`: nothing
    assert_eq!(run(&p.data[..end1 - 7]), None);
}

#[test]
fn cross_reference_streams() {
    let mut p = Pdf::new();
    p.data = b"%PDF-1.5\n".to_vec();
    p.object(1, "<< /Type /Catalog /Pages 2 0 R >>")
        .object(2, "<< /Type /Pages /Kids [] /Count 0 >>");
    p.stream_end(3, 4);
    let end1 = p.data.len();
    assert_eq!(summary(run(&p.data)), verified(end1));
    // an update with its own stream, /Prev to the first one
    p.object(2, "<< /Type /Pages /Kids [] /Count 0 /Rotate 90 >>");
    p.stream_end(4, 5);
    assert_eq!(summary(run(&p.data)), verified(p.data.len()));

    // a `startxref` that names an object which is not a cross-reference
    // stream: not this file's trailer (it used to be accepted)
    let mut bad = p.data[..end1].to_vec();
    let at = bad.len() - 6 - 1;
    let number_end = at;
    let number_at = bad[..number_end]
        .iter()
        .rposition(|c| !c.is_ascii_digit())
        .unwrap()
        + 1;
    let second = find(&bad, b"2 0 obj");
    let text = format!("{second:0width$}", width = number_end - number_at);
    bad[number_at..number_end].copy_from_slice(text.as_bytes());
    assert_eq!(run(&bad), None);
    // nor does `/XRefStm` count as `/XRef`
    let mut odd = p.data[..end1].to_vec();
    let t = find(&odd, b"/Type /XRef ");
    odd[t..t + 12].copy_from_slice(b"/XRefStm 0  ");
    assert_eq!(run(&odd), None);
}

#[test]
fn a_foreign_trailer_does_not_end_the_file() {
    // An outer document carries the text of another PDF in a content
    // stream; that PDF's `startxref` happens to name the offset of one of
    // the outer document's objects, and its `%%EOF` is followed by more
    // stream content. Without the cross-reference check that `%%EOF` was
    // taken as the end.
    let mut p = Pdf::basic();
    let inner_target = find(&p.data, b"3 0 obj");
    let inner =
        format!("%PDF-1.3\n% inner\ntrailer\n<< >>\nstartxref\n{inner_target}\n%%EOF\nq Q\n");
    p.object(
        5,
        &format!(
            "<< /Length {} >>\nstream\n{inner}\nendstream",
            inner.len() + 1
        ),
    );
    p.classic_end(6);
    let inner_end = find(&p.data, b"%%EOF\nq Q") + 6;
    assert!(inner_end < p.data.len());
    assert_eq!(summary(run(&p.data)), verified(p.data.len()));
}

#[test]
fn linearized_length_is_checked_against_the_chain() {
    // a minimal "linearized" layout: the parameter dictionary first, the
    // trailer at the stated length; the cross-reference chain decides
    // whether that end is verified
    let build = |damage: bool| {
        let mut p = Pdf::new();
        p.object(9, "<< /Linearized 1 /L 0000000000 >>");
        p.object(1, "<< /Type /Catalog /Pages 2 0 R >>")
            .object(2, "<< /Type /Pages /Kids [] /Count 0 >>");
        let xref = p.classic_end(10);
        let len = p.data.len();
        let at = find(&p.data, b"/L ") + 3;
        p.data[at..at + 10].copy_from_slice(format!("{len:010}").as_bytes());
        if damage {
            let one = find(&p.data, b"1 0 obj");
            let at = entry_of(&p.data, xref, one);
            p.data[at..at + 10].copy_from_slice(format!("{:010}", one + 2).as_bytes());
        }
        p.data
    };
    let good = build(false);
    let m = run(&good).unwrap();
    assert_eq!(
        (m.len, m.confidence, m.end_verified),
        (good.len() as u64, Confidence::Exact, true)
    );
    let bad = build(true);
    let m = run(&bad).unwrap();
    assert_eq!(
        (m.len, m.confidence, m.end_verified),
        (bad.len() as u64, Confidence::Exact, false)
    );
}

#[test]
fn many_revisions_are_verified_one_section_at_a_time() {
    // 40 incremental updates: longer than the chain walked from one
    // `startxref`, but each revision's chain was verified at its own
    // `%%EOF`, and the next one stops there
    let mut p = Pdf::basic();
    p.classic_end(5);
    for i in 0..40 {
        p.object(
            4,
            &format!(
                "<< /Length 21 >>\nstream\n0 0 m {:03} 100 l S\n\nendstream",
                i
            ),
        );
        p.classic_end(5);
    }
    assert_eq!(summary(run(&p.data)), verified(p.data.len()));
}

/// An update that lists many changed objects has a long `/Index` array in
/// its cross-reference stream: the dictionary is longer than 1 KiB (pypdf's
/// incremental writer and qpdf make such files). The update must not be
/// dropped, nor a single revision lose its end.
#[test]
fn long_cross_reference_stream_dictionaries() {
    // `/Index` pairs for 600 changed objects, every other number: ~4 KiB
    let index: String = (0..600).map(|i| format!("{} 1 ", 10 + 2 * i)).collect();
    let long = format!("/Extra [{index}] ");
    assert!(long.len() > 3000);

    // one revision
    let mut p = Pdf::new();
    p.data = b"%PDF-1.5\n".to_vec();
    p.object(1, "<< /Type /Catalog /Pages 2 0 R >>")
        .object(2, "<< /Type /Pages /Kids [] /Count 0 >>");
    p.stream_end_with(3, 4, &long);
    let end1 = p.data.len();
    assert_eq!(summary(run(&p.data)), verified(end1));

    // an update whose stream has the long dictionary
    p.object(2, "<< /Type /Pages /Kids [] /Count 0 /Rotate 90 >>");
    p.stream_end_with(4, 5, &long);
    assert_eq!(summary(run(&p.data)), verified(p.data.len()));

    // a dictionary longer than the 64 KiB read: the end is taken, unverified
    let huge: String = (0..12000).map(|i| format!("{} 1 ", 10 + i)).collect();
    let huge = format!("/Extra [{huge}] ");
    assert!(huge.len() > 64 * 1024);
    let mut q = Pdf::new();
    q.data = b"%PDF-1.5\n".to_vec();
    q.object(1, "<< /Type /Catalog /Pages 2 0 R >>")
        .object(2, "<< /Type /Pages /Kids [] /Count 0 >>");
    q.stream_end_with(3, 4, &huge);
    assert_eq!(summary(run(&q.data)), unverified(q.data.len()));
}
