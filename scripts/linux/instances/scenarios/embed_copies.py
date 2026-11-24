"""E6(b): the image's own file system holds copies of its MFT blocks (strays of the guest).  A stray copy
of a guest directory must never name a lost folder of the host that happens to share number and sequence."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident, generation

DESCRIPTION = "image file holding copies of its own MFT blocks"
RULES = 'D6 E6(b)'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    gimg = sc.aux_image('guest.raw', 96)
    g = sc.volume('guest', 'G_', device=gimg)
    sc.mkfs(g, T('2020-02-01 10:00:00'), windows=None)
    with sc.mount(g, T('2021-04-20 10:00:00')) as w:
        for top in ('Users', 'Data'):
            pop.tree(w, top, 400, rnd, cnt, avg_kib=8)
    snap = sc.snapshot(g, 'copy')
    rs = snap['rs']
    mo = snap['boot']['mft_lcn'] * snap['cs']
    with open(g.device, 'rb') as f:
        f.seek(mo + 40 * rs)
        blob = f.read(400 * rs)
    with sc.mount(g, T('2021-04-30 10:00:00')) as w:
        w.mkdir('G_shadow')
        w.write_bytes('G_shadow/G_store.bin', blob)
    sc.disk(320)
    h = sc.volume('lost', 'L_')
    sc.mkfs(h, T('2021-01-10 10:00:00'))
    generation(sc, h, T('2021-01-11 10:00:00'), 900, rnd, cnt, avg_kib=20, churn=0.03)
    with sc.mount(h, T('2021-05-01 10:00:00')) as w:
        w.mkdir('L_VM')
        sc.embed(g, w, 'L_VM/L_guest.img')
    accident(sc, h)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main', 'guest': 'embedded'}, 'lost',
                 validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
