"""Low-level NTFS structures read from raw bytes (boot sector, FILE record, INDX block).

Written for the instance-separation lab from docs/notes/ntfs-on-disk.md and
/usr/include/ntfs-3g/layout.h.  It shares no code with the engine
(engine/crates/hds-ntfs) or with scripts/linux/inspect-ntfs.py.

Only what ALGORITHMS.md A3-A5 stores is decoded:
  FILE  -> Record   (header, fixup status, $STANDARD_INFORMATION, every
                     $FILE_NAME, unnamed $DATA extents, $INDEX_ALLOCATION:$I30
                     extents, $ATTRIBUTE_LIST presence / resident entries for
                     0x80 and 0xA0, reparse tag, WofCompressedData, directory
                     entries of $INDEX_ROOT:$I30)
  INDX  -> IndexBlock with its entries
  boot  -> BootSector
All offsets are absolute byte offsets in the image unless said otherwise.
"""
import struct

SECTOR = 512
FT_1980 = 119600064000000000          # FILETIME of 1980-01-01 00:00:00 UTC
FT_DAY = 864000000000                 # one day in FILETIME ticks
FT_UNIX_EPOCH = 116444736000000000


def ft_to_unix(ft):
    return (ft - FT_UNIX_EPOCH) / 1e7


def unix_to_ft(t):
    return int(round(t * 1e7)) + FT_UNIX_EPOCH


