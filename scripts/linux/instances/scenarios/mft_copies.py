"""Instance separation: copies of MFT blocks inside files of the volume (backup / shadow-copy store), one of
them taken before many of the copied files were renamed.  Expected: the copies are strays, not a tree."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident, generation


DESCRIPTION = 'copies of MFT blocks inside files (one stale: taken before renames)'
RULES = 'D6 D7 E6'


def grab(sc, v, snap, first, n):
    rs = snap['rs']
    mo = snap['boot']['mft_lcn'] * snap['cs']
    with open(v.device, 'rb') as f:
        f.seek(mo + first * rs)
        return f.read(n * rs)


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(256)
    v = sc.volume('lost', 'L_')
    sc.mkfs(v, T('2021-01-10 10:00:00'))
    generation(sc, v, T('2021-01-11 10:00:00'), 2000, rnd, cnt, churn=0.03)
    snap = sc.snapshot(v, 'copy-early')
    c3 = grab(sc, v, snap, 1500, 256)
    # rename most files whose records were just copied
    by_slot = {r['slot']: r for r in snap['records']}
    with sc.mount(v, T('2021-09-01 10:00:00')) as w:
        w.mkdir('L_backup')
        w.write_bytes('L_backup/L_mftcopy-early.bin', c3)
        names = [by_slot[n]['name'] for n in range(1500, 1756) if n in by_slot and by_slot[n]['in_use']
                 and not by_slot[n]['is_dir'] and by_slot[n]['name'] in v.manifest]
        for nm in names[:int(len(names) * 0.6)]:
            rel = v.manifest[nm]['path']
            d = rel.rsplit('/', 1)[0]
            w.rename(rel, d + '/' + nm.replace('L_', 'L_ren_', 1))
    snap = sc.snapshot(v, 'copy')
    c1 = grab(sc, v, snap, 300, 512)
    c2 = grab(sc, v, snap, 1100, 256)
    with sc.mount(v, T('2022-03-01 10:00:00')) as w:
        w.write_bytes('L_backup/L_mftcopy.bin', c1)
        w.write_bytes('L_backup/L_mftcopy2.bin', c2)
    accident(sc, v)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main'}, 'lost', validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
