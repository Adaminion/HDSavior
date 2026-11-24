"""Instance separation: three generations on one partition (each format put the $MFT at the same place)."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident, generation

DESCRIPTION = 'three generations on one partition'
RULES = 'D2 D3 D4 D9'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(400)
    g0 = sc.volume('gen0', 'G0_')
    sc.mkfs(g0, T('2017-02-01 10:00:00'))
    generation(sc, g0, T('2017-02-02 10:00:00'), 3000, rnd, cnt, churn=0.05)
    g1 = sc.volume('gen1', 'G1_')
    sc.mkfs(g1, T('2019-02-01 10:00:00'))
    generation(sc, g1, T('2019-02-02 10:00:00'), 1600, rnd, cnt, churn=0.05)
    g2 = sc.volume('lost', 'L_')
    sc.mkfs(g2, T('2021-02-01 10:00:00'))
    generation(sc, g2, T('2021-02-02 10:00:00'), 800, rnd, cnt, churn=0.05)
    accident(sc, g2)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main', 'gen1': 'older', 'gen0': ['older', 'strays']}, 'lost',
                 validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
