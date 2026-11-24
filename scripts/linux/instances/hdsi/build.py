"""Builder framework for the instance-separation scenarios.

A scenario is a disk image built by real tools: mkntfs formats, the ntfs-3g FUSE driver writes,
sfdisk writes partition tables, qemu-img makes VHD containers.  Every step runs under libfaketime
at a scenario-defined date, so the history of each file system spans realistic time (old
generations years before the accident, the accident at a whole second after every old write).

Windows' rewrite of MFT slots 0..255 at format + first mount is imitated after each mkntfs by
overwriting the old slots up to 255 that mkntfs left alone (zeros by default, or unused FILE
records), restricted to clusters that are free in the new volume.

While building, the end-of-life state of every file system is recorded through its own
structures (boot sector -> $MFT runlist -> every slot below the initialized size): offset, raw
hash, number, sequence, flags, names, runs, times.  truth.py derives the ground truth from these
snapshots, the content manifests and the final image.
"""
import calendar
import contextlib
import hashlib
import json
import os
import random
import shutil
import struct
import subprocess
import time

from . import ntfsraw as nr

LAB = os.environ.get('HDSI_LAB', '/tmp/claude-0/-home-user-HDSavior/ac0f868e-c29d-52a4-b252-bb2ec8fb4924/'
                     'scratchpad/instance-lab')
FTLIB = '/usr/lib/x86_64-linux-gnu/faketime/libfaketime.so.1'


def T(s):
    """'YYYY-MM-DD HH:MM:SS' (UTC) -> epoch seconds."""
    return calendar.timegm(time.strptime(s, '%Y-%m-%d %H:%M:%S'))


def fmt_t(t):
    return time.strftime('%Y-%m-%d %H:%M:%S', time.gmtime(int(t)))


def fake_env(t):
    env = dict(os.environ)
    env.update(LD_PRELOAD=FTLIB, FAKETIME='@' + fmt_t(t), FAKETIME_DONT_FAKE_MONOTONIC='1', TZ='UTC')
    return env


