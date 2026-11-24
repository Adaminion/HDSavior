"""Restored from a backup after the accident (C3/C4 with copied times).  The lost volume is used
2021-2023 and quick-formatted on 2024-06-01.  The next day the user restores 500 files from a backup of
2019-2020 onto the new volume (the copies keep the sources' $STANDARD_INFORMATION times, MFT-change
time included; $FILE_NAME times are the moment of the copy) and deletes 350 of them again.  The deleted
copies' records sit in free slots of the current $MFT with $SI times years before the format: by the
raw MFT-change time C3 would call them old records of the lost volume; by the record time they are
post_format.  Expected: one main instance (lost), the restored copies hidden, never mixed in."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident, generation, ACCIDENT

DESCRIPTION = 'backup restored onto the new volume after the accident, most of it deleted again'
RULES = 'C3 C4 C5 D2 D9 (record time)'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(256)
    v = sc.volume('lost', 'L_')
    sc.mkfs(v, T('2021-03-01 10:00:00'))
    generation(sc, v, T('2021-03-02 10:00:00'), 1500, rnd, cnt, avg_kib=10, churn=0.05)
    cur = accident(sc, v)
    with sc.mount(cur, ACCIDENT + 86400) as w:
        dirs, files = [], []
        for top in ('RestorePhotos', 'RestoreDocs'):
            d, f = pop.tree(w, top, 250, rnd, cnt, avg_kib=10)
            dirs += d
            files += f
    sc.backdate_si(cur, T('2019-01-01 00:00:00'), T('2020-12-31 00:00:00'), rnd)
    with sc.mount(cur, ACCIDENT + 86400 + 3600) as w:
        for rel in rnd.sample(files, 350):
            w.delete(rel)
        w.delete('C_RestoreDocs')
    s, e = whole(sc, DESCRIPTION, {'lost': 'main'}, 'lost', validated={'lost': True},
                 protected={'lost': True},
                 notes=['500 files restored from a 2019-2020 backup on 2024-06-02, most deleted again'])
    sc.finish([s], {'whole': e})
