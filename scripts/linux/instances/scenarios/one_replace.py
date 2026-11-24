"""Instance separation: ONE file system with a 'delete the oldest folder, copy a new one' history.
Expected: one instance, nothing older (a time-only cut would split it)."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident, replace_history

DESCRIPTION = "one file system, 'delete the oldest folder, copy a new one'"
RULES = 'D2 D9'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(320)
    v = sc.volume('lost', 'L_')
    sc.mkfs(v, T('2021-01-10 10:00:00'))
    replace_history(sc, v, T('2021-01-11 10:00:00'), rnd, cnt, pre=170)
    accident(sc, v)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main'}, 'lost', validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
