"""Red team (cost of the E6 time floor): the folder structure and about 200 files were created on the day
after the format; everything that survives the Windows-like rewrite (record numbers above 255) was
added two months later into those folders.  So every surviving record is two months younger than the
index entries that name the (lost) folders.  Expected: the lost folders keep the names their index
entries supply."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident

DESCRIPTION = 'folders created at the start, every surviving record added two months later'
RULES = 'E6'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(256)
    v = sc.volume('lost', 'L_')
    sc.mkfs(v, T('2021-01-10 10:00:00'))
    dirs = []
    with sc.mount(v, T('2021-01-11 10:00:00')) as w:
        for top in ('Photos', 'Docs', 'Music', 'Work', 'Misc', 'Archive'):
            d, f = pop.tree(w, top, 26, rnd, cnt, per_dir=(2, 5), fan=(2, 3), depth=2)
            dirs += d
    with sc.mount(v, T('2021-03-15 10:00:00')) as w:
        pop.add_files(w, dirs, 900, rnd, cnt)
    accident(sc, v)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main'}, 'lost', validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
