//! File signature sniffing and structural length measurement for carving.
//!
//! When the MFT record of a file is gone, the file's name, size and location
//! are gone with it; the data is still on the disk. This crate answers the
//! two questions that are left:
//!
//! * [`sniff`]: do these bytes - the start of a disk sector - look like the
//!   start of a file of a known format? The scanner asks this for every
//!   512-byte sector of the disk, so "no" has to cost about a nanosecond and
//!   "yes" has to be rare on data that is not a file start.
//! * [`measure`]: given such a start, where does the file end? The answer
//!   comes from walking the format's own structure, assuming the file lies
//!   on the disk in one piece, and from rejecting data that does not hold
//!   up as that format.
//!
//! A third function, [`matches_extension`], serves files that *were* found
//! through the MFT: does the content start the way a file with that
//! extension can start?
//!
//! ```
//! use hds_carve::{measure, sniff, Confidence, FileKind};
//!
//! // a complete 1x1 PNG, followed by what the disk held after it
//! let mut disk = vec![
//!     0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
//!     0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x00, 0x00, 0x00, 0x00, 0x3A, 0x7E, 0x9B,
//!     0x55, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0x63, 0xA8, 0x07, 0x00, 0x00,
//!     0x81, 0x00, 0x80, 0x7E, 0x1C, 0x29, 0xC7, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
//!     0x42, 0x60, 0x82,
//! ];
//! disk.resize(4096, 0);
//!
//! let kind = sniff(&disk).expect("a PNG starts here");
//! assert_eq!(kind, FileKind::Png);
//! let file = measure(kind, &disk, disk.len() as u64).expect("and it is complete");
//! assert_eq!((file.len, file.confidence, file.ext), (67, Confidence::Exact, "png"));
//! ```
//!
//! # What the answers mean
//!
//! `sniff` returns a *candidate*. Every signature it accepts has at least 32
//! bits fixed by the format and nearly all have many more (the table below
//! says how many), and the first header behind the magic number is checked
//! for the values real encoders produce. On random data, which is what
//! compressed video looks like, the weakest signature decides: a TIFF header
//! whose directory lies elsewhere in the file, 2<sup>-32</sup> per sector or
//! one false candidate per 2 TiB. A run over 2<sup>34</sup> random sectors
//! found one (`tests/false_positives.rs`); megabytes of text, tested at
//! every offset, give none. A false candidate costs nothing until `measure`
//! looks at it, and `measure` turns it down after one or two small reads.
//!
//! `measure` returns the length with a [`Confidence`]:
//!
//! * [`Exact`](Confidence::Exact): the format's structure delimits the file
//!   and was followed to that end. The length is right *if the file is
//!   contiguous*.
//! * [`Probable`](Confidence::Probable): a plausible end was found, but the
//!   format cannot prove it: it has no end marker (TIFF, MP3), may have been
//!   appended to (PDF), or what was found leaves a doubt that is spelled out
//!   in the table.
//! * `None`: not a file of that kind, or not all of it is there (`max_len`,
//!   the end of the data, a read error), or it cannot be measured (see
//!   "known gaps"), or the work limit was reached. [`try_measure`] says
//!   which.
//!
//! `Exact` is a statement about length, not about content. How much the
//! content is vouched for differs:
//!
//! * **Verified:** PNG (every chunk has a CRC, all are checked). For 7z and
//!   RAR the archive's own headers are CRC-checked, including the one at the
//!   end; the packed data is not.
//! * **Anchored at the end:** JPEG (the scan must run into `EOI` without
//!   meeting an illegal marker), GIF (trailer), ZIP (central directory and
//!   end record where the local headers lead), MP4/MOV with the index last,
//!   AVI with an index, Matroska (the last element ends where the segment
//!   does), PDF (trailer pointing back at a cross-reference table). If such
//!   a file is fragmented, the walk runs from its first fragment into
//!   foreign data and practically always fails; the exception is foreign
//!   data of the same format (the second half of another JPEG).
//! * **From the header alone:** BMP, WAV, WebP, HEIC, and MP4 with the media
//!   data last. These end in payload whose length the header states;
//!   nothing behind the header can contradict it, so a fragmented file of
//!   these kinds is measured as `Exact` all the same. No carver can do better
//!   without decoding the payload.
//!
//! # Verified ends
//!
//! [`Measured::end_verified`] says whether the structure *proves* the end:
//! bytes at the computed end were read and confirmed by the format, so a
//! file whose clusters are not contiguous would almost certainly have
//! failed the walk instead of being measured. It is `true` exactly in these
//! cases:
//!
//! | Kind | `end_verified` |
//! |---|---|
//! | JPEG | always: the entropy-coded data was scanned into `EOI` |
//! | PNG | always: `IEND` after a chain of chunks whose CRCs all match |
//! | GIF | always: the trailer after a walk of every block |
//! | PDF | always: `%%EOF` whose `startxref` leads to a cross-reference section of this file (or a linearized length confirmed by `%%EOF`) |
//! | ZIP | when the end record also agrees with the entry count and directory size the walk found (that is, when `Exact`) |
//! | 7z | when the end header was read and its CRC matched (that is, when `Exact`) |
//! | RAR | when the end-of-archive block was found (its header CRC checked) |
//! | MP4, MOV, HEIC, AVIF, CR3 | when `Exact` and a structure box came after the last media box (`mdat`, `imda`, `moof`): a `moov` or `meta` whose children form a consistent chain, or an `mfra` ending in an `mfro` that repeats its size. Files that end in `mdat` (`faststart` MP4, every HEIC) are not verified |
//! | AVI, WAV, WebP | when `Exact` (the chunk chain lands on the RIFF size) and the last chunk is not bulk payload (`LIST movi`, `data`, `VP8 `, `VP8L`, `ALPH`, `ANMF`): an `idx1`, `LIST INFO`, `EXIF` read behind the payload |
//! | Matroska, WebM | when the segment has a size, the top-level chain lands on it, and the last element that is not padding is neither a `Cluster` nor `Attachments` (typically `Cues` or `Tags`) |
//! | BMP, TIFF, MP3 | never: header arithmetic, the furthest pointer, a stream that is complete after any frame |
//!
//! A carver cannot do better for the "never" kinds without decoding the
//! payload; the carving job rates such files `uncertain`.
//!
//! What is verified is the end, and only as deeply as each walker looks.
//! For the chain formats (ISO BMFF, RIFF, Matroska) the header of the last
//! structure is read where the chain leads, but its payload is not: foreign
//! data inside an `idx1`, a `Cues` element or the last child of `moov` goes
//! unnoticed. For PDF the trailer between the cross-reference section and
//! `%%EOF` is not checked. A JPEG continued by another JPEG's data passes
//! (see "Known gaps"). `tests/end_verified.rs` holds every kind to these
//! limits with cut, damaged and interrupted files.
//!
//! # Formats
//!
//! "Signature" is what `sniff` insists on, with the number of bits that a
//! random sector would have to get right. "End" is how `measure` finds the
//! end of the file. Details are in each module's documentation.
//!
//! | Kind, extensions | Signature | End | Confidence | Known gaps |
//! |---|---|---|---|---|
//! | [`Jpeg`](FileKind::Jpeg): `jpg` | `FF D8 FF` and a first segment as real encoders write it: APP0 `JFIF`/`JFXX`, APP1 `Exif`/XMP, APP2 ICC/MPF/FlashPix, APP8 SPIFF, APP12 `Ducky`, APP13 `Photoshop 3.0`, APP14 `Adobe`, a printable COM, or a structurally valid DQT, DHT, SOF or DRI (40 bits or more) | Marker segments by their lengths, tables and frame header checked; each scan's entropy-coded data scanned for the next marker (`FF00` stuffing, `RSTn` in order, fill bytes); ends at the first `EOI` at marker level, so an Exif thumbnail inside APP1 never ends the file | Exact | Data behind `EOI` is left out on purpose: further images of an MPO file, motion-photo video, vendor trailers. Not decoded: a fragmented file continued by another JPEG's data passes. Other first segments (APP3-7, 9-11, 15), hierarchical JPEG and JPEG-LS are not sniffed. |
//! | [`Png`](FileKind::Png): `png` | 8-byte signature, `IHDR` with length 13 and valid fields (128 bits) | Chunk walk through `IEND`, **every CRC verified** (the whole file is read: PNGs are small and this is the one image format that can prove it is intact) | Exact | Apple's `CgBI` variant, MNG and JNG are not sniffed. |
//! | [`Gif`](FileKind::Gif): `gif` | `GIF87a`/`GIF89a`, non-zero screen size (48 bits) | Block walk: image descriptors, the four extension types, data sub-blocks, trailer `3B`; at least one image | Exact | No checksums, LZW data not decoded. Files with stray bytes between blocks are rejected. A line of text starting with `GIF89a` can pass `sniff` (not `measure`). |
//! | [`Bmp`](FileKind::Bmp): `bmp` | `BM`, reserved words zero, a known DIB header size, one plane, valid depth and compression, sane dimensions (about 100 bits) | Arithmetic on the header: pixel offset + padded rows (+ V5 embedded profile), compared with the file-size field | Exact when both agree; Probable for RLE data, a missing or smaller size field, or up to 1 KiB of declared padding | Length from the header alone. JPEG/PNG-compressed DIBs are rejected. |
//! | [`Tiff`](FileKind::Tiff): `tif`, `dng`, `cr2`, `nef`, `arw`, `pef`, `srw`, `orf`, `rw2` | `II*\0`/`MM\0*` and an even directory offset of 8 or more (33 bits), plus a plausible first directory entry when it is inside the first 64 bytes (it is in every camera RAW file); CR2 `CR\x02\0`; ORF `IIRO`/`IIRS`/`MMOR` with the directory at 8; RW2 `IIU\0` and its 16-byte signature | Directory walk: the IFD chain and `SubIFDs` strictly, Exif/GPS/Interoperability IFDs leniently; the furthest byte of any directory, out-of-line value, strip, tile or `JPEGInterchangeFormat` preview. Extension from the header (CR2, ORF, RW2), `DNGVersion`, or Make plus sensor data (NEF, ARW, PEF, SRW) | Probable, always: TIFF has neither an end marker nor a length | See "TIFF-based RAW" below. Padding that nothing refers to is not included (Pillow pads multi-page files). Pointers inside maker notes are not followed. BigTIFF is not supported. |
//! | [`Pdf`](FileKind::Pdf): `pdf` | `%PDF-1.x` or `%PDF-2.x` (about 60 bits) | Linearized file: the length `/L` from the first object, confirmed by a `%%EOF` ending there. Otherwise a sequential scan for `%%EOF` whose `startxref` leads to a cross-reference table or stream *of this file* (an embedded or neighbouring PDF fails that); continues while an incremental update follows | Exact for a linearized file that was not updated afterwards; otherwise Probable | A wrong `startxref` (some generators, damaged files) means `None`. One end-of-line after the last `%%EOF` is included, further white space is not. Not sniffed when the header is not at offset 0. Non-linearized files beyond the read limit are not measured. |
//! | [`Zip`](FileKind::Zip): `zip`, `docx`, `xlsx`, `pptx`, `vsdx`, `odt`, `ods`, `odp`, `odg`, `epub`, `jar`, `apk` | `PK\x03\x04`, version needed up to 6.3, reserved flag bits clear, a method from the specification, a name without control characters (about 55 bits) | Local headers one after another (data skipped by the size in the header or its ZIP64 field; entries streamed with a data descriptor are scanned for it), central directory, ZIP64 end record and locator, end record. The directory offset in the end record must equal where the walk found the directory | Exact; Probable if entry count or directory size in the end record disagree | Nothing is inflated, CRCs are not checked. Streamed entries have to be read; an archive with more streamed data than the read limit (1 GiB) is not measured. Spanned archives, encrypted directories, and archives with gaps between entries are rejected. |
//! | [`Mp4`](FileKind::Mp4): `mp4`, `mov`, `m4v`, `m4a`, `m4b`, `3gp`, `3g2`, `f4v` | `ftyp` at offset 4 with a size of 16-1024 divisible by 4 and printable brands (about 60 bits). Without `ftyp`: `wide` + `mdat` (96 bits), `moov` starting with an `mvhd` of 108 or 120 bytes (over 90 bits), `mdat` with a 64-bit size (80 bits) or a 32-bit size below 16 MiB (40 bits) | Top-level box walk, 32- and 64-bit sizes, headers only. Needs a `moov` whose children are a consistent chain with `mvhd`, and media (`mdat`). Ends before the first thing that is not a known box; a second `ftyp`, or media after an index that followed media, starts the next file. An unknown box type is accepted only if a known box follows it | Exact; Probable without `ftyp`, for a fragmented file without `mfra` at the end, when an unknown but box-like structure follows, or at a second `moov` | A box that runs "to end of file" (size 0: an interrupted recording) cannot be measured. Sample tables are not compared with the media box. Data appended outside the boxes (Samsung, Insta360 trailers) is not included. |
//! | [`Heic`](FileKind::Heic): `heic`, `avif` | `ftyp` as above with major brand `heic`, `heix`, `heim`, `heis`, `hevc`, `hevx`, `hevm`, `hevs`, `mif1`, `msf1`, `miaf`, `avif` or `avis` | Same walk; needs `meta` with a handler, and item data (`mdat`, or `idat` inside `meta`) | Exact | Item locations are not compared with the media box. Length from the header alone. HEIF files with other major brands are sniffed as `Mp4` (and measured, with the extension set from the compatible brands). |
//! | [`Cr3`](FileKind::Cr3): `cr3`, `crm` | `ftyp` as above with major brand `crx ` | Same walk; needs `moov` and `mdat` | Exact | Tested with a reconstruction of the documented layout only: no CR3 encoder exists outside the cameras. |
//! | [`Avi`](FileKind::Avi): `avi` | `RIFF`, `AVI `, `LIST`, `hdrl` (128 bits) | RIFF size; the sub-chunks must end exactly there; needs `hdrl` and `movi`; continues over OpenDML `RIFF`-`AVIX` chunks | Exact; Probable when the chunk chain breaks before the stated end | A recording that was never finalised (size 0) cannot be measured. |
//! | [`Wav`](FileKind::Wav): `wav` | `RIFF`, `WAVE`, a printable chunk id (about 70 bits); or `RF64`/`BW64`, size `FFFFFFFF`, `WAVE`, `ds64` (128 bits) | RIFF size, from `ds64` for RF64/BW64; sub-chunk walk; needs `fmt ` and `data` | Exact; Probable as for AVI | Length from the header alone. Unfinalised recordings and Sony Wave64 are not measured. |
//! | [`Webp`](FileKind::Webp): `webp` | `RIFF`, `WEBP`, then `VP8 `, `VP8L` or `VP8X` (over 90 bits) | RIFF size; sub-chunk walk; needs an image chunk | Exact; Probable as for AVI | Length from the header alone. |
//! | [`Mkv`](FileKind::Mkv): `mkv`, `mka`, `webm` | EBML header `1A 45 DF A3` whose children are EBML header elements, with document type `matroska` or `webm` inside the first 64 bytes (over 100 bits) | Segment size; the top-level elements (known IDs, sized) must end exactly there; needs `Tracks`. Unknown segment size (written to a pipe, or never finished): complete top-level elements are followed as far as they go | Exact with a segment size; Probable without | Clusters of unknown size (live WebM) are not measured. One small read per cluster. |
//! | [`Mp3`](FileKind::Mp3): `mp3` | An ID3v2 header: version 2-4, revision 0, defined flags, 7-bit size bytes, a frame id in capitals (about 58 bits); or a layer III frame header carrying `Xing`, `Info` or `VBRI` (about 45 bits) | ID3v2 skipped; frames followed while valid and consistent, at least 4; stops at the length a `Xing` header states, else at the first non-frame; a further `Xing` frame or `ID3` tag is the next file; APEv2 and ID3v1 tags included | Probable, always | Bare frames without tag or VBR header are not sniffed (11 sync bits are no signature). Layer I/II are not measured. A partial last frame, Lyrics3 and ID3v2-at-end tags are left out. |
//! | [`SevenZip`](FileKind::SevenZip): `7z` | 6-byte signature, format version, and the start header's CRC-32 (over 90 bits) | 32 + `NextHeaderOffset` + `NextHeaderSize`; the end header's CRC is verified | Exact; Probable if the end header exceeds 64 MiB and is not read | Packed data is not verified. A multi-volume archive measures as the sum of its volumes. |
//! | [`Rar`](FileKind::Rar): `rar` | RAR 4: 7-byte marker and an archive header block (64 bits). RAR 5: 8-byte signature and an archive or encryption header (over 64 bits) | Block walk, every header's CRC verified, data skipped, to the end-of-archive block | Exact; Probable for RAR 4 without an end block (old versions, `-en`) | Archives with encrypted headers cannot be walked. Zero padding that RAR 5 adds behind the end block of a volume is not included. |
//!
//! ## TIFF-based RAW: how far the directory walk gets
//!
//! No camera RAW encoder is available to the test suite (the DNG samples
//! come from a library, the NEF-like sample is a TIFF arranged like one), so
//! the following is from the published structure of the formats, not from
//! tests:
//!
//! * **DNG**: all image data hangs off IFD0 and its `SubIFDs`; private data
//!   is stored as tag values. The walk covers the whole file.
//! * **Canon CR2**: four chained IFDs (preview, thumbnail, small RGB, raw);
//!   the losslessly compressed raw strip is last in the file. Checked by
//!   hand against the header and directories of one real file (from an EOS
//!   Rebel T5i; its 22 MB of image payload were not at hand): the walk ends
//!   exactly at the end of the raw strip.
//! * **Nikon NEF, Sony ARW, Pentax PEF, Samsung SRW**: raw strips and the
//!   large preview are reachable through IFD0, IFD1 and `SubIFDs`, and the
//!   raw data is written last. Smaller previews referenced only from the
//!   maker note (and Sony's `SR2Private` block) are not followed; they are
//!   expected to lie before the raw data.
//! * **Olympus ORF**: raw strips in IFD0; previews only through the maker
//!   note. Same expectation.
//! * **Panasonic RW2, Leica RWL**: the raw data is located by
//!   `RawDataOffset` and has no stated length. Recognised (so that
//!   [`matches_extension`] works) but not measured.
//!
//! Hence `Probable` for all of them: if a vendor stores something behind the
//! raw data that only the maker note points to, the measured file is short
//! by that much.
//!
//! ## Not supported
//!
//! * Camera RAW formats that are not TIFF: Fujifilm RAF, Sigma X3F, Canon
//!   CRW, Minolta MRW. RAF and CRW have strong signatures and headers that
//!   state all extents; they are left out only because nothing here can
//!   produce a sample to test them with.
//! * MPEG transport and program streams (AVCHD `.mts`, `.mpg`): no file
//!   header, one sync byte per packet. ASF/WMV, FLV, Ogg, FLAC, AAC: not in
//!   the candidate list.
//! * OLE2 compound files (`.doc`, `.xls`, `.ppt`, `.msg`): [`matches_extension`]
//!   knows the signature only as the form an encrypted `.docx` takes.
//!
//! # For the carving job
//!
//! * **Candidates inside files.** A sector-aligned position inside a file
//!   can look like a file start, and sometimes *is* one: a JPEG frame of a
//!   Motion-JPEG video, a picture stored uncompressed in an archive, the
//!   local header of the 500th entry of a ZIP, a `moov` box. `measure`
//!   rejects those that are not self-contained, but an embedded JPEG is a
//!   JPEG. Measure candidates in ascending offset order and skip every
//!   candidate that lies inside a file already measured.
//! * **`max_len`** should be the distance to the next cluster known to
//!   belong to another file. A file whose structure would have to extend
//!   beyond it is reported as `None`; `measure` does not shorten a file to
//!   make it fit. (Formats that are complete at more than one place - an
//!   MP3 after any frame, a PDF after any revision, a fragmented MP4 after
//!   any fragment - can come back shorter, as `Probable`. And a file made
//!   of self-contained top-level units, such as the RIFF chunks of an
//!   OpenDML AVI, is structurally whole if `max_len` falls exactly between
//!   two of them.)
//! * **The reader.** [`ReadAt`] offsets are relative to the candidate.
//!   Formats that are read sequentially are fetched in blocks that start at
//!   64 KiB and double up to 1 MiB (most candidates are small, or no files
//!   at all); the others in 16 KiB blocks around each header visited. Block
//!   reads start on multiples of 4096 relative to the candidate, which is
//!   cluster-aligned, and are whole multiples of 4096 long unless `max_len`
//!   cuts them; a few reads of some dozen bytes are neither. A read error
//!   abandons the candidate ([`MeasureError::Io`]), except that a failed
//!   read-ahead around a header is retried for just the header.
//! * **Cost on a hard disk.** MP4, MOV, AVI, WAV, WebP, HEIC, 7z, RAR and
//!   ZIP (with sizes in the local headers) cost a handful of seeks whatever
//!   their size; Matroska and fragmented MP4 one seek per cluster or
//!   fragment; TIFF one per directory. JPEG, PNG, GIF, PDF (not linearized),
//!   MP3 and streamed ZIP entries are read from start to end, once.
//!   [`Limits`] caps both kinds of work.
//!
//! # Tests
//!
//! `tests/data` holds 130-odd small files written by real encoders (FFmpeg,
//! libjpeg-turbo, Pillow, ImageMagick, libtiff, libheif, Ghostscript, qpdf,
//! Info-ZIP, 7-Zip, RAR 4/6/7, LAME, mkvmerge, office and e-book writers);
//! `tests/data/generate.sh` rebuilds them. `tests/samples.rs` holds each of
//! them to its exact length whatever follows it on the disk;
//! `tests/synthetic.rs` and `tests/large.rs` cover what those encoders do
//! not produce on demand (64-bit boxes, OpenDML, files without `ftyp`,
//! multi-gigabyte layouts, damage); `tests/end_verified.rs` holds every kind
//! to the "Verified ends" rule; `tests/robustness.rs` feeds mutated and
//! hostile input; `tests/false_positives.rs` and `tests/throughput.rs`
//! measure `sniff`. `examples/probe.rs` runs both functions over any files
//! given to it - the quickest way to try a camera's or a phone's output.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod bmff;
mod bmp;
mod ext;
mod gif;
mod jpeg;
mod mkv;
mod mp3;
mod pdf;
mod png;
mod rar;
mod reader;
mod riff;
mod sevenz;
mod sniff;
mod tiff;
mod util;
mod zip;

