"""Instance separation: the OLDER generation's $MFT is fragmented (many runs elsewhere on the disk); the
lost generation sits on top of its first fragment.  The older fragments must not join main."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident, generation

DESCRIPTION = 'older generation with a fragmented $MFT under the lost generation'
RULES = 'D2 D3 D4 D9'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(200)
    g0 = sc.volume('older', 'O_')
    sc.mkfs(g0, T('2018-05-01 10:00:00'))
    with sc.mount(g0, T('2018-05-02 10:00:00')) as w:
        pop.tree(w, 'Base', 300, rnd, cnt, avg_kib=3)
        pop.fill_and_punch(w, rnd, chunk=65536, keep_every=2, reserve=6 << 20)
    with sc.mount(g0, T('2018-06-01 10:00:00')) as w:
        for top in ('Late', 'More'):
            pop.tree(w, top, 1500, rnd, cnt, avg_kib=1, exts=['txt', 'jpg', 'pdf'])
    g1 = sc.volume('lost', 'L_')
    sc.mkfs(g1, T('2021-02-01 10:00:00'))
    generation(sc, g1, T('2021-02-02 10:00:00'), 900, rnd, cnt, avg_kib=8, churn=0.05)
    accident(sc, g1)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main', 'older': ['older', 'strays']}, 'lost',
                 validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
