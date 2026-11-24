"""Instance separation: a newer generation of about 20 surviving records (210 created; the Windows-like
wipe of slots up to 255 leaves about 20) on top of a large older generation."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident, generation

DESCRIPTION = 'newer generation of about 20 surviving records over a large older one'
RULES = 'D2 D6 D8 D9'


def build(sc, new_files=210, windows='zero', validated=True):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(320)
    g0 = sc.volume('older', 'O_')
    sc.mkfs(g0, T('2019-03-01 10:00:00'), windows=windows)
    generation(sc, g0, T('2019-03-02 10:00:00'), 3000, rnd, cnt, churn=0.05)
    g1 = sc.volume('lost', 'L_')
    sc.mkfs(g1, T('2022-01-10 10:00:00'), windows=windows)
    with sc.mount(g1, T('2022-01-11 10:00:00')) as w:
        pop.tree(w, 'Docs', new_files, rnd, cnt, avg_kib=8, per_dir=(10, 30))
    accident(sc, g1, windows=windows)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main', 'older': 'older'}, 'lost',
                 validated={'lost': validated}, protected={'lost': True},
                 notes=['F6: fewer than 20 checkable files leave validation to structural evidence'
                        ] if validated is None else [])
    sc.finish([s], {'whole': e})
