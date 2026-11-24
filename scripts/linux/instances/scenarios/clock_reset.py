"""Red team (D2 changepoint): two generations on one partition; while the newer (lost) file system was
in use, the PC's clock was reset (dead CMOS battery, no network) to 2009-01-01 for one session in
which 300 files were added.  So the lost volume holds records whose MFT-change times are older than the
older generation's.  Expected: the lost volume is main, the tail of the older one is older, no mixing."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident, generation

DESCRIPTION = 'two generations, newer one with a session under a reset clock (2009)'
RULES = 'D2 D3 D4 D9 E5'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(400)
    g0 = sc.volume('older', 'O_')
    sc.mkfs(g0, T('2018-03-01 10:00:00'))
    generation(sc, g0, T('2018-03-02 10:00:00'), 3600, rnd, cnt, churn=0.05)
    g1 = sc.volume('lost', 'L_')
    sc.mkfs(g1, T('2021-01-10 10:00:00'))
    with sc.mount(g1, T('2021-01-11 10:00:00')) as w:
        d1, f1 = pop.tree(w, 'Photos', 700, rnd, cnt)
    with sc.mount(g1, T('2009-01-01 00:05:00')) as w:
        d2, f2 = pop.tree(w, 'Scans', 300, rnd, cnt)
    g1.last_write = T('2021-03-01 10:00:00')
    with sc.mount(g1, T('2021-03-01 10:00:00')) as w:
        d3, f3 = pop.tree(w, 'Work', 600, rnd, cnt)
    with sc.mount(g1, T('2022-05-01 10:00:00')) as w:
        pop.churn(w, f1 + f3, d1 + d3, 0.05, rnd, cnt)
    accident(sc, g1)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main', 'older': 'older'}, 'lost',
                 validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
