#!/usr/bin/env python3
"""Rebuild the instance-separation scenarios and run the reference model on them.

    run-all.py                     build what is missing, evaluate everything, print the table
    run-all.py --rebuild           rebuild every scenario first
    run-all.py --only a,b          restrict to these scenarios
    run-all.py --fixes all         apply every proposed rule change of the first lab (or a comma list of fix names)
    run-all.py --fixes all,red     ... plus the red team's; 'all,red,-name' leaves one out
    run-all.py --set name=value    override a model parameter (repeatable), e.g. --set embed_tol=0.5
    run-all.py --sweep             threshold sensitivity table
    run-all.py --details           print the examples of every failing check
    run-all.py --json out.json     machine-readable results

Images and truth files go to $HDSI_LAB/scenarios/<name>/ (default: the session scratchpad).  Needs root,
mkntfs, ntfs-3g, losetup, sfdisk, qemu-img and libfaketime.
"""
import argparse
import json
import os
import sys
import time
import traceback
from concurrent.futures import ProcessPoolExecutor, as_completed

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

from hdsi import runner, model as hm, build as hb  # noqa: E402

FIXES = ['d4c-self', 'd6-copy-ctime', 'd4-older-side', 'd7-oldest', 'f5-hull', 'e6-match', 'e6b-copyset']
RED = ['d6-anchor', 'd8-newest-cv', 'd7-volstart', 'd4c-size', 'e6-floor', 'd2-fallback', 'f5-few']


def _build_one(name, out):
    try:
        return name, runner.build(name, out), None
    except Exception:
        return name, None, traceback.format_exc()


def parse_value(v):
    for f in (int, float):
        try:
            return f(v)
        except ValueError:
            pass
    if v in ('True', 'False'):
        return v == 'True'
    return v


def make_params(fixes, sets):
    p = hm.Params()
    if fixes:
        out = set()
        for x in fixes.split(','):
            if x == 'all':
                out |= set(FIXES)
            elif x == 'red':
                out |= set(RED)
            elif x.startswith('-'):
                out.discard(x[1:])
            elif x:
                out.add(x)
        p.fixes = out
    for s in sets:
        k, v = s.split('=', 1)
        setattr(p, k, parse_value(v))
    return p


def _eval_one(n, params, out, write):
    rows = []
    try:
        for scan, res, m, t in runner.evaluate(n, params, out, write=write):
            res.pop('log', None)
            rows.append((n, scan, res))
    except Exception:
        rows.append((n, '-', {'verdict': 'ERROR', 'hard': {}, 'soft': {}, 'examples': {},
                              'error': traceback.format_exc()}))
    return rows


JOBS = 3


def evaluate_all(names, params, out, details=False, quiet=False, write=False):
    rows = []
    if JOBS <= 1:
        for n in names:
            rows += _eval_one(n, params, out, write)
        return rows
    with ProcessPoolExecutor(JOBS) as ex:
        futs = [ex.submit(_eval_one, n, params, out, write) for n in names]
        for f in futs:
            rows += f.result()
    return rows


def table(rows, details=False):
    w = max(len('%s.%s' % (n, s)) for n, s, r in rows) + 2
    print('%-*s %-6s %s' % (w, 'scenario.scan', 'result', 'hard failures | soft findings'))
    for n, s, r in rows:
        hard = ' '.join('%s=%d' % kv for kv in sorted(r.get('hard', {}).items()))
        soft = ' '.join('%s=%d' % kv for kv in sorted(r.get('soft', {}).items()))
        print('%-*s %-6s %s%s%s' % (w, '%s.%s' % (n, s), r['verdict'], hard, ' | ' if soft else '', soft))
        if r['verdict'] == 'ERROR':
            print(r['error'])
        if details and (r.get('hard') or r.get('soft')):
            for k, v in r.get('examples', {}).items():
                for x in v:
                    print('      %-18s %s' % (k, x))
    npass = sum(1 for n, s, r in rows if r['verdict'] == 'PASS')
    print('%d of %d scans pass' % (npass, len(rows)))


