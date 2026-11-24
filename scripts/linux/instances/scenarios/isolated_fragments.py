"""D4 step 4: the lost volume's $MFT grew in holes far from its start, and the files in those fragments all
live in folders created first (records below 256, destroyed by the accident).  No reference can join the
fragments to main; only content (b) or equal validated geometry (d) can.
Expected: one main instance."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident

DESCRIPTION = '$MFT fragments with no surviving parent: joined only by content or geometry'
RULES = 'D4(b) D4(d) D5 F3 F6'


def build(sc, resident=False):
    rnd = random.Random(sc.seed)
    sc.disk(200)
    g1 = sc.volume('lost', 'L_')
    sc.mkfs(g1, T('2021-02-01 10:00:00'))
    with sc.mount(g1, T('2021-02-02 10:00:00')) as w:
        early = ['L_Early%02d' % i for i in range(40)]
        for d in early:
            w.mkdir(d)
        pop.fill_and_punch(w, rnd, chunk=1 << 20, keep_every=2, reserve=4 << 20)
    with sc.mount(g1, T('2021-03-01 10:00:00')) as w:
        for i in range(3000):
            size = rnd.randint(300, 600) if resident else rnd.randint(3000, 20000)
            w.write('%s/L_late_%05d.%s' % (rnd.choice(early), i, rnd.choice(['jpg', 'pdf', 'png'])), size)
    accident(sc, g1)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main'}, 'lost', validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
