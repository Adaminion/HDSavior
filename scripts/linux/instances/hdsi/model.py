"""Reference model of ALGORITHMS.md C, D, E (E1-E9 as far as placement needs them), F and G1/G2.

Independent of the engine.  Every step is written from the text of docs/ALGORITHMS.md; where the
text can be read in more than one way the reading is a parameter (Params) and the default is the
most literal one.  Rule changes proposed by the lab are also parameters, all off by default
(Params.fixes), so a run with defaults is "the rules as written".
"""
import bisect
import collections

from . import ntfsraw as nr
from . import sniff as sn

FT_DAY = nr.FT_DAY
MAIN, OTHER, OLDER, EMBEDDED, STRAYS = 'main', 'other', 'older', 'embedded', 'strays'
KIND_RANK = {MAIN: 0, OTHER: 1, OLDER: 2, STRAYS: 3}


class Params:
    def __init__(self, **kw):
        # thresholds (names follow the paragraphs)
        self.min_records = 16          # D5(3), D6
        self.ref_ratio = 0.90          # D4(a)
        self.content_ratio = 0.80      # D4(b)
        self.content_min = 3           # D4(b) and the content contradiction
        self.contra_ratio = 0.20       # D4 forbidden: references / content contradict
        self.contra_refs_min = 3       # D4 forbidden: references contradict
        self.mftmap_min = 16           # D4(c)
        self.copy_ratio = 0.90         # D6
        self.embed_ratio = 0.90        # D7
        self.embed_tol = 1.0           # D7, days
        self.cut_min_hi = 2            # D2 kept cut
        self.future_days = 1.0         # D2 timed
        self.sig_min_votes = 3         # F2 signature anchors
        self.sig_files = 64            # F2 signature anchors
        self.f3_candidates = 8         # F3
        self.f3_files = 200            # F3
        self.f4_min_checked = 20       # F4/F6
        self.f6_ratio = 0.80           # F6
        self.f6_half = 0.50            # F6
        self.f6_dirs = 3               # F6
        self.f6_mft = 16               # F6
        self.f5_rank = 4               # F5: 4th-largest
        # readings of ambiguous text
        self.a_dir_only = True         # D4(a) "parent references to directories": count a parent
                                       # reference only when the target on the other side is a directory
        self.ambiguity_scope = 'input'  # D4(a) "more than one scanned record could resolve": 'input' =
                                        # D's input (no live/post_format), 'all' = every stored record
        self.anchor_lowest = 'instance'  # D4 forbidden anchor rule: lowest old record at the origin
                                         # 'instance' (held by that instance) or 'origin' (all)
        self.copy_any_larger = True    # D6: a record may repeat one of ANY larger instance
        self.boot_readings = 'both'    # F2: 'both' readings, hidden-consistent one ranked first;
                                       # 'strict' = only the hidden-consistent reading when there is one
        self.lost_chain = 'attach'     # E6: named lost folder below a nameless lost parent ('attach')
                                       # or directly under [Lost folders] ('flat')
        self.g2_dirs = False           # G2: include directories' $I30 extents
        self.trace = False             # record margins (lab analysis only)
        self.strays_is_instance = True  # E6 'owner number not present in any instance': the strays
                                        # pseudo-instance counts as an instance
        self.seq0_wildcard = False     # reference rule: the text has no exception for sequence 0 (the
                                       # first model let s == 0 resolve to anything; True restores that)
        self.d2_fallback_frac = 0.25   # fix d2-fallback: at most this share of timed records out of order
        self.e6_floor_days = 1.0       # fix e6-floor: tolerance of the entry time floor
        self.d4c_tol_days = 1.0        # fix d4c-time: tolerance of the record-0 time test
        self.f2_mft_ext = True         # F2 MFT map through all $MFT extents, also those held in extension
                                       # records of record 0 (False: record 0's own extents, first model)
        self.oob_literal = True        # G1 out_of_bounds: 'a run with LCN >= total_clusters' (False: a run
                                       # reaching beyond total_clusters, first model)
        self.c5_embedded = True        # G1 overwritten from C5 also for embedded instances (False: skipped)
        self.time_basis = 'record'     # 'record': record time and entry time as ALGORITHMS.md defines them
                                       # (C3 C4 D2 D3 D7 D8 D9 E6 G2); 'si': the raw $SI MFT-change time and
                                       # the $FILE_NAME copy's MFT-change time (the rules before 2c63bdc)
        self.fixes = set()             # names of proposed rule changes to apply
        for k, v in kw.items():
            if not hasattr(self, k):
                raise KeyError(k)
            setattr(self, k, v)


class Geo:
    __slots__ = ('cs', 'off', 'source', 'dirs', 'mft_agree', 'checked', 'matched', 'validated',
                 'total_clusters', 'end_by_files', 'lvr', 'inherited', 'own_evidence', 'sig')

    def as_dict(self):
        d = {k: getattr(self, k) for k in self.__slots__}
        return d


class Group:
    """A file or directory: base record plus extension records (E1), merged extents (E2)."""
    __slots__ = ('base', 'exts', 'inst', 'num', 'data', 'ia', 'names', 'missing', 'sparse',
                 'compressed', 'encrypted', 'fixups_ok', 'parent', 'lost', 'flags', 'orphan', 'id',
                 'real', 'mapped')

    @property
    def in_use(self):
        return self.base.in_use

    @property
    def is_dir(self):
        return self.base.is_dir

    def rtime(self, basis):
        """Record time of the base record (the file's time in D7 and G2)."""
        t = self.base.rtime if basis == 'record' else self.base.chtime
        return t or 0

    def first_lcn(self):
        for svcn, runs in self.data:
            if svcn == 0 and runs and runs[0][0] is not None:
                return runs[0][0]
        return None


class Inst:
    def __init__(self, iid):
        self.id = iid
        self.segs = []
        self.recs = []
        self.nums = {}
        self.geo = None
        self.kind = None
        self.embedded_in = None      # (inst id, container group)
        self.stale_container = None
        self.alive = True
        self.groups = None
        self.label = None

    def add(self, recs):
        for r in recs:
            self.recs.append(r)
            if r.num is not None:
                self.nums[r.num] = r
            r.inst = self.id
        self.groups = None
        self._times = None
        self._extidx = None

    def times(self, timed, rt):
        if getattr(self, '_times', None) is None:
            ts = [rt(r) for r in self.recs if timed(r)]
            self._times = (min(ts), max(ts)) if ts else (None, None)
        return self._times


def merge_ranges(rs):
    rs = sorted(r for r in rs if r[1] > r[0])
    out = []
    for a, b in rs:
        if out and a <= out[-1][1]:
            if b > out[-1][1]:
                out[-1][1] = b
        else:
            out.append([a, b])
    return [tuple(x) for x in out]


class RangeSet:
    def __init__(self, ranges):
        self.r = merge_ranges(ranges)
        self.starts = [a for a, b in self.r]

    def intersects(self, a, b):
        i = bisect.bisect_right(self.starts, a) - 1
        if i >= 0 and self.r[i][1] > a:
            return True
        j = i + 1
        return j < len(self.r) and self.r[j][0] < b

    def contains(self, a, b):
        i = bisect.bisect_right(self.starts, a) - 1
        return i >= 0 and self.r[i][0] <= a and b <= self.r[i][1]


class CurrentVolume:
    pass


