"""Red team (D7 time test): as same_day_repart, but the re-filled disk was then only read (an archive
disk): every record of the lost volume dates from the day of the re-partitioning, within hours of the
stale container's last change.  Expected: the lost volume is main, never 'embedded'."""
from same_day_repart import build as _b
DESCRIPTION = 'stale container of the old layout, re-filled the same day and only read since, whole disk'
RULES = 'D7 D8 G1'


def build(sc):
    _b(sc, used_later=False)
