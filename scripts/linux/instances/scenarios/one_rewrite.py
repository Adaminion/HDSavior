"""Instance separation: ONE file system whose oldest tree was saved again file by file through
'write temp, rename over' (new records, old records freed).  Expected: one instance."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident, rewrite_history

DESCRIPTION = 'one file system, oldest tree re-saved through temp+rename'
RULES = 'D2 D9'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(320)
    v = sc.volume('lost', 'L_')
    sc.mkfs(v, T('2021-01-10 10:00:00'))
    rewrite_history(sc, v, T('2021-01-11 10:00:00'), rnd, cnt)
    accident(sc, v)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main'}, 'lost', validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
