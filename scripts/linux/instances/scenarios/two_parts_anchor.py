"""Red team: whole-disk scan of a disk with two partitions.  Partition 2 ('D:', data) was formatted in
2019 over an older file system of 2017 and has been in use since; it is a current volume and was not
touched by the accident.  Partition 1 held the lost volume (2021), quick-formatted by accident in 2024.
The 2017 file system's records behind partition 2's own records sit at the origin of a current $MFT
too, so there are two anchors; the 2017 one has more records.  Expected: the lost volume is main;
the 2017 file system is other or older."""
import random
from hdsi.build import T
from hdsi import pop
from _common import accident, generation

DESCRIPTION = 'two partitions, the intact one formatted over an older file system, whole disk'
RULES = 'C1 C3 D8 D9'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(640)
    s1, n1 = 2048, (300 << 20) // 512
    s2, n2 = s1 + n1, (330 << 20) // 512
    sc.partition_table('mbr', [(s1, n1), (s2, n2)])
    g = sc.volume('d2017', 'G_', offset=s2 * 512, length=n2 * 512)
    sc.mkfs(g, T('2017-04-01 10:00:00'))
    generation(sc, g, T('2017-04-02 10:00:00'), 2600, rnd, cnt, churn=0.05)
    d = sc.volume('data', 'D_', offset=s2 * 512, length=n2 * 512)
    sc.mkfs(d, T('2019-02-01 10:00:00'))
    generation(sc, d, T('2019-02-02 10:00:00'), 300, rnd, cnt, avg_kib=30)
    lost = sc.volume('lost', 'L_', offset=s1 * 512, length=n1 * 512)
    sc.mkfs(lost, T('2021-01-10 10:00:00'))
    generation(sc, lost, T('2021-01-11 10:00:00'), 900, rnd, cnt, churn=0.05)
    accident(sc, lost)
    with sc.mount(d, T('2024-05-20 10:00:00')) as w:
        w.write('D_late.txt', 5000)
    sc.finish([{'name': 'whole', 'start': 0, 'end': sc.image_size}],
              {'whole': {'description': DESCRIPTION, 'main': 'lost',
                         'kinds': {'lost': 'main', 'd2017': ['other', 'older']},
                         'validated': {'lost': True}, 'protected': {'lost': True}}})
