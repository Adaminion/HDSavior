"""Red team: a nearly empty lost file system on a re-used disk.  The disk carried an older file system
(2019, 3000 files); it was formatted in 2022 and received about 200 small files and, weeks later, a
folder of eight videos.  The Windows-like rewrite of MFT slots up to 255 at the accident leaves only the
video folder's records (numbers above 255, fewer than 16 records).  Expected: the lost volume is main
(it holds the old records at the origin of the current $MFT, segment 0), the older generation's tail is
older, and the intact videos are not flagged overwritten."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident, generation
from _redteam import fill_to_record

DESCRIPTION = 'nearly empty lost volume (9 surviving records) over a large older one'
RULES = 'D5 D6 D8 D9 G2'


def build(sc, older=True, survivors=8):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(320)
    if older:
        g0 = sc.volume('older', 'O_')
        sc.mkfs(g0, T('2019-03-01 10:00:00'))
        generation(sc, g0, T('2019-03-02 10:00:00'), 3000, rnd, cnt, churn=0.05)
    g1 = sc.volume('lost', 'L_')
    sc.mkfs(g1, T('2022-01-10 10:00:00'))
    with sc.mount(g1, T('2022-01-11 10:00:00')) as w:
        fill_to_record(w, 'L_Docs', 255, rnd, cnt)
    with sc.mount(g1, T('2022-03-11 10:00:00')) as w:
        w.mkdir('L_Videos')
        for i in range(survivors):
            w.write('L_Videos/L_clip_%02d.zip' % i, rnd.randint(1500000, 4000000))
    accident(sc, g1)
    kinds = {'lost': 'main', 'older': 'older'} if older else {'lost': 'main'}
    s, e = whole(sc, DESCRIPTION if older else DESCRIPTION.split(' over')[0] + ' on a new disk', kinds, 'lost',
                 validated={'lost': None}, protected={'lost': True})
    sc.finish([s], {'whole': e})
