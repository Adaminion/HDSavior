"""Restored from a backup, twice.  The older generation (formatted 2018) was itself filled from a
backup of files last written 2014-2016; the lost generation (formatted 2021) was filled from an older
archive, files last written 2011-2013.  Both keep the sources' $STANDARD_INFORMATION times (MFT-change
time included); only the $FILE_NAME times say when the records were written on this disk.  By the raw
$SI MFT-change time the generations are inverted; by the record time they are in order.
Expected: lost = main, older = older, no mixing."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident, restored_generation

DESCRIPTION = 'both generations filled from backups, the lost one from the older archive'
RULES = 'D2 D3 D4 D8 D9 E6 G2 (record time)'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(400)
    g0 = sc.volume('older', 'O_')
    sc.mkfs(g0, T('2018-05-01 10:00:00'))
    restored_generation(sc, g0, T('2018-05-02 10:00:00'), 2400, T('2014-01-01 00:00:00'),
                        T('2016-12-31 00:00:00'), rnd, cnt, later=300, churn=0.1)
    g1 = sc.volume('lost', 'L_')
    sc.mkfs(g1, T('2021-02-01 10:00:00'))
    restored_generation(sc, g1, T('2021-02-01 11:00:00'), 1500, T('2011-01-01 00:00:00'),
                        T('2013-12-31 00:00:00'), rnd, cnt)
    accident(sc, g1)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main', 'older': 'older'}, 'lost',
                 validated={'lost': True}, protected={'lost': True},
                 notes=['older generation restored from a 2014-2016 backup on 2018-05-02',
                        'lost generation restored from a 2011-2013 backup on 2021-02-01'])
    sc.finish([s], {'whole': e})
