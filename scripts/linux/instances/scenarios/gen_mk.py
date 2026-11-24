"""Instance separation (reviewer case gm1): two generations, plain mkntfs re-formats without any wipe."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident, generation

DESCRIPTION = 'two generations, plain mkntfs (no wipe)'
RULES = 'D2 D3 D4 E5'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(400)
    g0 = sc.volume('older', 'O_')
    sc.mkfs(g0, T('2018-05-01 10:00:00'), windows=None)
    generation(sc, g0, T('2018-05-02 10:00:00'), 3000, rnd, cnt, churn=0.1)
    g1 = sc.volume('lost', 'L_')
    sc.mkfs(g1, T('2021-02-01 10:00:00'), windows=None)
    generation(sc, g1, T('2021-02-02 10:00:00'), 1200, rnd, cnt, churn=0.05)
    accident(sc, g1, windows=None)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main', 'older': 'older'}, 'lost',
                 validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
