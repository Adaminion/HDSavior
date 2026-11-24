"""Plain case like basic, but the imitated Windows format leaves unused FILE records (Windows 7 style,
as scripts/linux/make-fixtures.sh does) instead of zeros in the slots up to 255."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident, generation

DESCRIPTION = 'one file system, Windows 7-style blank records in slots up to 255'
RULES = 'A3 C3 D1'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(256)
    v = sc.volume('lost', 'L_')
    sc.mkfs(v, T('2021-03-01 10:00:00'), windows='blank')
    generation(sc, v, T('2021-03-02 10:00:00'), 1500, rnd, cnt, churn=0.05)
    accident(sc, v, windows='blank')
    s, e = whole(sc, DESCRIPTION, {'lost': 'main'}, 'lost', validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
