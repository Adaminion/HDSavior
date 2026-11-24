"""Instance separation: the older tail is larger (records and data) than the newer generation.
Expected: the newer generation is main."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident, generation

DESCRIPTION = 'older tail larger than the newer generation'
RULES = 'D2 D8 D9 F'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(400)
    g0 = sc.volume('older', 'O_')
    sc.mkfs(g0, T('2018-03-01 10:00:00'))
    generation(sc, g0, T('2018-03-02 10:00:00'), 3500, rnd, cnt, avg_kib=30, churn=0.05)
    g1 = sc.volume('lost', 'L_')
    sc.mkfs(g1, T('2021-06-01 10:00:00'))
    generation(sc, g1, T('2021-06-02 10:00:00'), 1100, rnd, cnt, avg_kib=3, churn=0.02, tops=['Notes', 'Mail'])
    accident(sc, g1)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main', 'older': 'older'}, 'lost',
                 validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
