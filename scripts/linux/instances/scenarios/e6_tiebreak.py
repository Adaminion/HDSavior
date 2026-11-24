"""E6's choice between several usable entries for one lost folder: the latest entry time, then the
highest source LSN, then the lowest source offset.  One file system (2022-2024): L_Top holds 24
folders (record numbers below 256, so the accident's rewrite of slots up to 255 removes them) whose
files survive, and a few files of its own.  Ten days in, a raw copy of L_Top's index blocks is written
into a file allocated before them (a backup or defragmenter's copy; lower offsets than the originals);
a month in, half of the folders are renamed, and later the file with the copy is deleted.  ntfs-3g
writes LSN 0 everywhere, so the copy (old names, older entry times) and the current blocks (new names,
newer entry times) tie on LSN, and the lower offset would pick the copy's stale name.
Expected: one main instance; every renamed folder carries its new name (prefer_names)."""
import random
from hdsi.build import T, read_volume
from hdsi import pop
from _common import whole, accident

DESCRIPTION = 'renamed lost folders whose stale index-block copy lies at a lower offset (E6 tie-break)'
RULES = 'E6'

N = 24


def folder(i):
    return 'L_Folder_%02d_with_a_fairly_long_name_to_fill_index_blocks' % i


def renamed(i):
    return 'L_Renamed_%02d_with_a_fairly_long_name_to_fill_index_blocks' % i


def build(sc):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(256)
    v = sc.volume('lost', 'L_')
    t0 = T('2022-03-01 10:00:00')
    sc.mkfs(v, t0 - 3600)
    with sc.mount(v, t0) as w:
        # allocated first: its clusters lie below every index block written later
        w.write('L_pad.bin', 256 * 1024)
        w.mkdir('L_Top')
        for i in range(N):
            w.mkdir('L_Top/' + folder(i))
        pop.tree(w, 'Fill', 320, rnd, cnt)          # pushes later record numbers above 255
        for i in range(N):
            for k in range(3):
                w.write('L_Top/%s/L_in_%02d_%d.txt' % (folder(i), i, k), 3000 + 100 * k)
        for k in range(6):
            w.write('L_Top/L_note_%d.txt' % k, 2500)
    # ten days on: a raw copy of L_Top's index allocation inside L_pad.bin (the volume unmounted)
    snap = read_volume(v.device, v.offset)
    cs = snap['cs']
    top = [r for r in snap['records'] if r['in_use'] and r['name'] == 'L_Top']
    pad = [r for r in snap['records'] if r['in_use'] and r['name'] == 'L_pad.bin']
    if len(top) != 1 or len(pad) != 1 or not top[0]['ia']:
        raise RuntimeError('L_Top has no index allocation or L_pad.bin is missing')
    ia = [(l, n) for svcn, runs in top[0]['ia'] for l, n in runs if l is not None]
    pd = [(l, n) for d in pad[0]['data'] for l, n in d[2] if l is not None]
    if max(l + n for l, n in pd) > min(l for l, n in ia):
        raise RuntimeError('L_pad.bin does not lie below the index blocks: %s %s' % (pd, ia))
    with open(v.device, 'r+b') as f:
        data = b''
        for l, n in ia:
            f.seek(v.offset + l * cs)
            data += f.read(n * cs)
        if len(data) > sum(n for l, n in pd) * cs:
            raise RuntimeError('index allocation larger than L_pad.bin')
        pos = 0
        for l, n in pd:
            part = data[pos:pos + n * cs]
            if not part:
                break
            f.seek(v.offset + l * cs)
            f.write(part)
            pos += len(part)
    v.notes.append('L_Top index allocation %s copied into L_pad.bin %s' % (ia, pd))
    with sc.mount(v, t0 + 86400 * 30) as w:
        for i in range(0, N, 2):
            w.rename('L_Top/' + folder(i), 'L_Top/' + renamed(i))
    with sc.mount(v, t0 + 86400 * 60) as w:
        w.delete('L_pad.bin')
        pop.tree(w, 'Late', 40, rnd, cnt)
    accident(sc, v)
    s, e = whole(sc, DESCRIPTION, {'lost': 'main'}, 'lost', validated={'lost': True}, protected={'lost': True},
                 notes=['L_Top index blocks copied into L_pad.bin before half of its folders were renamed'])
    e['prefer_names'] = [renamed(i) for i in range(0, N, 2)]
    sc.finish([s], {'whole': e})
