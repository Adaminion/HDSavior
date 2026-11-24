"""Red team (D7 time test): an older partition layout (2019, partition at sector 63) whose big
container file (an encrypted-container or VM-disk file that filled the disk) was last written on the
morning of the day the user re-partitioned the disk (partition at 1 MiB) and formatted it again; the
data was copied back the same afternoon (the lost volume, 2019-06-02 18:00) and the disk was used for
months.  The accident in 2024 is a quick format with 64 KiB clusters (another tool or another cluster
size), so the new $MFT is not where the lost one is and the lost volume is no anchor.  Scanned as the
whole disk.  Expected: the lost volume is main and never 'embedded' in the stale container; the stale
container is flagged overwritten."""
import random
from hdsi.build import T
from hdsi import pop
from _common import accident, generation

DESCRIPTION = 'stale container of the old layout, re-partitioned and re-filled the same day, whole disk'
RULES = 'D7 D8 G1'


def build(sc, used_later=True):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    sc.disk(700)
    s0 = 63
    n0 = ((700 << 20) // 512) - 63 - 2048
    sc.partition_table('mbr', [(s0, n0)])
    old = sc.volume('layout2019', 'S_', offset=s0 * 512, length=n0 * 512)
    sc.mkfs(old, T('2019-01-10 10:00:00'))
    with sc.mount(old, T('2019-01-11 10:00:00')) as w:
        pop.tree(w, 'Old', 300, rnd, cnt, avg_kib=20)
        w.mkdir('S_vault')
        free = w.free_bytes() - (24 << 20)
        w.write_big('S_vault/S_container.hc', free)
    with sc.mount(old, T('2019-06-02 09:30:00')) as w:
        # last use of the container on the morning of the re-partitioning (content rewritten in place)
        with open(w.p('S_vault/S_container.hc'), 'r+b') as f:
            f.seek(1 << 20)
            f.write(b'\x5a' * 4096)
        import hashlib
        h = hashlib.sha256()
        with open(w.p('S_vault/S_container.hc'), 'rb') as f:
            for b in iter(lambda: f.read(1 << 20), b''):
                h.update(b)
        old.manifest['S_container.hc']['sha256'] = h.hexdigest()
    s1 = 2048
    n1 = (640 << 20) // 512
    sc.partition_table('mbr', [(s1, n1)])
    lost = sc.volume('lost', 'L_', offset=s1 * 512, length=n1 * 512)
    sc.mkfs(lost, T('2019-06-02 18:00:00'), windows=None)
    if used_later:
        generation(sc, lost, T('2019-06-02 18:20:00'), 1500, rnd, cnt, avg_kib=20, churn=0.05)
    else:
        with sc.mount(lost, T('2019-06-02 18:20:00')) as w:
            for top in ('Photos', 'Docs', 'Music', 'Work'):
                pop.tree(w, top, 375, rnd, cnt, avg_kib=20)
    accident(sc, lost, cluster=65536, windows=None)
    sc.finish([{'name': 'whole', 'start': 0, 'end': sc.image_size}],
              {'whole': {'description': DESCRIPTION, 'main': 'lost',
                         'kinds': {'lost': 'main', 'layout2019': ['other', 'older']},
                         'validated': {'lost': True}, 'protected': {'lost': True}}})