SWEEP = [
    ('min_records', [8, 12, 16, 24, 32]),
    ('ref_ratio', [0.7, 0.8, 0.9, 0.95, 1.0]),
    ('content_ratio', [0.6, 0.7, 0.8, 0.9]),
    ('contra_ratio', [0.1, 0.2, 0.3, 0.5]),
    ('contra_refs_min', [1, 2, 3, 5, 10]),
    ('mftmap_min', [4, 8, 16, 32]),
    ('copy_ratio', [0.7, 0.8, 0.9, 0.95, 1.0]),
    ('embed_ratio', [0.7, 0.8, 0.9, 0.95, 1.0]),
    ('embed_tol', [0.0, 0.5, 1.0, 2.0, 7.0]),
    ('cut_min_hi', [1, 2, 3, 5]),
    ('f5_rank', [1, 2, 4, 8]),
    ('f6_ratio', [0.6, 0.7, 0.8, 0.9]),
    ('f6_dirs', [1, 2, 3, 5]),
    ('f4_min_checked', [5, 10, 20, 40]),
]


def sweep(names, base_fixes, out):
    base = evaluate_all(names, make_params(base_fixes, []), out)
    base_v = {(n, s): r['verdict'] for n, s, r in base}
    print('baseline: %d of %d pass' % (sum(1 for v in base_v.values() if v == 'PASS'), len(base_v)))
    for pname, values in SWEEP:
        line = []
        for v in values:
            p = make_params(base_fixes, ['%s=%s' % (pname, v)])
            rows = evaluate_all(names, p, out)
            flips = ['%s.%s:%s' % (n, s, r['verdict']) for n, s, r in rows if r['verdict'] != base_v.get((n, s))]
            npass = sum(1 for n, s, r in rows if r['verdict'] == 'PASS')
            line.append('%s=%s: %d pass%s' % (pname, v, npass, (' (' + ', '.join(flips) + ')') if flips else ''))
        print('\n'.join(line))
        sys.stdout.flush()


def main():
    global JOBS
    ap = argparse.ArgumentParser()
    ap.add_argument('--rebuild', action='store_true')
    ap.add_argument('--only', default='')
    ap.add_argument('--skip', default='')
    ap.add_argument('--jobs', type=int, default=3)
    ap.add_argument('--fixes', default='')
    ap.add_argument('--set', action='append', default=[])
    ap.add_argument('--sweep', action='store_true')
    ap.add_argument('--details', action='store_true')
    ap.add_argument('--json', default='')
    ap.add_argument('--out', default=None)
    ap.add_argument('--no-eval', action='store_true')
    a = ap.parse_args()
    names = runner.scenario_names()
    if a.only:
        names = [n for n in names if n in a.only.split(',')]
    if a.skip:
        names = [n for n in names if n not in a.skip.split(',')]
    todo = [n for n in names if a.rebuild or not os.path.exists(os.path.join(runner.scan_dir(n, a.out),
                                                                             'build.json'))]
    if todo:
        t0 = time.time()
        with ProcessPoolExecutor(a.jobs) as ex:
            futs = [ex.submit(_build_one, n, a.out) for n in todo]
            for f in as_completed(futs):
                n, dt, err = f.result()
                if err:
                    print('BUILD FAILED %s\n%s' % (n, err))
                else:
                    print('built %-24s %6.1fs' % (n, dt))
        print('builds done in %.0fs' % (time.time() - t0))
    if a.no_eval:
        return
    if a.sweep:
        JOBS = a.jobs
        evaluate_all(names, make_params('', []), a.out, write=True)
        sweep(names, a.fixes, a.out)
        return
    JOBS = a.jobs
    rows = evaluate_all(names, make_params(a.fixes, a.set), a.out, write=True)
    table(rows, a.details)
    if a.json:
        with open(a.json, 'w') as f:
            json.dump([{'scenario': n, 'scan': s, 'verdict': r['verdict'], 'hard': r.get('hard'),
                        'soft': r.get('soft'), 'examples': r.get('examples'),
                        'instances': r.get('instances'), 'composition': r.get('composition')}
                       for n, s, r in rows], f, indent=1, default=str)


if __name__ == '__main__':
    main()
