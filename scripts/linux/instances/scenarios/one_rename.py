"""Instance separation: ONE file system whose oldest files were renamed last (their MFT-change times are
the newest, at the lowest record numbers).  Expected: one instance, nothing older."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident, rename_history

DESCRIPTION = 'one file system, oldest files renamed last'
RULES = 'D2 D9'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(320)
    v = sc.volume('lost', 'L_')
    sc.mkfs(v, T('2021-01-10 10:00:00'))
    rename_history(sc, v, T('2021-01-11 10:00:00'), rnd, cnt)
    accident(sc, v)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main'}, 'lost', validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
