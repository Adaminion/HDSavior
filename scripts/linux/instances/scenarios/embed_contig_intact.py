"""Instance separation (C2 verdict mk_embed2): a small contiguous disk-image file on the old volume.  The
image file must not be rated overwritten and its contents must appear as an embedded instance."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident

DESCRIPTION = 'contiguous small disk-image file on the lost volume'
RULES = 'C1 D7 G1'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    gimg = sc.aux_image('inner.raw', 8)
    g = sc.volume('inner', 'I_', device=gimg)
    sc.mkfs(g, T('2021-02-01 10:00:00'), windows=None)
    with sc.mount(g, T('2021-02-01 11:00:00')) as w:
        w.mkdir('I_guest')
        for i in range(40):
            w.write('I_guest/I_inner_%02d.dat' % i, 30000)
    sc.disk(160)
    h = sc.volume('lost', 'L_')
    sc.mkfs(h, T('2021-02-01 12:00:00'))
    with sc.mount(h, T('2021-02-02 10:00:00')) as w:
        pop.tree(w, 'Early', 200, rnd, cnt, avg_kib=3)
        w.mkdir('L_vm')
        sc.embed(g, w, 'L_vm/L_disk.img')
        w.mkdir('L_photos')
        for i in range(150):
            w.write('L_photos/L_outer_%03d.jpg' % i, 60000)
    accident(sc, h)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main', 'inner': 'embedded'}, 'lost',
                 validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
