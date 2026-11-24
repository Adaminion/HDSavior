"""Plain/geometry case (reviewer E7): no index blocks (every folder holds at most 4 entries, all resident)
and no file type with a signature.  Expected: right geometry (range start), not validated."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident

DESCRIPTION = 'no index blocks and no known file types'
RULES = 'F1 F2 F4 F6'


def build(sc):
    rnd = random.Random(sc.seed)
    sc.disk(256)
    v = sc.volume('lost', 'L_')
    sc.mkfs(v, T('2021-01-10 10:00:00'))
    k = [0]
    with sc.mount(v, T('2021-01-11 10:00:00')) as w:
        def rec(d, lvl):
            if lvl == 3:
                for i in range(4):
                    k[0] += 1
                    w.write('%s/L_f%05d.%s' % (d, k[0], rnd.choice(pop.EXT_UNKNOWN)), rnd.randint(5000, 20000))
                return
            for i in range(4):
                k[0] += 1
                sub = '%s/L_d%05d' % (d, k[0])
                w.mkdir(sub)
                rec(sub, lvl + 1)
        for top in ('L_a', 'L_b', 'L_c'):
            w.mkdir(top)
            rec(top, 0)
    accident(sc, v)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main'}, 'lost', validated={'lost': False})
    sc.finish([s], {'whole': e})
