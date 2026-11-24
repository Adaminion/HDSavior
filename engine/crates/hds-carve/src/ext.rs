//! File-name extensions: which kinds an extension may hold, and whether a
//! file's first bytes agree with its extension.
//!
//! This is the cross-check for files recovered through the MFT. It is
//! deliberately more lenient than [`sniff`](crate::sniff): `sniff` has to
//! keep false positives out of a scan of billions of sectors, whereas here
//! the file is known to exist and the only question is whether its content
//! can be what its name says. Any start that a valid file of the family can
//! have counts as a match.

use crate::{mp3, sniff, FileKind};

/// How the first bytes of a family of extensions are judged.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    Jpeg,
    Png,
    Gif,
    Bmp,
    Tiff,
    Pdf,
    Zip,
    /// Office Open XML: a ZIP, or an OLE2 container when password-protected.
    Ooxml,
    /// ISO-BMFF video and audio, QuickTime included.
    Mp4,
    /// ISO-BMFF still images and Canon CR3: always start with `ftyp`.
    BmffImage,
    Avi,
    Wav,
    Webp,
    Mkv,
    Mp3,
    SevenZip,
    Rar,
}

/// Lower-cases `ext` (at most 12 bytes, a leading dot removed) without
/// allocating.
fn normalize(ext: &str) -> Option<([u8; 12], usize)> {
    let ext = ext.strip_prefix('.').unwrap_or(ext).as_bytes();
    let mut out = [0u8; 12];
    let dst = out.get_mut(..ext.len())?;
    dst.copy_from_slice(ext);
    dst.make_ascii_lowercase();
    Some((out, ext.len()))
}

fn family(ext: &str) -> Option<(Family, &'static [FileKind])> {
    use FileKind as K;
    let (buf, len) = normalize(ext)?;
    let ext = buf.get(..len)?;
    Some(match ext {
        // JPEG and its aliases; camera thumbnails (.thm), Multi-Picture
        // (.mpo), stereo (.jps) and Insta360 (.insp) files are JPEG streams
        b"jpg" | b"jpeg" | b"jpe" | b"jfif" | b"jfi" | b"jif" | b"thm" | b"mpo" | b"jps"
        | b"insp" => (Family::Jpeg, &[K::Jpeg]),
        b"png" | b"apng" => (Family::Png, &[K::Png]),
        b"gif" => (Family::Gif, &[K::Gif]),
        b"bmp" | b"dib" => (Family::Bmp, &[K::Bmp]),
        // TIFF and the camera RAW formats structured as TIFF: Adobe DNG,
        // Canon CR2, Nikon NEF/NRW, Sony ARW/SR2/SRF/ARQ, Pentax PEF,
        // Samsung SRW, Olympus ORF/ORI, Panasonic RW2, Leica RWL,
        // Hasselblad 3FR, Epson ERF, Mamiya MEF, Leaf MOS, Kodak DCR/KDC,
        // GoPro GPR
        b"tif" | b"tiff" | b"dng" | b"cr2" | b"nef" | b"nrw" | b"arw" | b"sr2" | b"srf"
        | b"arq" | b"pef" | b"srw" | b"orf" | b"ori" | b"rw2" | b"rwl" | b"3fr" | b"erf"
        | b"mef" | b"mos" | b"dcr" | b"kdc" | b"gpr" => (Family::Tiff, &[K::Tiff]),
        b"pdf" => (Family::Pdf, &[K::Pdf]),
        // plain archives and the formats that are ZIP archives by definition
        b"zip" | b"jar" | b"war" | b"ear" | b"aar" | b"apk" | b"xapk" | b"aab" | b"ipa"
        | b"xpi" | b"epub" | b"odt" | b"ods" | b"odp" | b"odg" | b"odf" | b"odb" | b"odc"
        | b"odi" | b"ott" | b"ots" | b"otp" | b"kmz" | b"cbz" | b"3mf" | b"nupkg" | b"vsix"
        | b"whl" | b"xps" | b"oxps" | b"appx" | b"msix" | b"idml" | b"pages" | b"numbers" => {
            (Family::Zip, &[K::Zip])
        }
        b"docx" | b"docm" | b"dotx" | b"dotm" | b"xlsx" | b"xlsm" | b"xltx" | b"xltm" | b"xlsb"
        | b"xlam" | b"pptx" | b"pptm" | b"potx" | b"potm" | b"ppsx" | b"ppsm" | b"ppam"
        | b"thmx" | b"vsdx" => (Family::Ooxml, &[K::Zip]),
        // MP4 and QuickTime in all their spellings; GoPro low-resolution
        // (.lrv) and 360 (.360) files, Insta360 video (.insv)
        b"mp4" | b"m4v" | b"mov" | b"qt" | b"m4a" | b"m4b" | b"m4p" | b"m4r" | b"3gp" | b"3gpp"
        | b"3g2" | b"3gp2" | b"f4v" | b"f4a" | b"f4b" | b"lrv" | b"360" | b"insv" | b"mqv" => {
            (Family::Mp4, &[K::Mp4])
        }
        // a HEIF file with a brand this crate does not list still sniffs as
        // the generic kind
        b"heic" | b"heif" | b"hif" | b"heics" | b"heifs" | b"avif" | b"avifs" => {
            (Family::BmffImage, &[K::Heic, K::Mp4])
        }
        b"cr3" | b"crm" => (Family::BmffImage, &[K::Cr3]),
        b"avi" | b"divx" => (Family::Avi, &[K::Avi]),
        b"wav" | b"wave" | b"bwf" | b"rf64" => (Family::Wav, &[K::Wav]),
        b"webp" => (Family::Webp, &[K::Webp]),
        b"mkv" | b"mka" | b"mks" | b"mk3d" | b"webm" => (Family::Mkv, &[K::Mkv]),
        b"mp3" => (Family::Mp3, &[K::Mp3]),
        b"7z" => (Family::SevenZip, &[K::SevenZip]),
        b"rar" => (Family::Rar, &[K::Rar]),
        _ => return None,
    })
}

