"""D4(a) threshold probe: fragmented $MFT plus heavy folder churn.  Whole folders are deleted and new folders
created after a remount, so freed folder slots are reused by new folders and the parent references of
the deleted files left behind no longer resolve.  Expected: one main instance."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident

DESCRIPTION = 'fragmented $MFT with heavy folder churn (unresolvable references of deleted files)'
RULES = 'D4(a) thresholds'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(200)
    v = sc.volume('lost', 'L_')
    sc.mkfs(v, T('2021-02-01 10:00:00'))
    with sc.mount(v, T('2021-02-02 10:00:00')) as w:
        pop.tree(w, 'Early', 300, rnd, cnt, avg_kib=3)
        pop.fill_and_punch(w, rnd, chunk=65536, keep_every=2, reserve=8 << 20)
    t = T('2021-03-01 10:00:00')
    tops = []
    with sc.mount(v, t) as w:
        for k in range(8):
            top = 'R0_%d' % k
            pop.tree(w, top, 250, rnd, cnt, avg_kib=2, depth=2, per_dir=(5, 15))
            tops.append(w.vol.tag + top)
    for rnd_i in range(1, 7):
        t += 86400 * 20
        with sc.mount(v, t) as w:
            for top in rnd.sample(tops, 2):           # two whole folders deleted
                tops.remove(top)
                w.delete(top)
        t += 86400
        with sc.mount(v, t) as w:                     # a smaller one copied in: takes the lowest freed
            top = 'R%d_new' % rnd_i                   # slots, i.e. the deleted folders' own slots
            pop.tree(w, top, 60, rnd, cnt, avg_kib=2, depth=2, per_dir=(3, 8), fan=(2, 4))
            tops.append(w.vol.tag + top)
    accident(sc, v)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main'}, 'lost', validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
