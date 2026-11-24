"""Ground truth for one scan of a scenario image (format 'hds-instance-truth/1', see README.md).

Derived from what the builder knows (build.json: every file system's end-of-life snapshot taken
through its own $MFT, the content manifests, the expectations the scenario states) and from the
final image.  Records are identified by their absolute disk offset; a record found by a scan is
attributed to the file system whose end-of-life snapshot holds the same bytes (FNV-1a 64 of the
raw record) at the same offset ('primary'), inside a container file ('primary' of an embedded
file system), or elsewhere ('copy', 'mirror').
"""
import bisect
import collections
import hashlib
import json
import os

from . import ntfsraw as nr

FORMAT = 'hds-instance-truth/1'


def _runs_abs(off, cs, data):
    out = []
    for svcn, evcn, runs, real, init, alloc, aflags in data:
        for lcn, ln in runs:
            if lcn is not None:
                out.append((off + lcn * cs, off + (lcn + ln) * cs))
    return out


class RS:
    def __init__(self, ranges):
        rs = sorted(r for r in ranges if r[1] > r[0])
        m = []
        for a, b in rs:
            if m and a <= m[-1][1]:
                m[-1][1] = max(m[-1][1], b)
            else:
                m.append([a, b])
        self.r = m
        self.s = [a for a, b in m]

    def hit(self, a, b):
        i = bisect.bisect_right(self.s, a) - 1
        if i >= 0 and self.r[i][1] > a:
            return True
        return i + 1 < len(self.r) and self.r[i + 1][0] < b


