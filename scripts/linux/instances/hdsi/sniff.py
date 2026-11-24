"""Signature sniffing for carve hits (A3.4, F2 signature anchors) and the extension check of F3.

A Python model of the subset of engine/crates/hds-carve (sniff.rs, ext.rs) that the lab's
fixtures produce: JPEG (JFIF/Exif APP segments), PNG (signature + IHDR), GIF, PDF, ZIP and the
ZIP-based office formats.  `sniff` is strict (a scan of every sector), `matches_extension` is
lenient (the file is known to exist), as in the crate.
"""
import struct

SNIFF_LEN = 64

JPEG, PNG, GIF, PDF, ZIP = 1, 2, 3, 4, 5
KIND_NAMES = {JPEG: 'jpeg', PNG: 'png', GIF: 'gif', PDF: 'pdf', ZIP: 'zip'}

_EXT = {}
for e in ('jpg', 'jpeg', 'jpe', 'jfif', 'jfi', 'jif', 'thm'):
    _EXT[e] = ('jpeg', (JPEG,))
for e in ('png', 'apng'):
    _EXT[e] = ('png', (PNG,))
_EXT['gif'] = ('gif', (GIF,))
_EXT['pdf'] = ('pdf', (PDF,))
for e in ('zip', 'jar', 'apk', 'epub', 'odt', 'ods', 'odp', 'kmz', 'cbz'):
    _EXT[e] = ('zip', (ZIP,))
for e in ('docx', 'docm', 'xlsx', 'xlsm', 'pptx', 'pptm'):
    _EXT[e] = ('ooxml', (ZIP,))


def ext_of(name):
    if name is None or '.' not in name:
        return ''
    e = name.rsplit('.', 1)[1].lower()
    return e if len(e) <= 12 else ''


def kinds_for_extension(ext):
    f = _EXT.get(ext.lower())
    return f[1] if f else ()


def has_opinion(ext):
    return ext.lower() in _EXT


def _is_zip(h):
    return h[:4] in (b'PK\x03\x04', b'PK\x05\x06', b'PK\x07\x08')


def matches_extension(head, ext):
    """None: no opinion; True / False otherwise (ext.rs matches_extension, subset)."""
    f = _EXT.get(ext.lower())
    if f is None:
        return None
    fam = f[0]
    if fam == 'jpeg':
        return head[:3] == b'\xff\xd8\xff'
    if fam == 'png':
        return head[:8] == b'\x89PNG\r\n\x1a\n'
    if fam == 'gif':
        return head[:6] in (b'GIF87a', b'GIF89a')
    if fam == 'pdf':
        return b'%PDF-' in head[:1024]
    if fam == 'zip':
        return _is_zip(head)
    if fam == 'ooxml':
        return _is_zip(head) or head[:8] == b'\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1'
    return None


def _sniff_jpeg(h):
    if len(h) < 8 or h[2] != 0xFF:
        return False
    ln = struct.unpack_from('>H', h, 4)[0]
    body = h[6:]
    m = h[3]
    if m == 0xE0:
        return ln >= 16 and (body.startswith(b'JFIF\0') or body.startswith(b'JFXX\0'))
    if m == 0xE1:
        return ln >= 8 and (body.startswith(b'Exif\0\0') or body.startswith(b'http://ns.adobe.com/'))
    return False


def _sniff_png(h):
    if h[:16] != b'\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR':
        return False
    d = h[16:29]
    if len(d) < 13:
        return True
    w, hh = struct.unpack_from('>II', d, 0)
    depth, colour, comp, filt, inter = d[8], d[9], d[10], d[11], d[12]
    if colour == 0:
        dok = depth in (1, 2, 4, 8, 16)
    elif colour == 3:
        dok = depth in (1, 2, 4, 8)
    elif colour in (2, 4, 6):
        dok = depth in (8, 16)
    else:
        dok = False
    return w != 0 and hh != 0 and w < 2 ** 31 and hh < 2 ** 31 and dok and comp == 0 and filt == 0 and inter in (0, 1)


def _sniff_gif(h):
    if h[:6] not in (b'GIF87a', b'GIF89a') or len(h) < 13:
        return False
    w, hh = struct.unpack_from('<HH', h, 6)
    if w == 0 or hh == 0:
        return False
    packed = h[10]
    if packed & 0x80 == 0 and len(h) > 13 and h[13] not in (0x2C, 0x21):
        return False
    return True


def _sniff_pdf(h):
    return h[:5] == b'%PDF-' and len(h) >= 8 and h[5] in b'12' and h[6] == ord('.') and 48 <= h[7] <= 57


_ZIP_METHODS = set(range(0, 11)) | {12, 14, 16, 18, 19, 20} | set(range(93, 100))


def _sniff_zip(h):
    if len(h) < 30 or h[:4] != b'PK\x03\x04':
        return False
    version, flags, method = struct.unpack_from('<HHH', h, 4)
    nlen = struct.unpack_from('<H', h, 26)[0]
    if version & 0xFF > 63 or flags & 0xD780 or method not in _ZIP_METHODS or nlen == 0 or nlen > 4096:
        return False
    name = h[30:30 + nlen]
    return all(c >= 0x20 and c != 0x7F for c in name)


def sniff(h):
    """Kind code or 0."""
    h = h[:SNIFF_LEN]
    if len(h) < 8:
        return 0
    c = h[0]
    if c == 0xFF and h[1] == 0xD8:
        return JPEG if _sniff_jpeg(h) else 0
    if c == 0x89:
        return PNG if _sniff_png(h) else 0
    if c == 0x47:
        return GIF if _sniff_gif(h) else 0
    if c == 0x25:
        return PDF if _sniff_pdf(h) else 0
    if c == 0x50:
        return ZIP if _sniff_zip(h) else 0
    return 0


# prefixes the scanner looks for with bytes.find before calling sniff
PREFIXES = (b'\xff\xd8\xff', b'\x89PNG\r\n\x1a\n', b'GIF8', b'%PDF-', b'PK\x03\x04')
