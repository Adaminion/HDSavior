"""Plain case: the lost volume's $MFT is heavily fragmented (free space punched into a comb before most
files were created).  Expected: one main instance despite dozens of MFT fragments."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident

DESCRIPTION = 'heavily fragmented $MFT, one file system'
RULES = 'D1 D4(a) D4(c) D5 F'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(160)
    v = sc.volume('lost', 'L_')
    sc.mkfs(v, T('2021-02-01 10:00:00'))
    with sc.mount(v, T('2021-02-02 10:00:00')) as w:
        d0, f0 = pop.tree(w, 'Early', 300, rnd, cnt, avg_kib=3)
        pop.fill_and_punch(w, rnd, chunk=65536, keep_every=2, reserve=6 << 20)
    with sc.mount(v, T('2021-03-01 10:00:00')) as w:
        for top in ('Late', 'More', 'Most'):
            pop.tree(w, top, 1500, rnd, cnt, avg_kib=1, exts=['txt', 'jpg', 'pdf', 'dat'])
    accident(sc, v)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main'}, 'lost', validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