def make_truth(build_dir, scan_spec, scan_result):
    with open(os.path.join(build_dir, 'build.json')) as f:
        B = json.load(f)
    image = os.path.join(build_dir, B['image'])
    start, end = scan_spec['start'], scan_spec['end']
    exp = B['expect'][scan_spec['name']]
    vols = B['volumes']
    order = B['order']
    host_level = {k for k, v in vols.items() if v['device'] == B['image']}
    rank = {k: i for i, k in enumerate(order)}
    alive_at_end = {k for k, v in vols.items() if v['device'] == B['image'] and
                    not any(e[0] == 'mkfs' and e[1] != k and _overlap(vols[e[1]], v) and rank[e[1]] > rank[k]
                            for e in B['events'])}
    # current volumes per C1
    starts = {start}
    if start == 0 and B['ptable']:
        starts |= {p[0] * 512 for p in B['ptable']['parts']}
    current = [k for k in alive_at_end if vols[k]['offset'] in starts and start <= vols[k]['offset'] < end
               and not vols[k].get('embedded_in')]
    # hash index
    hidx = collections.defaultdict(list)
    for vid, v in vols.items():
        for tag, sn in v['snapshots'].items():
            for r in sn['records']:
                hidx[r['hash']].append((vid, tag, r))
    eol = {vid: {r['off']: r for r in v['snapshots']['eol']['records']} for vid, v in vols.items()
           if 'eol' in v['snapshots']}
    mirr = {}
    for vid, v in vols.items():
        if 'eol' in v['snapshots'] and vid in host_level:
            sn = v['snapshots']['eol']
            mirr[vid] = [(v['offset'] + l * sn['cs'], v['offset'] + (l + n) * sn['cs']) for l, n in sn['mirr_runs']]
    kinds = exp['kinds']
    # ------------------------------------------------------------------ embedded volumes: inner -> absolute
    maps = {}
    for vid, v in vols.items():
        emb_in = v.get('embedded_in')
        if not emb_in:
            continue
        hvid, cname, ckind = emb_in
        hs = vols[hvid]['snapshots'].get('eol')
        if hs is None:
            continue
        crec = [x for x in hs['records'] if x['name'] == cname and not x['base']]
        if not crec:
            continue
        crec = crec[-1]
        data = list(crec['data'])
        for x in hs['records']:
            if x['base'] == crec['slot'] and x['base']:
                data += x['data']
        segs = []
        for svcn, evcn, runs, real, init, alloc, aflags in sorted(data, key=lambda d: d[0]):
            vv = svcn
            for lcn, ln in runs:
                segs.append((vv * hs['cs'], (vv + ln) * hs['cs'], None if lcn is None else
                             vols[hvid]['offset'] + lcn * hs['cs']))
                vv += ln
        bat = None
        if ckind == 'vhd':
            bat = _vhd_bat(os.path.join(build_dir, os.path.basename(v['device']) + '.vhd'))
        maps[vid] = (segs, bat)

    def file_pos_to_abs(segs, pos):
        for a, b, base in segs:
            if a <= pos < b:
                return None if base is None else base + (pos - a)
        return None

    def inner_map(vid, inner_off):
        if vid not in maps:
            return None
        segs, bat = maps[vid]
        pos = inner_off
        if bat is not None:
            bs, table = bat
            blk, within = divmod(inner_off, bs)
            if blk >= len(table) or table[blk] == 0xFFFFFFFF:
                return None
            pos = table[blk] * 512 + 512 + within
        return file_pos_to_abs(segs, pos)

    def inner_unmap(vid, abs_off):
        if vid not in maps:
            return None
        segs, bat = maps[vid]
        pos = None
        for a, b, base in segs:
            if base is not None and base <= abs_off < base + (b - a):
                pos = a + (abs_off - base)
        if pos is None:
            return None
        if bat is not None:
            bs, table = bat
            for blk, sec in enumerate(table):
                if sec != 0xFFFFFFFF and sec * 512 + 512 <= pos < sec * 512 + 512 + bs:
                    return blk * bs + (pos - sec * 512 - 512)
            return None
        return pos
    # ------------------------------------------------------------------ records
    out_recs = []
    surv = collections.defaultdict(dict)            # vid -> num -> (scan record, snapshot record)
    label = {}
    for r in scan_result.records:
        cands = hidx.get(r.hash, [])
        role = 'unknown'
        vid = None
        snaprec = None
        prim = [c for c in cands if c[0] in host_level and c[1] == 'eol' and c[2]['off'] == r.off]
        if prim:
            prim.sort(key=lambda c: rank[c[0]])
            vid, _, snaprec = prim[-1]
            role = 'primary'
        else:
            emb = [c for c in cands if c[0] not in host_level and c[1] == 'eol']
            if emb:
                vid, _, snaprec = emb[-1]
                where = inner_map(vid, snaprec['off'])
                if where is None or where == r.off:
                    role = 'primary'
                else:
                    sn = vols[vid]['snapshots']['eol']
                    mr = [(l * sn['cs'], (l + n) * sn['cs']) for l, n in sn['mirr_runs']]
                    role = 'copy'
                    for c in emb:
                        if c[0] == vid:
                            pass
                    # is r a $MFTMirr copy (its inner position is inside the inner $MFTMirr)?
                    rel = inner_unmap(vid, r.off)
                    if rel is not None and any(a <= rel - vols[vid]['offset'] < b for a, b in mr):
                        role = 'mirror'
            else:
                cp = [c for c in cands if c[0] in host_level]
                if cp:
                    cp.sort(key=lambda c: (c[1] != 'eol', -rank[c[0]]))
                    vid, _, snaprec = cp[0]
                    role = 'mirror' if any(a <= r.off < b for a, b in mirr.get(vid, ())) else 'copy'
        if role == 'unknown':
            # bytes changed since the snapshot (e.g. a sector overwritten later): attribute by position
            for v2, sn2 in eol.items():
                x = sn2.get(r.off)
                if x is not None and x['num'] == r.num and v2 in host_level:
                    vid, snaprec = v2, x
                    role = 'mirror' if any(a <= r.off < b for a, b in mirr.get(v2, ())) else 'primary-torn'
                    break
            if role == 'unknown':
                for v2 in host_level:
                    if v2 in mirr and any(a <= r.off < b for a, b in mirr[v2]):
                        vid, role = v2, 'mirror'
        cls = 'old'
        if vid in current and role == 'primary':
            cls = 'live' if snaprec['used_bitmap'] else 'post_format'
            role = 'current-live' if cls == 'live' else 'current-deleted'
        elif vid in current and role == 'mirror':
            cls = 'live'
        if role == 'primary' and snaprec is not None and snaprec['num'] == 0 and snaprec['slot'] != 0:
            role = 'artifact'      # mkntfs leaves record number 0 in its never-used slots 16..23
        k = None
        if cls == 'old':
            if role == 'primary':
                k = kinds.get(vid)
            elif role in ('copy', 'mirror', 'artifact', 'primary-torn'):
                k = 'strays'
        rec = {'off': r.off, 'num': r.num, 'seq': r.seq, 'volume': vid, 'role': role, 'class': cls,
               'kind': k, 'in_use': r.in_use, 'dir': r.is_dir, 'base': r.base == 0,
               'name': (r.best_name().name if r.best_name() else None)}
        out_recs.append(rec)
        label[r.off] = rec
        if role == 'primary' and cls == 'old':
            surv[vid][r.num] = (r, snaprec)
    # ------------------------------------------------------------------ entries -> volumes (by tag)
    tags = sorted(((v['tag'], vid) for vid, v in vols.items()), key=lambda t: -len(t[0]))

    def vol_of_name(n):
        for t, vid in tags:
            if n.startswith(t):
                return vid
        return None
    cur_alloc = []
    for vid in current:
        sn = vols[vid]['snapshots']['eol']
        cur_alloc += [(vols[vid]['offset'] + a * sn['cs'], vols[vid]['offset'] + b * sn['cs']) for a, b in sn['alloc']]
    cur_rs = RS(cur_alloc)
    entries = collections.defaultdict(list)          # (vid, P, s) -> [entry]
    for e in scan_result.entries:
        vid = vol_of_name(e.name)
        if vid is None or vid in current:
            continue
        if e.src_kind == 'block' and cur_rs.hit(e.src_off, e.src_off + 1):
            continue
        entries[(vid, e.child, e.cseq)].append(e)
    for r in scan_result.records:
        lab = label.get(r.off)
        if lab and lab['role'] == 'copy' and r.in_use and r.is_dir and r.names:
            fn = r.best_name()
            e = nr.IndexEntry()
            e.child, e.cseq, e.parent, e.pseq, e.ns, e.name = r.num, r.seq, fn.parent, fn.pseq, fn.ns, fn.name
            e.src_off, e.src_lsn, e.src_kind, e.chtime, e.block, e.is_dir = r.off, r.lsn, 'stray', fn.chtime, None, True
            e.etime = fn.tmax
            entries[(lab['volume'], r.num, r.seq)].append(e)

    # ------------------------------------------------------------------ placement
    lost = {}

    def node_for(vid, P, s, depth=0):
        if P == 5:
            return ['root']
        if P == 11:
            return ['extend']
        t = surv[vid].get(P)
        if t is not None:
            tr, ts = t
            if tr.is_dir and nr.resolves(s, tr.in_use, tr.seq):
                return ['dir', tr.off]
        key = (vid, P, s)
        if key not in lost and depth < 64:
            es = entries.get(key, [])
            nondos = [e for e in es if e.ns != 2]
            if nondos:
                es = nondos
            lost[key] = {'volume': vid, 'key': [P, s], 'names': sorted({e.name for e in es}), 'parents': []}
            if es:
                pars = []
                for e in es:
                    pn = node_for(vid, e.parent, e.pseq, depth + 1)
                    if pn not in pars:
                        pars.append(pn)
                lost[key]['parents'] = pars
            else:
                lost[key]['parents'] = [['lostroot']]
        return ['lost', P, s]

    # ------------------------------------------------------------------ content and evidence
    f_img = open(image, 'rb')

    def read_runs(off, cs, data, size):
        h = hashlib.sha256()
        left = size
        for svcn, evcn, runs, real, init, alloc, aflags in sorted(data, key=lambda d: d[0]):
            for lcn, ln in runs:
                if left <= 0:
                    break
                n = min(left, ln * cs)
                if lcn is None:
                    h.update(bytes(n))
                else:
                    f_img.seek(off + lcn * cs)
                    h.update(f_img.read(n))
                left -= n
        return h.hexdigest()

    # every surviving file (all non-current host-level volumes): extents, intact?
    files = {}                                         # (vid, num) -> dict
    for vid, nums in surv.items():
        if vid not in host_level:
            continue
        v = vols[vid]
        sn = v['snapshots']['eol']
        cs = sn['cs']
        man = v['manifest']
        ext_by_base = collections.defaultdict(list)
        for num, (r, s) in nums.items():
            if s['base']:
                ext_by_base[s['base']].append(s)
        for num, (r, s) in nums.items():
            if s['base'] or s['is_dir']:
                continue
            data = list(s['data'])
            for x in ext_by_base.get(num, ()):
                data += x['data']
            name = s['name']
            m = man.get(name) if name else None
            intact = None
            if m is not None:
                if s['resident_data'] and not data:
                    intact = True
                elif data:
                    intact = read_runs(v['offset'], cs, data, m['size']) == m['sha256']
            files[(vid, num)] = {'abs': _runs_abs(v['offset'], cs, data), 'intact': intact,
                                 'in_use': s['in_use'], 'off': r.off, 'name': name}
    # evidence index: extents of other surviving intact files; records' own positions
    ext_list = []
    for key, fi in files.items():
        if fi['intact']:
            for a, b in fi['abs']:
                ext_list.append((a, b, key))
    ext_list.sort()
    ext_starts = [a for a, b, k in ext_list]
    rec_pos = sorted((r.off, label[r.off]['volume']) for r in scan_result.records
                     if label[r.off]['volume'] is not None and label[r.off]['role'] in ('primary', 'current-live',
                                                                                          'current-deleted'))
    rec_offs = [o for o, v in rec_pos]
    cur_ext = list(cur_alloc)
    for r in scan_result.records:
        lab = label[r.off]
        if lab['class'] == 'post_format':
            vid = lab['volume']
            sn = vols[vid]['snapshots']['eol']
            s = eol[vid].get(r.off)
            if s:
                cur_ext += _runs_abs(vols[vid]['offset'], sn['cs'], s['data'])
    cur_ext_rs = RS(cur_ext)

    def evidence(key, fi):
        for a, b in fi['abs']:
            if cur_ext_rs.hit(a, b):
                return 'current'
        for a, b in fi['abs']:
            i = bisect.bisect_left(ext_starts, b)
            for j in range(max(0, i - 400), i):
                a2, b2, k2 = ext_list[j]
                if k2 != key and a2 < b and a < b2:
                    return 'overlap'
            k0 = bisect.bisect_left(rec_offs, a)
            k1 = bisect.bisect_left(rec_offs, b)
            for j in range(k0, k1):
                if rec_pos[j][1] != key[0]:
                    return 'records-inside'
        return None

    flags = {}
    for key, fi in files.items():
        vid = key[0]
        ev = evidence(key, fi)
        if ev == 'current':
            ow = True
        elif fi['intact'] is True:
            ow = False
        elif fi['intact'] is False:
            ow = True if ev else None
        else:
            ow = None
        k = kinds.get(vid)
        k = k[0] if isinstance(k, list) else k
        og = True if k == 'older' else (False if k in ('main', 'other') else None)
        oob = any(a < start or b > end for a, b in fi['abs']) if fi['abs'] else False
        flags[fi['off']] = {'overwritten': ow, 'older_generation': og, 'out_of_bounds': oob, 'content':
                            None if fi['intact'] is None else ('intact' if fi['intact'] else 'damaged'),
                            'evidence': ev}
    # placement per primary old base record
    for rec in out_recs:
        if rec['role'] != 'primary' or rec['class'] != 'old' or not rec['base']:
            continue
        vid = rec['volume']
        kv = kinds.get(vid)
        kv = kv[0] if isinstance(kv, list) else kv
        if kv in ('embedded', 'strays', 'absent'):
            continue
        r, s = surv[vid][rec['num']]
        if rec['num'] < 16 or (rec['num'] < 24 and not s['names']):
            rec['parent'] = ['system']
            continue
        if not s['names']:
            continue
        P, ps = s['parent']
        rec['parent'] = node_for(vid, P, ps)
        if r.off in flags:
            rec['flags'] = flags[r.off]
    # ------------------------------------------------------------------ volumes
    vout = {}
    for vid, v in vols.items():
        if 'eol' not in v['snapshots']:
            continue
        sn = v['snapshots']['eol']
        k = kinds.get(vid, 'absent')
        acc = k if isinstance(k, list) else [k]
        n_surv = sum(1 for x in out_recs if x['volume'] == vid and x['role'] == 'primary' and x['class'] == 'old')
        d = {'tag': v['tag'], 'expected_kind': acc[0], 'accept_kinds': acc, 'records_surviving': n_surv,
             'device': v['device'], 'current': vid in current, 'format_time': sn['format_time_ft'],
             'format_utc': v['format_time'], 'cluster_size': sn['cs'], 'record_size': sn['rs'],
             'embedded_in': v.get('embedded_in')}
        if vid in host_level:
            bs = sn['boot']
            tr = [v['offset'], min(v['offset'] + bs['total_sectors'] * bs['bps'], B['image_size'])]
            ends = []
            lo_all = None
            for key, fi in files.items():
                if key[0] != vid or not fi['in_use'] or not fi['abs']:
                    continue
                hi = max(b for a, b in fi['abs'])
                lo = min(a for a, b in fi['abs'])
                ends.append(hi)
                lo_all = lo if lo_all is None else min(lo_all, lo)
            d['geometry'] = {'cluster_size': sn['cs'], 'volume_offset': v['offset'],
                             'validated': exp.get('validated', {}).get(vid)}
            d['lost_volume_range'] = {'true': tr, 'must_cover': [v['offset'], max(ends)] if ends else None,
                                      'protected': exp.get('protected', {}).get(vid)}
        vout[vid] = d
    f_img.close()
    lf = []
    for key, L in sorted(lost.items(), key=lambda kv: (kv[0][0], kv[0][1], kv[0][2])):
        lf.append(L)
    truth = {
        'format': FORMAT, 'scenario': B['scenario'], 'scan': scan_spec['name'],
        'description': exp.get('description', ''), 'image': B['image'], 'image_size': B['image_size'],
        'session_range': [start, end], 'partition_table': B['ptable'],
        'current_volumes': [{'volume': k, 'offset': vols[k]['offset'],
                             'format_time': vols[k]['snapshots']['eol']['format_time_ft']} for k in current],
        'main': exp.get('main'), 'volumes': vout, 'records': out_recs, 'lost_folders': lf,
        'notes': exp.get('notes', []) + B.get('notes', []),
    }
    return truth


