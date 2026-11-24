//! A disk image held in memory as a sparse set of pages.
//!
//! Scenario images are mostly zeros (a 4 GiB "disk" with a few megabytes of
//! structures), so only the pages that were ever written are stored. An
//! [`Image`] can be read at any offset (`read_at`), turned into a `Vec`, or
//! written to a file as a sparse file.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

use crate::hash::Sha256;

/// Size of one stored page.
pub const PAGE: usize = 1 << 16;

/// A sparse in-memory disk image of fixed length. Bytes never written read
/// as zero.
#[derive(Clone, Default)]
pub struct Image {
    len: u64,
    pages: BTreeMap<u64, Box<[u8]>>,
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Image")
            .field("len", &self.len)
            .field("stored_bytes", &self.stored_bytes())
            .finish()
    }
}

impl Image {
    /// An all-zero image of `len` bytes.
    pub fn new(len: u64) -> Self {
        Image { len, pages: BTreeMap::new() }
    }

    /// An image holding exactly `bytes`.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut image = Image::new(bytes.len() as u64);
        image.write_at(0, bytes);
        image
    }

    /// Length in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// `true` for a zero-length image.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bytes of memory held by stored pages.
    pub fn stored_bytes(&self) -> u64 {
        self.pages.len() as u64 * PAGE as u64
    }

    fn check(&self, offset: u64, len: usize, what: &str) {
        let end = offset.checked_add(len as u64);
        assert!(
            end.is_some_and(|e| e <= self.len),
            "{what} of {len} bytes at {offset} is outside the image ({} bytes)",
            self.len
        );
    }

    /// Writes `data` at `offset`. Panics if the range is outside the image:
    /// a builder that writes beyond its disk has a bug.
    pub fn write_at(&mut self, offset: u64, data: &[u8]) {
        self.check(offset, data.len(), "write");
        let mut pos = offset;
        let mut rest = data;
        while !rest.is_empty() {
            let page_no = pos / PAGE as u64;
            let in_page = (pos % PAGE as u64) as usize;
            let take = (PAGE - in_page).min(rest.len());
            let (chunk, tail) = rest.split_at(take);
            match self.pages.get_mut(&page_no) {
                Some(page) => page[in_page..in_page + take].copy_from_slice(chunk),
                None => {
                    // Do not materialise a page for zeros written over zeros.
                    if chunk.iter().any(|&b| b != 0) {
                        let mut page = vec![0u8; PAGE].into_boxed_slice();
                        page[in_page..in_page + take].copy_from_slice(chunk);
                        self.pages.insert(page_no, page);
                    }
                }
            }
            pos += take as u64;
            rest = tail;
        }
    }

    /// Fills `len` bytes at `offset` with `byte`.
    pub fn fill(&mut self, offset: u64, len: u64, byte: u8) {
        assert!(
            offset.checked_add(len).is_some_and(|e| e <= self.len),
            "fill of {len} bytes at {offset} is outside the image ({} bytes)",
            self.len
        );
        let chunk = vec![byte; PAGE];
        let mut pos = offset;
        let end = offset + len;
        while pos < end {
            let take = ((end - pos) as usize).min(PAGE - (pos % PAGE as u64) as usize);
            self.write_at(pos, &chunk[..take]);
            pos += take as u64;
        }
    }

    /// Reads into `buf`; bytes never written are zero. Fails with
    /// `UnexpectedEof` when the range is not inside the image, so it can be
    /// forwarded from a block-source adapter.
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let end = offset.checked_add(buf.len() as u64);
        if !end.is_some_and(|e| e <= self.len) {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "read of {} bytes at {offset} is outside the image ({} bytes)",
                    buf.len(),
                    self.len
                ),
            ));
        }
        let mut pos = offset;
        let mut rest = buf;
        while !rest.is_empty() {
            let page_no = pos / PAGE as u64;
            let in_page = (pos % PAGE as u64) as usize;
            let take = (PAGE - in_page).min(rest.len());
            let (chunk, tail) = rest.split_at_mut(take);
            match self.pages.get(&page_no) {
                Some(page) => chunk.copy_from_slice(&page[in_page..in_page + take]),
                None => chunk.fill(0),
            }
            pos += take as u64;
            rest = tail;
        }
        Ok(())
    }

    /// Reads `len` bytes at `offset`. Panics if the range is outside the image.
    pub fn read_vec(&self, offset: u64, len: usize) -> Vec<u8> {
        self.check(offset, len, "read");
        let mut buf = vec![0u8; len];
        self.read_at(offset, &mut buf).expect("range was checked");
        buf
    }

    /// Copies `len` bytes from `src` to `dst` inside the image (the ranges
    /// may overlap).
    pub fn copy_within(&mut self, src: u64, dst: u64, len: usize) {
        let data = self.read_vec(src, len);
        self.write_at(dst, &data);
    }

    /// `true` when every byte of the range is zero.
    pub fn is_zero(&self, offset: u64, len: u64) -> bool {
        assert!(offset.checked_add(len).is_some_and(|e| e <= self.len), "range outside the image");
        let end = offset + len;
        let first = offset / PAGE as u64;
        let last = if len == 0 { first } else { (end - 1) / PAGE as u64 };
        for (&page_no, page) in self.pages.range(first..=last) {
            let page_start = page_no * PAGE as u64;
            let from = offset.max(page_start) - page_start;
            let to = end.min(page_start + PAGE as u64) - page_start;
            if page[from as usize..to as usize].iter().any(|&b| b != 0) {
                return false;
            }
        }
        true
    }

    /// The whole image as one vector. Meant for small images; panics above
    /// 4 GiB so that a test cannot allocate a whole "disk" by accident.
    pub fn to_vec(&self) -> Vec<u8> {
        assert!(self.len <= 4 << 30, "image of {} bytes is too large for to_vec()", self.len);
        self.read_vec(0, self.len as usize)
    }

    /// Stored pages in ascending offset order as `(offset, bytes)`; the last
    /// page is cut at the image end. Regions between pages are zero.
    pub fn pages(&self) -> impl Iterator<Item = (u64, &[u8])> + '_ {
        self.pages.iter().map(move |(&no, page)| {
            let start = no * PAGE as u64;
            let take = ((self.len - start) as usize).min(PAGE);
            (start, &page[..take])
        })
    }

    /// SHA-256 over a canonical encoding of the content (length, then every
    /// page that holds a non-zero byte with its offset). Two images with
    /// equal bytes have equal digests however they were written; unlike a
    /// plain hash of all bytes this costs nothing for the empty space.
    pub fn digest(&self) -> String {
        let mut h = Sha256::new();
        h.update(b"hds-testkit image v1");
        h.update(&self.len.to_le_bytes());
        for (start, page) in self.pages() {
            if page.iter().any(|&b| b != 0) {
                h.update(&start.to_le_bytes());
                h.update(page);
                // a short last page hashes the same as its zero-padded form
                h.update(&vec![0u8; PAGE - page.len()]);
            }
        }
        h.finish_hex()
    }

    /// Writes the image to `out`, seeking over regions that were never
    /// written (they become holes where the file system supports that).
    pub fn write_to<W: Write + Seek>(&self, out: &mut W) -> io::Result<()> {
        let mut pos = 0u64;
        for (start, page) in self.pages() {
            if page.iter().all(|&b| b == 0) {
                continue;
            }
            if start != pos {
                out.seek(SeekFrom::Start(start))?;
            }
            out.write_all(page)?;
            pos = start + page.len() as u64;
        }
        if pos < self.len {
            // Materialise the length with one byte at the very end.
            out.seek(SeekFrom::Start(self.len - 1))?;
            out.write_all(&[0])?;
        }
        out.flush()
    }

    /// Creates (or truncates) `path` and writes the image to it as a sparse
    /// file.
    pub fn write_to_file(&self, path: &Path) -> io::Result<()> {
        let mut file = File::create(path)?;
        file.set_len(self.len)?;
        self.write_to(&mut file)?;
        file.sync_all()
    }
}

