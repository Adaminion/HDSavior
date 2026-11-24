"""Red team (F5 volume size): as few_big_repart, but the space freed by the smaller partition became a
second partition, formatted as well (so the old volume's backup boot sector, the last sector of the old
partition, is now the second partition's backup boot sector).  Only three files of the lost volume
survive, so end_by_files is 0 and the boot sector of the new, smaller partition decides total_clusters.
Expected: in the whole-disk scan the videos are not out of bounds and are protected."""
from few_big_repart import build as _b
DESCRIPTION = 'few big files, old volume re-partitioned into two'
RULES = 'F5 G1 S2'


def build(sc):
    _b(sc, second=True)
