"""Compare a catalog (the reference model's, or any implementation's exported in the same shape)
with a ground-truth file.  Produces per-check counts and a verdict.

Hard failures (verdict FAIL):
  class      a record expected live/post_format is not, or the reverse
  mix        a non-strays instance holds records of two file systems (foreign records)
  main       the main instance is not the expected file system
  kind       a file system's records sit in an instance of a kind it does not accept
  parent     a file or folder is attached to a different node than expected
  foreign    ... attached to a directory of another file system (the reviewers' worst case)
  lostname   a lost folder carries a name that no usable entry of its file system supplies
  geometry   cluster size or volume offset of a main/other file system wrong
  range      the lost-volume range of a main/other file system does not cover its files
  overwritten / older_generation   flag missing where the truth requires it, or set on an intact file
Soft findings (reported, verdict unaffected): lost folders left nameless although an entry exists,
records of a file system split off into strays, 'validated' different from an expectation of null.
"""
import collections


def catalog_from_model(m):
    """Export the reference model's result in the comparison shape."""
    cat = {'records': {}, 'instances': {}, 'lost': {}, 'main': None}
    for r in m.s.records:
        I = m.insts.get(r.inst) if r.inst is not None else None
        cat['records'][r.off] = {'class': r.cls if r.cls in ('live', 'post_format') else 'old',
                                 'inst': r.inst if r.cls == 'old' else None,
                                 'older_gen_mark': r.older_gen}
    for iid, I in m.insts.items():
        if not I.alive:
            continue
        g = I.geo
        cat['instances'][iid] = {
            'kind': I.kind, 'records': len(I.recs), 'label': I.label,
            'geometry': None if g is None else {'cluster_size': g.cs, 'volume_offset': g.off,
                                                'source': g.source, 'validated': bool(g.validated),
                                                'checked': g.checked, 'matched': g.matched, 'dirs': g.dirs,
                                                'lvr': list(g.lvr) if g.lvr else None,
                                                'total_clusters': g.total_clusters,
                                                'end_by_files': g.end_by_files},
            'embedded_in': None if not I.embedded_in else {
                'inst': I.embedded_in[0], 'container_off': I.embedded_in[1].base.off},
        }
    for g in m.files:
        cat['records'][g.base.off].update({'parent': list(g.parent) if g.parent else None,
                                           'flags': sorted(g.flags)})
    for key, L in m.lost.items():
        cat['lost'][key] = {'name': L['name'], 'parent': list(L['parent'])}
    cat['main'] = m.main.id if m.main else None
    cat['warnings'] = sorted(m.warnings)
    return cat


def _node_eq(a, b):
    if a is None or b is None:
        return a == b
    if a[0] == 'lost' and b[0] == 'lost':
        return list(a[-2:]) == list(b[-2:])
    return list(a) == list(b)