use std::fmt;
use std::str::FromStr;

pub use ext::{kinds_for_extension, matches_extension};
pub use reader::ReadAt;
pub use sniff::{sniff, SNIFF_LEN};

use reader::{Failure, Src};

/// A family of file formats that can be recognised and measured.
///
/// The variants are split where [`sniff`] can tell formats apart from the
/// first bytes and a user would look for them separately (HEIC photos and
/// Canon CR3 raw files are ISO-BMFF like MP4 video, but they are not
/// videos); formats that only differ deeper inside share a variant and are
/// told apart by [`Measured::ext`] (a `.docx` is a [`Zip`](Self::Zip), a
/// `.nef` a [`Tiff`](Self::Tiff), a `.mov` an [`Mp4`](Self::Mp4)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FileKind {
    /// JPEG image (JFIF, Exif, Adobe).
    Jpeg,
    /// PNG image, APNG included.
    Png,
    /// GIF image or animation.
    Gif,
    /// Windows or OS/2 bitmap.
    Bmp,
    /// TIFF image, and camera RAW formats structured as TIFF: DNG, CR2, NEF,
    /// ARW, PEF, SRW, ORF, RW2.
    Tiff,
    /// PDF document.
    Pdf,
    /// ZIP archive, and the document formats that are ZIP archives: Office
    /// Open XML, OpenDocument, EPUB, JAR, APK.
    Zip,
    /// ISO base media audio and video: MP4, QuickTime MOV, M4V, M4A, 3GP.
    Mp4,
    /// HEIF still images and sequences: HEIC, AVIF.
    Heic,
    /// Canon CR3 raw image (and CRM raw movie).
    Cr3,
    /// AVI video, OpenDML included.
    Avi,
    /// WAV audio, RF64 / BW64 included.
    Wav,
    /// WebP image.
    Webp,
    /// Matroska or WebM audio and video.
    Mkv,
    /// MP3 audio.
    Mp3,
    /// 7z archive.
    SevenZip,
    /// RAR archive, formats 1.5-4.x and 5.
    Rar,
}

