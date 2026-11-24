"""Red team: a second accident on top of the first.  The lost volume (2021, 1800 files) is quick-
formatted by accident on 2024-05-20; not noticing, the user copies about 600 files onto the new volume
(V1) over the next ten days; on 2024-06-01 the disk is quick-formatted again.  V1's records above 255
overwrite the lost volume's records with the same numbers; the rest of the lost volume survives behind
them.  The rules call the newest file system main (V1) and the one before the previous format older.
Checked here: no mixing, no old file attached to a V1 folder, V1's overwrites flagged."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident, generation

DESCRIPTION = 'second quick format ten days after the first, V1 received 600 files in between'
RULES = 'D2 D3 D4 D8 D9 E5 G2'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(400)
    g0 = sc.volume('lost', 'L_')
    sc.mkfs(g0, T('2021-03-01 10:00:00'))
    generation(sc, g0, T('2021-03-02 10:00:00'), 1800, rnd, cnt, churn=0.05)
    v1 = sc.volume('v1', 'V_')
    sc.mkfs(v1, T('2024-05-20 09:00:00'))
    with sc.mount(v1, T('2024-05-21 10:00:00')) as w:
        for top in ('Restore', 'New'):
            pop.tree(w, top, 300, rnd, cnt, avg_kib=10)
    accident(sc, v1)
    s, e = whole(sc, DESCRIPTION, {'v1': 'main', 'lost': ['older', 'other']}, 'v1',
                 validated={'v1': True}, protected={'v1': True})
    sc.finish([s], {'whole': e})
