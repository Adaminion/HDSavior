"""Shared building blocks of the scenario builders."""
import random

from hdsi.build import T
from hdsi import pop

ACCIDENT = T('2024-06-01 12:00:00')


def whole(sc, desc, kinds, main, validated=None, protected=None, notes=(), name='whole'):
    return {'name': name, 'start': 0, 'end': sc.image_size}, {
        'description': desc, 'main': main, 'kinds': kinds, 'validated': validated or {},
        'protected': protected or {}, 'notes': list(notes)}


def generation(sc, vol, t0, nfiles, rnd, cnt, avg_kib=6, churn=0.0, tops=None, later=0.2):
    """A file system's life: a random tree copied at t0, later additions, churn.  Returns (dirs, files)."""
    tops = tops or ['Photos', 'Docs', 'Music', 'Work', 'Misc', 'Archive']
    dirs, files = [], []
    first = int(nfiles * (1 - later))
    with sc.mount(vol, t0) as w:
        per = max(1, first // len(tops))
        for top in tops:
            if len(files) >= first:
                break
            d, f = pop.tree(w, top, per, rnd, cnt, avg_kib=avg_kib)
            dirs += d
            files += f
    if nfiles - len(files) > 0:
        with sc.mount(vol, t0 + 86400 * 40) as w:
            files += pop.add_files(w, dirs, nfiles - len(files), rnd, cnt, avg_kib=avg_kib)
    if churn:
        with sc.mount(vol, t0 + 86400 * 120) as w:
            pop.churn(w, files, dirs, churn, rnd, cnt, avg_kib=avg_kib)
    return dirs, files


def accident(sc, region_of, at=ACCIDENT, cluster=4096, windows='zero', vid='current', tag='C_'):
    v = sc.volume(vid, tag, offset=region_of.offset, length=region_of.length, device=region_of.device,
                  cluster=cluster, hidden=region_of.hidden)
    sc.mkfs(v, at, windows=windows)
    return v


def replace_history(sc, vol, t0, rnd, cnt, pre=0, old=900, keep=1500, new=600):
    """'Delete the oldest folder, copy a new one': PRE (kept), OLD, KEEP; weeks later OLD is deleted and
    NEW is copied in, taking the freed low record numbers."""
    with sc.mount(vol, t0) as w:
        if pre:
            pop.tree(w, 'PRE', pre, rnd, cnt)
        pop.tree(w, 'OLD', old, rnd, cnt)
        keepd, keepf = pop.tree(w, 'KEEP', keep, rnd, cnt)
    with sc.mount(vol, t0 + 86400 * 60) as w:
        w.delete(vol.tag + 'OLD')
    with sc.mount(vol, t0 + 86400 * 61) as w:
        pop.tree(w, 'NEW', new, rnd, cnt)
    return keepd


def rename_history(sc, vol, t0, rnd, cnt, a=700, b=1500):
    """The oldest files renamed last: tree A, tree B; months later every file of A is renamed."""
    with sc.mount(vol, t0) as w:
        ad, af = pop.tree(w, 'A', a, rnd, cnt)
        pop.tree(w, 'B', b, rnd, cnt)
    with sc.mount(vol, t0 + 86400 * 90) as w:
        for rel in af:
            d, base = rel.rsplit('/', 1)
            w.rename(rel, d + '/' + base.replace(vol.tag + 'A_', vol.tag + 'Arenamed_'))


def rewrite_history(sc, vol, t0, rnd, cnt, a=700, b=1500):
    """Every file of the oldest tree saved again through 'write temp file, rename over'."""
    with sc.mount(vol, t0) as w:
        ad, af = pop.tree(w, 'A', a, rnd, cnt)
        pop.tree(w, 'B', b, rnd, cnt)
    with sc.mount(vol, t0 + 86400 * 90) as w:
        for rel in af:
            d, base = rel.rsplit('/', 1)
            tmp = d + '/' + vol.tag + 'tmp_' + base[len(vol.tag):]
            size = vol.manifest[base]['size']
            w.write(tmp, size, seed='rewrite|' + rel)
            # rename over the original: the original record is freed, the temp record carries the name
            import os
            os.replace(w.p(tmp), w.p(rel))
            e = vol.manifest.pop(base[:0] + tmp.rsplit('/', 1)[1])
            vol.manifest[base] = e
            e['path'] = rel


def restored_generation(sc, vol, t0, nfiles, src_lo, src_hi, rnd, cnt, avg_kib=6, later=300, churn=0.05,
                        tops=('Photos', 'Docs', 'Music', 'Work')):
    """A file system filled from a backup at t0 (robocopy-style: every restored record keeps the
    source's $SI times, last written between src_lo and src_hi; $FILE_NAME times are t0), then used:
    `later` new files 60 days on, churn of `churn` 200 days on.  Returns (dirs, files)."""
    dirs, files = [], []
    with sc.mount(vol, t0) as w:
        per = max(1, nfiles // len(tops))
        for top in tops:
            d, f = pop.tree(w, 'Restore' + top, per, rnd, cnt, avg_kib=avg_kib)
            dirs += d
            files += f
    sc.backdate_si(vol, src_lo, src_hi, rnd)
    if later:
        with sc.mount(vol, t0 + 86400 * 60) as w:
            files += pop.add_files(w, dirs, later, rnd, cnt, avg_kib=avg_kib)
    if churn:
        with sc.mount(vol, t0 + 86400 * 200) as w:
            pop.churn(w, files, dirs, churn, rnd, cnt, avg_kib=avg_kib)
    return dirs, files
