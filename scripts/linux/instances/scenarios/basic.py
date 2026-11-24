"""Plain case: one file system, ordinary use for three years, quick format (Windows-like wipe of MFT
slots up to 255).  Expected: one main instance holding every surviving record, nothing older."""
import random

from hdsi.build import T
from hdsi import pop

DESCRIPTION = 'one file system after a normal quick format'
RULES = 'C1-C5 D1-D9 E5 E6 F1-F6 G2'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(256)
    old = sc.volume('lost', 'L_', cluster=4096)
    sc.mkfs(old, T('2021-03-01 10:00:00'))
    with sc.mount(old, T('2021-03-02 09:00:00')) as w:
        dirs = []
        files = []
        for top in ('Photos', 'Documents', 'Music'):
            d, f = pop.tree(w, top, 400, rnd, cnt)
            dirs += d
            files += f
    with sc.mount(old, T('2022-05-10 18:00:00')) as w:
        files += pop.add_files(w, dirs, 300, rnd, cnt)
    with sc.mount(old, T('2023-01-15 20:00:00')) as w:
        pop.churn(w, files, dirs, 0.05, rnd, cnt)
    new = sc.volume('current', 'C_', cluster=4096)
    sc.mkfs(new, T('2024-06-01 12:00:00'))
    sc.finish(scans=[{'name': 'whole', 'start': 0, 'end': sc.image_size}],
              expect={'whole': {'description': DESCRIPTION, 'main': 'lost', 'kinds': {'lost': 'main'},
                                'validated': {'lost': True}, 'protected': {'lost': True}}})
