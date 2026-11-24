//! File contents generated from a seed.
//!
//! A manifest does not carry file contents; it carries a [`ContentSpec`]
//! from which a test re-creates the bytes (`to_vec`, `read_at`) and their
//! SHA-256.
//!
//! * [`ContentKind::Tagged`] is the work horse: every 512-byte block starts
//!   with a text line `hds:<seed>:<block number>` followed by pseudo-random
//!   bytes, so a hex dump of a wrongly recovered file shows which file and
//!   which block the bytes really belong to. The first bytes of a block can
//!   never look like a `FILE` record, an `INDX` block, a boot sector or a
//!   file signature.
//! * The media kinds are complete, valid files of formats HDSavior carves
//!   and whose extension it checks (PNG, JPEG, BMP, PDF, ZIP, WAV), written
//!   from the format specifications. They exist for the geometry validation
//!   (ALGORITHMS F3), the signature flags (G1) and carving (I).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::hash::{adler32_update, crc32, crc32_update, sha256_hex, Sha256};
use crate::rng::{derive_n, mix64, Rng};

/// What a [`ContentSpec`] generates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    /// Self-describing pseudo-random blocks (see the module documentation).
    Tagged,
    /// All zero bytes.
    Zeros,
    /// A PNG image (8-bit grey, stored deflate blocks, all CRCs valid).
    Png,
    /// A baseline JPEG image (JFIF, one component).
    Jpeg,
    /// An uncompressed 8-bit BMP image.
    Bmp,
    /// A PDF 1.4 document with a classic cross-reference table.
    Pdf,
    /// A ZIP archive with one stored entry.
    Zip,
    /// A RIFF/WAVE file with 8-bit PCM samples.
    Wav,
    /// Bytes supplied by the scenario (for example a whole inner disk
    /// image). They are not part of the serialized manifest; a deserialized
    /// spec of this kind knows only length and hash.
    Blob,
}

impl ContentKind {
    /// The file extension that goes with the kind, without the dot.
    pub fn extension(self) -> Option<&'static str> {
        match self {
            ContentKind::Png => Some("png"),
            ContentKind::Jpeg => Some("jpg"),
            ContentKind::Bmp => Some("bmp"),
            ContentKind::Pdf => Some("pdf"),
            ContentKind::Zip => Some("zip"),
            ContentKind::Wav => Some("wav"),
            ContentKind::Tagged | ContentKind::Zeros | ContentKind::Blob => None,
        }
    }

    /// `true` for the kinds that are valid files of a carvable format.
    pub fn is_media(self) -> bool {
        self.extension().is_some()
    }
}

/// The recipe for the logical content of one file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentSpec {
    /// Generator.
    pub kind: ContentKind,
    /// Seed of the generator.
    pub seed: u64,
    /// Length of the content in bytes.
    pub len: u64,
    /// Byte ranges `(start, length)` that read as zero whatever the
    /// generator says: sparse holes and everything beyond a file's
    /// initialized size.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub zero_ranges: Vec<(u64, u64)>,
    #[serde(skip)]
    blob: Option<Arc<Vec<u8>>>,
}

impl ContentSpec {
    /// `len` bytes of tagged pseudo-random content.
    pub fn tagged(seed: u64, len: u64) -> Self {
        ContentSpec { kind: ContentKind::Tagged, seed, len, zero_ranges: Vec::new(), blob: None }
    }

    /// `len` zero bytes.
    pub fn zeros(len: u64) -> Self {
        ContentSpec { kind: ContentKind::Zeros, seed: 0, len, zero_ranges: Vec::new(), blob: None }
    }

    /// A valid file of a media kind, at least `min_len` bytes long and as
    /// close to it as the format allows (exactly `min_len` for PNG, JPEG,
    /// PDF and ZIP above a few hundred bytes). `len` holds the real length.
    pub fn media(kind: ContentKind, seed: u64, min_len: u64) -> Self {
        assert!(kind.is_media(), "{kind:?} is not a media kind");
        // The content of a media spec is defined as "what the generator
        // returns when asked for `len` bytes"; every generator returns
        // exactly the length it produced before when asked for it again.
        let len = generate_media(kind, seed, min_len).len() as u64;
        ContentSpec { kind, seed, len, zero_ranges: Vec::new(), blob: None }
    }