impl FileKind {
    const ALL: [FileKind; 17] = [
        FileKind::Jpeg,
        FileKind::Png,
        FileKind::Gif,
        FileKind::Bmp,
        FileKind::Tiff,
        FileKind::Pdf,
        FileKind::Zip,
        FileKind::Mp4,
        FileKind::Heic,
        FileKind::Cr3,
        FileKind::Avi,
        FileKind::Wav,
        FileKind::Webp,
        FileKind::Mkv,
        FileKind::Mp3,
        FileKind::SevenZip,
        FileKind::Rar,
    ];

    /// Stable lower-case identifier, used in the UI protocol and as a folder
    /// name. Never changes for an existing variant.
    pub fn name(self) -> &'static str {
        match self {
            FileKind::Jpeg => "jpeg",
            FileKind::Png => "png",
            FileKind::Gif => "gif",
            FileKind::Bmp => "bmp",
            FileKind::Tiff => "tiff",
            FileKind::Pdf => "pdf",
            FileKind::Zip => "zip",
            FileKind::Mp4 => "mp4",
            FileKind::Heic => "heic",
            FileKind::Cr3 => "cr3",
            FileKind::Avi => "avi",
            FileKind::Wav => "wav",
            FileKind::Webp => "webp",
            FileKind::Mkv => "mkv",
            FileKind::Mp3 => "mp3",
            FileKind::SevenZip => "7z",
            FileKind::Rar => "rar",
        }
    }

    /// The extension (lower case, no dot) a file of this kind gets when
    /// nothing more specific is known. [`measure`] may refine it, see
    /// [`Measured::ext`].
    pub fn default_ext(self) -> &'static str {
        match self {
            FileKind::Jpeg => "jpg",
            FileKind::Tiff => "tif",
            other => other.name(),
        }
    }

    /// A short human-readable description, for messages such as
    /// "The file starts with a valid JPEG image header."
    pub fn description(self) -> &'static str {
        match self {
            FileKind::Jpeg => "JPEG image",
            FileKind::Png => "PNG image",
            FileKind::Gif => "GIF image",
            FileKind::Bmp => "BMP image",
            FileKind::Tiff => "TIFF image or camera RAW file",
            FileKind::Pdf => "PDF document",
            FileKind::Zip => "ZIP archive or ZIP-based document",
            FileKind::Mp4 => "MP4 or QuickTime video",
            FileKind::Heic => "HEIC or AVIF image",
            FileKind::Cr3 => "Canon CR3 raw image",
            FileKind::Avi => "AVI video",
            FileKind::Wav => "WAV audio",
            FileKind::Webp => "WebP image",
            FileKind::Mkv => "Matroska or WebM video",
            FileKind::Mp3 => "MP3 audio",
            FileKind::SevenZip => "7z archive",
            FileKind::Rar => "RAR archive",
        }
    }

    /// Every supported kind, in a stable order.
    pub fn all() -> &'static [FileKind] {
        &Self::ALL
    }

    /// The kind with the given [`name`](Self::name).
    pub fn from_name(name: &str) -> Option<FileKind> {
        Self::ALL.iter().copied().find(|k| k.name() == name)
    }
}

