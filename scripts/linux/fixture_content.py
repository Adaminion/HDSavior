#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
fixture_content.py - file contents for the HDSavior fixtures.

Imported by fixture_helper.py (which is driven by make-fixtures.sh).  Three
kinds of content are produced here:

1. rnd(tag, n): arbitrary bytes.  SHAKE-256 of the fixed seed and a tag, so the
   same tag always gives the same bytes on every machine.

2. "Typed" files (gen_text, gen_bmp, gen_wav, gen_png, gen_tiff, gen_pdf,
   gen_zip): valid files of a well-known type whose every byte is produced by
   code in this module from rnd().  Their SHA-256 is the same everywhere.

3. Samples (make_samples): small real media and document files made with
   Pillow and ffmpeg (plus 7z / rar when installed).  They are valid files as
   an application would write them, but their bytes depend on the versions of
   those tools, so their hashes are only reproducible on the same toolchain.
   They are created when the fixtures are built and are never committed.
"""

import hashlib
import os
import shutil
import struct
import subprocess
import zlib

SEED = "HDSavior-fixtures-v1"


# --------------------------------------------------------------------------
# deterministic bytes and numbers
# --------------------------------------------------------------------------

def rnd(tag, n):
    """n pseudo-random bytes depending only on SEED and tag."""
    if n <= 0:
        return b""
    return hashlib.shake_256((SEED + "\x00" + tag).encode("utf-8")).digest(n)


def rint(tag, lo, hi):
    """Deterministic integer in [lo, hi]."""
    h = hashlib.sha256((SEED + "\x00" + tag).encode("utf-8")).digest()
    return lo + int.from_bytes(h[:8], "big") % (hi - lo + 1)


# --------------------------------------------------------------------------
# typed files, byte-for-byte deterministic
# --------------------------------------------------------------------------

_WORDS = ("disk sector cluster record index volume format rescue folder file name data run list "
          "offset length backup mirror bitmap journal attribute resident stream sparse table entry "
          "parent child the a of and to in is that it was for on are as with they at be this from "
          "or one had by word but not what all were we when your can said there use an each which "
          "she do how their if will up other about out many then them these so some her would make").split()


def gen_text(tag, size):
    """Exactly size bytes of ASCII words."""
    if size <= 0:
        return b""
    stream = rnd(tag, size // 2 + 16)
    parts = []
    total = 0
    col = 0
    for b in stream:
        w = _WORDS[b % len(_WORDS)]
        if col + len(w) > 72:
            parts.append("\n")
            total += 1
            col = 0
        elif col:
            parts.append(" ")
            total += 1
            col += 1
        parts.append(w)
        total += len(w)
        col += len(w)
        if total >= size:
            break
    return "".join(parts).encode("ascii")[:size].ljust(size, b".")


def gen_bmp(tag, size):
    """24-bit uncompressed BMP of about size bytes (random pixels)."""
    width = 64 if size < 200000 else 256        # multiples of 4: rows need no padding
    row = width * 3
    height = max(1, (size - 54) // row)
    pix = rnd(tag, row * height)
    return (b"BM" + struct.pack("<IHHI", 54 + len(pix), 0, 0, 54)
            + struct.pack("<IiiHHIIiiII", 40, width, height, 1, 24, 0, len(pix), 2835, 2835, 0, 0)
            + pix)


def gen_wav(tag, size):
    """16-bit mono PCM WAV of about size bytes (noise)."""
    n = max(2, size - 44) & ~1
    return (b"RIFF" + struct.pack("<I", 36 + n) + b"WAVEfmt "
            + struct.pack("<IHHIIHH", 16, 1, 1, 22050, 44100, 2, 16)
            + b"data" + struct.pack("<I", n) + rnd(tag, n))


def _zlib_stored(raw):
    """A zlib stream that stores raw uncompressed (identical with every zlib version)."""
    out = [b"\x78\x01"]
    pos = 0
    while True:
        chunk = raw[pos:pos + 65535]
        pos += len(chunk)
        final = 1 if pos >= len(raw) else 0
        out.append(struct.pack("<BHH", final, len(chunk), len(chunk) ^ 0xFFFF))
        out.append(chunk)
        if final:
            break
    out.append(struct.pack(">I", zlib.adler32(raw) & 0xFFFFFFFF))
    return b"".join(out)


def _png_chunk(kind, data):
    return (struct.pack(">I", len(data)) + kind + data
            + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF))


def gen_png(tag, size):
    """8-bit RGB PNG of about size bytes (random pixels, several IDAT chunks)."""
    width = 64 if size < 200000 else 256
    row = width * 3
    height = max(1, (size - 200) // (row + 1))
    pix = rnd(tag, row * height)
    raw = b"".join(b"\x00" + pix[y * row:(y + 1) * row] for y in range(height))
    z = _zlib_stored(raw)
    out = [b"\x89PNG\r\n\x1a\n",
           _png_chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))]
    for i in range(0, len(z), 32768):
        out.append(_png_chunk(b"IDAT", z[i:i + 32768]))
    out.append(_png_chunk(b"IEND", b""))
    return b"".join(out)


def gen_tiff(tag, size):
    """Little-endian uncompressed RGB TIFF of about size bytes, one strip, IFD after the pixels."""
    width = 64 if size < 200000 else 256
    row = width * 3
    height = max(1, (size - 200) // row)
    pix = rnd(tag, row * height)
    if len(pix) % 2:
        pix += b"\x00"
    ifd_off = 8 + len(pix)
    nent = 10
    bits_off = ifd_off + 2 + 12 * nent + 4

    def short(t, v):
        return struct.pack("<HHIHH", t, 3, 1, v, 0)

    def long(t, v):
        return struct.pack("<HHII", t, 4, 1, v)

    ifd = (struct.pack("<H", nent)
           + long(256, width) + long(257, height)
           + struct.pack("<HHII", 258, 3, 3, bits_off)      # BitsPerSample -> 8,8,8
           + short(259, 1) + short(262, 2)
           + long(273, 8)                                   # StripOffsets
           + short(277, 3) + long(278, height)
           + long(279, row * height)                        # StripByteCounts
           + short(284, 1)
           + struct.pack("<I", 0))
    return b"II*\x00" + struct.pack("<I", ifd_off) + pix + ifd + struct.pack("<HHH", 8, 8, 8)


def _pdf(objects, updates=None):
    """Serialise numbered PDF objects with a classic xref table.

    objects: list of object bodies (bytes) for object numbers 1..n.
    updates: optional dict {object number: new body}; appended as an
             incremental update (second xref section, /Prev, second %%EOF).
    """
    out = bytearray(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")
    offsets = []
    for i, body in enumerate(objects, 1):
        offsets.append(len(out))
        out += b"%d 0 obj\n" % i + body + b"\nendobj\n"
    xref = len(out)
    out += b"xref\n0 %d\n0000000000 65535 f \n" % (len(objects) + 1)
    for off in offsets:
        out += b"%010d 00000 n \n" % off
    out += (b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n"
            % (len(objects) + 1, xref))
    if updates:
        new_offsets = {}
        for num in sorted(updates):
            new_offsets[num] = len(out)
            out += b"%d 0 obj\n" % num + updates[num] + b"\nendobj\n"
        xref2 = len(out)
        out += b"xref\n"
        for num in sorted(updates):
            out += b"%d 1\n%010d 00000 n \n" % (num, new_offsets[num])
        out += (b"trailer\n<< /Size %d /Root 1 0 R /Prev %d >>\nstartxref\n%d\n%%%%EOF\n"
                % (len(objects) + 1, xref, xref2))
    return bytes(out)


def _pdf_stream(data, extra=b""):
    return b"<< /Length %d%s >>\nstream\n" % (len(data), extra) + data + b"\nendstream"


def _pdf_objects(text_lines, payload, compress):
    content = b"BT /F1 12 Tf 72 720 Td 14 TL\n"
    for line in text_lines:
        content += b"(" + line.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)").encode("latin-1") + b") Tj T*\n"
    content += b"ET"
    if compress:
        content_obj = _pdf_stream(zlib.compress(content, 6), b" /Filter /FlateDecode")
    else:
        content_obj = _pdf_stream(content)
    return [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R "
        b"/Resources << /Font << /F1 5 0 R >> >> >>",
        content_obj,
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        _pdf_stream(payload, b" /Type /EmbeddedFile"),
    ]


def gen_pdf(tag, size):
    """A one-page PDF of about size bytes (uncompressed text plus a binary stream object)."""
    lines = [gen_text(tag + "/line%d" % i, 60).decode("ascii").replace("\n", " ") for i in range(8)]
    payload = rnd(tag, max(16, size - 900))
    return _pdf(_pdf_objects(lines, payload, compress=False))


def zip_container(files, deflate=False):
    """A ZIP archive written field by field (no dependency on zipfile's defaults).

    files: list of (name, data).  With deflate=True every member except one
    called "mimetype" is deflated at level 6 (bytes then depend on the zlib
    implementation); otherwise everything is stored.
    """
    dos_time, dos_date = 0x6000, 0x5021          # 2020-01-01 12:00:00
    out = bytearray()
    central = bytearray()
    for name, data in files:
        nb = name.encode("utf-8")
        crc = zlib.crc32(data) & 0xFFFFFFFF
        method = 0
        packed = data
        if deflate and name != "mimetype" and data:
            c = zlib.compressobj(6, zlib.DEFLATED, -15)
            packed = c.compress(data) + c.flush()
            method = 8
        flags = 0x0800 if any(b > 127 for b in nb) else 0
        ver = 20 if method else 10
        off = len(out)
        out += struct.pack("<IHHHHHIIIHH", 0x04034B50, ver, flags, method, dos_time, dos_date,
                           crc, len(packed), len(data), len(nb), 0) + nb + packed
        central += struct.pack("<IHHHHHHIIIHHHHHII", 0x02014B50, 20, ver, flags, method, dos_time,
                               dos_date, crc, len(packed), len(data), len(nb), 0, 0, 0, 0, 0, off) + nb
    cd_off = len(out)
    out += central
    out += struct.pack("<IHHHHIIH", 0x06054B50, 0, 0, len(files), len(files), len(central), cd_off, 0)
    return bytes(out)


def gen_zip(tag, size):
    """A ZIP archive of about size bytes with three stored members."""
    body = max(48, size - 320)
    c = min(body // 4, 2000)
    a = (body - c) * 3 // 5
    b = body - c - a
    return zip_container([
        ("readme.txt", gen_text(tag + "/readme", c)),
        ("data/first.bin", rnd(tag + "/a", a)),
        ("data/second.bin", rnd(tag + "/b", b)),
    ])


TYPED = {            # extension -> (generator, content_type)
    "txt": (gen_text, "text"),
    "bmp": (gen_bmp, "bmp"),
    "wav": (gen_wav, "wav"),
    "png": (gen_png, "png"),
    "tif": (gen_tiff, "tiff"),
    "pdf": (gen_pdf, "pdf"),
    "zip": (gen_zip, "zip"),
}


# --------------------------------------------------------------------------
# samples made with Pillow, ffmpeg and archivers
# --------------------------------------------------------------------------

def _picture(tag, w, h):
    """A deterministic RGB picture: smooth colour field plus a few shapes."""
    from PIL import Image, ImageDraw
    resample = getattr(Image, "Resampling", Image).BICUBIC
    sw, sh = max(2, w // 16), max(2, h // 16)
    img = Image.frombytes("RGB", (sw, sh), rnd(tag + "/noise", sw * sh * 3)).resize((w, h), resample)
    d = ImageDraw.Draw(img)
    for i in range(6):
        x0 = rint("%s/x%d" % (tag, i), 0, max(1, w - 20))
        y0 = rint("%s/y%d" % (tag, i), 0, max(1, h - 20))
        x1 = x0 + rint("%s/w%d" % (tag, i), 10, max(11, w // 3))
        y1 = y0 + rint("%s/h%d" % (tag, i), 10, max(11, h // 3))
        color = tuple(rnd("%s/c%d" % (tag, i), 3))
        if i % 2:
            d.ellipse([x0, y0, x1, y1], fill=color)
        else:
            d.rectangle([x0, y0, x1, y1], fill=color)
    return img


def _jpeg_bytes(img, **kw):
    import io
    buf = io.BytesIO()
    img.save(buf, "JPEG", **kw)
    return buf.getvalue()


def _tiff_ifd(entries, base, next_ifd):
    """Encode one little-endian TIFF IFD at offset base; values over 4 bytes follow the table."""
    entries = sorted(entries)
    table_len = 2 + 12 * len(entries) + 4
    out = bytearray(struct.pack("<H", len(entries)))
    data = bytearray()
    for tag, typ, count, payload in entries:
        if len(payload) <= 4:
            out += struct.pack("<HHI", tag, typ, count) + payload.ljust(4, b"\x00")
        else:
            out += struct.pack("<HHII", tag, typ, count, base + table_len + len(data))
            data += payload
            if len(data) % 2:
                data += b"\x00"
    out += struct.pack("<I", next_ifd)
    return bytes(out + data)


def _exif_with_thumbnail(thumb):
    """An Exif APP1 payload whose IFD1 embeds a JPEG thumbnail (a JPEG inside the JPEG)."""
    ifd0_entries = [
        (0x010F, 2, 9, b"HDSavior\x00"),                         # Make
        (0x0110, 2, 15, b"Fixture Camera\x00"),                  # Model
        (0x0112, 3, 1, struct.pack("<H", 1)),                    # Orientation
        (0x0132, 2, 20, b"2019:07:14 10:30:00\x00"),             # DateTime
    ]
    ifd0_len = len(_tiff_ifd(ifd0_entries, 8, 0))
    ifd1_off = 8 + ifd0_len
    thumb_off = ifd1_off + 2 + 12 * 3 + 4
    ifd1 = _tiff_ifd([
        (0x0103, 3, 1, struct.pack("<H", 6)),                    # Compression = JPEG
        (0x0201, 4, 1, struct.pack("<I", thumb_off)),            # JPEGInterchangeFormat
        (0x0202, 4, 1, struct.pack("<I", len(thumb))),           # JPEGInterchangeFormatLength
    ], ifd1_off, 0)
    tiff = b"II*\x00" + struct.pack("<I", 8) + _tiff_ifd(ifd0_entries, 8, ifd1_off) + ifd1 + thumb
    return b"Exif\x00\x00" + tiff


_DOCX = {
    "[Content_Types].xml":
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
        '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
        '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
        '<Default Extension="xml" ContentType="application/xml"/>'
        '<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-'
        'officedocument.wordprocessingml.document.main+xml"/></Types>',
    "_rels/.rels":
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/'
        'relationships/officeDocument" Target="word/document.xml"/></Relationships>',
}

_XLSX = {
    "[Content_Types].xml":
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
        '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
        '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
        '<Default Extension="xml" ContentType="application/xml"/>'
        '<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-'
        'officedocument.spreadsheetml.sheet.main+xml"/>'
        '<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-'
        'officedocument.spreadsheetml.worksheet+xml"/></Types>',
    "_rels/.rels":
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/'
        'relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>',
    "xl/workbook.xml":
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
        '<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" '
        'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">'
        '<sheets><sheet name="Fixture" sheetId="1" r:id="rId1"/></sheets></workbook>',
    "xl/_rels/workbook.xml.rels":
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        '<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/'
        'relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>',
}

_PML = "http://schemas.openxmlformats.org/presentationml/2006/main"
_DML = "http://schemas.openxmlformats.org/drawingml/2006/main"
_REL = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
_PKG_REL = "http://schemas.openxmlformats.org/package/2006/relationships"
_XML = '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'
_SP_TREE = ('<p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/>'
            '</p:nvGrpSpPr><p:grpSpPr/>%s</p:spTree></p:cSld>')
_CLR_MAP = ('<p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" '
            'accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" '
            'folHlink="folHlink"/>')


def _pptx_parts(text):
    ns = 'xmlns:a="%s" xmlns:r="%s" xmlns:p="%s"' % (_DML, _REL, _PML)
    shape = ('<p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>'
             '<p:spPr><a:xfrm><a:off x="914400" y="914400"/><a:ext cx="7315200" cy="1828800"/></a:xfrm>'
             '<a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr>'
             '<p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-US"/><a:t>%s</a:t></a:r></a:p>'
             '</p:txBody></p:sp>' % text)

    def rels(items):
        return (_XML + '<Relationships xmlns="%s">' % _PKG_REL
                + "".join('<Relationship Id="%s" Type="%s/%s" Target="%s"/>' % (i, _REL, t, target)
                          for i, t, target in items) + "</Relationships>")

    font = '<a:latin typeface="Arial"/><a:ea typeface=""/><a:cs typeface=""/>'
    fill3 = '<a:solidFill><a:schemeClr val="phClr"/></a:solidFill>' * 3
    line3 = '<a:ln w="9525"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln>' * 3
    effect3 = '<a:effectStyle><a:effectLst/></a:effectStyle>' * 3
    colors = "".join('<a:%s><a:srgbClr val="%s"/></a:%s>' % (n, v, n) for n, v in [
        ("dk1", "000000"), ("lt1", "FFFFFF"), ("dk2", "1F497D"), ("lt2", "EEECE1"),
        ("accent1", "4F81BD"), ("accent2", "C0504D"), ("accent3", "9BBB59"), ("accent4", "8064A2"),
        ("accent5", "4BACC6"), ("accent6", "F79646"), ("hlink", "0000FF"), ("folHlink", "800080")])
    theme = (_XML + '<a:theme xmlns:a="%s" name="Fixture"><a:themeElements>'
             '<a:clrScheme name="Fixture">%s</a:clrScheme>'
             '<a:fontScheme name="Fixture"><a:majorFont>%s</a:majorFont><a:minorFont>%s</a:minorFont>'
             '</a:fontScheme>'
             '<a:fmtScheme name="Fixture"><a:fillStyleLst>%s</a:fillStyleLst><a:lnStyleLst>%s</a:lnStyleLst>'
             '<a:effectStyleLst>%s</a:effectStyleLst><a:bgFillStyleLst>%s</a:bgFillStyleLst></a:fmtScheme>'
             '</a:themeElements></a:theme>' % (_DML, colors, font, font, fill3, line3, effect3, fill3))
    ct = (_XML + '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
          '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
          '<Default Extension="xml" ContentType="application/xml"/>'
          + "".join('<Override PartName="%s" ContentType="application/vnd.openxmlformats-officedocument.%s+xml"/>'
                    % (p, t) for p, t in [
                        ("/ppt/presentation.xml", "presentationml.presentation.main"),
                        ("/ppt/slides/slide1.xml", "presentationml.slide"),
                        ("/ppt/slideLayouts/slideLayout1.xml", "presentationml.slideLayout"),
                        ("/ppt/slideMasters/slideMaster1.xml", "presentationml.slideMaster"),
                        ("/ppt/theme/theme1.xml", "theme")]) + "</Types>")
    return [
        ("[Content_Types].xml", ct),
        ("_rels/.rels", rels([("rId1", "officeDocument", "ppt/presentation.xml")])),
        ("ppt/presentation.xml",
         _XML + '<p:presentation %s><p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/>'
         '</p:sldMasterIdLst><p:sldIdLst><p:sldId id="256" r:id="rId2"/></p:sldIdLst>'
         '<p:sldSz cx="9144000" cy="6858000"/><p:notesSz cx="6858000" cy="9144000"/></p:presentation>' % ns),
        ("ppt/_rels/presentation.xml.rels", rels([
            ("rId1", "slideMaster", "slideMasters/slideMaster1.xml"),
            ("rId2", "slide", "slides/slide1.xml"),
            ("rId3", "theme", "theme/theme1.xml")])),
        ("ppt/slides/slide1.xml",
         _XML + '<p:sld %s>%s<p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>'
         % (ns, _SP_TREE % shape)),
        ("ppt/slides/_rels/slide1.xml.rels", rels([("rId1", "slideLayout", "../slideLayouts/slideLayout1.xml")])),
        ("ppt/slideLayouts/slideLayout1.xml",
         _XML + '<p:sldLayout %s type="blank">%s<p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr>'
         '</p:sldLayout>' % (ns, _SP_TREE % "")),
        ("ppt/slideLayouts/_rels/slideLayout1.xml.rels",
         rels([("rId1", "slideMaster", "../slideMasters/slideMaster1.xml")])),
        ("ppt/slideMasters/slideMaster1.xml",
         _XML + '<p:sldMaster %s>%s%s<p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/>'
         '</p:sldLayoutIdLst></p:sldMaster>' % (ns, _SP_TREE % "", _CLR_MAP)),
        ("ppt/slideMasters/_rels/slideMaster1.xml.rels", rels([
            ("rId1", "slideLayout", "../slideLayouts/slideLayout1.xml"),
            ("rId2", "theme", "../theme/theme1.xml")])),
        ("ppt/theme/theme1.xml", theme),
    ]


def _xml_ok(files):
    """Every .xml / .rels / .opf / .xhtml member must be well-formed XML."""
    import xml.dom.minidom
    for name, data in files:
        if name.rsplit(".", 1)[-1] in ("xml", "rels", "opf", "xhtml", "ncx"):
            xml.dom.minidom.parseString(data)


def _office_samples(out):
    """DOCX, XLSX, PPTX, ODT, EPUB and a plain ZIP; returns [(file name, content type)]."""
    made = []
    paras = "".join('<w:p><w:r><w:t xml:space="preserve">%s</w:t></w:r></w:p>'
                    % gen_text("docx/p%d" % i, 400).decode("ascii").replace("\n", " ") for i in range(40))
    docx = [(n, v.encode("utf-8")) for n, v in _DOCX.items()]
    docx.append(("word/document.xml", (
        _XML + '<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">'
        '<w:body>%s<w:sectPr><w:pgSz w:w="11906" w:h="16838"/></w:sectPr></w:body></w:document>'
        % paras).encode("utf-8")))
    rows = "".join('<row r="%d">%s</row>' % (r, "".join(
        '<c r="%s%d"><v>%d</v></c>' % ("ABCDEFGH"[c], r, rint("xlsx/%d/%d" % (r, c), 0, 99999))
        for c in range(8))) for r in range(1, 201))
    xlsx = [(n, v.encode("utf-8")) for n, v in _XLSX.items()]
    xlsx.append(("xl/worksheets/sheet1.xml", (
        _XML + '<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">'
        '<sheetData>%s</sheetData></worksheet>' % rows).encode("utf-8")))
    pptx = [(n, v.encode("utf-8")) for n, v in _pptx_parts("HDSavior fixture slide")]
    body = "".join("<text:p>%s</text:p>" % gen_text("odt/p%d" % i, 300).decode("ascii").replace("\n", " ")
                   for i in range(30))
    odt = [
        ("mimetype", b"application/vnd.oasis.opendocument.text"),
        ("META-INF/manifest.xml", (
            '<?xml version="1.0" encoding="UTF-8"?>\n<manifest:manifest xmlns:manifest='
            '"urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.2">'
            '<manifest:file-entry manifest:full-path="/" manifest:media-type='
            '"application/vnd.oasis.opendocument.text"/><manifest:file-entry manifest:full-path='
            '"content.xml" manifest:media-type="text/xml"/></manifest:manifest>').encode("utf-8")),
        ("content.xml", (
            '<?xml version="1.0" encoding="UTF-8"?>\n<office:document-content xmlns:office='
            '"urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text='
            '"urn:oasis:names:tc:opendocument:xmlns:text:1.0" office:version="1.2"><office:body>'
            '<office:text>%s</office:text></office:body></office:document-content>' % body).encode("utf-8")),
    ]
    chapter = "".join("<p>%s</p>" % gen_text("epub/p%d" % i, 500).decode("ascii").replace("\n", " ")
                      for i in range(40))
    epub = [
        ("mimetype", b"application/epub+zip"),
        ("META-INF/container.xml", (
            '<?xml version="1.0"?>\n<container version="1.0" xmlns='
            '"urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path='
            '"OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>'
        ).encode("utf-8")),
        ("OEBPS/content.opf", (
            '<?xml version="1.0" encoding="UTF-8"?>\n<package xmlns="http://www.idpf.org/2007/opf" '
            'version="3.0" unique-identifier="id"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/">'
            '<dc:identifier id="id">urn:uuid:48445361-7669-4f72-8000-000000000001</dc:identifier>'
            '<dc:title>HDSavior fixture</dc:title><dc:language>en</dc:language>'
            '<meta property="dcterms:modified">2020-01-01T12:00:00Z</meta></metadata><manifest>'
            '<item id="c1" href="chapter1.xhtml" media-type="application/xhtml+xml"/>'
            '<item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>'
            '</manifest><spine><itemref idref="c1"/></spine></package>').encode("utf-8")),
        ("OEBPS/nav.xhtml", (
            '<?xml version="1.0" encoding="UTF-8"?>\n<html xmlns="http://www.w3.org/1999/xhtml" '
            'xmlns:epub="http://www.idpf.org/2007/ops"><head><title>Contents</title></head><body>'
            '<nav epub:type="toc"><ol><li><a href="chapter1.xhtml">Chapter 1</a></li></ol></nav>'
            '</body></html>').encode("utf-8")),
        ("OEBPS/chapter1.xhtml", (
            '<?xml version="1.0" encoding="UTF-8"?>\n<html xmlns="http://www.w3.org/1999/xhtml">'
            '<head><title>Chapter 1</title></head><body><h1>Chapter 1</h1>%s</body></html>'
            % chapter).encode("utf-8")),
    ]
    plain = [
        ("notes/readme.txt", gen_text("zip/readme", 3000)),
        ("notes/Zażółć gęślą jaźń.txt", gen_text("zip/unicode", 1500)),
        ("bin/noise.bin", rnd("zip/noise", 40000)),
        ("bin/table.csv", gen_text("zip/csv", 20000)),
    ]
    for name, ctype, files in [("document.docx", "docx", docx), ("sheet.xlsx", "xlsx", xlsx),
                               ("slides.pptx", "pptx", pptx), ("text.odt", "odt", odt),
                               ("book.epub", "epub", epub), ("archive.zip", "zip", plain)]:
        _xml_ok(files)
        with open(os.path.join(out, name), "wb") as f:
            f.write(zip_container(files, deflate=True))
        made.append((name, ctype))
    return made


def _check_pdf(data):
    """Follow the last startxref of a PDF and check every xref entry points at its object."""
    import re
    assert data.startswith(b"%PDF-") and data.rstrip().endswith(b"%%EOF")
    pos = int(re.findall(rb"startxref\s+(\d+)\s+%%EOF", data)[-1])
    seen = 0
    while pos is not None:
        m = re.match(rb"xref\s+", data[pos:pos + 16])
        assert m, "startxref does not point at an xref table"
        cur = pos + m.end()
        while True:
            m = re.match(rb"(\d+) (\d+)\s+", data[cur:cur + 32])
            if not m:
                break
            first, count = int(m.group(1)), int(m.group(2))
            cur += m.end()
            for i in range(count):
                entry = data[cur:cur + 20]
                cur += 20
                if entry[17:18] == b"n":
                    off = int(entry[:10])
                    assert data[off:off + 16].startswith(b"%d 0 obj" % (first + i)), "bad xref entry"
                    seen += 1
        m = re.search(rb"/Prev (\d+)", data[cur:cur + 200])
        pos = int(m.group(1)) if m else None
    assert seen >= 6


def ffmpeg_encoders():
    try:
        out = subprocess.run(["ffmpeg", "-hide_banner", "-encoders"], stdout=subprocess.PIPE,
                             stderr=subprocess.DEVNULL, universal_newlines=True, check=True).stdout
    except (OSError, subprocess.CalledProcessError):
        return None
    names = set()
    for line in out.splitlines():
        parts = line.split()
        if len(parts) >= 2 and len(parts[0]) == 6:
            names.add(parts[1])
    return names


def make_samples(out, log=print):
    """Create the sample set in directory out.

    Returns (samples, skipped): samples is a list of dicts
    {"name", "content_type", "group", "size", "sha256", "note"}, skipped a list
    of "name: reason" strings.  Raises if a tool that is always required
    (Pillow, ffmpeg) is missing or a sample fails its validity check.
    """
    from PIL import Image, features
    out = os.path.abspath(out)
    os.makedirs(out, exist_ok=True)
    made = []          # (name, content_type, group, note)
    skipped = []

    # ---- images (Pillow) -------------------------------------------------
    pic = _picture("sample/photo", 640, 480)
    with open(os.path.join(out, "photo.jpg"), "wb") as f:
        f.write(_jpeg_bytes(pic, quality=88))
    made.append(("photo.jpg", "jpeg", "image", "baseline JFIF"))
    thumb = _jpeg_bytes(pic.resize((160, 120)), quality=70)
    with open(os.path.join(out, "photo-exif.jpg"), "wb") as f:
        f.write(_jpeg_bytes(_picture("sample/photo2", 800, 600), quality=85,
                            exif=_exif_with_thumbnail(thumb)))
    made.append(("photo-exif.jpg", "jpeg", "image",
                 "Exif APP1 with an embedded JPEG thumbnail (a second SOI..EOI inside the file)"))
    with open(os.path.join(out, "photo-progressive.jpg"), "wb") as f:
        f.write(_jpeg_bytes(_picture("sample/photo3", 640, 480), quality=80, progressive=True))
    made.append(("photo-progressive.jpg", "jpeg", "image", "progressive (SOF2), several scans"))
    _picture("sample/png", 400, 300).save(os.path.join(out, "picture.png"), "PNG")
    made.append(("picture.png", "png", "image", "deflated, several IDAT chunks"))
    frames = [_picture("sample/gif%d" % i, 160, 120).convert("P", palette=getattr(Image, "Palette", Image).ADAPTIVE)
              for i in range(6)]
    frames[0].save(os.path.join(out, "animation.gif"), "GIF", save_all=True, append_images=frames[1:],
                   duration=120, loop=0)
    made.append(("animation.gif", "gif", "image", "GIF89a, 6 frames"))
    _picture("sample/bmp", 200, 150).save(os.path.join(out, "picture.bmp"), "BMP")
    made.append(("picture.bmp", "bmp", "image", "24-bit uncompressed"))
    _picture("sample/tif", 240, 180).save(os.path.join(out, "picture.tif"), "TIFF")
    made.append(("picture.tif", "tiff", "image", "uncompressed RGB strips"))
    if features.check("webp"):
        _picture("sample/webp", 480, 360).save(os.path.join(out, "picture.webp"), "WEBP", quality=80)
        made.append(("picture.webp", "webp", "image", "lossy VP8 in RIFF"))
    else:
        skipped.append("picture.webp: this Pillow build has no WebP support")
    for name, _, group, _ in made:
        if group == "image":
            with Image.open(os.path.join(out, name)) as im:
                im.load()
                n = getattr(im, "n_frames", 1)
                if name == "animation.gif":
                    assert n == 6, "animation.gif has %d frames" % n

    # ---- audio and video (ffmpeg) ---------------------------------------
    enc = ffmpeg_encoders()
    if enc is None:
        raise RuntimeError("ffmpeg is required to build the media samples")
    base = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-threads", "1"]
    video_in = ["-f", "lavfi", "-i", "testsrc2=size=320x240:rate=15:duration=2.4"]
    audio_in = ["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100:duration=2.4"]
    exact = ["-map_metadata", "-1", "-fflags", "+bitexact", "-flags:v", "+bitexact", "-flags:a", "+bitexact"]
    exact_a = ["-map_metadata", "-1", "-fflags", "+bitexact", "-flags:a", "+bitexact"]
    h264 = (["-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p", "-b:v", "260k"]
            if "libx264" in enc else ["-c:v", "mpeg4", "-b:v", "300k"])
    aac = ["-c:a", "aac", "-b:a", "64k", "-ac", "1"]
    jobs = [
        ("clip.mp4", "mp4", "video", "moov after mdat (the ffmpeg default)",
         video_in + audio_in + h264 + aac + exact + ["-f", "mp4"], None),
        ("clip-faststart.mp4", "mp4", "video", "moov before mdat (-movflags +faststart)",
         video_in + audio_in + h264 + aac + exact + ["-movflags", "+faststart", "-f", "mp4"], None),
        ("clip.mov", "mov", "video", "QuickTime container",
         video_in + audio_in + h264 + aac + exact + ["-f", "mov"], None),
        ("clip.3gp", "3gp", "video", "H.263 176x144 + AAC",
         ["-f", "lavfi", "-i", "testsrc2=size=176x144:rate=15:duration=2.4"] + audio_in
         + ["-c:v", "h263", "-b:v", "120k", "-c:a", "aac", "-b:a", "32k", "-ac", "1", "-ar", "16000"]
         + exact + ["-f", "3gp"], "h263"),
        ("clip.avi", "avi", "video", "MPEG-4 part 2 + MP3 in RIFF",
         video_in + audio_in + ["-c:v", "mpeg4", "-vtag", "xvid", "-b:v", "200k", "-c:a", "libmp3lame",
                                "-b:a", "64k", "-ac", "1"] + exact + ["-f", "avi"], "libmp3lame"),
        ("clip.mkv", "mkv", "video", "Matroska",
         video_in + audio_in + h264 + ["-c:a", "libvorbis", "-b:a", "64k", "-ac", "2"] + exact
         + ["-f", "matroska"], "libvorbis"),
        ("clip.webm", "webm", "video", "VP8 + Vorbis",
         video_in + audio_in + ["-c:v", "libvpx", "-b:v", "220k", "-deadline", "good", "-cpu-used", "4",
                                "-c:a", "libvorbis", "-b:a", "64k", "-ac", "2"] + exact + ["-f", "webm"],
         "libvpx"),
        ("sound.wav", "wav", "audio", "PCM 16-bit mono 22.05 kHz",
         audio_in + ["-c:a", "pcm_s16le", "-ar", "22050", "-ac", "1"] + exact_a + ["-f", "wav"], None),
        ("song.mp3", "mp3", "audio", "starts with an ID3v2.3 tag",
         audio_in + ["-c:a", "libmp3lame", "-b:a", "96k", "-ac", "2"] + exact_a
         + ["-id3v2_version", "3", "-metadata", "title=HDSavior fixture", "-metadata", "artist=make-fixtures",
            "-f", "mp3"], "libmp3lame"),
        ("song-noid3.mp3", "mp3", "audio", "no ID3 tag: starts with an MPEG frame sync",
         audio_in + ["-c:a", "libmp3lame", "-b:a", "64k", "-ac", "1"] + exact_a
         + ["-id3v2_version", "0", "-write_xing", "0", "-f", "mp3"], "libmp3lame"),
        ("voice.m4a", "m4a", "audio", "AAC in ISO-BMFF (M4A brand)",
         audio_in + aac + exact_a + ["-f", "ipod"], None),
        ("track.flac", "flac", "audio", "FLAC (not a type HDSavior carves)",
         audio_in + ["-c:a", "flac", "-ar", "22050", "-ac", "1"] + exact_a + ["-f", "flac"], None),
        ("track.ogg", "ogg", "audio", "Ogg Vorbis (not a type HDSavior carves)",
         audio_in + ["-c:a", "libvorbis", "-b:a", "64k", "-ac", "2"] + exact_a + ["-f", "ogg"], "libvorbis"),
    ]
    # The fixtures cannot do without these; any other sample that this ffmpeg
    # build cannot produce is skipped and reported in the manifests.
    required = {"clip.mp4", "clip.mov", "clip.avi", "sound.wav", "song.mp3"}
    for name, ctype, group, note, args, needs in jobs:
        if needs and needs not in enc:
            if name in required:
                raise RuntimeError("ffmpeg has no %s encoder, which the sample %s needs" % (needs, name))
            skipped.append("%s: ffmpeg has no %s encoder" % (name, needs))
            continue
        path = os.path.join(out, name)
        run = subprocess.run(base + args + [path], stderr=subprocess.PIPE, universal_newlines=True)
        ok = run.returncode == 0
        if ok:
            probe = subprocess.run(["ffprobe", "-v", "error", "-show_entries", "format=format_name",
                                    "-of", "csv=p=0", path], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   universal_newlines=True)
            ok = probe.returncode == 0 and bool(probe.stdout.strip())
            run = probe
        if not ok:
            reason = (run.stderr or "").strip().split("\n")[-1][:200]
            if name in required:
                raise RuntimeError("could not make the sample %s: %s" % (name, reason))
            skipped.append("%s: ffmpeg failed: %s" % (name, reason))
            if os.path.exists(path):
                os.remove(path)
            continue
        made.append((name, ctype, group, note))

    # ---- documents and archives -----------------------------------------
    lines = [gen_text("sample/pdf/line%d" % i, 70).decode("ascii").replace("\n", " ") for i in range(30)]
    payload = rnd("sample/pdf/payload", 60000)
    objs = _pdf_objects(lines, payload, compress=True)
    data = _pdf(objs)
    _check_pdf(data)
    with open(os.path.join(out, "report.pdf"), "wb") as f:
        f.write(data)
    made.append(("report.pdf", "pdf", "document", "one %%EOF, Flate content stream"))
    new_content = _pdf_stream(zlib.compress(b"BT /F1 12 Tf 72 720 Td (Revised by an incremental update) Tj ET", 6),
                              b" /Filter /FlateDecode")
    data = _pdf(objs, updates={4: new_content})
    _check_pdf(data)
    with open(os.path.join(out, "report-updated.pdf"), "wb") as f:
        f.write(data)
    made.append(("report-updated.pdf", "pdf", "document",
                 "incremental update: two xref sections, two %%EOF markers"))
    for name, ctype in _office_samples(out):
        made.append((name, ctype, "document",
                     "written by fixture_content.py; checked as ZIP with well-formed XML parts only"
                     if ctype in ("docx", "xlsx", "pptx", "odt", "epub") else "deflated members"))
    import zipfile
    for name, ctype, group, _ in made:
        if ctype in ("docx", "xlsx", "pptx", "odt", "epub", "zip"):
            with zipfile.ZipFile(os.path.join(out, name)) as z:
                if z.testzip() is not None:
                    raise RuntimeError("zipfile rejects the sample %s" % name)

    # 7z and RAR need external archivers; both are optional.
    stage = os.path.join(out, ".stage")
    os.makedirs(stage, exist_ok=True)
    for n, d in [("readme.txt", gen_text("arc/readme", 4000)), ("noise.bin", rnd("arc/noise", 50000)),
                 ("table.csv", gen_text("arc/csv", 30000))]:
        p = os.path.join(stage, n)
        with open(p, "wb") as f:
            f.write(d)
        os.utime(p, (1577880000, 1577880000))
    members = ["readme.txt", "noise.bin", "table.csv"]
    seven = next((t for t in ("7z", "7zz", "7za", "7zr") if shutil.which(t)), None)
    if seven:
        target = os.path.join(out, "archive.7z")
        subprocess.run([seven, "a", "-t7z", "-mx=5", "-mmt=1", "-bd", "-y", target] + members, cwd=stage,
                       check=True, stdout=subprocess.DEVNULL)
        subprocess.run([seven, "t", "-bd", target], check=True, stdout=subprocess.DEVNULL)
        made.append(("archive.7z", "7z", "document", "LZMA2, written by " + seven))
    else:
        skipped.append("archive.7z: no 7z / 7zz / 7za / 7zr in PATH")
    if shutil.which("rar"):
        target = os.path.join(out, "archive.rar")
        subprocess.run(["rar", "a", "-ma5", "-m3", "-ep", "-idq", "-y", target] + members, cwd=stage,
                       check=True, stdout=subprocess.DEVNULL)
        made.append(("archive.rar", "rar", "document", "RAR 5, written by the rar binary"))
    else:
        skipped.append("archive.rar: no rar binary in PATH (it is non-free and absent from CI runners)")
    shutil.rmtree(stage)

    samples = []
    for name, ctype, group, note in made:
        with open(os.path.join(out, name), "rb") as f:
            data = f.read()
        if len(data) > 256 * 1024:
            raise RuntimeError("sample %s is %d bytes; samples must stay under 256 KiB" % (name, len(data)))
        samples.append({"name": name, "content_type": ctype, "group": group, "size": len(data),
                        "sha256": hashlib.sha256(data).hexdigest(), "note": note})
    return samples, skipped