/// The kinds a file with extension `ext` may contain.
///
/// `ext` is given without the dot; case does not matter. An empty slice
/// means this crate has no opinion about the extension (which is the case
/// for most extensions: text, executables, and every format not listed in
/// the crate documentation).
///
/// To check a file against its extension use [`matches_extension`], not
/// `kinds_for_extension(ext).contains(&sniff(head)?)`: `sniff` is strict
/// enough to reject unusual but valid files.
pub fn kinds_for_extension(ext: &str) -> &'static [FileKind] {
    family(ext).map_or(&[], |(_, kinds)| kinds)
}

/// Does content that starts with `head` fit a file named `*.ext`?
///
/// * `None`: no opinion - the extension is not one this crate knows, or
///   (MP3 only) the content is not recognisable either way.
/// * `Some(true)`: the content starts the way such a file can start. All
///   aliases and containers are taken into account: a `.docx` is a ZIP, a
///   `.nef` is a TIFF, a `.mov` may start with `ftyp`, `moov`, `mdat`,
///   `wide`, `free`, ...
/// * `Some(false)`: it does not. The file was overwritten, belongs to a
///   different cluster chain, or is misnamed.
///
/// Pass the first [`SNIFF_LEN`](crate::SNIFF_LEN) bytes of the file or more
/// (a PDF header may sit anywhere in the first kilobyte). If the file is
/// shorter than that, pass all of it.
pub fn matches_extension(head: &[u8], ext: &str) -> Option<bool> {
    let (family, kinds) = family(ext)?;
    let at4 = head.get(4..8);
    Some(match family {
        Family::Jpeg => head.starts_with(&[0xFF, 0xD8, 0xFF]),
        Family::Png => head.starts_with(b"\x89PNG\r\n\x1a\n"),
        Family::Gif => head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a"),
        Family::Bmp => head.starts_with(b"BM"),
        Family::Tiff => is_tiff(head),
        Family::Pdf => {
            let window = head.get(..1024).unwrap_or(head);
            crate::util::find_sub(window, b"%PDF-").is_some()
        }
        Family::Zip => is_zip(head),
        Family::Ooxml => {
            is_zip(head) || head.starts_with(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1])
        }
        Family::Mp4 => matches!(
            at4,
            Some(
                b"ftyp"
                    | b"moov"
                    | b"mdat"
                    | b"wide"
                    | b"free"
                    | b"skip"
                    | b"pnot"
                    | b"PICT"
                    | b"pict"
                    | b"junk"
                    | b"uuid"
                    | b"styp"
                    | b"sidx"
                    | b"moof"
            )
        ),
        Family::BmffImage => at4 == Some(b"ftyp"),
        Family::Avi => head.starts_with(b"RIFF") && head.get(8..12) == Some(b"AVI "),
        Family::Wav => {
            matches!(head.get(..4), Some(b"RIFF" | b"RF64" | b"BW64" | b"RIFX"))
                && head.get(8..12) == Some(b"WAVE")
        }
        Family::Webp => head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP"),
        Family::Mkv => head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]),
        Family::SevenZip => head.starts_with(&crate::sevenz::SIGNATURE),
        Family::Rar => head.starts_with(b"Rar!\x1A\x07"),
        Family::Mp3 => {
            // An MP3 needs no header at all: players resynchronise on the
            // first frame, wherever it is. Only a positive identification
            // of something else is a mismatch.
            let frame = crate::util::rd::<4>(head, 0).and_then(mp3::parse_frame);
            // (a RIFF/WAVE wrapper around MPEG audio is legal, too)
            let riff_wave = head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WAVE");
            if head.starts_with(b"ID3") || frame.is_some() || riff_wave {
                true
            } else if sniff(head).is_some_and(|k| !kinds.contains(&k)) {
                false
            } else {
                return None;
            }
        }
    })
}

