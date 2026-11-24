"""The scan of ALGORITHMS.md A3: every 512-byte offset of the session range is tested for a FILE
record, an INDX block, an NTFS boot sector and a carve signature.  No byte outside the range is
read (A1).  Results are cached next to the image (keyed by size, mtime and range).
"""
import mmap
import os
import pickle
import struct

from . import ntfsraw as nr
from . import sniff as sn

CACHE_VERSION = 7


class ScanResult:
    def __init__(self):
        self.path = None
        self.image_size = 0
        self.range = (0, 0)
        self.records = []          # Record, ascending offset
        self.dropped = {'never-used': 0, 'all-fixups-bad': 0, 'bad': 0}
        self.blocks = []           # IndexBlock
        self.entries = []          # IndexEntry, directory entries only (blocks and $INDEX_ROOTs)
        self.boots = []            # BootSector
        self.carve = []            # (offset, kind)
        self.partitions = []       # (start, length) from a partition table inside the range


def _hits(mm, needle, start, end, delta=0):
    """Offsets o (512-aligned) in [start, end) with needle at o + delta."""
    out = []
    pos = start + delta
    lim = end
    while True:
        i = mm.find(needle, pos, lim)
        if i < 0:
            return out
        o = i - delta
        if o % 512 == 0 and o >= start:
            out.append(o)
            pos = i + 1
        else:
            pos = i + 1


def parse_partitions(read, start, end):
    """MBR or GPT at the range start (only when the table itself lies inside the range)."""
    if start != 0 or end < 1024:
        return []
    mbr = read(0, 512)
    if mbr[510:512] != b'\x55\xaa':
        return []
    parts = []
    gpt = False
    for i in range(4):
        e = mbr[0x1BE + 16 * i:0x1BE + 16 * (i + 1)]
        typ = e[4]
        lba, cnt = struct.unpack_from('<II', e, 8)
        if typ == 0xEE:
            gpt = True
        elif typ not in (0, 0x05, 0x0F) and cnt:
            parts.append((lba * 512, cnt * 512))
    if gpt:
        parts = []
        hdr = read(512, 512)
        if hdr[:8] == b'EFI PART':
            ent_lba, n, esz = struct.unpack_from('<QII', hdr, 0x48)
            for i in range(min(n, 256)):
                o = ent_lba * 512 + i * esz
                if o + esz > end:
                    break
                e = read(o, esz)
                if e[:16] == b'\0' * 16:
                    continue
                first, last = struct.unpack_from('<QQ', e, 32)
                parts.append((first * 512, (last - first + 1) * 512))
    return sorted(p for p in parts if p[0] < end)


def scan(path, start=0, end=None, use_cache=True, cache_dir=None):
    st = os.stat(path)
    if end is None:
        end = st.st_size
    key = (CACHE_VERSION, st.st_size, int(st.st_mtime_ns), start, end)
    cpath = None
    if use_cache:
        cdir = cache_dir or os.path.dirname(os.path.abspath(path))
        cpath = os.path.join(cdir, '.%s.%d-%d.scan.pkl' % (os.path.basename(path), start, end))
        if os.path.exists(cpath):
            try:
                with open(cpath, 'rb') as f:
                    k, res = pickle.load(f)
                if k == key:
                    return res
            except Exception:
                pass
    res = ScanResult()
    res.path = path
    res.image_size = st.st_size
    res.range = (start, end)
    with open(path, 'rb') as f:
        mm = mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ)
        try:
            def read(o, n):
                # every read stays inside the session range (A1)
                o2 = min(max(o, start), end)
                e2 = min(max(o + n, start), end)
                return mm[o2:e2]
            res.partitions = parse_partitions(read, start, end)
            for o in _hits(mm, b'FILE', start, end):
                raw = read(o, 4096)
                r = nr.parse_record(raw, o)
                if isinstance(r, str):
                    res.dropped[r] += 1
                    continue
                res.records.append(r)
            for o in _hits(mm, b'INDX', start, end):
                ib = nr.parse_indx(read(o, 65536 if o + 65536 <= end else end - o), o)
                if ib is None:
                    continue
                res.blocks.append(ib)
            for o in _hits(mm, b'NTFS    ', start, end, delta=3):
                bs = nr.parse_boot(read(o, 512), o)
                if bs is not None:
                    res.boots.append(bs)
            hits = set()
            for pre in sn.PREFIXES:
                for o in _hits(mm, pre, start, end):
                    if o in hits:
                        continue
                    k = sn.sniff(read(o, sn.SNIFF_LEN))
                    if k:
                        hits.add(o)
                        res.carve.append((o, k))
            res.carve.sort()
        finally:
            mm.close()
    for i, r in enumerate(res.records):
        r.idx = i
    for i, b in enumerate(res.blocks):
        b.idx = i
    for b in res.blocks:
        res.entries.extend(e for e in b.entries if e.is_dir)
    for r in res.records:
        res.entries.extend(e for e in r.root_entries if e.is_dir)
    if cpath:
        try:
            with open(cpath + '.tmp', 'wb') as f:
                pickle.dump((key, res), f, protocol=pickle.HIGHEST_PROTOCOL)
            os.replace(cpath + '.tmp', cpath)
        except OSError:
            pass
    return res


class RangeReader:
    """Reads of the model (C2 snapshot, F3 heads): never outside the session range."""

    def __init__(self, path, start, end):
        self.f = open(path, 'rb')
        self.start, self.end = start, end
        self.outside = 0

    def read(self, o, n):
        if o < self.start or o + n > self.end:
            self.outside += 1
            o2 = min(max(o, self.start), self.end)
            e2 = min(max(o + n, self.start), self.end)
            if e2 <= o2:
                return None
            self.f.seek(o2)
            return self.f.read(e2 - o2)
        self.f.seek(o)
        return self.f.read(n)

    def inside(self, o, n):
        return self.start <= o and o + n <= self.end

    def close(self):
        self.f.close()