impl fmt::Display for FileKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Error of [`FileKind::from_str`]: the name is not a kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownKind;

impl fmt::Display for UnknownKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unknown file kind")
    }
}

impl std::error::Error for UnknownKind {}

impl FromStr for FileKind {
    type Err = UnknownKind;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        FileKind::from_name(s).ok_or(UnknownKind)
    }
}

/// How much a measured length can be relied on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Confidence {
    /// The format's own structure delimits the file, and it was followed and
    /// validated to that end.
    Exact,
    /// A plausible end was found, but the format cannot prove that the file
    /// ends there (and not a little earlier or later).
    Probable,
}

/// The result of [`measure`]: where a contiguous file ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measured {
    /// Length of the file in bytes, from the candidate's first byte.
    pub len: u64,
    /// Whether the structure proves `len` or merely suggests it.
    pub confidence: Confidence,
    /// Extension for the recovered file (lower case, no dot). Either the
    /// kind's [`default_ext`](FileKind::default_ext) or something more
    /// specific found inside: `docx`, `mov`, `nef`, `webm`, ...
    pub ext: &'static str,
    /// Whether the structure proves the end: the walk reached `len` by
    /// following the format's own chain through every payload, and what it
    /// found there confirms the end (an end marker, an end record, an
    /// index or header read *behind* the last payload). `false` when the
    /// end is only arithmetic on a header written before the payload, or
    /// when the format has no end at all. The rule for every kind is in
    /// the crate documentation ("Verified ends"); the carving job rates a
    /// carved file `good` only when this is `true` (ALGORITHMS I3).
    ///
    /// This is a statement about where the file ends, independent of
    /// [`confidence`](Self::confidence): a PDF that may have been appended
    /// to is `Probable` but its `%%EOF` is verified; a BMP whose size field
    /// agrees with its pixel arithmetic is `Exact` but nothing at its end
    /// was looked at.
    pub end_verified: bool,
}