    /// Content given as bytes.
    pub fn blob(bytes: Arc<Vec<u8>>) -> Self {
        ContentSpec {
            kind: ContentKind::Blob,
            seed: 0,
            len: bytes.len() as u64,
            zero_ranges: Vec::new(),
            blob: Some(bytes),
        }
    }

    /// Adds a range that reads as zero.
    pub fn with_zero_range(mut self, start: u64, len: u64) -> Self {
        if len > 0 {
            self.zero_ranges.push((start, len));
        }
        self
    }

    /// `false` only for a deserialized [`ContentKind::Blob`], whose bytes
    /// did not travel with the manifest.
    pub fn is_available(&self) -> bool {
        self.kind != ContentKind::Blob || self.blob.is_some()
    }

    fn generate_all(&self) -> Vec<u8> {
        let mut data = match self.kind {
            ContentKind::Tagged => {
                let mut v = vec![0u8; self.len as usize];
                fill_tagged(b"hds:", self.seed, 0, &mut v);
                v
            }
            ContentKind::Zeros => vec![0u8; self.len as usize],
            ContentKind::Blob => self
                .blob
                .as_ref()
                .expect("the bytes of a blob are not part of a serialized manifest")
                .as_ref()
                .clone(),
            kind => generate_media(kind, self.seed, self.len),
        };
        assert_eq!(data.len() as u64, self.len, "{:?} generator changed its length", self.kind);
        self.apply_zero_ranges(0, &mut data);
        data
    }

    fn apply_zero_ranges(&self, offset: u64, buf: &mut [u8]) {
        let end = offset + buf.len() as u64;
        for &(start, len) in &self.zero_ranges {
            let from = start.max(offset);
            let to = (start + len).min(end);
            if from < to {
                buf[(from - offset) as usize..(to - offset) as usize].fill(0);
            }
        }
    }

    /// The whole content.
    pub fn to_vec(&self) -> Vec<u8> {
        self.generate_all()
    }

    /// Reads `buf.len()` bytes of the content at `offset`. Panics when the
    /// range is not inside the content.
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) {
        let end = offset.checked_add(buf.len() as u64);
        assert!(end.is_some_and(|e| e <= self.len), "read outside the content");
        match self.kind {
            ContentKind::Tagged => {
                fill_tagged(b"hds:", self.seed, offset, buf);
                self.apply_zero_ranges(offset, buf);
            }
            ContentKind::Zeros => buf.fill(0),
            _ => {
                let all = self.generate_all();
                buf.copy_from_slice(&all[offset as usize..offset as usize + buf.len()]);
            }
        }
    }

    /// SHA-256 of the content as lower-case hex.
    pub fn sha256(&self) -> String {
        match self.kind {
            ContentKind::Tagged | ContentKind::Zeros => {
                let mut h = Sha256::new();
                let mut buf = vec![0u8; 1 << 16];
                let mut pos = 0u64;
                while pos < self.len {
                    let take = ((self.len - pos) as usize).min(buf.len());
                    self.read_at(pos, &mut buf[..take]);
                    h.update(&buf[..take]);
                    pos += take as u64;
                }
                h.finish_hex()
            }
            _ => sha256_hex(&self.generate_all()),
        }
    }
}

/// Size of the self-describing blocks of tagged content.
pub const TAG_BLOCK: usize = 512;

/// Fills `buf` with the tagged stream of `seed` starting at byte `offset`.
/// `prefix` is the four-character tag: `hds:` for file content, `old:` for
/// the stale bytes the builder leaves in unused parts of clusters.
pub fn fill_tagged(prefix: &[u8; 4], seed: u64, offset: u64, buf: &mut [u8]) {
    let mut block = [0u8; TAG_BLOCK];
    let mut pos = offset;
    let mut rest = buf;
    while !rest.is_empty() {
        let block_no = pos / TAG_BLOCK as u64;
        let in_block = (pos % TAG_BLOCK as u64) as usize;
        let take = (TAG_BLOCK - in_block).min(rest.len());
        tagged_block(prefix, seed, block_no, &mut block);
        let (chunk, tail) = rest.split_at_mut(take);
        chunk.copy_from_slice(&block[in_block..in_block + take]);
        pos += take as u64;
        rest = tail;
    }
}

