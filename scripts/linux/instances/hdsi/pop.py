"""Populating volumes: seeded random trees, churn patterns, MFT fragmentation."""
import os
import random

EXT_MIX = ['jpg', 'jpg', 'jpg', 'png', 'pdf', 'zip', 'docx', 'gif', 'txt', 'txt']
EXT_UNKNOWN = ['dat', 'bin', 'raw', 'xyz', 'log']


class Counter:
    def __init__(self):
        self.n = 0

    def next(self):
        self.n += 1
        return self.n


def file_size(rnd, avg_kib, resident_share=0.05):
    if rnd.random() < resident_share:
        return rnd.randint(50, 600)                 # resident in the MFT record
    s = int(rnd.expovariate(1.0 / (avg_kib * 1024)))
    return max(1200, min(s, avg_kib * 1024 * 6))


def tree(w, top, nfiles, rnd, cnt, avg_kib=6, exts=EXT_MIX, depth=3, fan=(1, 4), per_dir=(3, 30),
         under=''):
    """A random folder tree of about nfiles files below <under>/<tag><top>.  Returns (dirs, files)."""
    tag = w.vol.tag
    dirs, files = [], []
    made = [0]

    def newfile(d):
        k = cnt.next()
        e = rnd.choice(exts)
        rel = '%s/%s%s_f%05d.%s' % (d, tag, top, k, e)
        w.write(rel, file_size(rnd, avg_kib))
        files.append(rel)
        made[0] += 1

    def rec(d, lvl):
        w.mkdir(d)
        dirs.append(d)
        for _ in range(rnd.randint(*per_dir)):
            if made[0] < nfiles:
                newfile(d)
        if lvl < depth:
            for _ in range(rnd.randint(*fan)):
                if made[0] < nfiles:
                    rec('%s/%s%s_d%04d' % (d, tag, top, cnt.next()), lvl + 1)
    base = (under + '/' if under else '') + tag + top
    w.mkdir(base)
    dirs.append(base)
    while made[0] < nfiles:
        rec('%s/%s%s_d%04d' % (base, tag, top, cnt.next()), 1)
    return dirs, files


def add_files(w, dirs, n, rnd, cnt, top='late', avg_kib=6, exts=EXT_MIX):
    files = []
    tag = w.vol.tag
    for _ in range(n):
        d = rnd.choice(dirs)
        k = cnt.next()
        rel = '%s/%s%s_f%05d.%s' % (d, tag, top, k, rnd.choice(exts))
        w.write(rel, file_size(rnd, avg_kib))
        files.append(rel)
    return files


def churn(w, files, dirs, frac, rnd, cnt, avg_kib=6):
    """Delete frac of the files and create as many new ones in random folders (record reuse)."""
    n = int(len(files) * frac)
    gone = rnd.sample(files, n)
    for rel in gone:
        w.delete(rel)
        files.remove(rel)
    files += add_files(w, dirs, n, rnd, cnt, top='churn', avg_kib=avg_kib)
    return files


def fill_and_punch(w, rnd, chunk=65536, keep_every=3, top='fill', reserve=8 << 20):
    """Fill the free space with files of `chunk` bytes, then delete every keep_every-th one, so the
    free space is a comb of holes (fragments whatever grows next, e.g. the $MFT)."""
    tag = w.vol.tag
    d = tag + top
    w.mkdir(d)
    names = []
    i = 0
    while w.free_bytes() > reserve + chunk:
        rel = '%s/%s%s_%05d.bin' % (d, tag, top, i)
        try:
            w.write(rel, chunk)
        except OSError:
            break
        names.append(rel)
        i += 1
    for j in range(0, len(names), keep_every):
        w.delete(names[j])
    return names