/// Bounds on the work one [`measure`] call may do.
///
/// A length field in a damaged or hostile header can point anywhere. Walkers
/// that only hop between headers are unimpressed, but those that have to
/// read payload (JPEG, PNG, GIF, PDF, MP3, streamed ZIP entries) would follow
/// it across the disk. When either limit is hit the candidate is abandoned
/// (`None`), never reported with a guessed length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Most bytes read from the source, in total.
    pub max_read_bytes: u64,
    /// Most separate reads issued to the source. On a hard disk each one can
    /// be a seek.
    pub max_reads: u64,
}

impl Limits {
    /// The limits [`measure`] uses: 1 GiB read, 100 000 reads.
    ///
    /// 1 GiB is more than any still image, document or song, and about
    /// seven seconds of sequential reading on a hard disk; formats that can
    /// be larger (video, archives) are walked header by header and read
    /// almost nothing.
    pub const DEFAULT: Limits = Limits {
        max_read_bytes: 1 << 30,
        max_reads: 100_000,
    };
}

impl Default for Limits {
    fn default() -> Self {
        Limits::DEFAULT
    }
}

/// Why [`try_measure`] did not produce a length.
#[derive(Debug)]
pub enum MeasureError {
    /// The data is not a valid, complete, measurable file of that kind
    /// within `max_len`.
    NotMeasurable,
    /// The [`Limits`] were reached before the walk was finished. Nothing is
    /// known about the candidate; trying again with larger limits may give
    /// an answer (at the cost the limits were there to avoid).
    LimitReached,
    /// The source reported a read error on bytes the walk needed.
    Io(std::io::Error),
}