def run(args, env=None, check=True):
    p = subprocess.run(args, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    if check and p.returncode:
        raise RuntimeError('%s failed (%d): %s' % (' '.join(map(str, args)), p.returncode, p.stdout[-2000:]))
    return p.stdout


# ----------------------------------------------------------------------------- content

def _header(ext, rnd):
    e = ext.lower()
    if e in ('jpg', 'jpeg'):
        return (b'\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00'
                b'\xff\xdb\x00\x43\x00' + rnd.randbytes(64))
    if e == 'png':
        w, h = rnd.randint(16, 4000), rnd.randint(16, 3000)
        return b'\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR' + struct.pack('>II', w, h) + b'\x08\x02\x00\x00\x00' + rnd.randbytes(4)
    if e == 'gif':
        return b'GIF89a' + struct.pack('<HH', rnd.randint(16, 900), rnd.randint(16, 900)) + b'\xf7\x00\x00'
    if e == 'pdf':
        return b'%PDF-1.4\n%\xe2\xe3\xcf\xd3\n'
    if e in ('zip', 'docx', 'xlsx'):
        name = b'word/document.xml' if e == 'docx' else b'data/content.bin'
        return (b'PK\x03\x04\x14\x00\x00\x00\x08\x00' + rnd.randbytes(16) + struct.pack('<HH', len(name), 0) + name)
    if e == 'txt':
        words = [b'lorem', b'ipsum', b'dolor', b'sit', b'amet', b'disk', b'file', b'note', b'data']
        out = bytearray()
        while len(out) < 256:
            out += rnd.choice(words) + b' '
        return bytes(out)
    return b''


def content(seed, size, ext):
    rnd = random.Random(seed)
    head = _header(ext, rnd)
    if ext.lower() == 'txt':
        out = bytearray(head)
        words = [b'lorem', b'ipsum', b'dolor', b'sit', b'amet', b'disk', b'file', b'note', b'data', b'\n']
        while len(out) < size:
            out += rnd.choice(words) + b' '
        return bytes(out[:size])
    body = rnd.randbytes(max(0, size - len(head)))
    return (head + body)[:size]


# ----------------------------------------------------------------------------- volumes

class Volume:
    def __init__(self, vid, tag, device, offset, length, cluster, sector, hidden, label):
        self.vid = vid                 # stable id used in the ground truth
        self.tag = tag                 # prefix of every name created on it
        self.device = device           # file holding it (the disk image, or a separate image)
        self.offset = offset           # byte offset in device
        self.length = length           # bytes of the region (partition) it was formatted on
        self.cluster = cluster
        self.sector = sector
        self.hidden = hidden
        self.label = label
        self.format_time = None        # epoch seconds (fake clock)
        self.last_write = None
        self.manifest = {}             # basename -> {path, sha256, size, deleted, version}
        self.dirs = {}                 # basename -> path
        self.snapshots = {}            # tag -> snapshot dict
        self.alive = True
        self.notes = []
        self.embedded_in = None        # (host volume id, container basename, kind 'raw'|'vhd')

    def info(self):
        return {'vid': self.vid, 'tag': self.tag, 'device': os.path.basename(self.device),
                'offset': self.offset, 'length': self.length, 'cluster': self.cluster,
                'sector': self.sector, 'hidden': self.hidden, 'label': self.label,
                'format_time': self.format_time, 'last_write': self.last_write,
                'embedded_in': self.embedded_in, 'notes': self.notes}


def _mft_map(extents, cs):
    """extents: list of (svcn, runs) -> function slot byte position -> device-relative volume byte."""
    segs = []
    for svcn, runs in sorted(extents):
        v = svcn
        for lcn, ln in runs:
            segs.append((v, v + ln, lcn))
            v += ln
    def m(pos):
        vcn, within = divmod(pos, cs)
        for a, b, lcn in segs:
            if a <= vcn < b:
                return None if lcn is None else (lcn + vcn - a) * cs + within
        return None
    return m, segs


def read_volume(device, off):
    """Parse a volume through its own structures.  Returns a snapshot dict."""
    with open(device, 'rb') as f:
        def rd(o, n):
            f.seek(o)
            return f.read(n)
        bs = nr.parse_boot(rd(off, 512), off)
        if bs is None:
            raise RuntimeError('no NTFS boot sector at %d in %s' % (off, device))
        cs, rs = bs.cs, bs.rec_size
        r0 = nr.parse_record(rd(off + bs.mft_lcn * cs, rs), off + bs.mft_lcn * cs, want_bitmap=True)
        if isinstance(r0, str):
            raise RuntimeError('unusable $MFT record 0 at %d' % (off + bs.mft_lcn * cs))
        exts = [(d.svcn, d.runs) for d in r0.data if not d.resident]
        init = [d.init for d in r0.data if not d.resident and d.svcn == 0][0]
        m, _ = _mft_map(exts, cs)
        # $MFT attribute list (fragmented $MFT): further $DATA extents live in extension records
        if r0.has_attrlist:
            al = r0.attrlist
            ents = []
            if isinstance(al, tuple) and al[0] == 'nonres':
                buf = bytearray()
                for lcn, ln in al[1]:
                    buf += rd(off + lcn * cs, ln * cs)
                buf = bytes(buf[:al[2]])
                q = 0
                while q + 0x1A <= len(buf):
                    at, el = struct.unpack_from('<IH', buf, q)
                    if el < 0x1A:
                        break
                    svcn = struct.unpack_from('<q', buf, q + 8)[0]
                    ref = struct.unpack_from('<Q', buf, q + 0x10)[0]
                    if at == 0x80:
                        ents.append((svcn, ref & 0xFFFFFFFFFFFF))
                    q += el
            elif isinstance(al, list):
                ents = [(s, n) for (t, s, n, q2) in al if t == 0x80]
            for svcn, n in ents:
                if n == 0:
                    continue
                p = m(n * rs)
                if p is None:
                    continue
                rx = nr.parse_record(rd(off + p, rs), off + p)
                if isinstance(rx, str):
                    continue
                for d in rx.data:
                    if not d.resident and (d.svcn, d.runs) not in exts:
                        exts.append((d.svcn, d.runs))
            m, _ = _mft_map(exts, cs)
        nslots = init // rs
        # $MFT:$BITMAP
        bm = r0.bitmap_attr
        if bm and bm[0] == 'res':
            mbits = bm[1]
        elif bm:
            buf = bytearray()
            for lcn, ln in bm[1]:
                buf += rd(off + lcn * cs, ln * cs) if lcn is not None else bytes(ln * cs)
            mbits = bytes(buf[:bm[2]])
        else:
            mbits = b''
        recs = []
        for n in range(nslots):
            p = m(n * rs)
            if p is None:
                continue
            raw = rd(off + p, rs)
            r = nr.parse_record(raw, off + p)
            used = bool(mbits[n >> 3] >> (n & 7) & 1) if (n >> 3) < len(mbits) else False
            if isinstance(r, str):
                continue
            recs.append(snap_record(r, n, used))
        # cluster bitmap ($Bitmap, record 6)
        p6 = m(6 * rs)
        r6 = nr.parse_record(rd(off + p6, rs), off + p6)
        alloc = []
        if not isinstance(r6, str):
            total_cl = bs.total_sectors * bs.bps // cs
            buf = bytearray()
            for d in r6.data:
                if not d.resident:
                    for lcn, ln in d.runs:
                        buf += rd(off + lcn * cs, ln * cs) if lcn is not None else bytes(ln * cs)
            alloc = bits_to_ranges(bytes(buf), total_cl)
        # $MFTMirr runs (record 1)
        p1 = m(1 * rs)
        r1 = nr.parse_record(rd(off + p1, rs), off + p1)
        mirr = []
        if not isinstance(r1, str):
            for d in r1.data:
                if not d.resident:
                    mirr += [(lcn, ln) for lcn, ln in d.runs if lcn is not None]
        fmt_time = None
        p3 = m(3 * rs)
        r3 = nr.parse_record(rd(off + p3, rs), off + p3)
        if not isinstance(r3, str) and r3.si:
            fmt_time = r3.si[0]
    return {'boot': bs.as_dict(), 'offset': off, 'cs': cs, 'rs': rs, 'mft_extents': exts,
            'mft_init': init, 'records': recs, 'alloc': alloc, 'mirr_runs': mirr,
            'format_time_ft': fmt_time, 'total_clusters': bs.total_sectors * bs.bps // cs}


def bits_to_ranges(bits, nbits):
    out = []
    start = None
    for i in range(nbits):
        byte = bits[i >> 3] if (i >> 3) < len(bits) else 0
        on = byte >> (i & 7) & 1
        if on and start is None:
            start = i
        elif not on and start is not None:
            out.append((start, i))
            start = None
    if start is not None:
        out.append((start, nbits))
    return out


def snap_record(r, slot, used):
    fn = r.best_name()
    return {
        'slot': slot, 'num': r.num, 'off': r.off, 'hash': r.hash, 'seq': r.seq, 'flags': r.flags,
        'used_bitmap': used, 'base': r.base, 'base_seq': r.base_seq, 'lsn': r.lsn,
        'si': list(r.si[:4]) if r.si else None, 'fixup_bad': r.fixup_bad,
        'names': [[x.parent, x.pseq, x.ns, x.name] for x in r.names],
        'name': fn.name if fn else None, 'parent': [fn.parent, fn.pseq] if fn else None,
        'data': [[d.svcn, d.evcn, [list(x) for x in d.runs], d.real, d.init, d.alloc, d.aflags]
                 for d in r.data if not d.resident],
        'resident_data': [[d.res_off - r.off, d.res_len] for d in r.data if d.resident],
        'ia': [[d.svcn, [list(x) for x in d.runs]] for d in r.ia],
        'is_dir': r.is_dir, 'in_use': r.in_use,
    }


# ----------------------------------------------------------------------------- scenario

class Scenario:
    def __init__(self, name, out=None):
        self.name = name
        self.dir = os.path.join(out or os.path.join(LAB, 'scenarios'), name)
        os.makedirs(self.dir, exist_ok=True)
        self.mnt = os.path.join(LAB, 'mnt', name)
        os.makedirs(self.mnt, exist_ok=True)
        self.image = None
        self.image_size = 0
        self.volumes = {}
        self.order = []                # volume ids in creation order
        self.events = []
        self.scans = []
        self.expect = {}
        self.extra = {}
        self.ptable = None
        self.seed = 'hdsi-' + name

    # ---- images
    def disk(self, size_mib, fname=None):
        self.image = os.path.join(self.dir, fname or (self.name + '.img'))
        if os.path.exists(self.image):
            os.unlink(self.image)
        with open(self.image, 'wb') as f:
            f.truncate(size_mib << 20)
        self.image_size = size_mib << 20
        return self.image

    def aux_image(self, fname, size_mib):
        p = os.path.join(self.dir, fname)
        if os.path.exists(p):
            os.unlink(p)
        with open(p, 'wb') as f:
            f.truncate(size_mib << 20)
        return p

    def partition_table(self, kind, parts, device=None):
        """parts: list of (start_sector, sectors[, type]); kind 'mbr' or 'gpt'."""
        dev = device or self.image
        lines = ['label: %s' % ('dos' if kind == 'mbr' else 'gpt'), 'unit: sectors', 'sector-size: 512']
        for p in parts:
            if kind == 'mbr':
                lines.append('start=%d, size=%d, type=%s' % (p[0], p[1], p[2] if len(p) > 2 else '7'))
            else:
                lines.append('start=%d, size=%d, type=%s' % (p[0], p[1], p[2] if len(p) > 2 else
                                                             'EBD0A0A2-B9E5-4433-87C0-68B6B72699C7'))
        p = subprocess.run(['sfdisk', '--no-reread', '--no-tell-kernel', '-q', '-f', dev],
                           input='\n'.join(lines) + '\n', text=True, stdout=subprocess.PIPE,
                           stderr=subprocess.STDOUT)
        if p.returncode:
            raise RuntimeError('sfdisk: ' + p.stdout)
        self.ptable = {'kind': kind, 'parts': [list(x[:2]) for x in parts]}
        self.events.append(('ptable', kind, [list(x[:2]) for x in parts]))

    # ---- loop devices for volumes that do not fill their image file
    @contextlib.contextmanager
    def _dev(self, vol):
        whole = vol.offset == 0 and os.path.getsize(vol.device) == vol.length
        if whole:
            yield vol.device
            return
        dev = run(['losetup', '-f', '--show', '-o', str(vol.offset), '--sizelimit', str(vol.length),
                   vol.device]).strip()
        try:
            yield dev
        finally:
            for _ in range(50):
                if subprocess.run(['losetup', '-d', dev], stdout=subprocess.DEVNULL,
                                  stderr=subprocess.DEVNULL).returncode == 0:
                    break
                time.sleep(0.2)

    # ---- file systems
    def volume(self, vid, tag, offset=0, length=None, device=None, cluster=4096, sector=512,
               hidden=None, label=None):
        device = device or self.image
        if length is None:
            length = os.path.getsize(device) - offset
        v = Volume(vid, tag, device, offset, length, cluster, sector,
                   hidden if hidden is not None else offset // sector, label or vid.upper()[:16])
        return v

    def mkfs(self, vol, at, windows='zero', extra=()):
        """Quick-format vol's region at fake time `at`.  Every live volume that overlaps the region is
        snapshotted first (end of its life).  windows: 'zero' | 'blank' | None."""
        for o in list(self.volumes.values()):
            if o.alive and o.device == vol.device and o.offset < vol.offset + vol.length and \
                    vol.offset < o.offset + o.length:
                self.end_of_life(o)
        args = ['mkntfs', '-Q', '-F', '-q', '-c', str(vol.cluster), '-s', str(vol.sector),
                '-p', str(vol.hidden), '-H', '255', '-S', '63', '-L', vol.label] + list(extra)
        with self._dev(vol) as dev:
            run(args + [dev], env=fake_env(at))
        vol.format_time = at
        vol.last_write = at
        self.volumes[vol.vid] = vol
        if vol.vid not in self.order:
            self.order.append(vol.vid)
        self.events.append(('mkfs', vol.vid, at, vol.offset, vol.length, vol.cluster))
        if windows:
            self.windows_wipe(vol, mode=windows)
        return vol

    def windows_wipe(self, vol, mode='zero', last=255):
        """Overwrite MFT slots init..last of the new volume that mkntfs did not write, inside clusters
        that are free in the new volume (imitates Windows' 256-record initial $MFT)."""
        snap = read_volume(vol.device, vol.offset)
        cs, rs = snap['cs'], snap['rs']
        b = snap['boot']
        first = snap['mft_init'] // rs
        mstart = vol.offset + b['mft_lcn'] * cs
        free = _free_checker(snap['alloc'])
        mft_alloc = set()
        for svcn, runs in snap['mft_extents']:
            for lcn, ln in runs:
                if lcn is not None:
                    mft_alloc.update(range(lcn, lcn + ln))
        wiped = []
        with open(vol.device, 'r+b') as f:
            for n in range(first, last + 1):
                o = mstart + n * rs
                c0 = (o - vol.offset) // cs
                c1 = (o + rs - 1 - vol.offset) // cs
                if not all(free(c) or c in mft_alloc for c in range(c0, c1 + 1)):
                    continue
                f.seek(o)
                if mode == 'zero':
                    f.write(b'\0' * rs)
                else:
                    f.write(blank_record(n, rs))
                wiped.append(n)
        vol.notes.append('windows-wipe %s slots %d..%d (%d written)' % (mode, first, last, len(wiped)))
        self.events.append(('wipe', vol.vid, mode, first, last, len(wiped)))

    def backdate_si(self, vol, lo, hi, rnd, pick=None):
        """Imitate a copy from a backup (robocopy, Copy-Item, Move-Item across volumes): every in-use
        record numbered 24 or above whose name carries vol's tag (and that `pick(snapshot record)`
        accepts) gets the four $STANDARD_INFORMATION times of a source file last written between `lo`
        and `hi` (epoch seconds).  Windows carries the source's MFT-change time over to the tick, while
        every $FILE_NAME time stays the moment of the copy (docs/notes/quick-format-behaviour.md 7a).
        ntfs-3g cannot do this: its system.ntfs_times xattr sets creation, write and access time, stamps
        the MFT-change time with the current clock and rewrites the $FILE_NAME times as well.  So the
        $SI times are patched in the unmounted volume; $FILE_NAME attributes and index entries keep the
        times the driver wrote at the copy.  Returns the number of records changed."""
        snap = read_volume(vol.device, vol.offset)
        rs = snap['rs']
        n = 0
        with open(vol.device, 'r+b') as f:
            for r in snap['records']:
                if not r['in_use'] or r['num'] is None or r['num'] < 24 or r['base']:
                    continue
                if not r['name'] or not r['name'].startswith(vol.tag) or (pick and not pick(r)):
                    continue
                f.seek(r['off'])
                raw = bytearray(f.read(rs))
                p = struct.unpack_from('<H', raw, 0x14)[0]
                while p + 0x18 <= rs:
                    t, ln = struct.unpack_from('<II', raw, p)
                    if t == 0xFFFFFFFF or ln < 0x18 or p + ln > rs:
                        p = None
                        break
                    if t == 0x10:
                        break
                    p += ln
                if p is None or raw[p + 8] != 0:
                    continue
                v = p + struct.unpack_from('<H', raw, p + 0x14)[0]
                # the four times must not touch the last two bytes of a sector (update sequence)
                if any((v + k) % 512 >= 510 for k in range(32)):
                    raise RuntimeError('SI times of record %d cross a sector end' % r['num'])
                c = rnd.randint(lo, hi)
                wr = min(hi, c + rnd.randint(0, 200 * 86400))
                ch = min(hi, wr + rnd.randint(0, 3600))
                ac = min(hi, ch + rnd.randint(0, 30 * 86400))
                struct.pack_into('<4Q', raw, v, *(nr.unix_to_ft(x) + rnd.randint(0, 9999999)
                                                  for x in (c, wr, ch, ac)))
                f.seek(r['off'])
                f.write(raw)
                n += 1
        vol.notes.append('backdate_si %d records to %s..%s' % (n, fmt_t(lo), fmt_t(hi)))
        self.events.append(('backdate_si', vol.vid, lo, hi, n))
        return n

    def sync_entry_times(self, vol, from_si=False, access_days=None, until=None):
        """Imitate Windows' duplicated information in the $I30 entries of vol (INDX blocks and
        $INDEX_ROOTs in vol's region whose names carry vol's tag; the volume must be unmounted).
        Windows keeps a parent's index entry in step with the child's $STANDARD_INFORMATION, access
        time included (windows_volume.raw: every entry's four times equal the child's $SI times);
        ntfs-3g writes the times of its own $FILE_NAME instead.
        from_si: every entry gets the four $SI times of the record it names (number, sequence, name
          and parent equal; for a deleted child the sequence before the free), so a copy restored
          from a backup (backdate_si) carries the source's old times in its entry, not the copy time.
        access_days: every entry's last-access time becomes its latest other time + that many days
          (capped at `until`, epoch seconds): folders listed and files read after their last change.
        Returns the number of entries changed."""
        from . import scan as hs
        res = hs.scan(vol.device, vol.offset, vol.offset + vol.length, use_cache=False)
        si = {}
        if from_si:
            for r in res.records:
                if r.num is None or not r.si or r.base:
                    continue
                for fn in r.names:
                    si[(r.num, r.seq, fn.name, fn.parent)] = r.si[:4]
                    si.setdefault((r.num, (r.seq - 1) or 0xFFFF, fn.name, fn.parent), r.si[:4])
        cap = nr.unix_to_ft(until) if until is not None else None
        dt = int(access_days * 86400 * 10 ** 7) if access_days is not None else None

        def patch(b, start, end):
            q, n = start, 0
            while q + 16 <= end:
                cref, elen, klen, efl = struct.unpack_from('<QHHH', b, q)
                if elen < 16 or q + elen > end or efl & 2:
                    break
                fn = nr._parse_file_name(b[q + 16:q + 16 + klen]) if klen >= 66 else None
                if fn is not None and fn.name.startswith(vol.tag):
                    k = q + 16 + 8
                    if from_si:
                        t = si.get((cref & 0xFFFFFFFFFFFF, cref >> 48, fn.name, fn.parent))
                        if t is not None:
                            struct.pack_into('<4Q', b, k, *t)
                            n += 1
                    if dt is not None:
                        a = max(struct.unpack_from('<3Q', b, k)) + dt
                        struct.pack_into('<Q', b, k + 24, a if cap is None else min(a, cap))
                        n += 1
                q += elen
            return n

        def protect(b):
            usa_ofs, usa_cnt = struct.unpack_from('<HH', b, 4)
            usn = bytes(b[usa_ofs:usa_ofs + 2])
            for i in range(1, usa_cnt):
                e = i * 512
                b[usa_ofs + 2 * i:usa_ofs + 2 * i + 2] = b[e - 2:e]
                b[e - 2:e] = usn
            return b

        total = 0
        with open(vol.device, 'r+b') as f:
            for blk in res.blocks:
                f.seek(blk.off)
                fx = nr.apply_fixups(f.read(blk.size), blk.size)
                if fx is None or fx[1]:
                    continue
                b = fx[0]
                eo, isz = struct.unpack_from('<II', b, 0x18)
                n = patch(b, 0x18 + eo, min(blk.size, 0x18 + isz))
                if n:
                    f.seek(blk.off)
                    f.write(protect(b))
                    total += n
            for r in res.records:
                f.seek(r.off)
                fx = nr.apply_fixups(f.read(r.size), r.size)
                if fx is None or fx[1]:
                    continue
                b = fx[0]
                p = struct.unpack_from('<H', b, 0x14)[0]
                n = 0
                while p + 0x18 <= r.size:
                    t, ln = struct.unpack_from('<II', b, p)
                    if t == 0xFFFFFFFF or ln < 0x18 or p + ln > r.size:
                        break
                    if t == 0x90 and b[p + 8] == 0:
                        hdr = p + struct.unpack_from('<H', b, p + 0x14)[0] + 0x10
                        eo, isz = struct.unpack_from('<II', b, hdr)
                        n += patch(b, hdr + eo, min(r.size, hdr + isz))
                    p += ln
                if n:
                    f.seek(r.off)
                    f.write(protect(b))
                    total += n
        vol.notes.append('sync_entry_times from_si=%s access_days=%s: %d changes' % (from_si, access_days, total))
        self.events.append(('sync_entry_times', vol.vid, from_si, access_days, total))
        return total

    @contextlib.contextmanager
    def mount(self, vol, at=None):
        if at is None:
            at = vol.last_write + 3
        mnt = os.path.join(self.mnt, vol.vid)
        os.makedirs(mnt, exist_ok=True)
        with self._dev(vol) as dev:
            t0 = time.time()
            run(['ntfs-3g', '-o', 'big_writes', dev, mnt], env=fake_env(at))
            w = Writer(self, vol, mnt, at)
            try:
                yield w
            finally:
                umount(mnt)
                vol.last_write = at + (time.time() - t0) + 1
        self.events.append(('mount', vol.vid, at, vol.last_write))

    def snapshot(self, vol, tag):
        vol.snapshots[tag] = read_volume(vol.device, vol.offset)
        return vol.snapshots[tag]

    def end_of_life(self, vol):
        if vol.alive:
            self.snapshot(vol, 'eol')
            vol.alive = False

    # ---- embedding: copy a whole volume image into a file of another volume
    def embed(self, inner, w, path, as_vhd=False):
        """Copy inner's image file into the mounted host (Writer w) at path."""
        self.end_of_life(inner)
        src = inner.device
        if as_vhd:
            vhd = src + '.vhd'
            run(['qemu-img', 'convert', '-f', 'raw', '-O', 'vpc', '-o', 'subformat=dynamic', src, vhd])
            src = vhd
        w.copyin(path, src)
        inner.embedded_in = (w.vol.vid, os.path.basename(path), 'vhd' if as_vhd else 'raw')
        self.events.append(('embed', inner.vid, w.vol.vid, path, as_vhd))

    # ---- finish
    def finish(self, scans, expect, notes=()):
        """scans: list of dicts {name, start, end}; expect: per scan name -> expectations (truth.py)."""
        for v in self.volumes.values():
            if v.alive:
                self.snapshot(v, 'eol')
        self.scans = scans
        self.expect = expect
        out = {
            'format': 'hdsi-build/1', 'scenario': self.name, 'image': os.path.basename(self.image),
            'image_size': os.path.getsize(self.image), 'ptable': self.ptable,
            'volumes': {k: dict(v.info(), manifest=v.manifest, dirs=v.dirs,
                                snapshots=v.snapshots) for k, v in self.volumes.items()},
            'order': self.order, 'events': self.events, 'scans': scans, 'expect': expect,
            'notes': list(notes), 'extra': self.extra,
        }
        with open(os.path.join(self.dir, 'build.json'), 'w') as f:
            json.dump(out, f)
        return out


def _free_checker(alloc):
    import bisect
    starts = [a for a, b in alloc]
    def free(c):
        i = bisect.bisect_right(starts, c) - 1
        return not (i >= 0 and alloc[i][0] <= c < alloc[i][1])
    return free


def blank_record(n, rs):
    rec = bytearray(rs)
    rec[0:4] = b'FILE'
    struct.pack_into('<HH', rec, 4, 0x30, rs // 512 + 1)
    struct.pack_into('<HHHH', rec, 0x10, 1, 0, 0x38, 0)
    struct.pack_into('<II', rec, 0x18, 0x40, rs)
    struct.pack_into('<I', rec, 0x2C, n)
    struct.pack_into('<I', rec, 0x38, 0xFFFFFFFF)
    usn = b'\x01\x00'
    rec[0x30:0x32] = usn
    for i in range(1, rs // 512 + 1):
        e = i * 512
        rec[0x30 + 2 * i:0x32 + 2 * i] = rec[e - 2:e]
        rec[e - 2:e] = usn
    return bytes(rec)


def umount(mnt):
    for _ in range(100):
        p = subprocess.run(['umount', mnt], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        if p.returncode == 0:
            break
        time.sleep(0.2)
    else:
        subprocess.run(['umount', '-l', mnt])
        raise RuntimeError('umount %s failed: %s' % (mnt, p.stdout))
    # wait until the ntfs-3g daemon has exited (it flushes the image on exit)
    for _ in range(200):
        out = subprocess.run(['pgrep', '-f', 'ntfs-3g .* ' + mnt + '$'], stdout=subprocess.PIPE,
                             text=True).stdout
        if not out.strip():
            break
        time.sleep(0.05)


class Writer:
    """File operations on a mounted volume; every created name carries the volume's tag."""

    def __init__(self, sc, vol, mnt, at):
        self.sc, self.vol, self.mnt, self.at = sc, vol, mnt, at

    def p(self, rel):
        return os.path.join(self.mnt, rel)

    def _check(self, rel):
        base = os.path.basename(rel)
        if not base.startswith(self.vol.tag):
            raise ValueError('name %r lacks the volume tag %r' % (base, self.vol.tag))
        return base

    def mkdir(self, rel):
        base = self._check(rel)
        os.mkdir(self.p(rel))
        self.vol.dirs[base] = rel

    def makedirs(self, rel):
        parts = rel.split('/')
        for i in range(1, len(parts) + 1):
            sub = '/'.join(parts[:i])
            if not os.path.isdir(self.p(sub)):
                self.mkdir(sub)

    def write(self, rel, size, seed=None):
        base = self._check(rel)
        ext = base.rsplit('.', 1)[1] if '.' in base else ''
        old = self.vol.manifest.get(base)
        version = (old['version'] + 1) if old else 0
        data = content(seed or ('%s|%s|%d' % (self.sc.seed, base, version)), size, ext)
        with open(self.p(rel), 'wb') as f:
            f.write(data)
        self.vol.manifest[base] = {'path': rel, 'sha256': hashlib.sha256(data).hexdigest(),
                                   'size': size, 'deleted': False, 'version': version}
        return data

    def write_big(self, rel, size, chunk=1 << 20):
        """A large file written in chunks (content seeded per chunk)."""
        base = self._check(rel)
        ext = base.rsplit('.', 1)[1] if '.' in base else ''
        h = hashlib.sha256()
        with open(self.p(rel), 'wb') as f:
            done = 0
            k = 0
            while done < size:
                n = min(chunk, size - done)
                data = content('%s|%s|%d' % (self.sc.seed, base, k), n, ext if k == 0 else '')
                f.write(data)
                h.update(data)
                done += n
                k += 1
        self.vol.manifest[base] = {'path': rel, 'sha256': h.hexdigest(), 'size': size, 'deleted': False,
                                   'version': 0}

    def copyin(self, rel, src):
        base = self._check(rel)
        h = hashlib.sha256()
        with open(src, 'rb') as fi, open(self.p(rel), 'wb') as fo:
            while True:
                b = fi.read(1 << 20)
                if not b:
                    break
                h.update(b)
                fo.write(b)
        self.vol.manifest[base] = {'path': rel, 'sha256': h.hexdigest(), 'size': os.path.getsize(src),
                                   'deleted': False, 'version': 0}

    def write_bytes(self, rel, data):
        base = self._check(rel)
        with open(self.p(rel), 'wb') as f:
            f.write(data)
        self.vol.manifest[base] = {'path': rel, 'sha256': hashlib.sha256(data).hexdigest(),
                                   'size': len(data), 'deleted': False, 'version': 0}

    def delete(self, rel):
        full = self.p(rel)
        if os.path.isdir(full):
            for root, dirs, files in os.walk(full, topdown=False):
                for fn in files:
                    os.unlink(os.path.join(root, fn))
                    if fn in self.vol.manifest:
                        self.vol.manifest[fn]['deleted'] = True
                for dn in dirs:
                    os.rmdir(os.path.join(root, dn))
            os.rmdir(full)
        else:
            os.unlink(full)
            base = os.path.basename(rel)
            if base in self.vol.manifest:
                self.vol.manifest[base]['deleted'] = True

    def rename(self, rel, newrel):
        nb = self._check(newrel)
        ob = os.path.basename(rel)
        os.rename(self.p(rel), self.p(newrel))
        if ob in self.vol.manifest:
            e = self.vol.manifest.pop(ob)
            e['path'] = newrel
            e.setdefault('old_names', []).append(ob)
            self.vol.manifest[nb] = e
        if ob in self.vol.dirs:
            self.vol.dirs.pop(ob)
            self.vol.dirs[nb] = newrel

    def exists(self, rel):
        return os.path.exists(self.p(rel))

    def free_bytes(self):
        s = os.statvfs(self.mnt)
        return s.f_bavail * s.f_frsize
