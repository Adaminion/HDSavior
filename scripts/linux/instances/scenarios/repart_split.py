"""Plain/geometry case (S2 verdicts): one old volume re-partitioned into two partitions, both quick-
formatted.  Scanned as partition 1 and as the whole disk.  Expected: the lost-volume range reaches into
partition 2 (where most old files are); no intact file looks out of bounds in the whole-disk scan."""
import random
from hdsi.build import T
from hdsi import pop
from _common import ACCIDENT

DESCRIPTION = 'old volume larger than the scanned partition (re-partitioned into two)'
RULES = 'C1 F5 G1 S2'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(520)
    s0, n0 = 2048, (512 << 20) // 512
    sc.partition_table('mbr', [(s0, n0)])
    old = sc.volume('lost', 'L_', offset=s0 * 512, length=n0 * 512)
    sc.mkfs(old, T('2021-01-10 10:00:00'))
    with sc.mount(old, T('2021-01-11 10:00:00')) as w:
        pop.tree(w, 'Small', 300, rnd, cnt, avg_kib=4)
        w.mkdir('L_photos')
        for i in range(220):
            w.write('L_photos/L_img_%04d.jpg' % i, rnd.randint(500000, 3500000))
    p1 = (256 << 20) // 512
    sc.partition_table('mbr', [(s0, p1), (s0 + p1, n0 - p1)])
    c1 = sc.volume('current1', 'C_', offset=s0 * 512, length=p1 * 512)
    sc.mkfs(c1, ACCIDENT)
    c2 = sc.volume('current2', 'D_', offset=(s0 + p1) * 512, length=(n0 - p1) * 512)
    sc.mkfs(c2, ACCIDENT + 300, windows=None)
    exp = {'main': 'lost', 'kinds': {'lost': 'main'}, 'validated': {'lost': True}, 'protected': {'lost': True}}
    sc.finish([{'name': 'part1', 'start': s0 * 512, 'end': (s0 + p1) * 512},
               {'name': 'whole', 'start': 0, 'end': sc.image_size}],
              {'part1': dict(exp, description=DESCRIPTION + ', partition 1 only'),
               'whole': dict(exp, description=DESCRIPTION + ', whole disk')})
