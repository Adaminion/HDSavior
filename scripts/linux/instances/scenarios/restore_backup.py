"""Restored from a backup (docs/notes/quick-format-behaviour.md 7a).  An older generation is used
2018-2020; on 2021-02-01 the region is re-formatted and refilled from a backup whose files were last
written 2015-2017.  Windows' copy tools carry the source's $STANDARD_INFORMATION times over, the
MFT-change time included, while every $FILE_NAME time is the moment of the copy.  The lost generation
is then used for three years and quick-formatted on 2024-06-01.  By the raw $SI MFT-change time the
lost generation's records look older than the older generation's tail behind them; by the record time
(ALGORITHMS.md vocabulary) they are the newer.  Expected: lost = main, older = older, no mixing."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident, generation, restored_generation

DESCRIPTION = 'lost generation refilled from a backup (old $SI times) over an older generation'
RULES = 'D2 D3 D4 D8 D9 E6 G2 (record time)'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(400)
    g0 = sc.volume('older', 'O_')
    sc.mkfs(g0, T('2018-05-01 10:00:00'))
    generation(sc, g0, T('2018-05-02 10:00:00'), 2600, rnd, cnt, churn=0.1)
    g1 = sc.volume('lost', 'L_')
    sc.mkfs(g1, T('2021-02-01 10:00:00'))
    restored_generation(sc, g1, T('2021-02-01 11:00:00'), 1500, T('2015-01-01 00:00:00'),
                        T('2017-12-31 00:00:00'), rnd, cnt)
    accident(sc, g1)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main', 'older': 'older'}, 'lost',
                 validated={'lost': True}, protected={'lost': True},
                 notes=['lost generation restored from a 2015-2017 backup on 2021-02-01'])
    sc.finish([s], {'whole': e})
