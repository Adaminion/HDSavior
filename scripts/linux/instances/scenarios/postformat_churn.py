"""Instance separation: files created and deleted after the accident (60 copied in, all deleted)."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident, generation, ACCIDENT

DESCRIPTION = 'files created and deleted after the accident'
RULES = 'C3 C5 D2 D9'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(256)
    v = sc.volume('lost', 'L_')
    sc.mkfs(v, T('2021-03-01 10:00:00'))
    generation(sc, v, T('2021-03-02 10:00:00'), 1500, rnd, cnt, avg_kib=20, churn=0.05)
    cur = accident(sc, v)
    with sc.mount(cur, ACCIDENT + 600) as w:
        w.mkdir('C_copy')
        names = []
        for i in range(60):
            rel = 'C_copy/C_tmp%03d.jpg' % i
            w.write(rel, 200000)
            names.append(rel)
    with sc.mount(cur, ACCIDENT + 1200) as w:
        w.delete('C_copy')
    s, e = whole(sc, DESCRIPTION, {'lost': 'main'}, 'lost', validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
