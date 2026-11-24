"""Shared helpers of the red-team scenarios."""
import os

from hdsi import pop


def fill_to_record(w, folder, last, rnd, cnt, avg_kib=8):
    """Create small files in <folder> until the next MFT record number would exceed `last`
    (ntfs-3g reports the MFT record number as the inode number).  Returns the files."""
    files = []
    if not w.exists(folder):
        w.mkdir(folder)
    tag = w.vol.tag
    while True:
        k = cnt.next()
        rel = '%s/%sdoc_%05d.%s' % (folder, tag, k, rnd.choice(pop.EXT_MIX))
        w.write(rel, pop.file_size(rnd, avg_kib))
        files.append(rel)
        if os.stat(w.p(rel)).st_ino >= last:
            return files


def ino(w, rel):
    return os.stat(w.p(rel)).st_ino
