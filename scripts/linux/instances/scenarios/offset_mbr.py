"""Plain case: NTFS partition at 1 MiB of an MBR disk; scanned as the whole disk and as the partition."""
import random
from hdsi.build import T
from hdsi import pop
from _common import accident, generation

DESCRIPTION = 'volume at a partition offset (MBR, 1 MiB)'
RULES = 'C1 F2 F5 D8'
START = 2048


def build(sc, start=START, gpt=False):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(300)
    size = (256 << 20) // 512
    if gpt:
        sc.partition_table('gpt', [(2048, 30720, 'E3C9E316-0B5C-4DB8-817D-F92DF00215AE'), (start, size)])
    else:
        sc.partition_table('mbr', [(start, size)])
    v = sc.volume('lost', 'L_', offset=start * 512, length=size * 512)
    sc.mkfs(v, T('2021-04-01 10:00:00'))
    generation(sc, v, T('2021-04-02 10:00:00'), 1500, rnd, cnt, churn=0.05)
    accident(sc, v)
    kinds = {'lost': 'main'}
    sc.finish([{'name': 'whole', 'start': 0, 'end': sc.image_size},
               {'name': 'part', 'start': start * 512, 'end': (start + size) * 512}],
              {'whole': {'description': DESCRIPTION + ', whole disk', 'main': 'lost', 'kinds': kinds,
                         'validated': {'lost': True}, 'protected': {'lost': True}},
               'part': {'description': DESCRIPTION + ', partition only', 'main': 'lost', 'kinds': kinds,
                        'validated': {'lost': True}, 'protected': {'lost': True}}})