/// Something image bytes can be written to: an in-memory [`Image`] or a
/// file. The scale scenario streams through this trait so that a
/// million-record volume never has to exist in memory.
pub trait Sink {
    /// Writes `data` at `offset`.
    fn put(&mut self, offset: u64, data: &[u8]) -> io::Result<()>;
}

impl Sink for Image {
    fn put(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.write_at(offset, data);
        Ok(())
    }
}

/// A [`Sink`] that writes to a file, buffering consecutive writes. Gaps
/// between writes are never written (sparse file).
pub struct FileSink {
    file: File,
    buf: Vec<u8>,
    buf_start: u64,
    bytes_written: u64,
}

impl FileSink {
    const BUFFER: usize = 4 << 20;

    /// Creates (or truncates) `path` with a length of `len` bytes.
    pub fn create(path: &Path, len: u64) -> io::Result<Self> {
        let file = File::create(path)?;
        file.set_len(len)?;
        Ok(FileSink { file, buf: Vec::with_capacity(Self::BUFFER), buf_start: 0, bytes_written: 0 })
    }

    fn flush_buf(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            self.file.seek(SeekFrom::Start(self.buf_start))?;
            self.file.write_all(&self.buf)?;
            self.bytes_written += self.buf.len() as u64;
            self.buf.clear();
        }
        Ok(())
    }

    /// Bytes handed to the operating system so far.
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// Flushes everything and syncs the file.
    pub fn finish(mut self) -> io::Result<u64> {
        self.flush_buf()?;
        self.file.sync_all()?;
        Ok(self.bytes_written)
    }
}

