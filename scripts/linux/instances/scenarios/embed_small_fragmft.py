"""Instance separation (reviewer mf1): a small NTFS image (fewer than 256 records) on a volume whose
$MFT is fragmented.  Expected: the small volume is a separate embedded instance, the host one instance."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident

DESCRIPTION = 'small image (under 256 records) on a volume with a fragmented $MFT'
RULES = 'D4 D5 D7'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    gimg = sc.aux_image('stick.raw', 16)
    g = sc.volume('stick', 'S_', device=gimg)
    sc.mkfs(g, T('2020-06-01 10:00:00'), windows=None)
    with sc.mount(g, T('2021-03-30 10:00:00')) as w:
        for d in ('Recovery', 'Boot', 'Tools', 'Docs', 'Pics', 'Misc'):
            w.mkdir('S_' + d)
            for i in range(rnd.randint(10, 20)):
                w.write('S_%s/S_stick_%04d.%s' % (d, cnt.next(), rnd.choice(['jpg', 'pdf', 'txt'])),
                        rnd.randint(3000, 30000))
    sc.disk(128)
    h = sc.volume('lost', 'L_')
    sc.mkfs(h, T('2021-01-10 10:00:00'))
    with sc.mount(h, T('2021-04-01 10:00:00')) as w:
        pop.tree(w, 'Early', 600, rnd, cnt, avg_kib=2)
        w.mkdir('L_Backup')
        sc.embed(g, w, 'L_Backup/L_usbstick.img')
        pop.fill_and_punch(w, rnd, chunk=65536, keep_every=3, reserve=4 << 20)
    with sc.mount(h, T('2021-06-01 10:00:00')) as w:
        for top in ('Late', 'More', 'Most'):
            pop.tree(w, top, 2500, rnd, cnt, avg_kib=1, exts=['txt', 'jpg', 'pdf'])
    accident(sc, h)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main', 'stick': 'embedded'}, 'lost',
                 validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