fn tagged_block(prefix: &[u8; 4], seed: u64, block_no: u64, block: &mut [u8; TAG_BLOCK]) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    // "hds:" + 16 hex digits of the seed + ":" + 10 hex digits of the block
    // number + "\n" = 32 bytes. Upper-case digits: the byte at offset 4 is
    // never 'f', 'm' or 'w', the letters a box-format sniffer looks for.
    block[..4].copy_from_slice(prefix);
    for i in 0..16 {
        block[4 + i] = HEX[((seed >> (60 - 4 * i)) & 15) as usize];
    }
    block[20] = b':';
    for i in 0..10 {
        block[21 + i] = HEX[((block_no >> (36 - 4 * i)) & 15) as usize];
    }
    block[31] = b'\n';
    let base = mix64(seed) ^ mix64(block_no.wrapping_mul(0xD6E8_FEB8_6659_FD93));
    for (i, chunk) in block[32..].chunks_exact_mut(8).enumerate() {
        chunk.copy_from_slice(&mix64(base.wrapping_add(i as u64)).to_le_bytes());
    }
}

fn generate_media(kind: ContentKind, seed: u64, min_len: u64) -> Vec<u8> {
    let min_len = usize::try_from(min_len).expect("media file too large");
    match kind {
        ContentKind::Png => png(seed, min_len),
        ContentKind::Jpeg => jpeg(seed, min_len),
        ContentKind::Bmp => bmp(seed, min_len),
        ContentKind::Pdf => pdf(seed, min_len),
        ContentKind::Zip => zip(seed, min_len),
        ContentKind::Wav => wav(seed, min_len),
        other => unreachable!("{other:?} is not a media kind"),
    }
}

fn random_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    Rng::new(seed).fill(&mut v);
    v
}

fn printable(seed: u64, len: usize) -> Vec<u8> {
    const ALPHABET: &[u8] = b"0123456789abcdef";
    let mut rng = Rng::new(seed);
    (0..len)
        .map(|i| if i % 65 == 64 { b'\n' } else { ALPHABET[rng.below(16) as usize] })
        .collect()
}

// ------------------------------------------------------------------ PNG --

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32_update(crc32(kind), data).to_be_bytes());
}

/// PNG (ISO/IEC 15948): signature, IHDR, a tEXt chunk that makes up the
/// requested length, IDAT chunks holding a zlib stream of stored blocks,
/// IEND.
fn png(seed: u64, min_len: usize) -> Vec<u8> {
    const WIDTH: usize = 64;
    const ROW: usize = WIDTH + 1; // filter byte + pixels
    const IDAT: usize = 32_768;
    const KEYWORD: &[u8] = b"Comment\0";
    let fixed = 8 + 25 + 12; // signature, IHDR, IEND
    let idat_total = |height: usize| {
        let raw = height * ROW;
        let zlib = 2 + 5 * raw.div_ceil(65_535) + raw + 4;
        zlib + 12 * zlib.div_ceil(IDAT)
    };
    let text_min = 12 + KEYWORD.len();
    let target = min_len.max(fixed + text_min + idat_total(1));
    let budget = target - fixed - text_min;
    let mut height = (budget / ROW).max(1);
    while idat_total(height) > budget {
        height -= 1;
    }
    let text_len = target - fixed - idat_total(height) - 12;

    let mut rng = Rng::new(seed);
    let mut raw = vec![0u8; height * ROW];
    for row in raw.chunks_exact_mut(ROW) {
        rng.fill(&mut row[1..]); // filter type 0
    }
    let mut zlib = vec![0x78, 0x01];
    let blocks = raw.chunks(65_535).count();
    for (i, block) in raw.chunks(65_535).enumerate() {
        zlib.push(u8::from(i + 1 == blocks));
        zlib.extend_from_slice(&(block.len() as u16).to_le_bytes());
        zlib.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    zlib.extend_from_slice(&adler32_update(1, &raw).to_be_bytes());

    let mut out = Vec::with_capacity(target);
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(WIDTH as u32).to_be_bytes());
    ihdr.extend_from_slice(&(height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 0, 0, 0, 0]); // 8 bit, grey, deflate, adaptive, not interlaced
    png_chunk(&mut out, b"IHDR", &ihdr);
    let mut text = KEYWORD.to_vec();
    text.extend(printable(derive_n(seed, 1), text_len - KEYWORD.len()).iter().map(|&c| {
        if c == b'\n' {
            b' '
        } else {
            c
        }
    }));
    png_chunk(&mut out, b"tEXt", &text);
    for chunk in zlib.chunks(IDAT) {
        png_chunk(&mut out, b"IDAT", chunk);
    }
    png_chunk(&mut out, b"IEND", &[]);
    debug_assert_eq!(out.len(), target);
    out
}