/// Classic TIFF, BigTIFF, Olympus ORF and Panasonic RW2 byte-order marks.
fn is_tiff(head: &[u8]) -> bool {
    matches!(
        head.get(..4),
        Some(
            [b'I', b'I', 0x2A | 0x2B | 0x55, 0]
                | [b'M', b'M', 0, 0x2A | 0x2B | 0x55]
                | [b'I', b'I', b'R', b'O' | b'S']
                | [b'M', b'M', b'O' | b'S', b'R']
        )
    )
}

/// A local file header, an empty archive, or a spanned-archive marker.
fn is_zip(head: &[u8]) -> bool {
    matches!(
        head.get(..4),
        Some(b"PK\x03\x04" | b"PK\x05\x06" | b"PK\x07\x08")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_lookup() {
        assert_eq!(kinds_for_extension("jpg"), &[FileKind::Jpeg]);
        assert_eq!(kinds_for_extension("JPEG"), &[FileKind::Jpeg]);
        assert_eq!(kinds_for_extension(".Jpg"), &[FileKind::Jpeg]);
        assert_eq!(kinds_for_extension("docx"), &[FileKind::Zip]);
        assert_eq!(kinds_for_extension("nef"), &[FileKind::Tiff]);
        assert_eq!(kinds_for_extension("mov"), &[FileKind::Mp4]);
        assert_eq!(
            kinds_for_extension("heic"),
            &[FileKind::Heic, FileKind::Mp4]
        );
        assert_eq!(kinds_for_extension("cr3"), &[FileKind::Cr3]);
        assert_eq!(kinds_for_extension("webm"), &[FileKind::Mkv]);
        assert_eq!(kinds_for_extension("7z"), &[FileKind::SevenZip]);
        assert!(kinds_for_extension("txt").is_empty());
        assert!(kinds_for_extension("").is_empty());
        assert!(kinds_for_extension("averyveryverylongextension").is_empty());
        assert!(kinds_for_extension("jp\u{e9}g").is_empty());
    }

    #[test]
    fn every_kind_has_its_default_extension() {
        for &kind in FileKind::all() {
            assert!(
                kinds_for_extension(kind.default_ext()).contains(&kind),
                "{kind:?}: .{} does not map back",
                kind.default_ext()
            );
        }
    }

    #[test]
    fn loose_matching() {
        let m = matches_extension;
        assert_eq!(
            m(b"\xFF\xD8\xFF\xE3\x00\x10Meta", "jpg"),
            Some(true),
            "unusual first segment"
        );
        assert_eq!(m(b"\xFF\xD8\xFF\xE3\x00\x10Meta", "png"), Some(false));
        assert_eq!(m(b"\0\0\0\0\0\0\0\0", "jpg"), Some(false));
        assert_eq!(m(b"", "jpg"), Some(false));
        assert_eq!(m(b"anything", "txt"), None);
        assert_eq!(m(b"II*\0\x08\0\0\0", "nef"), Some(true));
        assert_eq!(m(b"MM\0*\0\0\0\x08", "nef"), Some(true));
        assert_eq!(m(b"II+\0\x08\0\0\0", "tif"), Some(true), "BigTIFF");
        assert_eq!(m(b"IIRO\x08\0\0\0", "orf"), Some(true));
        assert_eq!(m(b"IIU\0\x18\0\0\0", "rw2"), Some(true));
        assert_eq!(m(b"II*\0", "cr2"), Some(true));
        assert_eq!(m(b"PK\x03\x04\x14\0\0\0", "docx"), Some(true));
        assert_eq!(
            m(b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1", "docx"),
            Some(true),
            "encrypted OOXML is OLE2"
        );
        assert_eq!(m(b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1", "zip"), Some(false));
        assert_eq!(m(b"\0\0\0\x08wide\0\x01\0\0mdat", "mov"), Some(true));
        assert_eq!(m(b"\x00\x10\x00\x00mdat", "mov"), Some(true));
        assert_eq!(m(b"\x00\x10\x00\x00mdat", "heic"), Some(false));
        assert_eq!(m(b"\0\0\0\x18ftypheic", "heic"), Some(true));
        assert_eq!(m(b"\0\0\0\x18ftypcrx ", "cr3"), Some(true));
        assert_eq!(m(b"RIFF\x24\0\0\0WAVEfmt ", "wav"), Some(true));
        assert_eq!(m(b"RIFF\x24\0\0\0WAVEfmt ", "avi"), Some(false));
        assert_eq!(m(b"\n\n%PDF-1.4\n", "pdf"), Some(true), "header after junk");
        assert_eq!(m(b"<html>", "pdf"), Some(false));
        assert_eq!(m(b"Rar!\x1A\x07\x01\0", "rar"), Some(true));
        assert_eq!(m(b"7z\xBC\xAF\x27\x1C\0\x04", "7z"), Some(true));
        assert_eq!(m(b"\x1A\x45\xDF\xA3\x93\x42\x82\x88", "mka"), Some(true));
    }

    #[test]
    fn mp3_is_weak() {
        let m = matches_extension;
        assert_eq!(m(b"ID3\x03\0\0\0\0\x0f\x76", "mp3"), Some(true));
        assert_eq!(m(b"\xFF\xFB\x90\x64\0\0\0\0", "mp3"), Some(true));
        assert_eq!(m(b"\xFF\xF3\x84\x64\0\0\0\0", "mp3"), Some(true));
        // leading padding or junk: cannot be judged
        assert_eq!(m(&[0u8; 64], "mp3"), None);
        assert_eq!(m(b"misc est un canard\r\xff", "mp3"), None);
        // clearly another format
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        png.extend([0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0, 0, 0]);
        assert_eq!(m(&png, "mp3"), Some(false));
    }
}