impl fmt::Display for MeasureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MeasureError::NotMeasurable => f.write_str("not a complete file of this kind"),
            MeasureError::LimitReached => {
                f.write_str("work limit reached before the end of the file was found")
            }
            MeasureError::Io(e) => write!(f, "read error: {e}"),
        }
    }
}

impl std::error::Error for MeasureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MeasureError::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// Finds the end of a file of kind `kind` that starts at offset 0 of `r` and
/// is stored contiguously, by walking the format's own structure.
///
/// `max_len` is the furthest the file can possibly extend (the distance to
/// the next cluster known to belong to another file, or to the end of the
/// source). Nothing at or beyond `max_len` is read, and a returned length
/// never exceeds it.
///
/// Returns `None` when the data is not a valid file of that kind, when its
/// structure runs into `max_len` or into the end of the data, when a read
/// fails, or when [`Limits::DEFAULT`] is exhausted. [`try_measure`] tells
/// these apart.
pub fn measure(kind: FileKind, r: &dyn ReadAt, max_len: u64) -> Option<Measured> {
    try_measure(kind, r, max_len, &Limits::DEFAULT).ok()
}

/// [`measure`] with caller-chosen [`Limits`].
pub fn measure_with_limits(
    kind: FileKind,
    r: &dyn ReadAt,
    max_len: u64,
    limits: &Limits,
) -> Option<Measured> {
    try_measure(kind, r, max_len, limits).ok()
}

