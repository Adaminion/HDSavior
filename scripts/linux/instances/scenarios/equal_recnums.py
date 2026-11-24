"""Instance separation (reviewer E11): host and image volume built the same way, so folders sit at equal
record numbers above 255 with equal sequence numbers; the image's $MFT is fragmented into many pieces."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident

DESCRIPTION = 'two volumes with folders at equal record numbers above 255'
RULES = 'D4(a) D7 E6'


def populate(w, seed, tag_top):
    rnd = random.Random(seed)
    cnt = pop.Counter()
    pop.tree(w, 'Early', 250, rnd, cnt, avg_kib=2)
    dirs = []
    for top in ('Users', 'Photos', 'Projects'):
        d, f = pop.tree(w, top, 700, rnd, cnt, avg_kib=3)
        dirs += d


def build(sc):
    gimg = sc.aux_image('guest.raw', 96)
    g = sc.volume('guest', 'G_', device=gimg)
    sc.mkfs(g, T('2020-02-01 10:00:00'), windows=None)
    with sc.mount(g, T('2021-03-30 10:00:00')) as w:
        rnd = random.Random('frag')
        populate(w, 'same-shape', 'G_')
        pop.fill_and_punch(w, rnd, chunk=32768, keep_every=2, reserve=2 << 20)
    with sc.mount(g, T('2021-04-10 10:00:00')) as w:
        rnd = random.Random('guest-late')
        cnt = pop.Counter()
        pop.tree(w, 'Late', 1500, rnd, cnt, avg_kib=1, exts=['txt'])
    sc.disk(320)
    h = sc.volume('lost', 'L_')
    sc.mkfs(h, T('2021-01-10 10:00:00'))
    with sc.mount(h, T('2021-01-11 10:00:00')) as w:
        populate(w, 'same-shape', 'L_')
    with sc.mount(h, T('2021-05-01 10:00:00')) as w:
        w.mkdir('L_VM')
        sc.embed(g, w, 'L_VM/L_guest.img')
    accident(sc, h)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main', 'guest': 'embedded'}, 'lost',
                 validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