// ----------------------------------------------------------------- JPEG --

struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    bits: u32,
}

impl BitWriter {
    fn put(&mut self, value: u32, count: u32) {
        debug_assert!(count <= 16 && value < (1 << count));
        self.acc = (self.acc << count) | value;
        self.bits += count;
        while self.bits >= 8 {
            let byte = (self.acc >> (self.bits - 8)) as u8;
            self.out.push(byte);
            if byte == 0xFF {
                self.out.push(0x00); // byte stuffing
            }
            self.bits -= 8;
        }
        self.acc &= (1 << self.bits) - 1;
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bits > 0 {
            let pad = 8 - self.bits;
            self.put((1 << pad) - 1, pad); // pad with one bits
        }
        self.out
    }
}

/// Entropy-coded data of `blocks` 8x8 blocks that carry only a DC value:
/// per block a 4-bit code for the size category of the DC difference, the
/// difference itself, and the one-bit end-of-block code.
fn jpeg_scan(seed: u64, blocks: usize) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let mut w = BitWriter { out: Vec::with_capacity(blocks * 2), acc: 0, bits: 0 };
    let mut previous = 0i32;
    for _ in 0..blocks {
        let value = rng.range(0, 1000) as i32 - 500;
        let diff = value - previous;
        previous = value;
        let category = 32 - diff.unsigned_abs().leading_zeros();
        w.put(category, 4);
        if category > 0 {
            let bits = if diff > 0 { diff } else { diff + (1 << category) - 1 };
            w.put(bits as u32, category);
        }
        w.put(0, 1);
    }
    w.finish()
}