class Model:
    def resolves(self, s, in_use, seq):
        if s == 0 and not self.p.seq0_wildcard:
            return (seq == 0) if in_use else (seq == nr.next_seq(0))
        return nr.resolves(s, in_use, seq)

    def __init__(self, scan, reader, params=None, scan_time=None):
        self.s = scan
        self.rd = reader
        self.p = params or Params()
        self.fixes = self.p.fixes
        self.scan_time = scan_time
        self.start, self.end = scan.range
        self.source_size = scan.image_size
        self.warnings = set()
        self.log = []
        self.heads = {}
        self.cv = []                  # current volumes
        self.tr = collections.defaultdict(list)
        self.overwritten = RangeSet([])
        self.live_alloc = RangeSet([])

    # ------------------------------------------------------------------ helpers
    def rt(self, r):
        """Record time (or, with time_basis 'si', the raw $SI MFT-change time); None when there is none."""
        return r.rtime if self.p.time_basis == 'record' else r.chtime

    def et(self, e):
        """Entry time of an IndexEntry (or, with time_basis 'si', its $FILE_NAME MFT-change time)."""
        return e.etime if self.p.time_basis == 'record' else e.chtime

    def itimes(self, I):
        """(oldest, newest) record time of an instance's timed records."""
        return I.times(self.timed, self.rt)

    def timed(self, r):
        t = self.rt(r) or 0
        return bool(t) and t >= nr.FT_1980 and t <= self.scan_time + int(self.p.future_days * FT_DAY)

    def head(self, off):
        if off in self.heads:
            return self.heads[off]
        h = self.rd.read(off, sn.SNIFF_LEN)
        self.heads[off] = h
        return h

    # ================================================================== C. current volumes
    def c1_c2(self):
        cands = [self.start] + [p for p, ln in self.s.partitions if self.start <= p < self.end]
        seen = set()
        for S in cands:
            if S in seen:
                continue
            seen.add(S)
            b = self.rd.read(S, 512)
            if not b or len(b) < 512:
                continue
            bs = nr.parse_boot(b, S)
            if bs is None:
                continue
            mo = S + bs.mft_lcn * bs.cs
            raw = self.rd.read(mo, bs.rec_size)
            if not raw or len(raw) < bs.rec_size:
                continue
            r0 = nr.parse_record(raw, mo, want_bitmap=True)
            if isinstance(r0, str) or not r0.in_use or r0.num not in (0, None) or \
                    not any(fn.name == '$MFT' for fn in r0.names):
                continue
            cv = CurrentVolume()
            cv.off, cv.bs, cv.cs, cv.rs = S, bs, bs.cs, bs.rec_size
            cv.incomplete = False
            self._snapshot(cv, r0)
            self.cv.append(cv)
            self.log.append('C1 current volume at %d (cluster %d, $MFT at %d, init %d, format_time %s)'
                            % (S, cv.cs, mo, cv.init, cv.format_time))
        if not self.cv:
            # a boot sector at the range start or a partition start that is not a readable NTFS
            for S in cands:
                b = self.rd.read(S, 512)
                if b and len(b) == 512 and b[3:11] == b'NTFS    ':
                    self.warnings.add('writes_not_detectable')
        alloc = []
        for cv in self.cv:
            alloc += cv.alloc
        self.live_alloc = RangeSet(alloc)

    def _read_attr(self, cv, runs, size):
        buf = bytearray()
        for lcn, ln in runs:
            if lcn is None:
                buf += bytes(ln * cv.cs)
            else:
                b = self.rd.read(cv.off + lcn * cv.cs, ln * cv.cs)
                if b is None or len(b) < ln * cv.cs:
                    cv.incomplete = True
                    b = (b or b'') + bytes(ln * cv.cs - len(b or b''))
                buf += b
        return bytes(buf[:size])

    def _snapshot(self, cv, r0):
        cs, rs = cv.cs, cv.rs
        exts = [(d.svcn, d.runs) for d in r0.data if not d.resident]
        init = [d.init for d in r0.data if not d.resident and d.svcn == 0]
        cv.init = init[0] if init else 0
        segs = self._segs(exts)
        if r0.has_attrlist:
            ents = []
            al = r0.attrlist
            if isinstance(al, tuple):
                buf = self._read_attr(cv, al[1], al[2])
                q = 0
                import struct
                while q + 0x1A <= len(buf):
                    at, el = struct.unpack_from('<IH', buf, q)
                    if el < 0x1A:
                        break
                    svcn = struct.unpack_from('<q', buf, q + 8)[0]
                    ref = struct.unpack_from('<Q', buf, q + 0x10)[0]
                    if at == 0x80:
                        ents.append(ref & 0xFFFFFFFFFFFF)
                    q += el
            elif isinstance(al, list):
                ents = [n for (t, s, n, q) in al if t == 0x80]
            for n in set(ents) - {0}:
                p = self._map(segs, n * rs, cs)
                if p is None:
                    cv.incomplete = True
                    continue
                raw = self.rd.read(cv.off + p, rs)
                rx = nr.parse_record(raw or b'', cv.off + p)
                if isinstance(rx, str):
                    cv.incomplete = True
                    continue
                exts += [(d.svcn, d.runs) for d in rx.data if not d.resident]
            segs = self._segs(exts)
        cv.mft_segs = segs
        # absolute byte extents of $MFT:$DATA below the initialized size
        cv.mft_ranges = []
        for a, b, lcn in segs:
            if lcn is None:
                continue
            lo, hi = a * cs, min(b * cs, cv.init)
            if hi > lo:
                cv.mft_ranges.append((cv.off + lcn * cs, cv.off + lcn * cs + (hi - lo), lo))
        cv.mft_origin = cv.off + cv.bs.mft_lcn * cs
        bm = r0.bitmap_attr
        if bm is None:
            cv.mbits = b''
            cv.incomplete = True
        elif bm[0] == 'res':
            cv.mbits = bm[1]
        else:
            cv.mbits = self._read_attr(cv, bm[1], bm[2])

        def rec(n):
            p = self._map(segs, n * rs, cs)
            if p is None:
                return None
            raw = self.rd.read(cv.off + p, rs)
            r = nr.parse_record(raw or b'', cv.off + p)
            return None if isinstance(r, str) else r
        r3 = rec(3)
        if r3 is not None and r3.si:
            cv.format_time = r3.si[0]
        else:
            cv.format_time = r0.si[0] if r0.si else 0
        r6 = rec(6)
        cv.alloc = []
        cv.total_clusters = cv.bs.total_sectors * cv.bs.bps // cs
        if r6 is None:
            cv.incomplete = True
        else:
            for d in r6.data:
                if d.resident:
                    continue
                bits = self._read_attr(cv, d.runs, (cv.total_clusters + 7) // 8)
                start = None
                for i in range(cv.total_clusters):
                    on = bits[i >> 3] >> (i & 7) & 1 if (i >> 3) < len(bits) else 0
                    if on and start is None:
                        start = i
                    elif not on and start is not None:
                        cv.alloc.append((cv.off + start * cs, cv.off + i * cs))
                        start = None
                if start is not None:
                    cv.alloc.append((cv.off + start * cs, cv.off + cv.total_clusters * cs))
                break
        r1 = rec(1)
        cv.mirr_ranges = []
        if r1 is not None:
            for d in r1.data:
                if not d.resident:
                    for lcn, ln in d.runs:
                        if lcn is not None:
                            cv.mirr_ranges.append((cv.off + lcn * cs, cv.off + (lcn + ln) * cs))
        if cv.incomplete:
            self.warnings.add('live_volume_incomplete')

    @staticmethod
    def _segs(exts):
        segs = []
        for svcn, runs in sorted(exts, key=lambda e: e[0]):
            v = svcn
            for lcn, ln in runs:
                segs.append((v, v + ln, lcn))
                v += ln
        return segs

    @staticmethod
    def _map(segs, pos, cs):
        vcn, within = divmod(pos, cs)
        for a, b, lcn in segs:
            if a <= vcn < b:
                return None if lcn is None else (lcn + vcn - a) * cs + within
        return None

    def c3(self):
        recs = self.s.records
        for r in recs:
            r.cls = 'old'
            r.older_gen = False
        for cv in self.cv:
            cv.slot_rec = {}
            for r in recs:
                for a, b, vpos in cv.mft_ranges:
                    if a <= r.off < b:
                        pos = vpos + (r.off - a)
                        if pos % cv.rs:
                            break
                        n = pos // cv.rs
                        cv.slot_rec[n] = r
                        r.cls = ('cv', cv, n)
                        break
            for n, r in list(cv.slot_rec.items()):
                used = (n >> 3) < len(cv.mbits) and (cv.mbits[n >> 3] >> (n & 7) & 1)
                if used:
                    r.cls = 'live'
                elif r.si is not None:
                    r.cls = 'post_format' if (self.rt(r) or 0) >= cv.format_time else 'old'
            for n, r in list(cv.slot_rec.items()):
                if isinstance(r.cls, tuple):
                    if r.in_use:
                        r.cls = 'old'
                    else:
                        b = cv.slot_rec.get(r.base)
                        ok = b is not None and b.cls == 'old' and self.resolves(r.base_seq, b.in_use, b.seq)
                        r.cls = 'old' if ok else 'post_format'
            for r in recs:
                if any(a <= r.off < b for a, b in cv.mirr_ranges):
                    r.cls = 'live'
        self.counts = collections.Counter(r.cls for r in recs)

    def c4_c5(self):
        cur_recs = {r.off for r in self.s.records if r.cls in ('live', 'post_format')}
        for b in self.s.blocks:
            b.current = self.live_alloc.contains(b.off, b.off + b.size) or \
                self.live_alloc.intersects(b.off, b.off + 1)
        mints = min((cv.format_time for cv in self.cv), default=None)

        def ftime_for(off):
            for cv in self.cv:
                if cv.off <= off < cv.off + cv.total_clusters * cv.cs:
                    return cv.format_time
            return mints
        for e in self.s.entries:
            cur = False
            if e.src_kind == 'block' and e.block.current:
                cur = True
            elif e.src_kind == 'root' and e.src_off in cur_recs:
                cur = True
            else:
                ft = ftime_for(e.src_off)
                if ft is not None and self.et(e) >= ft:
                    cur = True
            e.current = cur
        ow = list(self.live_alloc.r)
        for r in self.s.records:
            if r.cls == 'post_format':
                cv = [c for c in self.cv if any(a <= r.off < b for a, b, _ in c.mft_ranges)][0]
                for d in r.data:
                    if d.resident:
                        continue
                    for lcn, ln in d.runs:
                        if lcn is not None:
                            ow.append((cv.off + lcn * cv.cs, cv.off + (lcn + ln) * cv.cs))
        self.overwritten = RangeSet(ow)

    # ================================================================== D. instances
    def d1(self):
        self.input = [r for r in self.s.records if r.cls == 'old']
        frags = collections.defaultdict(list)
        unnumbered = []
        for r in self.input:
            if r.num is None:
                unnumbered.append(r)
            else:
                frags[(r.size, r.off - r.num * r.size)].append(r)
        self.frags = {}
        for i, (k, rs) in enumerate(sorted(frags.items(), key=lambda kv: (-len(kv[1]), kv[0]))):
            rs.sort(key=lambda r: r.num)
            self.frags[i] = {'key': k, 'recs': rs, 'origin': k[1], 'size': k[0]}
            for r in rs:
                r.frag = i
        self.stray_recs = []
        for r in unnumbered:
            placed = False
            for fi, f in self.frags.items():
                if f['size'] != r.size:
                    continue
                d = r.off - f['origin']
                if d % r.size:
                    continue
                n = d // r.size
                lo, hi = f['recs'][0].num, f['recs'][-1].num
                if lo - 4096 <= n <= hi + 4096 and all(x.num != n for x in f['recs']):
                    r.num = n
                    r.frag = fi
                    f['recs'].append(r)
                    f['recs'].sort(key=lambda x: x.num)
                    placed = True
                    break
            if not placed:
                self.stray_recs.append(r)
        self.log.append('D1 %d fragments, %d unnumbered strays' % (len(self.frags), len(self.stray_recs)))

    def d2_d3(self):
        self.cuts = []                      # kept cuts: dict(frag, p, B, A, below, above)
        self.segments = {}                  # seg id -> dict(frag, index, recs)
        self.contradictions = []
        sid = 0
        for fi, f in self.frags.items():
            recs = f['recs']
            by_num = {r.num: r for r in recs}
            # changepoints: candidate p = every record number present
            timed = [(r.num, self.rt(r)) for r in recs if self.timed(r)]
            nums = [r.num for r in recs]
            # prefix min of times (numbers < p) and suffix max (numbers >= p)
            tn = sorted(timed)
            pref_min = []
            m = None
            for n, t in tn:
                m = t if m is None else min(m, t)
                pref_min.append(m)
            suf_max = [None] * len(tn)
            m = None
            for i in range(len(tn) - 1, -1, -1):
                m = tn[i][1] if m is None else max(m, tn[i][1])
                suf_max[i] = m
            tnums = [n for n, t in tn]
            cps = []
            for p in nums:
                k = bisect.bisect_left(tnums, p)        # timed records < p: [0, k)
                if k == 0 or k == len(tn):
                    continue
                if suf_max[k] < pref_min[k - 1]:
                    cps.append(p)
            # contradictions
            pairs = {}

            def add(a, b):
                lo, hi = min(a, b), max(a, b)
                if lo == hi:
                    return
                if hi not in pairs or pairs[hi] < lo:
                    pairs[hi] = lo
            cand = [r for r in recs if r.in_use and r.fixup_ok]
            # (A) shared cluster numbers
            ivs = []
            for r in cand:
                for d in r.data + r.ia:
                    if d.resident:
                        continue
                    for lcn, ln in d.runs:
                        if lcn is not None:
                            ivs.append((lcn, lcn + ln, r.num))
            ivs.sort()
            active = []
            for a, b, n in ivs:
                active = [x for x in active if x[1] > a]
                for x in active:
                    if x[2] != n:
                        add(x[2], n)
                active.append((a, b, n))
            # (B) reference contradictions
            for r in cand:
                refs = [(fn.parent, fn.pseq, 'p') for fn in r.names]
                if r.base:
                    refs.append((r.base, r.base_seq, 'b'))
                for n, s, kind in refs:
                    q = by_num.get(n)
                    if q is None or q is r or not (q.in_use and q.fixup_ok):
                        continue
                    seq_bad = (s != 0 and q.seq != s) if self.p.seq0_wildcard else q.seq != s
                    bad = seq_bad or (kind == 'p' and not q.is_dir)
                    if bad:
                        add(r.num, n)
            contra = sorted((lo, hi) for hi, lo in pairs.items())
            contra.sort(key=lambda x: x[1])
            chosen = []
            cut_of = {}
            for lo, hi in contra:
                if any(lo < c <= hi for c in chosen):
                    continue
                inside = [p for p in cps if lo < p <= hi]
                if inside:
                    chosen.append(max(inside))
                elif 'd2-fallback' in self.fixes:
                    c = self.robust_cut(recs, lo, hi)
                    if c is not None:
                        chosen.append(c)
            kept = []
            for c in chosen:
                his = {hi for lo, hi in contra if lo < c <= hi}
                if len(his) >= self.p.cut_min_hi:
                    kept.append(c)
            kept.sort()
            unexplained = [(lo, hi) for lo, hi in contra if not any(lo < c <= hi for c in kept)]
            for lo, hi in unexplained:
                by_num[hi].older_gen = True
                self.warnings.add('mixed_generations')
            self.contradictions.append((fi, contra, chosen, kept, unexplained, cps))
            # segments
            bounds = [None] + kept + [None]
            segids = []
            for k in range(len(kept) + 1):
                lo_b, hi_b = bounds[k], bounds[k + 1]
                srecs = [r for r in recs if (lo_b is None or r.num >= lo_b) and (hi_b is None or r.num < hi_b)]
                self.segments[sid] = {'frag': fi, 'index': k, 'recs': srecs, 'id': sid}
                for r in srecs:
                    r.seg = sid
                segids.append(sid)
                sid += 1
            for k, c in enumerate(kept):
                below, above = segids[k], segids[k + 1]
                tb = [self.rt(r) for r in self.segments[above]['recs'] if self.timed(r)]
                ta = [self.rt(r) for r in self.segments[below]['recs'] if self.timed(r)]
                if 'd2-fallback' in self.fixes and tb and any(t <= max(tb) for t in ta):
                    # D3 for a fallback cut: A_c from the records below that are newer than B_c
                    ta = [t for t in ta if t > max(tb)] or ta
                self.cuts.append({'frag': fi, 'p': c, 'B': max(tb) if tb else None,
                                  'A': min(ta) if ta else None, 'below': below, 'above': above})
            f['segs'] = segids
        n_contra = sum(len(c[1]) for c in self.contradictions)
        self.log.append('D2 %d contradictions, %d kept cuts %s' % (
            n_contra, len(self.cuts), [(c['frag'], c['p']) for c in self.cuts]))

    def robust_cut(self, recs, lo, hi):
        """fix d2-fallback: no changepoint in (lo, hi]: the record number p in (lo, hi] with the fewest
        timed records out of order for the best time T (numbered < p and not newer than T, or numbered
        >= p and newer than T); on ties the highest p."""
        timed = sorted((r.num, self.rt(r)) for r in recs if self.timed(r))
        cand_p = sorted({r.num for r in recs if lo < r.num <= hi})
        if not cand_p or not timed:
            return None
        best = None
        for p in cand_p:
            ev = sorted([(t, 0) for n, t in timed if n < p] + [(t, 1) for n, t in timed if n >= p])
            # T below every time: all 'above' records count as newer than T
            k = sum(1 for t, side in ev if side == 1)
            sc = k
            i = 0
            while i < len(ev):
                t = ev[i][0]
                while i < len(ev) and ev[i][0] == t:      # move T to t: records at t become <= T
                    k += 1 if ev[i][1] == 0 else -1
                    i += 1
                sc = min(sc, k)
            key = (sc, -p)
            if best is None or key < best[0]:
                best = (key, p)
        if best[0][0] > self.p.d2_fallback_frac * len(timed):
            return None
        return best[1]

    def rel_to_cut(self, times, c):
        """D3: 'before', 'after' or 'undecided' for a set of times (oldest, newest)."""
        old, new = times
        if old is None or c['B'] is None:
            return 'undecided'
        if 'd3-before-A' in self.fixes:
            if c['A'] is not None and new < c['A']:
                return 'before'
        elif new <= c['B']:
            return 'before'
        if old > c['B'] and c['A'] is not None and new >= c['A']:
            return 'after'
        return 'undecided'

    # ------------------------------------------------------------------ joining
    def init_instances(self):
        self.insts = {}
        for sid, sg in self.segments.items():
            I = Inst(sid)
            I.segs = [sid]
            I.add(sg['recs'])
            self.insts[sid] = I
        # anchor fragments: old records at the origin of a current $MFT
        self.anchor_frags = []
        for cv in self.cv:
            for fi, f in self.frags.items():
                if f['origin'] == cv.mft_origin and f['size'] == cv.rs:
                    self.anchor_frags.append((fi, cv))
        # references
        by_num = collections.defaultdict(list)
        scope = self.input if self.p.ambiguity_scope == 'input' else self.s.records
        for r in scope:
            if r.num is not None:
                by_num[r.num].append(r)
        in_by_num = collections.defaultdict(list)
        for r in self.input:
            if r.num is not None:
                in_by_num[r.num].append(r)
        self.refs = []                      # (src record, N, s, kind, target candidates)
        for r in self.input:
            seen = set()
            items = [(fn.parent, fn.pseq, 'p') for fn in r.names]
            if r.base:
                items.append((r.base, r.base_seq, 'b'))
            for n, s, kind in items:
                if (n, s, kind) in seen or n in (5, 11):
                    continue
                seen.add((n, s, kind))
                res = {x.hash for x in by_num.get(n, ()) if x is not r and self.resolves(s, x.in_use, x.seq)}
                if len(res) > 1:
                    continue              # more than one scanned record could resolve it
                tg = [x for x in in_by_num.get(n, ()) if x is not r]
                if tg:
                    self.refs.append((r, n, s, kind, tg))

    def anchor_insts(self):
        out = []
        for fi, cv in self.anchor_frags:
            sid0 = self.frags[fi]['segs'][0]
            I = self.insts[self.seg_owner(sid0)]
            out.append((I, cv))
        return out

    def seg_owner(self, sid):
        return self.segments[sid]['recs'][0].inst if self.segments[sid]['recs'] else None

    def pair_refs(self):
        """D4(a): (counted, resolved) per unordered instance pair."""
        st = collections.defaultdict(lambda: [0, 0])
        for r, n, s, kind, tg in self.refs:
            I = r.inst
            for t in tg:
                J = t.inst
                if J == I or J is None:
                    continue
                if not self.insts[J].alive:
                    continue
                tt = self.insts[J].nums.get(n)
                if tt is not t:
                    continue
                if kind == 'p' and self.p.a_dir_only and not tt.is_dir:
                    continue
                k = (min(I, J), max(I, J))
                st[k][0] += 1
                st[k][1] += 1 if self.resolves(s, tt.in_use, tt.seq) else 0
        return st

    def mft_map_evidence(self, X, J, self_map=False):
        """D4(c): records of J placed by X's record 0 ($MFT) with one volume offset."""
        r0 = X.nums.get(0)
        if r0 is None or not any(fn.name == '$MFT' for fn in r0.names):
            return 0, None
        cs = self.f1_cs(X)
        exts = [(d.svcn, d.runs) for d in r0.data if not d.resident]
        for r in X.recs:
            if r.base == 0 and r is not r0 and r.num is not None and self.resolves(r.base_seq, r0.in_use, r0.seq) \
                    and r.base_seq and r.num != 0:
                exts += [(d.svcn, d.runs) for d in r.data if not d.resident]
        segs = self._segs(exts)
        first = None
        for a, b, lcn in segs:
            if a == 0:
                first = (a, b)
        cnt = collections.Counter()
        oldest_at = {}
        for t in J.recs:
            if t.num is None:
                continue
            if 'd4c-not-first' in self.fixes and first is not None:
                vcn = t.num * t.size // cs
                if first[0] <= vcn < first[1]:
                    continue
            B = self._map(segs, t.num * t.size, cs)
            if B is None:
                continue
            cnt[t.off - B] += 1
            if self.timed(t):
                o = t.off - B
                oldest_at[o] = min(oldest_at.get(o, self.rt(t)), self.rt(t))
        if 'd4c-size' in self.fixes and not self_map:
            # the record 0 must describe a $MFT that holds every record number of the other side
            init = [d.init for d in r0.data if not d.resident and d.svcn == 0]
            rsz = r0.size
            if not init or any(t.num is not None and t.num * rsz >= init[0] for t in J.recs):
                return 0, None
        if 'd4c-time' in self.fixes and not self_map:
            # the record 0 must not be older than every record it places (a $MFT record of a file system
            # that ended before these records were written cannot describe them)
            t0 = self.rt(r0) if r0.si and self.timed(r0) else None
            tol = int(self.p.d4c_tol_days * FT_DAY)
            for o in list(cnt):
                if t0 is not None and o in oldest_at and t0 < oldest_at[o] - tol:
                    del cnt[o]
        if not cnt:
            return 0, None
        if 'd4c-self' in self.fixes and not self_map:
            # the offset must be the one at which the record-0 side itself lies: record 0 at the start
            # of its own $MFT, or (a copy) at the start of the $MFTMirr named by the side's record 1
            own = set()
            b0 = self._map(segs, 0, cs)
            if b0 is not None:
                own.add(r0.off - b0)
            r1 = X.nums.get(1)
            if r1 is not None:
                for d in r1.data:
                    if not d.resident and d.svcn == 0 and d.runs and d.runs[0][0] is not None:
                        own.add(r0.off - d.runs[0][0] * cs)
            n = max((cnt.get(o, 0) for o in own), default=0)
            return (n, max(own, key=lambda o: cnt.get(o, 0))) if n else (0, None)
        off, n = cnt.most_common(1)[0]
        return n, off

    def forbidden(self, I, J, step):
        if set(I.nums) & set(J.nums):
            return 'numbers'
        for c in self.cuts:
            hb = self.seg_owner(c['below'])
            ha = self.seg_owner(c['above'])
            for X, Y in ((I, J), (J, I)):
                if X.id == hb and Y.id == ha:
                    return 'cut'
                if X.id == hb and self.rel_to_cut(self.itimes(Y), c) != 'after':
                    return 'cut'
                if X.id == ha:
                    rel = self.rel_to_cut(self.itimes(Y), c)
                    if ('d4-older-side' in self.fixes and rel == 'after') or \
                            ('d4-older-side' not in self.fixes and rel != 'before'):
                        return 'cut'
        for X, Y in ((I, J), (J, I)):
            for fi, cv in self.anchor_frags:
                at = [r.num for r in X.recs if r.frag == fi]
                if not at:
                    continue
                if self.p.anchor_lowest == 'instance':
                    low = min(at)
                else:
                    low = min(r.num for r in self.frags[fi]['recs'])
                if any(n < low for n in Y.nums):
                    if self.mft_map_evidence(X, Y)[0] < self.p.mftmap_min and \
                            self.mft_map_evidence(Y, X)[0] < self.p.mftmap_min:
                        return 'anchor'
        if step == 4:
            if (I.embedded_in and I.embedded_in[0] == J.id) or (J.embedded_in and J.embedded_in[0] == I.id):
                return 'embedded'
            for X, Y in ((I, J), (J, I)):
                if X.geo and X.geo.validated:
                    chk, mt = self.content_check(X.geo, Y)
                    if chk >= self.p.content_min and mt < self.p.contra_ratio * chk:
                        return 'content'
        return None

    def evidence(self, I, J, step, refst):
        """Best evidence (rank tuple) or None."""
        k = (min(I.id, J.id), max(I.id, J.id))
        cnt, res = refst.get(k, (0, 0))
        if cnt >= self.p.contra_refs_min and res < self.p.contra_ratio * cnt:
            return None, 'refs-contradict'
        best = None
        if step == 2:
            n1, _ = self.mft_map_evidence(I, J)
            n2, _ = self.mft_map_evidence(J, I)
            if max(n1, n2) >= self.p.mftmap_min:
                best = (3, max(n1, n2))
            elif res >= 1 and res >= self.p.ref_ratio * cnt:
                best = (2, res)
        else:
            for X, Y in ((I, J), (J, I)):
                if X.geo and X.geo.validated:
                    chk, mt = self.content_check(X.geo, Y)
                    if chk >= self.p.content_min and mt >= self.p.content_ratio * chk:
                        cand = (1, mt / chk)
                        if best is None or cand > best:
                            best = cand
            if best is None and I.geo and J.geo and I.geo.validated and J.geo.validated and \
                    (I.geo.cs, I.geo.off) == (J.geo.cs, J.geo.off):
                best = (0, 0)
        return best, None

    def join(self, step):
        merged_any = False
        while True:
            alive = [I for I in self.insts.values() if I.alive]
            refst = self.pair_refs()
            cands = []
            if step == 2:
                pairs = set(refst.keys())
                holders = [I for I in alive if 0 in I.nums]
                for X in holders:
                    for Y in alive:
                        if Y is not X:
                            pairs.add((min(X.id, Y.id), max(X.id, Y.id)))
            else:
                val = [I for I in alive if I.geo and I.geo.validated]
                pairs = set()
                for X in val:
                    for Y in alive:
                        if Y is not X:
                            pairs.add((min(X.id, Y.id), max(X.id, Y.id)))
            for a, b in pairs:
                I, J = self.insts[a], self.insts[b]
                if not (I.alive and J.alive):
                    continue
                ev, why = self.evidence(I, J, step, refst)
                fb = self.forbidden(I, J, step) if ev is not None else None
                if self.p.trace:
                    cnt, res = refst.get((a, b), (0, 0))
                    self.tr['join'].append((step, a, b, len(I.recs), len(J.recs), cnt, res, ev, fb, why))
                if ev is None or fb:
                    continue
                cands.append((ev, -a, -b))
            if not cands:
                return merged_any
            ev, a, b = max(cands)
            self.merge(self.insts[-a], self.insts[-b])
            merged_any = True
            self.log.append('D4 step %d merged %d+%d by %s' % (step, -a, -b, ev))

    def merge(self, I, J):
        if len(J.recs) > len(I.recs):
            I, J = J, I
        I.segs += J.segs
        I.add(J.recs)
        J.alive = False
        J.recs = []
        J.nums = {}
        I.geo = None
        I.embedded_in = None
        return I

    # ------------------------------------------------------------------ geometry (F)
    def groups_of(self, I):
        if I.groups is not None:
            return I.groups
        bases = {}
        exts = []
        for r in I.recs:
            if r.base == 0:
                if r.num is not None:
                    bases[r.num] = r
            else:
                exts.append(r)
        groups = {}
        for n, r in bases.items():
            g = Group()
            g.base, g.exts, g.inst, g.num = r, [], I.id, n
            g.orphan = False
            groups[n] = g
        orphans = collections.defaultdict(list)
        for x in exts:
            b = bases.get(x.base)
            ok = False
            if b is not None:
                if x.in_use and b.in_use and b.seq == x.base_seq:
                    ok = True
                elif not x.in_use and not b.in_use and b.has_attrs and b.seq in (x.base_seq, nr.next_seq(x.base_seq)):
                    ok = True
                elif not x.in_use and b.in_use:
                    continue          # leftover
            if ok:
                groups[x.base].exts.append(x)
            else:
                orphans[(x.base, x.base_seq)].append(x)
        out = list(groups.values())
        for (bn, bs), xs in orphans.items():
            g = Group()
            g.base, g.exts, g.inst, g.num = xs[0], xs[1:], I.id, bn
            g.orphan = True
            out.append(g)
        for g in out:
            self._merge_extents(g)
        I.groups = out
        return out

    def _merge_extents(self, g):
        recs = [g.base] + g.exts
        names = []
        data = []
        ia = []
        for r in recs:
            names += r.names
            for d in r.data:
                if not d.resident:
                    data.append(d)
            ia += r.ia
        g.names = names
        g.fixups_ok = all(r.fixup_ok for r in recs)
        g.compressed = any((d.aflags & 0x00FF) for d in data) or g.base.wof or g.base.reparse == 0x80000017
        g.encrypted = any(d.aflags & 0x4000 for d in data)
        g.sparse = any(d.aflags & 0x8000 for d in data) or any(lcn is None for d in data for lcn, ln in d.runs)
        g.missing = False
        data.sort(key=lambda d: d.svcn)
        exp = 0
        for d in data:
            if d.svcn != exp:
                g.missing = True
            exp = d.svcn + sum(ln for lcn, ln in d.runs)
        v0 = [d for d in data if d.svcn == 0]
        if data and not v0:
            g.missing = True
        g.data = [(d.svcn, d.runs) for d in data]
        g.ia = [(d.svcn, d.runs) for d in sorted(ia, key=lambda d: d.svcn)]
        g.flags = set()
        g.parent = None
        g.lost = None
        g.real = v0[0].real if v0 else None
        g.mapped = sum(ln for d in data for lcn, ln in d.runs)

    def f1_cs(self, I):
        cnt = collections.Counter()
        for r in I.recs:
            for d in r.data + r.ia:
                if d.resident or d.svcn != 0 or d.evcn < 0:
                    continue
                if d.alloc and d.alloc % (d.evcn + 1) == 0:
                    c = d.alloc // (d.evcn + 1)
                    if 512 <= c <= (2 << 20) and c & (c - 1) == 0:
                        cnt[c] += 1
        if cnt:
            return max(cnt.items(), key=lambda kv: (kv[1], -kv[0]))[0]
        return None

    def checkable(self, g):
        if not (g.in_use and not g.is_dir and g.data and not g.sparse and not g.compressed and
                not g.encrypted and g.fixups_ok):
            return False
        n = g.base.best_name()
        if n is None:
            return False
        return sn.has_opinion(sn.ext_of(n.name))

    def content_check(self, geo, Y, limit=None):
        """F3 over Y's checkable files under geometry geo: (checked, matched)."""
        files = [g for g in self.groups_of(Y) if self.checkable(g)]
        files.sort(key=lambda g: g.num)
        limit = limit or self.p.f3_files
        if len(files) > limit:
            step = len(files) / limit
            files = [files[int(i * step)] for i in range(limit)]
        chk = mt = 0
        for g in files:
            l = g.first_lcn()
            if l is None:
                continue
            o = geo.off + l * geo.cs
            if not self.rd.inside(o, 512) or self.overwritten.intersects(o, o + geo.cs):
                continue
            h = self.head(o)
            if h is None:
                continue
            m = sn.matches_extension(h, sn.ext_of(g.base.best_name().name))
            if m is None:
                continue
            chk += 1
            mt += 1 if m else 0
        return chk, mt

    def geometry(self, I, main_geo=None):
        p = self.p
        cs = self.f1_cs(I)
        src_cs = 'attr'
        if cs is None:
            bcs = [b.cs for b in self.s.boots]
            cs = bcs[0] if bcs else 4096
            src_cs = 'boot' if bcs else 'default'
        groups = self.groups_of(I)
        cands = {}                         # (cs, off) -> info

        def cand(off, source, **kw):
            if off < 0 or off % 512 or off >= self.source_size:
                return None
            k = (cs, off)
            c = cands.setdefault(k, {'sources': set(), 'dirs': 0, 'mft_agree': 0, 'sig': 0,
                                     'boot_pref': False})
            c['sources'].add(source)
            for kk, v in kw.items():
                if kk in ('dirs', 'mft_agree', 'sig'):
                    c[kk] = max(c[kk], v)
                else:
                    c[kk] = v or c[kk]
            return c
        # MFT map
        r0 = I.nums.get(0)
        if r0 is not None and any(fn.name == '$MFT' for fn in r0.names):
            exts = [(d.svcn, d.runs) for d in r0.data if not d.resident]
            if self.p.f2_mft_ext:
                for r in I.recs:
                    if r.base == 0 and r is not r0 and r.num not in (None, 0) and r.base_seq and \
                            self.resolves(r.base_seq, r0.in_use, r0.seq):
                        exts += [(d.svcn, d.runs) for d in r.data if not d.resident]
            segs = self._segs(exts)
            cnt = collections.Counter()
            for r in I.recs:
                if r.num is None:
                    continue
                B = self._map(segs, r.num * r.size, cs)
                if B is not None:
                    cnt[r.off - B] += 1
            if cnt:
                off, n = cnt.most_common(1)[0]
                b0 = self._map(segs, 0, cs)
                self_agrees = b0 is not None and r0.off - b0 == off
                cand(off, 'mft', mft_agree=n - (1 if self_agrees else 0))
        # index votes, per distinct directory
        dirs = {g.num: g for g in groups if g.is_dir and not g.orphan}
        votes = collections.defaultdict(set)
        for b in self.s.blocks:
            if b.current or b.owner is None:
                continue
            g = dirs.get(b.owner)
            if g is None or not g.ia or not self.resolves(b.oseq, g.base.in_use, g.base.seq):
                continue
            pos = b.vcn * cs if cs <= b.size else b.vcn * 512
            segs = self._segs(g.ia)
            B = self._map(segs, pos, cs)
            if B is None:
                continue
            votes[b.off - B].add(g.num)
        for off, ds in votes.items():
            cand(off, 'index', dirs=len(ds))
        # boot sectors
        for bh in self.s.boots:
            if bh.cs != cs:
                continue
            prim, back = bh.off, bh.off - bh.total_sectors * bh.bps
            pc = bh.hidden * bh.bps == prim
            bc = bh.hidden * bh.bps == back
            if p.boot_readings == 'strict' and pc != bc:
                if pc:
                    cand(prim, 'boot', boot_pref=True)
                else:
                    cand(back, 'backup_boot', boot_pref=True)
            else:
                cand(prim, 'boot', boot_pref=pc and not bc)
                cand(back, 'backup_boot', boot_pref=bc and not pc)
        # partition starts and the range start
        for o in [self.start] + [ps for ps, ln in self.s.partitions if self.start <= ps < self.end]:
            cand(o, 'default')
        # signature anchors, only when there are no index votes
        if not votes:
            files = [g for g in groups if g.in_use and not g.is_dir and g.first_lcn() is not None and
                     g.base.best_name() and sn.kinds_for_extension(sn.ext_of(g.base.best_name().name))]
            files.sort(key=lambda g: g.num)
            if len(files) > p.sig_files:
                st = len(files) / p.sig_files
                files = [files[int(i * st)] for i in range(p.sig_files)]
            byk = collections.defaultdict(list)
            for o, k in self.s.carve:
                byk[k].append(o)
            sv = collections.Counter()
            for g in files:
                kinds = sn.kinds_for_extension(sn.ext_of(g.base.best_name().name))
                l = g.first_lcn()
                offs = set()
                for k in kinds:
                    for o in byk.get(k, ()):
                        offs.add(o - l * cs)
                for o in offs:
                    sv[o] += 1
            for o, n in sv.items():
                if n >= p.sig_min_votes:
                    cand(o, 'signature', sig=n)
        if not cands:
            return None
        order = {'mft': 0, 'boot': 1, 'backup_boot': 2, 'default': 3, 'index': 4, 'signature': 5}

        def srank(c):
            return min(order[s] for s in c['sources'])

        def prerank(item):
            k, c = item
            return (-c['dirs'], -c['sig'], 0 if c['boot_pref'] else 1, srank(c), -c['mft_agree'], k[1])
        best = sorted(cands.items(), key=prerank)[:p.f3_candidates]
        scored = []
        for k, c in best:
            g = Geo()
            g.cs, g.off = k
            g.checked, g.matched = self.content_check(g, I)
            scored.append((k, c, g))

        def frank(item):
            k, c, g = item
            sigkey = g.matched if g.checked >= p.f4_min_checked else -1
            return (-sigkey, -c['dirs'], 0 if c['boot_pref'] else 1, srank(c), k[1])
        k, c, g = sorted(scored, key=frank)[0]
        if g.checked >= p.f4_min_checked and c['sig'] and 'signature' in c['sources'] and not c['dirs']:
            g.source = 'signature'
        elif c['dirs']:
            g.source = 'index'
        else:
            g.source = sorted(c['sources'], key=lambda s: order[s])[0]
        g.dirs = c['dirs']
        g.sig = c['sig']
        g.mft_agree = c['mft_agree']
        structural = c['dirs'] >= p.f6_dirs or c['mft_agree'] >= p.f6_mft
        if g.checked >= p.f4_min_checked:
            g.validated = g.matched >= p.f6_ratio * g.checked or \
                (g.matched >= p.f6_half * g.checked and structural)
        else:
            g.validated = structural
        g.inherited = False
        self.f5(I, g)
        if self.p.trace:
            self.tr['geo'].append((I.id, len(I.recs), g.cs, g.off, g.source, g.checked, g.matched, g.dirs,
                                   g.mft_agree, bool(g.validated), [(k[1], c2['dirs'], g2.checked, g2.matched)
                                                                    for k, c2, g2 in scored]))
        g.own_evidence = bool(c['dirs'] or c['mft_agree'] or c['sig'] or g.matched)
        return g

    def f5(self, I, g):
        ends = []
        for gr in self.groups_of(I):
            if not gr.in_use or not gr.fixups_ok or gr.missing or not gr.data:
                continue
            hi = 0
            for svcn, runs in gr.data:
                for lcn, ln in runs:
                    if lcn is not None:
                        hi = max(hi, lcn + ln)
            if hi:
                ends.append(hi)
        ends.sort(reverse=True)
        ebf = ends[self.p.f5_rank - 1] if len(ends) >= self.p.f5_rank else 0
        if 'f5-few' in self.fixes and 0 < len(ends) < self.p.f5_rank:
            ebf = ends[-1]                  # fewer than 4 files: the smallest of their ends
        if self.p.trace and ends:
            self.tr['f5'].append((I.id, len(ends), ends[:6]))
        g.end_by_files = ebf
        tc = None
        for bh in self.s.boots:
            if bh.cs != g.cs:
                continue
            for vo in (bh.off, bh.off - bh.total_sectors * bh.bps):
                if vo == g.off:
                    t = bh.total_sectors * bh.bps // g.cs
                    if ebf <= t:
                        tc = t if tc is None else max(tc, t)
        if tc is None:
            tc = max(ebf, (self.end - g.off) // g.cs)
        g.total_clusters = tc
        lo = g.off
        top = tc
        if 'f5-hull' in self.fixes and ends:
            top = max(top, ends[0])
        hi = min(g.off + top * g.cs, self.source_size)
        g.lvr = (lo, hi)

    # ------------------------------------------------------------------ D7, D6, D8, D9
    def file_extent_index(self, Y):
        g = Y.geo
        ivs = []
        for gr in self.groups_of(Y):
            if not gr.in_use or gr.is_dir:
                continue
            for svcn, runs in gr.data:
                for lcn, ln in runs:
                    if lcn is not None:
                        ivs.append((g.off + lcn * g.cs, g.off + (lcn + ln) * g.cs, gr))
        ivs.sort(key=lambda x: x[0])
        return ivs

    def container_of(self, X, Y, ivs=None):
        ivs = ivs if ivs is not None else self.file_extent_index(Y)
        starts = [a for a, b, gr in ivs]
        hold = collections.Counter()
        inside = 0
        for r in X.recs:
            i = bisect.bisect_right(starts, r.off) - 1
            if i >= 0 and ivs[i][0] <= r.off and r.off + r.size <= ivs[i][1]:
                inside += 1
                hold[id(ivs[i][2])] += 1
        if not hold:
            return 0, None
        gid = hold.most_common(1)[0][0]
        gr = [x[2] for x in ivs if id(x[2]) == gid][0]
        return inside, gr

    def mft_map_at_start(self, X):
        """fix d7-volstart: X's validated geometry puts its volume at the range start or at a partition
        start, and X is not a copy (fewer than copy_ratio of its records repeat a record of another
        instance as D6 compares them)."""
        if X.geo is None or not X.geo.validated or X.geo.off not in self.part_starts():
            return False
        keys = set()
        for J in self.insts.values():
            if J.alive and J is not X:
                keys |= {self.copy_key(r) for r in J.recs}
        rep = sum(1 for r in X.recs if self.copy_key(r) in keys)
        return rep < self.p.copy_ratio * len(X.recs)

    def copy_key(self, r):
        return (r.num, r.seq, r.si[0] if r.si else ('ext', r.base, r.base_seq))

    def part_starts(self):
        return {self.start} | {ps for ps, ln in self.s.partitions if self.start <= ps < self.end}

    def anchor_ids(self):
        return {I.id for I, cv in self.anchor_insts()}

    def d7(self):
        changed = False
        anchors = self.anchor_ids()
        alive = [I for I in self.insts.values() if I.alive]
        val = [Y for Y in alive if Y.geo and Y.geo.validated]
        idx = {Y.id: self.file_extent_index(Y) for Y in val}
        for X in alive:
            old = X.embedded_in
            X.embedded_in = None
            if not X.recs:
                continue
            newest = self.itimes(X)[1]
            oldest = self.itimes(X)[0]
            best = None
            for Y in val:
                if Y is X:
                    continue
                inside, gr = self.container_of(X, Y, idx[Y.id])
                if inside >= self.p.embed_ratio * len(X.recs) and gr is not None:
                    if best is None or inside > best[0]:
                        best = (inside, Y, gr)
            if self.p.trace:
                for Y in val:
                    if Y is not X:
                        ins, gr2 = self.container_of(X, Y, idx[Y.id])
                        if ins:
                            self.tr['d7'].append((X.id, Y.id, len(X.recs), ins / len(X.recs),
                                                  None if gr2 is None or newest is None else (gr2.rtime(self.p.time_basis) - newest) / FT_DAY,
                                                  None if gr2 is None or oldest is None else (gr2.rtime(self.p.time_basis) - oldest) / FT_DAY))
            if best:
                inside, Y, gr = best
                ct = gr.rtime(self.p.time_basis)
                ref = newest
                if 'd7-oldest' in self.fixes:
                    ref = oldest
                at_start = 'd7-volstart' in self.fixes and self.mft_map_at_start(X)
                if at_start or (ref is not None and ct < ref - int(self.p.embed_tol * FT_DAY)):
                    X.stale_container = (Y.id, gr)
                    gr.flags.add('overwritten')
                elif X.id not in anchors:
                    X.embedded_in = (Y.id, gr)
                    nm = gr.base.best_name()
                    X.label = '[Inside %s]' % (nm.name if nm else '#%d' % gr.num)
            if (old and old[0]) != (X.embedded_in and X.embedded_in[0]):
                changed = True
        return changed

    def d6(self):
        anchors = self.anchor_ids()
        alive = sorted([I for I in self.insts.values() if I.alive], key=lambda I: -len(I.recs))
        self.strays = Inst('strays')
        self.strays.kind = STRAYS
        for I in alive:
            if 'd6-anchor' in self.fixes and I.id in anchors:
                continue
            if len(I.recs) < self.p.min_records:
                I.kind = STRAYS
                continue
            if I.id in anchors:
                continue
            larger = [J for J in alive if len(J.recs) > len(I.recs)]
            if not larger:
                continue
            if self.p.copy_any_larger:
                keyset = set()
                for J in larger:
                    if 'd6-copy-hash' in self.fixes:
                        keyset |= {(r.num, r.seq, r.hash) for r in J.recs}
                    elif 'd6-copy-name' in self.fixes:
                        keyset |= {(r.num, r.seq, tuple((fn.parent, fn.name) for fn in r.names)) for r in J.recs}
                    elif 'd6-copy-ctime' in self.fixes:
                        keyset |= {(r.num, r.seq, r.si[0] if r.si else ('ext', r.base, r.base_seq)) for r in J.recs}
                    else:
                        keyset |= {(r.num, r.seq) for r in J.recs}
                if 'd6-copy-hash' in self.fixes:
                    rep = sum(1 for r in I.recs if (r.num, r.seq, r.hash) in keyset)
                elif 'd6-copy-name' in self.fixes:
                    rep = sum(1 for r in I.recs if (r.num, r.seq, tuple((fn.parent, fn.name) for fn in r.names)) in keyset)
                elif 'd6-copy-ctime' in self.fixes:
                    rep = sum(1 for r in I.recs if (r.num, r.seq, r.si[0] if r.si else ('ext', r.base, r.base_seq))
                              in keyset)
                else:
                    rep = sum(1 for r in I.recs if (r.num, r.seq) in keyset)
                if self.p.trace:
                    k2 = {(r.num, r.seq) for J in larger for r in J.recs}
                    rep_ns = sum(1 for r in I.recs if (r.num, r.seq) in k2)
                    k3 = {(r.num, r.seq, r.si[0] if r.si else None) for J in larger for r in J.recs}
                    rep_ct = sum(1 for r in I.recs if (r.num, r.seq, r.si[0] if r.si else None) in k3)
                    self.tr['d6'].append((I.id, len(I.recs), rep_ns / len(I.recs), rep_ct / len(I.recs)))
                if rep >= self.p.copy_ratio * len(I.recs):
                    I.kind = STRAYS
                    I.copy_set = True
        # which larger instance does each stray group copy?  (number, sequence, names)
        def ident(r):
            if 'd6-copy-ctime' in self.fixes:
                return (r.num, r.seq, r.si[0] if r.si else ('ext', r.base, r.base_seq))
            return (r.num, r.seq, tuple(sorted((fn.parent, fn.pseq, fn.name) for fn in r.names)))
        idents = {}
        for J in alive:
            if J.kind != STRAYS:
                for r in J.recs:
                    idents.setdefault(ident(r), J.id)
        stray_recs = list(self.stray_recs)
        for I in alive:
            if I.kind == STRAYS:
                c = collections.Counter(idents.get(ident(r)) for r in I.recs)
                c.pop(None, None)
                cp = None
                if c:
                    j, n = c.most_common(1)[0]
                    if n >= self.p.copy_ratio * len(I.recs):
                        cp = j
                for r in I.recs:
                    r.copy_of = cp
                stray_recs += I.recs
                I.alive = False
                I.merged_into_strays = True
        for r in self.stray_recs:
            r.copy_of = None
        self.strays.add(stray_recs)
        for r in stray_recs:
            r.inst = 'strays'
        self.insts['strays'] = self.strays

    def d8_d9(self):
        alive = [I for I in self.insts.values() if I.alive and I.id != 'strays']
        cand = [I for I in alive if not I.embedded_in]
        anchors = [(I, cv) for I, cv in self.anchor_insts() if I.alive and not I.embedded_in and I.id != 'strays']
        if 'd8-newest-cv' in self.fixes and anchors:
            newest = max(cv.format_time for I, cv in anchors)
            anchors = [(I, cv) for I, cv in anchors if cv.format_time == newest]
        main = None
        why = None
        if anchors:
            main = max(anchors, key=lambda x: len(x[0].recs))[0]
            why = 'anchor'
        if main is None:
            starts = {self.start} | {ps for ps, ln in self.s.partitions if self.start <= ps < self.end}
            c2 = [I for I in cand if I.geo and I.geo.validated and I.geo.off in starts]
            if c2:
                main = max(c2, key=lambda I: (self.itimes(I)[1] or 0, len(I.recs)))
                why = 'validated-at-start'
        if main is None and cand:
            timed_c = [I for I in cand if self.itimes(I)[1] is not None]
            if timed_c:
                main = max(timed_c, key=lambda I: (self.itimes(I)[1], len(I.recs)))
                why = 'newest'
            else:
                main = max(cand, key=lambda I: len(I.recs))
                why = 'most-records'
        self.main = main
        self.main_why = why
        for I in alive:
            if I.embedded_in:
                I.kind = EMBEDDED
            elif I is main:
                I.kind = MAIN
            else:
                I.kind = None
        if main is None:
            for I in alive:
                if I.kind is None:
                    I.kind = OTHER
            return
        mt = self.itimes(main)
        for I in alive:
            if I.kind is not None:
                continue
            older = False
            for c in self.cuts:
                if self.seg_owner(c['below']) == main.id:
                    if self.seg_owner(c['above']) == I.id or self.rel_to_cut(self.itimes(I), c) == 'before':
                        older = True
            if not older:
                it = self.itimes(I)
                if it[1] is not None and mt[0] is not None and it[1] < mt[0]:
                    if not (I.geo and I.geo.validated and main.geo and I.geo.off != main.geo.off):
                        older = True
            I.kind = OLDER if older else OTHER

    # ------------------------------------------------------------------ E: trees
    def trees(self):
        self.lost = {}                    # (inst, P, s) -> dict(name, parent)
        self.files = []
        # usable entries (E6)
        ent_by_child = collections.defaultdict(list)
        for e in self.s.entries:
            if not getattr(e, 'current', False):
                ent_by_child[(e.child, e.cseq)].append(e)
        self.ent_by_child = ent_by_child
        alive = [I for I in self.insts.values() if I.alive]
        self.dirmap = {}
        for I in alive:
            gs = self.groups_of(I)
            self.dirmap[I.id] = {g.num: g for g in gs if not g.orphan and g.is_dir}
        rec_inst = {}
        for I in alive:
            for r in I.recs:
                rec_inst[r.off] = I.id
        self.rec_inst = rec_inst
        # strays: directory records by (num, seq), in use
        self.stray_dirs = collections.defaultdict(list)
        for r in self.strays.recs:
            if r.in_use and r.is_dir and r.num is not None:
                self.stray_dirs[(r.num, r.seq)].append(r)
        # E6 attribution of index blocks by their entries (fix e6-match): instances that hold a
        # record matching an entry exactly (number, sequence, name and parent reference)
        self.block_match = {}
        if 'e6-match' in self.fixes:
            exact = collections.defaultdict(set)
            for I in alive:
                if I.id == 'strays':
                    continue
                for r in I.recs:
                    if r.num is None:
                        continue
                    owner = I.id
                    for fn in r.names:
                        exact[(r.num, r.seq, fn.name, fn.parent, fn.pseq)].add(owner)
            for b in self.s.blocks:
                m = set()
                for e in b.entries:
                    m |= exact.get((e.child, e.cseq, e.name, e.parent, e.pseq), set())
                self.block_match[b.off] = m
            self.root_match = {}
        nums_any = set()
        for I in alive:
            if I.id != 'strays' or self.p.strays_is_instance:
                nums_any |= set(I.nums)
        self.nums_any = nums_any
        for I in alive:
            for g in self.groups_of(I):
                if g.orphan:
                    g.parent = ('orphan',)
                    continue
                if g.num is not None and g.num < 16:
                    g.parent = ('system',)
                    continue
                if g.num is not None and g.num < 24 and not g.names:
                    g.parent = ('system',)
                    continue
                g.parent = self.parent_node(I, g)
                self.files.append(g)

    def best_fn(self, I, g):
        """E4: best namespace; among those, one whose parent resolves to a surviving directory."""
        best = None
        for fn in g.names:
            rank = {1: 0, 3: 0, 0: 1, 2: 2}.get(fn.ns, 3)
            resolv = 0 if self.resolve_dir(I, fn.parent, fn.pseq) is not None else 1
            k = (rank, resolv, fn.attr_id)
            if best is None or k < best[0]:
                best = (k, fn)
        return best[1] if best else None

    def resolve_dir(self, I, P, s):
        if I.id == 'strays':
            return None
        d = self.dirmap.get(I.id, {}).get(P)
        if d is None or d.base.older_gen or not d.is_dir:
            return None
        if not self.resolves(s, d.base.in_use, d.base.seq):
            return None
        return d

    def parent_node(self, I, g):
        fn = self.best_fn(I, g)
        if fn is None:
            return ('orphan',)
        P, s = fn.parent, fn.pseq
        return self.attach(I, P, s, depth=0)

    def attach(self, I, P, s, depth):
        if I.id != 'strays':
            if P == 5:
                return ('root',)
            if P == 11:
                return ('extend',)
            d = self.resolve_dir(I, P, s)
            if d is not None:
                return ('dir', d.base.off)
        key = (I.id, P, s)
        if key not in self.lost:
            self.lost[key] = {'name': None, 'parent': ('lostroot',)}
            if depth < 64:
                e = self.name_for(I, P, s)
                if e is not None:
                    self.lost[key]['name'] = e.name
                    self.lost[key]['src'] = e.src_off
                    if I.id == 'strays':
                        par = ('lostroot',)
                    else:
                        par = self.attach(I, e.parent, e.pseq, depth + 1)
                        if self.p.lost_chain == 'flat' and par[0] == 'lost' and \
                                self.lost.get(par[1:])['name'] is None:
                            par = ('lostroot',)
                    self.lost[key]['parent'] = par
        return ('lost',) + key

    def entry_usable(self, I, e):
        if getattr(e, 'current', False):
            return False
        belongs = False
        if e.src_kind == 'root':
            belongs = self.rec_inst.get(e.src_off) == I.id
        elif 'e6-match' in self.fixes and self.block_match.get(e.block.off):
            belongs = self.block_match[e.block.off] == {I.id}
        else:
            b = e.block
            if b.owner is not None:
                d = self.dirmap.get(I.id, {}).get(b.owner)
                if d is not None and self.resolves(b.oseq, d.base.in_use, d.base.seq):
                    belongs = True
                elif b.owner not in self.nums_any:
                    # "failing both": the owner number is not present in any instance
                    belongs = True
                    if 'e6-floor' in self.fixes:
                        old = self.itimes(I)[0]
                        if old is not None and self.et(e) < old - int(self.p.e6_floor_days * FT_DAY):
                            belongs = False
        if not belongs:
            return False
        for c in self.cuts:
            hb = self.seg_owner(c['below'])
            ha = self.seg_owner(c['above'])
            rel = self.rel_to_cut((self.et(e), self.et(e)), c)
            if hb == I.id and rel != 'after':
                return False
            if ha == I.id and rel != 'before':
                return False
        if I.geo:
            for a, b2, gr in self.ext_index(I):
                if a <= e.src_off < b2:
                    return False
        return True

    def ext_index(self, I):
        if not hasattr(I, '_extidx') or I._extidx is None:
            I._extidx = self.file_extent_index(I) if I.geo else []
        return I._extidx

    def name_for(self, I, P, s):
        cands = [e for e in self.ent_by_child.get((P, s), ()) if self.entry_usable(I, e)]
        if not cands:
            rs = self.stray_dirs.get((P, s), [])
            if 'e6b-copyset' in self.fixes:
                rs = [x for x in rs if getattr(x, 'copy_of', None) == I.id]
            r = rs[0] if rs else None
            if r is not None and r.names:
                fn = r.best_name()
                e = nr.IndexEntry()
                e.child, e.cseq, e.parent, e.pseq, e.ns, e.name = P, s, fn.parent, fn.pseq, fn.ns, fn.name
                e.chtime, e.src_off, e.src_lsn, e.src_kind, e.block, e.is_dir = fn.chtime, r.off, r.lsn, 'stray', None, True
                e.etime = fn.tmax
                return e
            return None
        nondos = [e for e in cands if e.ns != 2]
        if nondos:
            cands = nondos
        if 'e6-newest' in self.fixes:
            cands.sort(key=lambda e: (-self.et(e), -e.src_lsn, e.src_off))
        else:
            cands.sort(key=lambda e: (-e.src_lsn, e.src_off))
        return cands[0]

    # ------------------------------------------------------------------ G
    def health(self):
        alive = [I for I in self.insts.values() if I.alive]
        for I in alive:
            older = I.kind == OLDER
            for g in self.groups_of(I):
                if older or g.base.older_gen:
                    g.flags.add('older_generation')
                if I.kind == EMBEDDED:
                    g.flags.add('embedded')
        # C5 overwritten set and G2 overlap conflicts
        ivs = []
        for I in alive:
            if I.kind == EMBEDDED:
                if self.p.c5_embedded and I.geo is not None:
                    for g in self.groups_of(I):
                        for svcn, runs in g.data:
                            for lcn, ln in runs:
                                if lcn is not None and self.overwritten.intersects(
                                        I.geo.off + lcn * I.geo.cs, I.geo.off + (lcn + ln) * I.geo.cs):
                                    g.flags.add('overwritten')
                continue
            geo = I.geo or (self.main.geo if self.main else None)
            if geo is None:
                continue
            for g in self.groups_of(I):
                if g.is_dir and not self.p.g2_dirs:
                    continue
                exts = g.data + (g.ia if self.p.g2_dirs else [])
                for svcn, runs in exts:
                    for lcn, ln in runs:
                        if lcn is None:
                            continue
                        a, b = geo.off + lcn * geo.cs, geo.off + (lcn + ln) * geo.cs
                        if self.overwritten.intersects(a, b):
                            g.flags.add('overwritten')
                        tc = geo.total_clusters
                        beyond = tc is not None and (lcn >= tc if self.p.oob_literal else lcn + ln > tc)
                        if beyond or a < self.start or b > self.end:
                            g.flags.add('out_of_bounds')
                        ivs.append((a, b, g, I))
        ivs.sort(key=lambda x: x[0])
        active = []
        seen = set()
        for a, b, g, I in ivs:
            active = [x for x in active if x[1] > a]
            for x in active:
                g2, I2 = x[2], x[3]
                if g2 is g:
                    continue
                k = (id(g), id(g2)) if id(g) < id(g2) else (id(g2), id(g))
                if k in seen:
                    continue
                seen.add(k)
                self.conflict(g, I, g2, I2)
            active.append((a, b, g, I))

    def conflict(self, g1, I1, g2, I2):
        def key(g, I):
            return (-KIND_RANK.get(I.kind, 3), 1 if g.in_use else 0, g.rtime(self.p.time_basis), g.base.lsn)
        k1, k2 = key(g1, I1), key(g2, I2)
        if k1 == k2:
            g1.flags.add('overlap')
            g2.flags.add('overlap')
            return
        w, l = (g1, g2) if k1 > k2 else (g2, g1)
        Iw, Il = (I1, I2) if k1 > k2 else (I2, I1)
        l.flags.add('overwritten')
        if w.in_use and l.in_use and Iw.kind == Il.kind:
            w.flags.add('overlap')

    # ------------------------------------------------------------------ driver
    def run(self):
        self.c1_c2()
        self.c3()
        self.c4_c5()
        self.d1()
        self.d2_d3()
        self.init_instances()
        self.join(2)
        anchors = self.anchor_ids() if 'd6-anchor' in self.fixes else set()
        for I in self.insts.values():
            if I.alive and (len(I.recs) >= self.p.min_records or I.id in anchors):
                I.geo = self.geometry(I)
        for _ in range(50):
            ch = self.d7()
            m = self.join(4)
            anchors = self.anchor_ids() if 'd6-anchor' in self.fixes else set()
            for I in self.insts.values():
                if I.alive and I.geo is None and (len(I.recs) >= self.p.min_records or I.id in anchors):
                    I.geo = self.geometry(I)
            if not ch and not m:
                break
        self.d6()
        self.d8_d9()
        self.inherit_geometry()
        self.trees()
        self.health()
        return self

    def inherit_geometry(self):
        mg = self.main.geo if self.main else None
        for I in self.insts.values():
            if not I.alive or I.kind not in (OLDER, STRAYS):
                continue
            if I.geo is None or not getattr(I.geo, 'own_evidence', False):
                if mg is not None:
                    g = Geo()
                    for k in Geo.__slots__:
                        setattr(g, k, getattr(mg, k, None))
                    g.source = 'inherited'
                    g.inherited = True
                    I.geo = g

