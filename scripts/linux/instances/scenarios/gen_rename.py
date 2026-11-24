"""Instance separation: an older generation, then the lost generation whose oldest files were renamed last."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident, generation, rename_history

DESCRIPTION = 'two generations, oldest files of the newer one renamed last'
RULES = 'D2 D3 D4 D8 D9'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(400)
    g0 = sc.volume('older', 'O_')
    sc.mkfs(g0, T('2018-03-01 10:00:00'))
    generation(sc, g0, T('2018-03-02 10:00:00'), 3600, rnd, cnt, churn=0.05)
    g1 = sc.volume('lost', 'L_')
    sc.mkfs(g1, T('2021-01-10 10:00:00'))
    rename_history(sc, g1, T('2021-01-11 10:00:00'), rnd, cnt)
    accident(sc, g1)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main', 'older': 'older'}, 'lost',
                 validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