/// Baseline JPEG (ITU-T T.81) in a JFIF wrapper: one 8-bit component,
/// quantisation table of ones, a DC Huffman table with eleven 4-bit codes,
/// an AC table whose only code is end-of-block. Comment segments make up the
/// requested length.
fn jpeg(seed: u64, min_len: usize) -> Vec<u8> {
    // SOI + APP0 + DQT + SOF0 + two DHT + SOS + EOI
    const FIXED: usize = 2 + 18 + 69 + 13 + 32 + 22 + 10 + 2;
    let budget = min_len.saturating_sub(FIXED);
    // a block takes about 1.7 bytes; start a little low and shrink until the
    // remainder can be filled with comment segments (none, or 4 bytes and up)
    let mut blocks_wide = ((budget as f64 / 1.75).sqrt() as usize).clamp(1, 8191);
    let mut blocks_high = (budget * 4 / 7 / blocks_wide).clamp(1, 8191);
    let scan = loop {
        let scan = jpeg_scan(seed, blocks_wide * blocks_high);
        let fits = match min_len.checked_sub(FIXED + scan.len()) {
            Some(rest) => rest == 0 || rest >= 4,
            None => false,
        };
        if fits || (blocks_wide == 1 && blocks_high == 1) {
            break scan;
        }
        if blocks_high > 1 {
            blocks_high -= 1;
        } else {
            blocks_wide -= 1;
        }
    };
    // Fewer than four missing bytes cannot be a comment segment; the file
    // then gets a minimal one and is that much longer than asked for.
    let mut padding = match min_len.saturating_sub(FIXED + scan.len()) {
        rest @ 1..=3 => rest.max(4),
        rest => rest,
    };

    let mut out = Vec::with_capacity(min_len.max(FIXED + scan.len()));
    out.extend_from_slice(&[0xFF, 0xD8]);
    out.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x10]);
    out.extend_from_slice(b"JFIF\0\x01\x01\x00\x00\x01\x00\x01\x00\x00");
    let mut comment_seed = derive_n(seed, 2);
    while padding > 0 {
        // a comment segment: marker, length (counts itself), text
        let take = padding.min(4 + 65_533);
        let take = if padding - take > 0 && padding - take < 4 { take - 4 } else { take };
        out.extend_from_slice(&[0xFF, 0xFE]);
        out.extend_from_slice(&((take - 2) as u16).to_be_bytes());
        out.extend_from_slice(&printable(comment_seed, take - 4));
        comment_seed = derive_n(comment_seed, 3);
        padding -= take;
    }
    out.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43, 0x00]);
    out.extend_from_slice(&[1u8; 64]);
    out.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 0x08]);
    out.extend_from_slice(&((blocks_high * 8) as u16).to_be_bytes());
    out.extend_from_slice(&((blocks_wide * 8) as u16).to_be_bytes());
    out.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]);
    // DC table 0: eleven codes of length 4 for the categories 0 to 10
    out.extend_from_slice(&[0xFF, 0xC4, 0x00, 0x1E, 0x00]);
    out.extend_from_slice(&[0, 0, 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    out.extend_from_slice(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
    // AC table 0: one code of length 1 for end-of-block
    out.extend_from_slice(&[0xFF, 0xC4, 0x00, 0x14, 0x10]);
    out.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    out.push(0x00);
    out.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00]);
    out.extend_from_slice(&scan);
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

// ------------------------------------------------------------------ BMP --

/// BMP: `BITMAPFILEHEADER`, `BITMAPINFOHEADER`, a grey palette of 256
/// entries and 8-bit pixels. The size is 1078 bytes plus width times height,
/// so only some lengths exist.
fn bmp(seed: u64, min_len: usize) -> Vec<u8> {
    const HEADERS: usize = 14 + 40 + 1024;
    let pixels = min_len.saturating_sub(HEADERS).max(4);
    let width = (pixels.div_ceil(4 * 65_536) * 4).max(4);
    let height = pixels.div_ceil(width);
    let size = HEADERS + width * height;
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(size as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&(HEADERS as u32).to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(width as i32).to_le_bytes());
    out.extend_from_slice(&(height as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&8u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&((width * height) as u32).to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes()); // 72 dpi
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&256u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for grey in 0..=255u8 {
        out.extend_from_slice(&[grey, grey, grey, 0]);
    }
    out.extend_from_slice(&random_bytes(seed, width * height));
    debug_assert_eq!(out.len(), size);
    out
}

// ------------------------------------------------------------------ PDF --

/// PDF 1.4: catalog, page tree, one page with a one-line content stream, a
/// stream object that makes up the requested length, classic xref table,
/// trailer.
fn pdf(seed: u64, min_len: usize) -> Vec<u8> {
    let head = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let page_stream = "0.5 w 72 72 m 540 720 l S";
    let objects = [
        "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n".to_string(),
        "2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n".to_string(),
        "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << >> >>\nendobj\n"
            .to_string(),
        format!("4 0 obj\n<< /Length {} >>\nstream\n{}\nendstream\nendobj\n", page_stream.len(), page_stream),
    ];
    let filler_open = |n: usize| format!("5 0 obj\n<< /Length {n:<10} >>\nstream\n");
    let filler_close = "\nendstream\nendobj\n";
    let before_filler: usize = head.len() + objects.iter().map(String::len).sum::<usize>() + filler_open(0).len();
    let xref_len = "xref\n0 6\n".len() + 6 * 20;
    let trailer_fixed = "trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n".len() + "\n%%EOF\n".len();
    let fixed = before_filler + filler_close.len() + xref_len + trailer_fixed;
    // The decimal offset in "startxref" has a length that depends on the
    // filler size: find the consistent pair.
    let mut target = min_len.max(fixed + 1);
    let filler_len = loop {
        let found = (1..=12usize).find_map(|digits| {
            let n = target.checked_sub(fixed + digits)?;
            let xref_at = before_filler + n + filler_close.len();
            (xref_at.to_string().len() == digits).then_some(n)
        });
        match found {
            Some(n) => break n,
            None => target += 1,
        }
    };

    let mut out = head;
    let mut offsets = Vec::with_capacity(5);
    for object in &objects {
        offsets.push(out.len());
        out.extend_from_slice(object.as_bytes());
    }
    offsets.push(out.len());
    out.extend_from_slice(filler_open(filler_len).as_bytes());
    out.extend_from_slice(&printable(seed, filler_len));
    out.extend_from_slice(filler_close.as_bytes());
    let xref_at = out.len();
    out.extend_from_slice(b"xref\n0 6\n0000000000 65535 f \n");
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n").as_bytes());
    debug_assert_eq!(out.len(), target);
    out
}

// ------------------------------------------------------------------ ZIP --

/// ZIP (APPNOTE 6.3): one stored entry `data.bin`, central directory, end
/// record.
fn zip(seed: u64, min_len: usize) -> Vec<u8> {
    const NAME: &[u8] = b"data.bin";
    const DOS_DATE: u16 = ((2020 - 1980) << 9) | (1 << 5) | 1;
    let overhead = 30 + NAME.len() + 46 + NAME.len() + 22;
    let data_len = min_len.saturating_sub(overhead).max(1);
    let mut data = vec![0u8; data_len];
    fill_tagged(b"zip:", seed, 0, &mut data);
    let crc = crc32(&data);

    let mut out = Vec::with_capacity(overhead + data_len);
    out.extend_from_slice(&[b'P', b'K', 3, 4]);
    out.extend_from_slice(&10u16.to_le_bytes()); // version needed: 1.0
    out.extend_from_slice(&0u16.to_le_bytes()); // flags
    out.extend_from_slice(&0u16.to_le_bytes()); // method: stored
    out.extend_from_slice(&0u16.to_le_bytes()); // time
    out.extend_from_slice(&DOS_DATE.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    out.extend_from_slice(&(NAME.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(NAME);
    out.extend_from_slice(&data);
    let directory_at = out.len();
    out.extend_from_slice(&[b'P', b'K', 1, 2]);
    out.extend_from_slice(&20u16.to_le_bytes()); // made by: 2.0, FAT attributes
    out.extend_from_slice(&10u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&DOS_DATE.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    out.extend_from_slice(&(NAME.len() as u16).to_le_bytes());
    out.extend_from_slice(&[0; 8]); // extra, comment, disk, internal attributes
    out.extend_from_slice(&0x20u32.to_le_bytes()); // external attributes: archive
    out.extend_from_slice(&0u32.to_le_bytes()); // offset of the local header
    out.extend_from_slice(NAME);
    let directory_len = out.len() - directory_at;
    out.extend_from_slice(&[b'P', b'K', 5, 6, 0, 0, 0, 0]);
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(directory_len as u32).to_le_bytes());
    out.extend_from_slice(&(directory_at as u32).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

// ------------------------------------------------------------------ WAV --

/// RIFF/WAVE: `fmt ` chunk for 8-bit mono PCM at 8 kHz, `data` chunk. The
/// length is always even.
fn wav(seed: u64, min_len: usize) -> Vec<u8> {
    let data_len = (min_len.saturating_sub(44).max(2) + 1) & !1;
    let mut out = Vec::with_capacity(44 + data_len);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&8000u32.to_le_bytes());
    out.extend_from_slice(&8000u32.to_le_bytes()); // bytes per second
    out.extend_from_slice(&1u16.to_le_bytes()); // block align
    out.extend_from_slice(&8u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    out.extend_from_slice(&random_bytes(seed, data_len));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tagged_blocks_describe_themselves() {
        let spec = ContentSpec::tagged(0x0123_4567_89AB_CDEF, 2000);
        let data = spec.to_vec();
        assert_eq!(data.len(), 2000);
        assert_eq!(&data[..32], b"hds:0123456789ABCDEF:0000000000\n");
        assert_eq!(&data[512..544], b"hds:0123456789ABCDEF:0000000001\n");
        assert_eq!(&data[1536..1568], b"hds:0123456789ABCDEF:0000000003\n");
        // the random part is not constant and differs between blocks and seeds
        assert_ne!(&data[32..512], &data[544..1024]);
        assert_ne!(ContentSpec::tagged(1, 2000).to_vec()[32..512], data[32..512]);
        assert!(data[32..512].iter().any(|&b| b != data[32]));
    }

    #[test]
    fn random_access_equals_sequential_generation() {
        let spec = ContentSpec::tagged(77, 5000).with_zero_range(700, 900).with_zero_range(4990, 100);
        let all = spec.to_vec();
        assert!(all[700..1600].iter().all(|&b| b == 0));
        assert!(all[4990..].iter().all(|&b| b == 0));
        assert_ne!(all[699], 0);
        for (offset, len) in [(0, 5000), (1, 511), (511, 2), (699, 3), (1599, 600), (4096, 904), (4999, 1), (5000, 0)] {
            let mut buf = vec![0xAAu8; len];
            spec.read_at(offset, &mut buf);
            assert_eq!(buf, &all[offset as usize..offset as usize + len], "offset {offset} len {len}");
        }
        assert_eq!(spec.sha256(), sha256_hex(&all));
        assert_eq!(ContentSpec::zeros(100_000).sha256(), sha256_hex(&vec![0u8; 100_000]));
    }

    #[test]
    fn no_block_start_looks_like_an_ntfs_structure_or_a_file_signature() {
        for seed in [0u64, 0xF000_0000_0000_0000, 0x6D6F_6F76_6D64_6174, u64::MAX] {
            let data = ContentSpec::tagged(seed, 64 * 512).to_vec();
            for block in data.chunks(512) {
                assert_eq!(&block[..4], b"hds:");
                assert!(!matches!(block[4], b'f' | b'm' | b'w'));
                assert_ne!(&block[3..11], b"NTFS    ");
            }
        }
    }

    #[test]
    fn media_lengths() {
        // exact above the minimum for PNG, JPEG, PDF and ZIP
        for kind in [ContentKind::Png, ContentKind::Jpeg, ContentKind::Pdf, ContentKind::Zip] {
            for want in [1000u64, 4096, 4097, 65_535, 65_536, 70_001, 300_000] {
                let spec = ContentSpec::media(kind, 5, want);
                assert_eq!(spec.len, want, "{kind:?} of {want} bytes");
                assert_eq!(spec.to_vec().len() as u64, want);
            }
        }
        // rounded for the formats that cannot have every length
        assert_eq!(ContentSpec::media(ContentKind::Wav, 5, 1001).len, 1002);
        assert_eq!(ContentSpec::media(ContentKind::Wav, 5, 1002).len, 1002);
        let bmp = ContentSpec::media(ContentKind::Bmp, 5, 10_000);
        assert!((10_000..10_004).contains(&bmp.len));
        // a request below the smallest possible file gives that file, and
        // asking for the resulting length again gives the same bytes
        for kind in [ContentKind::Png, ContentKind::Jpeg, ContentKind::Bmp, ContentKind::Pdf, ContentKind::Zip, ContentKind::Wav] {
            let tiny = ContentSpec::media(kind, 9, 0);
            assert!(tiny.len > 0 && tiny.len < 1200, "{kind:?}: {}", tiny.len);
            let again = ContentSpec::media(kind, 9, tiny.len);
            assert_eq!(again.len, tiny.len);
            assert_eq!(again.to_vec(), tiny.to_vec());
            let mut part = vec![0u8; 10];
            tiny.read_at(tiny.len - 10, &mut part);
            assert_eq!(part, tiny.to_vec()[tiny.len as usize - 10..]);
        }
    }

    #[test]
    fn media_files_start_with_their_signatures_and_end_properly() {
        let png = ContentSpec::media(ContentKind::Png, 1, 5000).to_vec();
        assert_eq!(&png[..16], b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR");
        assert_eq!(&png[png.len() - 12..], &[0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82]);
        let jpeg = ContentSpec::media(ContentKind::Jpeg, 1, 5000).to_vec();
        assert_eq!(&jpeg[..11], b"\xFF\xD8\xFF\xE0\x00\x10JFIF\0");
        assert_eq!(&jpeg[jpeg.len() - 2..], &[0xFF, 0xD9]);
        let bmp = ContentSpec::media(ContentKind::Bmp, 1, 5000).to_vec();
        assert_eq!(&bmp[..2], b"BM");
        assert_eq!(u32::from_le_bytes(bmp[2..6].try_into().unwrap()) as usize, bmp.len());
        let pdf = ContentSpec::media(ContentKind::Pdf, 1, 5000).to_vec();
        assert!(pdf.starts_with(b"%PDF-1.4\n"));
        assert!(pdf.ends_with(b"%%EOF\n"));
        let zip = ContentSpec::media(ContentKind::Zip, 1, 5000).to_vec();
        assert_eq!(&zip[..4], b"PK\x03\x04");
        assert_eq!(&zip[zip.len() - 22..zip.len() - 18], b"PK\x05\x06");
        let wav = ContentSpec::media(ContentKind::Wav, 1, 5000).to_vec();
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()) as usize, wav.len() - 8);
    }

    #[test]
    fn pdf_cross_reference_offsets_point_at_the_objects() {
        fn find_last(haystack: &[u8], needle: &[u8]) -> usize {
            haystack.windows(needle.len()).rposition(|w| w == needle).expect("needle present")
        }
        fn number_at(bytes: &[u8], at: usize) -> usize {
            let digits: Vec<u8> = bytes[at..].iter().copied().take_while(u8::is_ascii_digit).collect();
            String::from_utf8(digits).unwrap().parse().unwrap()
        }
        for want in [0usize, 700, 9_999, 10_000, 100_000, 1_000_000] {
            let pdf = ContentSpec::media(ContentKind::Pdf, 3, want as u64).to_vec();
            let startxref = find_last(&pdf, b"startxref\n");
            let xref_at = number_at(&pdf, startxref + 10);
            assert_eq!(&pdf[xref_at..xref_at + 9], b"xref\n0 6\n");
            // entry 0 is the free list head; entries 1 to 5 are the objects
            for object in 1..=5 {
                let entry = xref_at + 9 + 20 * object;
                assert_eq!(&pdf[entry + 10..entry + 20], b" 00000 n \n");
                let offset = number_at(&pdf, entry);
                let expected = format!("{object} 0 obj\n");
                assert_eq!(&pdf[offset..offset + expected.len()], expected.as_bytes());
            }
            // the filler stream is exactly as long as its /Length says
            let object5 = number_at(&pdf, xref_at + 9 + 20 * 5);
            let length_at = object5 + "5 0 obj\n<< /Length ".len();
            let length = number_at(&pdf, length_at);
            let stream_at = object5 + "5 0 obj\n<< /Length ".len() + 10 + " >>\nstream\n".len();
            assert_eq!(&pdf[stream_at + length..stream_at + length + 11], b"\nendstream\n");
        }
    }

    #[test]
    fn jpeg_scan_has_no_markers_and_the_dc_values_decode() {
        let scan = jpeg_scan(11, 5000);
        // every FF is followed by a stuffed zero
        for (i, &b) in scan.iter().enumerate() {
            if b == 0xFF {
                assert_eq!(scan.get(i + 1), Some(&0), "unstuffed FF at {i}");
            }
        }
        // decode the bit stream back: 4-bit category, difference, EOB bit
        let bytes: Vec<u8> = {
            let mut v = Vec::new();
            let mut i = 0;
            while i < scan.len() {
                v.push(scan[i]);
                i += if scan[i] == 0xFF { 2 } else { 1 };
            }
            v
        };
        let mut pos = 0usize;
        let mut take = |n: u32| -> u32 {
            let mut v = 0;
            for _ in 0..n {
                v = (v << 1) | ((bytes[pos / 8] >> (7 - pos % 8)) & 1) as u32;
                pos += 1;
            }
            v
        };
        let mut rng = Rng::new(11);
        let mut previous = 0i32;
        for _ in 0..5000 {
            let category = take(4);
            assert!(category <= 10);
            let raw = take(category) as i32;
            let diff = if category == 0 {
                0
            } else if raw >= 1 << (category - 1) {
                raw
            } else {
                raw - (1 << category) + 1
            };
            assert_eq!(take(1), 0, "end of block");
            let value = previous + diff;
            assert_eq!(value, rng.range(0, 1000) as i32 - 500);
            previous = value;
        }
    }

    #[test]
    fn blob_content_and_serialization() {
        let blob = ContentSpec::blob(Arc::new(vec![1, 2, 3, 4, 5]));
        assert_eq!(blob.to_vec(), [1, 2, 3, 4, 5]);
        assert_eq!(blob.sha256(), sha256_hex(&[1, 2, 3, 4, 5]));
        let json = serde_json::to_string(&blob).unwrap();
        let back: ContentSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len, 5);
        assert!(!back.is_available());
        let tagged = ContentSpec::tagged(9, 100).with_zero_range(10, 20);
        let back: ContentSpec = serde_json::from_str(&serde_json::to_string(&tagged).unwrap()).unwrap();
        assert_eq!(back, tagged);
        assert_eq!(back.to_vec(), tagged.to_vec());
    }
}