def _vhd_bat(path):
    import struct
    with open(path, 'rb') as f:
        f.seek(-512, 2)
        foot = f.read(512)
        if foot[:8] != b'conectix':
            return None
        doff = struct.unpack_from('>Q', foot, 16)[0]
        f.seek(doff)
        dh = f.read(1024)
        table_off = struct.unpack_from('>Q', dh, 16)[0]
        n = struct.unpack_from('>I', dh, 28)[0]
        bs = struct.unpack_from('>I', dh, 32)[0]
        f.seek(table_off)
        raw = f.read(4 * n)
    return bs, list(struct.unpack('>%dI' % n, raw))


def _overlap(a, b):
    return a['device'] == b['device'] and a['offset'] < b['offset'] + b['length'] and \
        b['offset'] < a['offset'] + a['length']


def write_truth(t, path):
    """One member per line, records one per line: diff-friendly and stable."""
    with open(path, 'w') as f:
        f.write('{\n')
        keys = [k for k in t if k not in ('records', 'lost_folders')]
        for k in keys:
            f.write('%s: %s,\n' % (json.dumps(k), json.dumps(t[k], sort_keys=True)))
        f.write('"lost_folders": [\n')
        f.write(',\n'.join(json.dumps(x, sort_keys=True) for x in t['lost_folders']))
        f.write('\n],\n"records": [\n')
        f.write(',\n'.join(json.dumps(x, sort_keys=True) for x in t['records']))
        f.write('\n]\n}\n')