impl Sink for FileSink {
    fn put(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        let contiguous = offset == self.buf_start + self.buf.len() as u64;
        if !contiguous || self.buf.len() + data.len() > Self::BUFFER {
            self.flush_buf()?;
            self.buf_start = offset;
        }
        if data.len() >= Self::BUFFER {
            self.file.seek(SeekFrom::Start(offset))?;
            self.file.write_all(data)?;
            self.bytes_written += data.len() as u64;
            self.buf_start = offset + data.len() as u64;
        } else {
            self.buf.extend_from_slice(data);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwritten_bytes_are_zero_and_writes_read_back() {
        let mut image = Image::new(10 * PAGE as u64 + 100);
        assert_eq!(image.stored_bytes(), 0);
        assert!(image.is_zero(0, image.len()));
        // a write that crosses a page boundary
        let data: Vec<u8> = (0..300u32).map(|i| (i % 251 + 1) as u8).collect();
        let at = 3 * PAGE as u64 - 100;
        image.write_at(at, &data);
        assert_eq!(image.read_vec(at, data.len()), data);
        assert_eq!(image.read_vec(at - 5, 5), [0; 5]);
        assert_eq!(image.read_vec(at + 300, 5), [0; 5]);
        assert_eq!(image.stored_bytes(), 2 * PAGE as u64);
        assert!(!image.is_zero(at, 1));
        assert!(image.is_zero(0, at));
        assert!(image.is_zero(at + 300, image.len() - at - 300));
        assert!(!image.is_zero(at + 299, image.len() - at - 299));
        // the last, short page
        image.write_at(image.len() - 3, b"end");
        assert_eq!(image.read_vec(image.len() - 3, 3), b"end");
        let full = image.to_vec();
        assert_eq!(full.len() as u64, image.len());
        assert_eq!(&full[at as usize..at as usize + 300], &data[..]);
        assert!(full.ends_with(b"end"));
    }

    #[test]
    fn reads_outside_the_image_fail_instead_of_inventing_data() {
        let image = Image::new(1000);
        let mut buf = [0u8; 16];
        assert!(image.read_at(984, &mut buf).is_ok());
        let err = image.read_at(985, &mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert!(image.read_at(u64::MAX - 3, &mut buf).is_err());
    }

    #[test]
    #[should_panic(expected = "outside the image")]
    fn writes_outside_the_image_panic() {
        let mut image = Image::new(1000);
        image.write_at(990, &[1u8; 11]);
    }

    #[test]
    fn zero_writes_do_not_allocate_and_fill_works() {
        let mut image = Image::new(4 * PAGE as u64);
        image.write_at(100, &[0u8; 1000]);
        assert_eq!(image.stored_bytes(), 0);
        image.fill(PAGE as u64 - 10, 30, 0xFF);
        assert_eq!(image.read_vec(PAGE as u64 - 11, 32), {
            let mut v = vec![0xFFu8; 32];
            v[0] = 0;
            v[31] = 0;
            v
        });
        // zeros over data really overwrite
        image.fill(PAGE as u64 - 10, 30, 0);
        assert!(image.is_zero(0, image.len()));
    }

    #[test]
    fn copy_within_handles_overlap() {
        let mut image = Image::new(4096);
        image.write_at(0, b"abcdefgh");
        image.copy_within(0, 4, 8);
        assert_eq!(image.read_vec(0, 12), b"abcdabcdefgh");
    }

    #[test]
    fn digest_depends_on_content_only() {
        let mut a = Image::new(5 * PAGE as u64 + 17);
        let mut b = Image::new(5 * PAGE as u64 + 17);
        a.write_at(70_000, b"hello");
        // b reaches the same content through a detour that leaves an
        // all-zero page stored
        b.write_at(200_000, b"junk");
        b.write_at(200_000, &[0; 4]);
        b.write_at(70_000, b"hello");
        assert_eq!(a.digest(), b.digest());
        b.write_at(70_004, b"O");
        assert_ne!(a.digest(), b.digest());
        // same bytes at another offset, and another length, differ too
        let mut c = Image::new(5 * PAGE as u64 + 17);
        c.write_at(70_001, b"hello");
        assert_ne!(a.digest(), c.digest());
        let mut d = Image::new(5 * PAGE as u64 + 18);
        d.write_at(70_000, b"hello");
        assert_ne!(a.digest(), d.digest());
        // data in the short last page
        a.write_at(a.len() - 1, &[9]);
        let mut e = Image::new(5 * PAGE as u64 + 17);
        e.write_at(70_000, b"hello");
        e.write_at(e.len() - 1, &[9]);
        assert_eq!(a.digest(), e.digest());
    }

    #[test]
    fn written_file_has_the_same_bytes() {
        let mut image = Image::new(3 * PAGE as u64 + 123);
        image.write_at(10, b"start");
        image.write_at(2 * PAGE as u64 + 50, b"middle");
        let dir = std::env::temp_dir();
        let path = dir.join(format!("hds-testkit-image-{}.img", std::process::id()));
        image.write_to_file(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(bytes, image.to_vec());

        // an image whose tail was never written still gets its full length
        let empty_tail = Image::new(PAGE as u64 * 2);
        let mut cursor = io::Cursor::new(Vec::new());
        empty_tail.write_to(&mut cursor).unwrap();
        assert_eq!(cursor.into_inner().len(), 2 * PAGE);
    }

    #[test]
    fn file_sink_writes_sparse_and_buffered() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("hds-testkit-sink-{}.img", std::process::id()));
        let len = 64 << 20;
        let mut sink = FileSink::create(&path, len).unwrap();
        sink.put(0, b"abc").unwrap();
        sink.put(3, b"def").unwrap(); // contiguous: same buffer
        sink.put(1 << 20, b"far").unwrap(); // gap
        sink.put(0, b"A").unwrap(); // going back
        let big = vec![7u8; FileSink::BUFFER + 5];
        sink.put(8 << 20, &big).unwrap();
        sink.put((8 << 20) + big.len() as u64, b"tail").unwrap();
        let written = sink.finish().unwrap();
        assert_eq!(written, 3 + 3 + 3 + 1 + big.len() as u64 + 4);
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(bytes.len() as u64, len);
        assert_eq!(&bytes[..6], b"Abcdef");
        assert_eq!(&bytes[1 << 20..(1 << 20) + 3], b"far");
        assert_eq!(&bytes[8 << 20..(8 << 20) + big.len()], &big[..]);
        assert_eq!(&bytes[(8 << 20) + big.len()..(8 << 20) + big.len() + 4], b"tail");
        assert!(bytes[6..1 << 20].iter().all(|&b| b == 0));
    }
}
