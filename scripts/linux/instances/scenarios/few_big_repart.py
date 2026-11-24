"""Red team (F5 volume size): an external disk used for a few big files: about 200 documents and three
large videos (written after a big temporary file that was later deleted, so they lie in the second half
of the volume).  The accident re-partitions the disk to a smaller partition at the same start and
quick-formats it.  Only the video folder's records survive (numbers above 255).  Scanned as the whole
disk and as the new partition.  Expected: the lost volume is main; in the whole-disk scan the intact
videos are not out of bounds and the lost-volume range covers them."""
import random
from hdsi.build import T
from hdsi import pop
from _common import ACCIDENT
from _redteam import fill_to_record

DESCRIPTION = 'few big files, re-partitioned smaller at the same start'
RULES = 'D6 F5 G1 S2'


def build(sc, videos=3, second=False):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(700)
    s0, n0 = 2048, (680 << 20) // 512
    sc.partition_table('mbr', [(s0, n0)])
    lost = sc.volume('lost', 'L_', offset=s0 * 512, length=n0 * 512)
    sc.mkfs(lost, T('2021-01-10 10:00:00'))
    with sc.mount(lost, T('2021-01-11 10:00:00')) as w:
        w.write_big('L_temp.bin', 330 << 20)
        fill_to_record(w, 'L_Docs', 255, rnd, cnt)
    with sc.mount(lost, T('2021-02-11 10:00:00')) as w:
        w.mkdir('L_Videos')
        for i in range(videos):
            w.write_big('L_Videos/L_movie_%d.zip' % i, rnd.randint(60, 90) << 20)
        w.delete('L_temp.bin')
    n1 = (300 << 20) // 512
    if second:
        sc.partition_table('mbr', [(s0, n1), (s0 + n1, n0 - n1)])
    else:
        sc.partition_table('mbr', [(s0, n1)])
    cur = sc.volume('current', 'C_', offset=s0 * 512, length=n1 * 512)
    sc.mkfs(cur, ACCIDENT)
    if second:
        c2 = sc.volume('current2', 'D_', offset=(s0 + n1) * 512, length=(n0 - n1) * 512)
        sc.mkfs(c2, ACCIDENT + 300)
    exp = {'main': 'lost', 'kinds': {'lost': 'main'}, 'validated': {'lost': None}, 'protected': {'lost': True}}
    sc.finish([{'name': 'whole', 'start': 0, 'end': sc.image_size},
               {'name': 'part', 'start': s0 * 512, 'end': (s0 + n1) * 512}],
              {'whole': dict(exp, description=DESCRIPTION + ', whole disk'),
               'part': dict(exp, description=DESCRIPTION + ', new partition only')})
