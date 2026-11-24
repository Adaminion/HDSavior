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
