"""Instance separation: an older generation, then the lost generation with a 'delete the oldest folder,
copy a new one' history.  Expected: lost generation = main (one instance), the tail of the older = older."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident, generation, replace_history

DESCRIPTION = 'two generations, replace history inside the newer one'
RULES = 'D2 D3 D4 D8 D9 E5 G2'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(400)
    g0 = sc.volume('older', 'O_')
    sc.mkfs(g0, T('2018-03-01 10:00:00'))
    generation(sc, g0, T('2018-03-02 10:00:00'), 3600, rnd, cnt, churn=0.05)
    g1 = sc.volume('lost', 'L_')
    sc.mkfs(g1, T('2021-01-10 10:00:00'))
    replace_history(sc, g1, T('2021-01-11 10:00:00'), rnd, cnt)
    accident(sc, g1)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main', 'older': 'older'}, 'lost',
                 validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
