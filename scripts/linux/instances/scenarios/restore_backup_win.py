"""restore_backup with Windows' duplicated information in the $I30 entries.  Windows keeps a parent's
index entry in step with the child's $STANDARD_INFORMATION, last-access time included (seen on the
Windows-written windows_volume.raw); ntfs-3g writes its own $FILE_NAME times there.  So:
- the older generation's entries carry access times 30 days after their latest other time (folders
  listed and files read after their last change, still long before the next format).  The entry time
  leaves the access time out (ALGORITHMS.md vocabulary); with it, those entries would look later than
  the older segment's records and fail E6's "before the cut", and the older instance's lost folders
  would lose their names;
- the lost generation's entries carry the four $SI times of the records they name, so the entries of
  the restored copies hold the backup's old times, not the copy time.  C4 and E6 must not depend on
  an entry of a copy carrying the copy time.
Expected: lost = main, older = older, no mixing, lost folder names kept."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident, generation, restored_generation

DESCRIPTION = 'restore_backup with Windows-style $I30 entry times (access times, $SI times of copies)'
RULES = 'D2 D3 D8 D9 E6 C4 (record time, entry time)'


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(400)
    g0 = sc.volume('older', 'O_')
    sc.mkfs(g0, T('2018-05-01 10:00:00'))
    generation(sc, g0, T('2018-05-02 10:00:00'), 2600, rnd, cnt, churn=0.1)
    reformat = T('2021-02-01 10:00:00')
    sc.sync_entry_times(g0, access_days=30, until=reformat - 86400)
    g1 = sc.volume('lost', 'L_')
    sc.mkfs(g1, reformat)
    restored_generation(sc, g1, T('2021-02-01 11:00:00'), 1500, T('2015-01-01 00:00:00'),
                        T('2017-12-31 00:00:00'), rnd, cnt)
    sc.sync_entry_times(g1, from_si=True)
    accident(sc, g1)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main', 'older': 'older'}, 'lost',
                 validated={'lost': True}, protected={'lost': True},
                 notes=['lost generation restored from a 2015-2017 backup on 2021-02-01',
                        'older entries: access time +30 days; lost entries: the $SI times of the child'])
    sc.finish([s], {'whole': e})
