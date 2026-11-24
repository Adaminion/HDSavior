"""Instance separation (C2 verdict disk2): re-partitioned disk; the old volume (partition at 129 MiB) is
intact inside the new partition at 1 MiB, which was quick-formatted and received a few files.
Expected: the old volume's tree is shown (main), not hidden as a current volume."""
import random
from hdsi.build import T
from hdsi import pop
from _common import ACCIDENT

DESCRIPTION = 're-partitioned disk, old volume intact at another offset'
RULES = 'C1 D8 F2 F5'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(700)
    s0, n0 = 264192, 1146880
    sc.partition_table('mbr', [(s0, n0)])
    old = sc.volume('old', 'O_', offset=s0 * 512, length=n0 * 512)
    sc.mkfs(old, T('2020-03-01 10:00:00'))
    with sc.mount(old, T('2020-03-02 10:00:00')) as w:
        for top in ('Photos', 'Docs'):
            pop.tree(w, top, 140, rnd, cnt, avg_kib=20)
    s1, n1 = 2048, 1413120
    sc.partition_table('mbr', [(s1, n1)])
    cur = sc.volume('current', 'C_', offset=s1 * 512, length=n1 * 512)
    sc.mkfs(cur, ACCIDENT)
    with sc.mount(cur, ACCIDENT + 600) as w:
        w.mkdir('C_new-data')
        for i in range(20):
            w.write('C_new-data/C_NEW_%02d.bin' % i, 100000)
        w.delete('C_new-data/C_NEW_19.bin')
    exp = {'main': 'old', 'kinds': {'old': 'main'}, 'validated': {'old': True}, 'protected': {'old': True}}
    sc.finish([{'name': 'whole', 'start': 0, 'end': sc.image_size},
               {'name': 'part', 'start': s1 * 512, 'end': (s1 + n1) * 512}],
              {'whole': dict(exp, description=DESCRIPTION + ', whole disk'),
               'part': dict(exp, description=DESCRIPTION + ', new partition only')})