def compare(truth, cat):
    res = collections.OrderedDict()
    hard = collections.Counter()
    soft = collections.Counter()
    ex = collections.defaultdict(list)

    def note(kind, msg, is_hard=True):
        (hard if is_hard else soft)[kind] += 1
        if len(ex[kind]) < 6:
            ex[kind].append(msg)
    T = truth
    recs = {r['off']: r for r in T['records']}
    crec = cat['records']
    # ---- C3 classes
    for off, r in recs.items():
        c = crec.get(off)
        if c is None:
            continue
        if r['class'] != c['class']:
            note('class', '%d %s(%s) expected %s, got %s' % (off, r['name'], r['volume'], r['class'], c['class']))
    # ---- instance composition
    vol_insts = collections.defaultdict(collections.Counter)
    inst_vols = collections.defaultdict(collections.Counter)
    for off, r in recs.items():
        c = crec.get(off)
        if c is None or r['class'] != 'old' or c['class'] != 'old':
            continue
        iid = c['inst']
        if r['volume'] is not None and r['role'] == 'primary':
            vol_insts[r['volume']][iid] += 1
        inst_vols[iid][(r['volume'], r['role'] if r['role'] != 'primary' else '')] += 1
    insts = cat['instances']
    # majority file system per instance
    majority = {}
    for iid, cnt in inst_vols.items():
        prim = collections.Counter({k: n for k, n in cnt.items() if k[1] == ''})
        majority[iid] = prim.most_common(1)[0][0][0] if prim else None
        kind = insts.get(iid, {}).get('kind')
        if kind == 'strays':
            continue
        for (vid, role), n in cnt.items():
            if vid != majority[iid] or role == 'unknown':
                if role == 'artifact' and vid == majority[iid]:
                    continue
                note('mix', 'instance %s (%s, majority %s) holds %d %s records of %s' %
                     (iid, kind, majority[iid], n, role or 'primary', vid))
    # kinds
    for off, r in recs.items():
        c = crec.get(off)
        if c is None or r['class'] != 'old' or c['class'] != 'old' or r['kind'] is None:
            continue
        vacc = T['volumes'].get(r['volume'], {}).get('accept_kinds', [r['kind']])
        acc = vacc if r['role'] == 'primary' else ['strays'] + vacc
        got = insts.get(c['inst'], {}).get('kind')
        if got not in acc:
            if got == 'strays':
                note('split', '%d %s of %s in strays (expected %s)' % (off, r['name'], r['volume'], acc), False)
            else:
                note('kind', '%d %s (%s, %s) in %s instance %s, expected %s' %
                     (off, r['name'], r['volume'], r['role'], got, c['inst'], acc))
    # one file system spread over several non-strays instances
    for vid, cnt in vol_insts.items():
        parts = [iid for iid in cnt if insts.get(iid, {}).get('kind') not in ('strays', None)]
        if len(parts) > 1:
            note('split_instances', '%s is spread over instances %s' % (
                vid, ', '.join('%s(%s:%d)' % (i, insts[i]['kind'], cnt[i]) for i in parts)), False)
    # main
    if T.get('main') is not None:
        mi = cat.get('main')
        if mi is None or majority.get(mi) != T['main']:
            note('main', 'main instance %s holds %s, expected %s' % (mi, majority.get(mi), T['main']))
    # ---- placement
    vol_of_dir = {off: r['volume'] for off, r in recs.items()}
    n_par = 0
    for off, r in recs.items():
        if 'parent' not in r or r['parent'] == ['system']:
            continue
        c = crec.get(off)
        if c is None or c.get('parent') is None:
            continue
        n_par += 1
        e, g = r['parent'], c['parent']
        if _node_eq(e, g):
            continue
        if g[0] == 'dir' and vol_of_dir.get(g[1]) != r['volume']:
            note('foreign', '%d %s (%s) attached to dir @%d of %s, expected %s' %
                 (off, r['name'], r['volume'], g[1], vol_of_dir.get(g[1]), e))
        else:
            note('parent', '%d %s (%s): expected %s, got %s' % (off, r['name'], r['volume'], e, g))
    res['parents_checked'] = n_par
    # ---- lost folders
    # model lost folder for (volume, P, s): the instance that holds most of the volume's records
    vol_main_inst = {vid: cnt.most_common(1)[0][0] for vid, cnt in vol_insts.items() if cnt}
    clost = cat['lost']
    n_lost = 0
    for L in T['lost_folders']:
        vid = L['volume']
        iid = vol_main_inst.get(vid)
        key = (iid, L['key'][0], L['key'][1])
        cl = clost.get(key)
        if cl is None:
            continue
        n_lost += 1
        names = L['names']
        if cl['name'] is None:
            if names:
                note('lostname_missing', '%s #%d.%d: expected one of %s, got none' % (vid, L['key'][0], L['key'][1],
                                                                                  names), False)
        elif cl['name'] not in names:
            note('lostname', '%s #%d.%d: got %r, expected %s' % (vid, L['key'][0], L['key'][1], cl['name'],
                                                                 names or 'no name'))
        else:
            if not any(_node_eq(p, cl['parent']) for p in L['parents']):
                note('lostparent', '%s #%d.%d (%s): parent %s, expected %s' % (
                    vid, L['key'][0], L['key'][1], cl['name'], cl['parent'], L['parents']))
    res['lost_checked'] = n_lost
    # ---- geometry and ranges
    for vid, V in T['volumes'].items():
        if 'geometry' not in V or V['expected_kind'] not in ('main', 'other', 'older'):
            continue
        iid = vol_main_inst.get(vid)
        if iid is None:
            continue
        I = insts.get(iid)
        G = I.get('geometry') if I else None
        tg = V['geometry']
        if G is None:
            if V['expected_kind'] in ('main', 'other'):
                note('geometry', '%s: no geometry' % vid)
            continue
        if (G['cluster_size'], G['volume_offset']) != (tg['cluster_size'], tg['volume_offset']):
            note('geometry', '%s: got (%d, %d) %s, expected (%d, %d)' % (
                vid, G['cluster_size'], G['volume_offset'], G['source'], tg['cluster_size'], tg['volume_offset']),
                V['expected_kind'] in ('main', 'other'))
        if tg.get('validated') is not None and bool(G['validated']) != tg['validated']:
            note('validated', '%s: validated %s, expected %s (%d/%d checked, %d dirs)' % (
                vid, G['validated'], tg['validated'], G['matched'], G['checked'], G['dirs']))
        lv = V['lost_volume_range']
        if lv['must_cover'] and G['lvr'] and V['expected_kind'] in ('main', 'other'):
            a, b = lv['must_cover']
            if not (G['lvr'][0] <= a and b <= G['lvr'][1]):
                note('range', '%s: lost-volume range %s does not cover the files %s (true %s)' % (
                    vid, G['lvr'], lv['must_cover'], lv['true']))
            elif G['lvr'][1] > max(lv['true'][1], T['session_range'][1]):
                note('range_over', '%s: lost-volume range %s reaches beyond the volume %s' % (
                    vid, G['lvr'], lv['true']), False)
    # ---- flags
    for off, r in recs.items():
        f = r.get('flags')
        c = crec.get(off)
        if not f or c is None or c.get('flags') is None:
            continue
        got = set(c['flags'])
        if f['overwritten'] is True and 'overwritten' not in got:
            note('overwritten_missed', '%d %s (%s): damaged (%s), not flagged' % (off, r['name'], r['volume'],
                                                                                f.get('evidence')))
        if f['overwritten'] is False and 'overwritten' in got:
            note('overwritten_false', '%d %s (%s): intact, flagged overwritten' % (off, r['name'], r['volume']))
        if f.get('out_of_bounds') is True and 'out_of_bounds' not in got:
            note('oob_missed', '%d %s (%s): runs outside the session range, not out_of_bounds' % (
                off, r['name'], r['volume']))
        if f.get('out_of_bounds') is False and 'out_of_bounds' in got and r['kind'] in ('main', 'other'):
            note('oob_false', '%d %s (%s): every run inside the range, flagged out_of_bounds' % (
                off, r['name'], r['volume']))
        if f['older_generation'] is True and 'older_generation' not in got:
            note('older_missed', '%d %s (%s)' % (off, r['name'], r['volume']))
        if f['older_generation'] is False and 'older_generation' in got:
            note('older_false', '%d %s (%s)' % (off, r['name'], r['volume']))
    res['hard'] = dict(hard)
    res['soft'] = dict(soft)
    res['examples'] = dict(ex)
    res['verdict'] = 'PASS' if not hard else 'FAIL'
    res['composition'] = {vid: dict((str(k), v) for k, v in cnt.items()) for vid, cnt in vol_insts.items()}
    res['instances'] = {str(k): {'kind': v['kind'], 'records': v['records'], 'majority': majority.get(k),
                                 'geometry': v['geometry'] and {kk: v['geometry'][kk] for kk in
                                                                ('cluster_size', 'volume_offset', 'source',
                                                                 'validated', 'lvr')}}
                        for k, v in insts.items()}
    return res
