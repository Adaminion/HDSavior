"""E6 block attribution: host and image volume with the SAME history (same shape, churn, then new folders
that reuse freed low record numbers), so index blocks of both have owners and children at equal
(number, sequence).  Lost host folders must get host names only."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident

DESCRIPTION = 'two volumes with the same history: equal owners and children in index blocks'
RULES = 'E6 F2'


def history(sc, vol, t0):
    rnd = random.Random('same-history')
    cnt = pop.Counter()
    with sc.mount(vol, t0) as w:
        d0, f0 = pop.tree(w, 'Early', 200, rnd, cnt, avg_kib=2)
        dirs, files = [], []
        for top in ('Users', 'Photos', 'Projects'):
            d, f = pop.tree(w, top, 500, rnd, cnt, avg_kib=3)
            dirs += d
            files += f
    with sc.mount(vol, t0 + 86400 * 30) as w:
        for rel in f0:
            w.delete(rel)                    # frees low record numbers (64..264)
    with sc.mount(vol, t0 + 86400 * 31) as w:
        # new folders inside late (high-numbered) folders take the freed low numbers
        tag = vol.tag
        for i, d in enumerate(dirs[-60:]):
            sub = '%s/%sNew_d%04d' % (d, tag, i)
            w.mkdir(sub)
            for j in range(3):
                w.write('%s/%sNew_f%04d_%d.jpg' % (sub, tag, i, j), 4000)


def build(sc):
    gimg = sc.aux_image('guest.raw', 96)
    g = sc.volume('guest', 'G_', device=gimg)
    sc.mkfs(g, T('2020-02-01 10:00:00'), windows=None)
    history(sc, g, T('2020-02-02 10:00:00'))
    sc.disk(320)
    h = sc.volume('lost', 'L_')
    sc.mkfs(h, T('2021-01-10 10:00:00'))
    history(sc, h, T('2021-01-11 10:00:00'))
    with sc.mount(h, T('2021-05-01 10:00:00')) as w:
        w.mkdir('L_VM')
        sc.embed(g, w, 'L_VM/L_guest.img')
    accident(sc, h)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main', 'guest': 'embedded'}, 'lost',
                 validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