/// [`measure`] with caller-chosen [`Limits`] and a reason when there is no
/// result.
///
/// The reason matters for one decision: a candidate that ran into the
/// limits ([`MeasureError::LimitReached`]) is not known to be invalid. The
/// formats that have to be read from start to end - above all ZIP archives
/// written by streaming tools, whose every entry must be scanned for its
/// data descriptor - can legitimately need more than the default 1 GiB.
pub fn try_measure(
    kind: FileKind,
    r: &dyn ReadAt,
    max_len: u64,
    limits: &Limits,
) -> Result<Measured, MeasureError> {
    walk(kind, Src::new(r, max_len, limits), max_len)
}

fn walk(kind: FileKind, mut src: Src, max_len: u64) -> Result<Measured, MeasureError> {
    let measured = match kind {
        FileKind::Jpeg => jpeg::measure(&mut src),
        FileKind::Png => png::measure(&mut src),
        FileKind::Gif => gif::measure(&mut src),
        FileKind::Bmp => bmp::measure(&mut src),
        FileKind::Tiff => tiff::measure(&mut src),
        FileKind::Pdf => pdf::measure(&mut src),
        FileKind::Zip => zip::measure(&mut src),
        FileKind::Mp4 | FileKind::Heic | FileKind::Cr3 => bmff::measure(&mut src, kind),
        FileKind::Avi | FileKind::Wav | FileKind::Webp => riff::measure(&mut src, kind),
        FileKind::Mkv => mkv::measure(&mut src),
        FileKind::Mp3 => mp3::measure(&mut src),
        FileKind::SevenZip => sevenz::measure(&mut src),
        FileKind::Rar => rar::measure(&mut src),
    };
    // Invariants every walker is held to, whatever it computed: the length
    // is positive and within `max_len`, and every byte of the file exists
    // (walkers that hop over payload have not touched the last one).
    let measured = measured.filter(|m| m.len != 0 && m.len <= max_len && src.has_byte(m.len - 1));
    // A read that failed or a budget that ran out on the way looks like the
    // end of the data to a walker. Whatever it concluded then - a shorter
    // file, or no file - is not a conclusion about the data.
    match src.take_failure() {
        Some(Failure::Limit) => Err(MeasureError::LimitReached),
        Some(Failure::Io(e)) => Err(MeasureError::Io(e)),
        None => measured.ok_or(MeasureError::NotMeasurable),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        let mut seen = std::collections::HashSet::new();
        for &k in FileKind::all() {
            assert_eq!(FileKind::from_name(k.name()), Some(k));
            assert_eq!(k.name().parse::<FileKind>(), Ok(k));
            assert_eq!(k.to_string(), k.name());
            assert!(seen.insert(k.name()), "duplicate name {}", k.name());
            assert!(k
                .name()
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
            assert!(k
                .default_ext()
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
            assert!(!k.description().is_empty());
        }
        assert_eq!(FileKind::all().len(), 17);
        assert_eq!(FileKind::from_name("JPEG"), None);
        assert_eq!(FileKind::from_name(""), None);
        assert_eq!("nope".parse::<FileKind>(), Err(UnknownKind));
        assert_eq!(FileKind::Jpeg.default_ext(), "jpg");
        assert_eq!(FileKind::SevenZip.name(), "7z");
    }

    /// Every file in `tests/data`, with the kind its name announces.
    fn sample_files() -> Vec<(String, FileKind, Vec<u8>)> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("data");
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("tests/data") {
            let name = entry.unwrap().file_name().into_string().unwrap();
            let kind = name
                .strip_prefix("special_")
                .unwrap_or(&name)
                .split('_')
                .next()
                .and_then(FileKind::from_name);
            if let Some(kind) = kind {
                out.push((name.clone(), kind, std::fs::read(dir.join(&name)).unwrap()));
            }
        }
        out.sort();
        assert!(out.len() > 100);
        out
    }

    /// The walkers read through a buffer that is refilled block by block. A
    /// marker, a keyword or a header that lies across the edge between two
    /// refills is where a scanner goes wrong. Here every sample is measured
    /// with 64-byte refills on a grid shifted through all 64 positions, so
    /// that every byte of every file is the first byte of a refill once -
    /// every multi-byte structure is split at each of its inner positions -
    /// and the result must be what the normal block sizes give.
    #[test]
    fn results_do_not_depend_on_where_refills_fall() {
        use reader::Tuning;
        use std::cell::Cell;

        struct Counted<'a>(&'a [u8], Cell<u64>);
        impl ReadAt for Counted<'_> {
            fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
                self.1.set(self.1.get() + 1);
                self.0.read_at(offset, buf)
            }
        }
        let unlimited = Limits {
            max_read_bytes: u64::MAX,
            max_reads: u64::MAX,
        };

        for (name, kind, data) in sample_files() {
            let len = data.len() as u64;
            let expected = measure(kind, &data, len);
            for phase in 0..64 {
                let counted = Counted(&data, Cell::new(0));
                let src = Src::tuned(&counted, len, &unlimited, Tuning::tiny(64, phase));
                let got = walk(kind, src, len).ok();
                assert_eq!(
                    got, expected,
                    "{name} with 64-byte refills, grid shifted by {phase}"
                );
                // the refills really are that small: a format that reads
                // every byte needs one for every 64 bytes or so
                if kind == FileKind::Png {
                    let reads = counted.1.get();
                    assert!(
                        reads > len / 130,
                        "{name}: only {reads} reads for {len} bytes"
                    );
                }
            }
            // a second file right behind: block edges inside the first
            // file's last structures and the second file's first ones
            let mut two = data.clone();
            two.extend_from_slice(&data);
            let expected = measure(kind, &two, two.len() as u64);
            for phase in (0..64).step_by(7) {
                let src = Src::tuned(&two, two.len() as u64, &unlimited, Tuning::tiny(64, phase));
                let got = walk(kind, src, two.len() as u64).ok();
                assert_eq!(got, expected, "{name} twice, grid shifted by {phase}");
            }
        }
    }

    #[test]
    fn empty_and_tiny_inputs() {
        let empty: &[u8] = &[];
        for &k in FileKind::all() {
            assert_eq!(measure(k, &empty, 0), None);
            assert_eq!(measure(k, &empty, u64::MAX), None);
            assert_eq!(measure(k, &[0u8; 3], 3), None);
            assert_eq!(measure(k, &vec![0xFFu8; 100], 100), None);
        }
        assert_eq!(sniff(empty), None);
        assert_eq!(sniff(&[0xFF]), None);
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), None);
    }
}
