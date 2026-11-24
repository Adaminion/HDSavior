"""Plain case: old volume 4 KiB clusters, accident with 8 KiB clusters (same $MFT position, wipe)."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident, generation

DESCRIPTION = 'cluster size change 4 KiB -> 8 KiB'
RULES = 'F1 F2 F4 D8'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(256)
    v = sc.volume('lost', 'L_', cluster=4096)
    sc.mkfs(v, T('2021-05-01 10:00:00'))
    generation(sc, v, T('2021-05-02 10:00:00'), 1500, rnd, cnt, churn=0.05)
    accident(sc, v, cluster=8192)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main'}, 'lost', validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
