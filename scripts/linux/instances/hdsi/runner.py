"""Build scenarios, run the reference model on every scan, write ground truth, compare."""
import importlib
import json
import os
import sys
import time
import traceback

from . import build as hb
from . import compare as hc
from . import model as hm
from . import ntfsraw as nr
from . import scan as hs
from . import truth as ht

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def scenario_names():
    d = os.path.join(HERE, 'scenarios')
    return sorted(f[:-3] for f in os.listdir(d) if f.endswith('.py') and not f.startswith('_'))


def load(name):
    for d in (HERE, os.path.join(HERE, 'scenarios')):
        if d not in sys.path:
            sys.path.insert(0, d)
    return importlib.import_module(name)


def build(name, out=None):
    mod = load(name)
    sc = hb.Scenario(name, out=out)
    t0 = time.time()
    mod.build(sc)
    return time.time() - t0


def scan_dir(name, out=None):
    return os.path.join(out or os.path.join(hb.LAB, 'scenarios'), name)


def evaluate(name, params=None, out=None, write=True, scan_time=None):
    d = scan_dir(name, out)
    with open(os.path.join(d, 'build.json')) as f:
        B = json.load(f)
    img = os.path.join(d, B['image'])
    results = []
    st = scan_time or nr.unix_to_ft(time.time())
    for spec in B['scans']:
        s = hs.scan(img, spec['start'], spec['end'], cache_dir=d)
        rd = hs.RangeReader(img, spec['start'], spec['end'])
        m = hm.Model(s, rd, params or hm.Params(), scan_time=st).run()
        cat = hc.catalog_from_model(m)
        tpath = os.path.join(d, '%s.%s.truth.json' % (name, spec['name']))
        if write or not os.path.exists(tpath):
            t = ht.make_truth(d, spec, s)
            ht.write_truth(t, tpath)
        else:
            with open(tpath) as f:
                t = json.load(f)
        res = hc.compare(t, cat)
        res['scan'] = spec['name']
        res['outside_reads'] = rd.outside
        res['log'] = m.log
        res['warnings'] = sorted(m.warnings)
        res['main_why'] = m.main_why
        rd.close()
        results.append((spec['name'], res, m, t))
    return results
