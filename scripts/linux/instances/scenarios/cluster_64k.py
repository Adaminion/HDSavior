"""Plain case: old 4 KiB clusters, accident with 64 KiB clusters.  mkntfs then puts the new $MFT at
128 KiB instead of 16 KiB, so old records 0..111 (old $MFT, root) survive and the new $MFT covers old
slots 112..138; no Windows wipe (the new $MFT is not where the old one was)."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident, generation

DESCRIPTION = 'cluster size change 4 KiB -> 64 KiB, $MFT moved, old record 0 survives'
RULES = 'F1 F2(mft) F4 D4(c) D8'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(256)
    v = sc.volume('lost', 'L_', cluster=4096)
    sc.mkfs(v, T('2021-05-01 10:00:00'), windows=None)
    generation(sc, v, T('2021-05-02 10:00:00'), 1500, rnd, cnt, churn=0.05)
    accident(sc, v, cluster=65536, windows=None)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main'}, 'lost', validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