def fnv1a64(b):
    h = 0xCBF29CE484222325
    for x in b:
        h ^= x
        h = (h * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h


try:                                   # a faster hash of the same function when available
    import hashlib as _hl  # noqa: F401
except ImportError:  # pragma: no cover
    pass


def next_seq(s):
    return 1 if s == 0xFFFF else s + 1


def resolves(ref_seq, rec_in_use, rec_seq):
    """The reference rule of ALGORITHMS.md (vocabulary)."""
    if ref_seq == 0:
        return True
    if rec_in_use:
        return rec_seq == ref_seq
    return rec_seq == next_seq(ref_seq)


# --------------------------------------------------------------------------- runlists

def decode_runs(b, start, end):
    """Mapping pairs -> list of (lcn or None for a hole, length).  ok=False when malformed."""
    runs = []
    i = start
    lcn = 0
    while i < end:
        h = b[i]
        if h == 0:
            return runs, True
        ls = h & 0x0F
        os_ = h >> 4
        if ls == 0 or ls > 8 or os_ > 8 or i + 1 + ls + os_ > end:
            return runs, False
        length = int.from_bytes(b[i + 1:i + 1 + ls], 'little', signed=True)
        if length <= 0:
            return runs, False
        if os_:
            lcn += int.from_bytes(b[i + 1 + ls:i + 1 + ls + os_], 'little', signed=True)
            if lcn < 0:
                return runs, False
            runs.append((lcn, length))
        else:
            runs.append((None, length))
        i += 1 + ls + os_
    return runs, False


# --------------------------------------------------------------------------- boot sector

class BootSector:
    __slots__ = ('off', 'bps', 'spc', 'cs', 'hidden', 'total_sectors', 'mft_lcn', 'mirr_lcn',
                 'rec_size', 'idx_size', 'serial')

    def as_dict(self):
        return {k: getattr(self, k) for k in self.__slots__}


def parse_boot(b, off):
    """b: at least 512 bytes.  None when it is not a usable NTFS boot sector."""
    if len(b) < 512 or b[3:11] != b'NTFS    ' or b[510:512] != b'\x55\xaa':
        return None
    bps = struct.unpack_from('<H', b, 0x0B)[0]
    if bps not in (512, 1024, 2048, 4096):
        return None
    v = b[0x0D]
    if 1 <= v <= 0x80 and (v & (v - 1)) == 0:
        spc = v
    elif v >= 0xF4:
        spc = 1 << (256 - v)
    else:
        return None
    cs = bps * spc
    if cs > (2 << 20):
        return None
    total = struct.unpack_from('<q', b, 0x28)[0]
    mft_lcn, mirr_lcn = struct.unpack_from('<qq', b, 0x30)
    if total <= 0 or mft_lcn < 0 or mirr_lcn < 0:
        return None

    def sz(raw):
        raw = struct.unpack('<b', bytes([raw]))[0]
        if raw > 0:
            return raw * cs
        if -31 <= raw < 0:
            e = -raw
            return (1 << e) if e <= 12 else None
        return None
    rs = sz(b[0x40])
    isz = sz(b[0x44])
    if not rs or rs < 256 or rs > 65536:
        return None
    bs = BootSector()
    bs.off = off
    bs.bps = bps
    bs.spc = spc
    bs.cs = cs
    bs.hidden = struct.unpack_from('<I', b, 0x1C)[0]
    bs.total_sectors = total
    bs.mft_lcn = mft_lcn
    bs.mirr_lcn = mirr_lcn
    bs.rec_size = rs
    bs.idx_size = isz or 4096
    bs.serial = struct.unpack_from('<Q', b, 0x48)[0]
    return bs


# --------------------------------------------------------------------------- fixups

def apply_fixups(raw, size):
    """Returns (fixed bytearray, bad_block_count, block_count) or None when the header is unusable."""
    if len(raw) < size:
        return None
    usa_ofs, usa_cnt = struct.unpack_from('<HH', raw, 4)
    blocks = size // SECTOR
    if usa_cnt != blocks + 1 or usa_ofs < 0x28 or usa_ofs + 2 * usa_cnt > 510 or usa_ofs & 1:
        return None
    b = bytearray(raw[:size])
    usn = bytes(b[usa_ofs:usa_ofs + 2])
    bad = 0
    for i in range(1, usa_cnt):
        e = i * SECTOR
        if b[e - 2:e] != usn:
            bad += 1
        else:
            b[e - 2:e] = b[usa_ofs + 2 * i:usa_ofs + 2 * i + 2]
    return b, bad, blocks


# --------------------------------------------------------------------------- FILE record

class FileName:
    __slots__ = ('parent', 'pseq', 'ns', 'flags', 'attr_id', 'name', 'ctime', 'chtime', 'tmax')

    def __repr__(self):
        return 'FN(%r <- %d.%d ns%d)' % (self.name, self.parent, self.pseq, self.ns)


class DataExt:
    """One extent of the unnamed $DATA (or of $INDEX_ALLOCATION:$I30)."""
    __slots__ = ('resident', 'res_off', 'res_len', 'svcn', 'evcn', 'runs', 'runs_ok', 'aflags',
                 'alloc', 'real', 'init')


class IndexEntry:
    __slots__ = ('child', 'cseq', 'parent', 'pseq', 'ns', 'name', 'chtime', 'etime', 'is_dir',
                 'src_off', 'src_lsn', 'src_kind', 'block', 'current')
    # chtime: the MFT-change time of the $FILE_NAME copy; etime: the entry time (ALGORITHMS.md
    # vocabulary), the latest of its four times


class Record:
    __slots__ = ('off', 'size', 'num', 'seq', 'flags', 'links', 'lsn', 'base', 'base_seq', 'hash',
                 'fixup_bad', 'fixup_blocks', 'si', 'names', 'data', 'ia', 'wof', 'has_attrlist',
                 'attrlist', 'reparse', 'has_attrs', 'root_entries', 'attr_types', 'vol_name',
                 'bitmap_attr', 'cls', 'frag', 'seg', 'inst', 'older_gen', 'idx', 'copy_of')

    @property
    def in_use(self):
        return bool(self.flags & 1)

    @property
    def is_dir(self):
        return bool(self.flags & 2) or any(fn.flags & 0x10000000 for fn in self.names)

    @property
    def fixup_ok(self):
        return self.fixup_bad == 0

    @property
    def chtime(self):
        """The raw $STANDARD_INFORMATION MFT-change time (the rules' time before 'record time')."""
        return self.si[2] if self.si else None

    @property
    def rtime(self):
        """Record time (ALGORITHMS.md vocabulary): the latest of the $SI MFT-change time and the four
        times of every $FILE_NAME; None when the record has neither."""
        ts = [fn.tmax for fn in self.names]
        if self.si:
            ts.append(self.si[2])
        return max(ts) if ts else None

    def best_name(self):
        """E4 namespace preference (Win32 or Win32+DOS, then POSIX, then DOS), lowest attribute id."""
        best = None
        for fn in self.names:
            rank = {1: 0, 3: 0, 0: 1, 2: 2}.get(fn.ns, 3)
            k = (rank, fn.attr_id)
            if best is None or k < best[0]:
                best = (k, fn)
        return best[1] if best else None

    def __repr__(self):
        n = self.best_name()
        return 'Rec(#%s.%d @%d %s%s %r)' % (self.num, self.seq, self.off, 'U' if self.in_use else '-',
                                            'D' if self.is_dir else 'F', n.name if n else None)


def _parse_file_name(c):
    if len(c) < 66:
        return None
    fn = FileName()
    pref = struct.unpack_from('<Q', c, 0)[0]
    fn.parent = pref & 0xFFFFFFFFFFFF
    fn.pseq = pref >> 48
    fn.ctime = struct.unpack_from('<Q', c, 8)[0]
    fn.chtime = struct.unpack_from('<Q', c, 0x18)[0]
    fn.tmax = max(struct.unpack_from('<QQQQ', c, 8))
    fn.flags = struct.unpack_from('<I', c, 0x38)[0]
    nl = c[0x40]
    fn.ns = c[0x41]
    if 66 + 2 * nl > len(c):
        return None
    fn.name = bytes(c[66:66 + 2 * nl]).decode('utf-16-le', 'surrogatepass')
    return fn


def parse_index_entries(b, start, end, src_off, src_lsn, src_kind):
    """Entries of one index node (bytes b[start:end]).  Returns every entry with a $FILE_NAME key."""
    out = []
    q = start
    while q + 16 <= end:
        cref, elen, klen, efl = struct.unpack_from('<QHHH', b, q)
        if elen < 16 or q + elen > end:
            break
        if efl & 2:
            break
        if klen >= 66 and q + 16 + klen <= end:
            fn = _parse_file_name(b[q + 16:q + 16 + klen])
            if fn is not None:
                e = IndexEntry()
                e.child = cref & 0xFFFFFFFFFFFF
                e.cseq = cref >> 48
                e.parent = fn.parent
                e.pseq = fn.pseq
                e.ns = fn.ns
                e.name = fn.name
                e.chtime = fn.chtime
                e.etime = fn.tmax
                e.is_dir = bool(fn.flags & 0x10000000)
                e.src_off = src_off
                e.src_lsn = src_lsn
                e.src_kind = src_kind
                e.block = None
                e.current = False
                out.append(e)
        q += elen
    return out


def parse_record(raw, off, want_bitmap=False):
    """raw: bytes starting at a 'FILE' magic (at least the record size).  Returns a Record, or a
    string saying why the record was dropped / unusable ('never-used', 'all-fixups-bad', 'bad')."""
    if len(raw) < 0x30 or raw[:4] != b'FILE':
        return 'bad'
    size = struct.unpack_from('<I', raw, 0x1C)[0]
    if size < 512 or size > 65536 or size & (size - 1) or size > len(raw):
        return 'bad'
    fx = apply_fixups(raw, size)
    if fx is None:
        return 'bad'
    b, bad, blocks = fx
    r = Record()
    r.off = off
    r.size = size
    r.hash = fnv1a64(raw[:size])
    r.fixup_bad = bad
    r.fixup_blocks = blocks
    usa_ofs = struct.unpack_from('<H', b, 4)[0]
    r.lsn = struct.unpack_from('<Q', b, 8)[0]
    r.seq, r.links, attr_ofs, r.flags = struct.unpack_from('<HHHH', b, 0x10)
    used = struct.unpack_from('<I', b, 0x18)[0]
    bref = struct.unpack_from('<Q', b, 0x20)[0]
    r.base = bref & 0xFFFFFFFFFFFF
    r.base_seq = bref >> 48
    if usa_ofs >= 0x30:
        r.num = struct.unpack_from('<I', b, 0x2C)[0] | (struct.unpack_from('<H', b, 0x2A)[0] << 32)
    else:
        r.num = None
    r.si = None
    r.names = []
    r.data = []
    r.ia = []
    r.wof = False
    r.has_attrlist = False
    r.attrlist = None
    r.reparse = None
    r.has_attrs = False
    r.root_entries = []
    r.attr_types = []
    r.vol_name = None
    r.bitmap_attr = None
    r.cls = None
    r.frag = None
    r.seg = None
    r.inst = None
    r.older_gen = False
    r.idx = None
    r.copy_of = None
    limit = min(size, used if 0 < used <= size else size)
    p = attr_ofs
    while 0 < p and p + 16 <= limit:
        t, ln = struct.unpack_from('<II', b, p)
        if t == 0xFFFFFFFF:
            break
        if ln < 16 or ln & 7 or p + ln > size:
            break
        r.has_attrs = True
        r.attr_types.append(t)
        nonres = b[p + 8]
        nlen = b[p + 9]
        nofs = struct.unpack_from('<H', b, p + 10)[0]
        aflags = struct.unpack_from('<H', b, p + 12)[0]
        aid = struct.unpack_from('<H', b, p + 14)[0]
        name = ''
        if nlen:
            if p + nofs + 2 * nlen <= p + ln:
                name = bytes(b[p + nofs:p + nofs + 2 * nlen]).decode('utf-16-le', 'surrogatepass')
            else:
                name = '?'
        if not nonres:
            if p + 24 > p + ln:
                p += ln
                continue
            vlen, vofs = struct.unpack_from('<IH', b, p + 16)
            if p + vofs + vlen > p + ln:
                p += ln
                continue
            c = b[p + vofs:p + vofs + vlen]
            if t == 0x10 and vlen >= 48:
                r.si = struct.unpack_from('<QQQQI', c, 0)
            elif t == 0x30:
                fn = _parse_file_name(c)
                if fn is not None:
                    fn.attr_id = aid
                    r.names.append(fn)
            elif t == 0x80:
                if name == '':
                    d = DataExt()
                    d.resident = True
                    d.res_off = p + vofs
                    d.res_len = vlen
                    d.svcn = 0
                    d.evcn = -1
                    d.runs = []
                    d.runs_ok = True
                    d.aflags = aflags
                    d.alloc = d.real = d.init = vlen
                    r.data.append(d)
                elif name == 'WofCompressedData':
                    r.wof = True
            elif t == 0x90 and name == '$I30' and vlen >= 32:
                eo, isz = struct.unpack_from('<II', c, 16)
                r.root_entries = parse_index_entries(c, 16 + eo, min(vlen, 16 + isz), off, r.lsn, 'root')
            elif t == 0x20:
                r.has_attrlist = True
                ents = []
                q = 0
                while q + 0x1A <= vlen:
                    at, el = struct.unpack_from('<IH', c, q)
                    if el < 0x1A or q + el > vlen:
                        break
                    svcn = struct.unpack_from('<q', c, q + 8)[0]
                    ref = struct.unpack_from('<Q', c, q + 0x10)[0]
                    if at in (0x80, 0xA0):
                        ents.append((at, svcn, ref & 0xFFFFFFFFFFFF, ref >> 48))
                    q += el
                r.attrlist = ents
            elif t == 0xC0 and vlen >= 4:
                r.reparse = struct.unpack_from('<I', c, 0)[0]
            elif t == 0x60:
                r.vol_name = bytes(c).decode('utf-16-le', 'replace')
            elif t == 0xB0 and name == '' and want_bitmap:
                r.bitmap_attr = ('res', bytes(c))
        else:
            if p + 0x40 > p + ln:
                p += ln
                continue
            svcn, evcn = struct.unpack_from('<qq', b, p + 16)
            rofs = struct.unpack_from('<H', b, p + 0x20)[0]
            alloc, real, init = struct.unpack_from('<QQQ', b, p + 0x28)
            runs, ok = decode_runs(b, p + rofs, p + ln) if rofs < ln else ([], False)
            if t == 0x80 and name == '':
                d = DataExt()
                d.resident = False
                d.res_off = d.res_len = 0
                d.svcn, d.evcn, d.runs, d.runs_ok, d.aflags = svcn, evcn, runs, ok, aflags
                d.alloc, d.real, d.init = alloc, real, init
                r.data.append(d)
            elif t == 0x80 and name == 'WofCompressedData':
                r.wof = True
            elif t == 0xA0 and name == '$I30':
                d = DataExt()
                d.resident = False
                d.res_off = d.res_len = 0
                d.svcn, d.evcn, d.runs, d.runs_ok, d.aflags = svcn, evcn, runs, ok, aflags
                d.alloc, d.real, d.init = alloc, real, init
                r.ia.append(d)
            elif t == 0x20:
                r.has_attrlist = True
                if want_bitmap:
                    r.attrlist = ('nonres', runs, real)
            elif t == 0xC0:
                r.reparse = -1          # non-resident: tag unknown
            elif t == 0xB0 and name == '' and want_bitmap:
                r.bitmap_attr = ('nonres', runs, real)
        p += ln
    if not r.in_use and not r.has_attrs:
        return 'never-used'
    if bad == blocks:
        return 'all-fixups-bad'
    return r


# --------------------------------------------------------------------------- INDX block

class IndexBlock:
    __slots__ = ('off', 'vcn', 'size', 'lsn', 'owner', 'oseq', 'fixup_bad', 'entries', 'current',
                 'idx')


def parse_indx(raw, off):
    if len(raw) < 0x40 or raw[:4] != b'INDX':
        return None
    usa_cnt = struct.unpack_from('<H', raw, 6)[0]
    size = (usa_cnt - 1) * SECTOR
    if size < 512 or size > 65536 or size > len(raw):
        return None
    fx = apply_fixups(raw, size)
    if fx is None:
        return None
    b, bad, blocks = fx
    if bad == blocks:
        return None
    ib = IndexBlock()
    ib.off = off
    ib.size = size
    ib.fixup_bad = bad
    ib.lsn = struct.unpack_from('<Q', b, 8)[0]
    ib.vcn = struct.unpack_from('<q', b, 0x10)[0]
    eo, isz, asz = struct.unpack_from('<III', b, 0x18)
    ib.entries = parse_index_entries(b, 0x18 + eo, min(size, 0x18 + isz), off, ib.lsn, 'block')
    cnt = {}
    for e in ib.entries:
        e.block = ib
        k = (e.parent, e.pseq)
        cnt[k] = cnt.get(k, 0) + 1
    if cnt:
        (ib.owner, ib.oseq), _ = max(cnt.items(), key=lambda kv: (kv[1], -kv[0][0]))
    else:
        ib.owner = ib.oseq = None
    ib.current = False
    ib.idx = None
    return ib
