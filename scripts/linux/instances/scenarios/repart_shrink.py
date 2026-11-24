"""Red team (D4(c) with the same volume start): a 600 MiB volume made by a Linux tool in 2019 (mkntfs:
$MFTMirr in the middle of the volume) held a few hundred files.  In 2021 the partition was shrunk to
400 MiB at the same start (a second partition was added behind it) and formatted again with the same
tool; the lost volume received more files than the old one ever had, so no record of the 2019 volume
survives in the $MFT area, but its $MFTMirr (copies of its records 0-3) survives at the old middle,
inside the new partition.  Its record 0 places the lost volume's records exactly, at the lost volume's
own start.  Expected: the lost volume is main and holds no record of the 2019 volume."""
import random
from hdsi.build import T
from hdsi import pop
from _common import ACCIDENT, generation

DESCRIPTION = 'partition shrunk at the same start, old $MFTMirr inside the lost volume'
RULES = 'D4(c) D8 G2'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(640)
    s0, n0 = 2048, (600 << 20) // 512
    sc.partition_table('mbr', [(s0, n0)])
    old = sc.volume('v2019', 'O_', offset=s0 * 512, length=n0 * 512)
    sc.mkfs(old, T('2019-02-01 10:00:00'))
    generation(sc, old, T('2019-02-02 10:00:00'), 300, rnd, cnt, avg_kib=40)
    n1 = (400 << 20) // 512
    sc.partition_table('mbr', [(s0, n1), (s0 + n1, n0 - n1)])
    lost = sc.volume('lost', 'L_', offset=s0 * 512, length=n1 * 512)
    sc.mkfs(lost, T('2021-01-10 10:00:00'))
    generation(sc, lost, T('2021-01-11 10:00:00'), 1200, rnd, cnt, avg_kib=12, churn=0.05)
    p2 = sc.volume('data2', 'D_', offset=(s0 + n1) * 512, length=(n0 - n1) * 512)
    sc.mkfs(p2, T('2021-01-12 10:00:00'))
    with sc.mount(p2, T('2021-01-12 11:00:00')) as w:
        pop.tree(w, 'Data', 100, rnd, cnt, avg_kib=40)
    cur = sc.volume('current', 'C_', offset=s0 * 512, length=n1 * 512)
    sc.mkfs(cur, ACCIDENT)
    exp = {'main': 'lost', 'kinds': {'lost': 'main', 'v2019': ['strays', 'other', 'older']},
           'validated': {'lost': True}, 'protected': {'lost': True}}
    sc.finish([{'name': 'part1', 'start': s0 * 512, 'end': (s0 + n1) * 512},
               {'name': 'whole', 'start': 0, 'end': sc.image_size}],
              {'part1': dict(exp, description=DESCRIPTION + ', partition 1'),
               'whole': dict(exp, description=DESCRIPTION + ', whole disk')})
