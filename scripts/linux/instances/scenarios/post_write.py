"""Plain case: files written to (and partly deleted from) the new volume after the accident."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident, generation, ACCIDENT

DESCRIPTION = 'files written after the accident (live and post_format records, overwritten old files)'
RULES = 'C2 C3 C4 C5 G1'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(256)
    v = sc.volume('lost', 'L_')
    sc.mkfs(v, T('2021-06-01 10:00:00'))
    generation(sc, v, T('2021-06-02 10:00:00'), 1500, rnd, cnt, avg_kib=20, churn=0.05)
    cur = accident(sc, v)
    with sc.mount(cur, ACCIDENT + 1800) as w:
        d, f = pop.tree(w, 'new', 150, rnd, cnt, avg_kib=60)
    with sc.mount(cur, ACCIDENT + 7200) as w:
        for rel in rnd.sample(f, 40):
            w.delete(rel)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main'}, 'lost', validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
