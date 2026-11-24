"""Plain case / D4(c): the old volume's $MFT is fragmented and the accident used 64 KiB clusters, so the new
$MFT moved and the old record 0 ($MFT, with the runlist of every fragment) survives.  Expected: one
main instance; D4(c) may join the fragments through the old runlist."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident

DESCRIPTION = 'fragmented old $MFT whose record 0 survives (cluster change moved the new $MFT)'
RULES = 'D4(c) F2(mft) F6'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(160)
    v = sc.volume('lost', 'L_')
    sc.mkfs(v, T('2021-02-01 10:00:00'), windows=None)
    with sc.mount(v, T('2021-02-02 10:00:00')) as w:
        pop.tree(w, 'Early', 300, rnd, cnt, avg_kib=3)
        pop.fill_and_punch(w, rnd, chunk=65536, keep_every=2, reserve=6 << 20)
    with sc.mount(v, T('2021-03-01 10:00:00')) as w:
        for top in ('Late', 'More', 'Most'):
            pop.tree(w, top, 1500, rnd, cnt, avg_kib=1, exts=['txt', 'jpg', 'pdf', 'dat'])
    accident(sc, v, cluster=65536, windows=None)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main'}, 'lost', validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
