"""Instance separation (reviewer E6): an older partition layout (2019, partition at sector 63) whose
volume-filling file covers the area where the lost volume (2022, partition at 1 MiB) later put its
$MFT; the 2022 volume is quick-formatted (2024).  Scanned as the whole disk.  Expected: the 2022 volume
is main and never 'embedded' in the stale file; the stale file is flagged overwritten."""
import random
from hdsi.build import T
from hdsi import pop
from _common import accident, generation

DESCRIPTION = 'stale file of an older partition layout covering the lost volume, whole disk'
RULES = 'C1 D7 D8 D9 F5 G1'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(700)
    s0 = 63
    n0 = ((700 << 20) // 512) - 63 - 2048
    sc.partition_table('mbr', [(s0, n0)])
    old = sc.volume('layout2019', 'S_', offset=s0 * 512, length=n0 * 512)
    sc.mkfs(old, T('2019-06-01 10:00:00'))
    with sc.mount(old, T('2019-06-02 10:00:00')) as w:
        pop.tree(w, 'Old', 300, rnd, cnt, avg_kib=20)
        w.mkdir('S_backup')
        free = w.free_bytes() - (24 << 20)
        w.write_big('S_backup/S_old-backup.bin', free)
    s1 = 2048
    n1 = (640 << 20) // 512
    sc.partition_table('mbr', [(s1, n1)])
    lost = sc.volume('lost', 'L_', offset=s1 * 512, length=n1 * 512)
    sc.mkfs(lost, T('2022-01-10 10:00:00'))
    generation(sc, lost, T('2022-01-11 10:00:00'), 1500, rnd, cnt, avg_kib=20, churn=0.05)
    accident(sc, lost)
    sc.finish([{'name': 'whole', 'start': 0, 'end': sc.image_size}],
              {'whole': {'description': DESCRIPTION, 'main': 'lost',
                         'kinds': {'lost': 'main', 'layout2019': ['other', 'older']},
                         'validated': {'lost': True}, 'protected': {'lost': True}}})
