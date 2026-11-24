"""Instance separation: an NTFS image file with more records than its host (raw image of a VM disk).
Expected: host = main, the image's file system = embedded (labelled with the container)."""
import random
from hdsi.build import T
from hdsi import pop
from _common import whole, accident, generation

DESCRIPTION = 'image file with more records than its host'
RULES = 'D4 D7 D8 E6 F G2'


def build(sc, guest_files=1500, host_files=600, as_vhd=False, guest_mib=120, host_mib=400, host_cluster=4096,
          skew_days=0.0, container_first=False, host_avg=40, guest_avg=12, guest_frag=False):
    rnd = random.Random(sc.seed)
    cnt = pop.Counter()
    gimg = sc.aux_image('guest.raw', guest_mib)
    g = sc.volume('guest', 'G_', device=gimg)
    sc.mkfs(g, T('2020-02-01 10:00:00'), windows=None)
    with sc.mount(g, T('2021-04-30 10:00:00') + int(skew_days * 86400)) as w:
        if guest_frag:
            pop.tree(w, 'Early', 250, rnd, cnt, avg_kib=2)
            pop.fill_and_punch(w, rnd, chunk=65536, keep_every=2, reserve=4 << 20)
        per = guest_files // 3
        for top in ('Users', 'Program', 'Data'):
            pop.tree(w, top, per, rnd, cnt, avg_kib=guest_avg)
    sc.disk(host_mib)
    h = sc.volume('lost', 'L_', cluster=host_cluster)
    sc.mkfs(h, T('2021-01-10 10:00:00'))
    if container_first:
        with sc.mount(h, T('2021-01-11 10:00:00')) as w:
            w.mkdir('L_VM')
            sc.embed(g, w, 'L_VM/L_disk.img', as_vhd=as_vhd)
        generation(sc, h, T('2021-05-02 10:00:00'), host_files, rnd, cnt, avg_kib=host_avg, churn=0.03)
    else:
        generation(sc, h, T('2021-01-11 10:00:00'), host_files, rnd, cnt, avg_kib=host_avg, churn=0.03)
        with sc.mount(h, T('2021-05-01 10:00:00')) as w:
            w.mkdir('L_VM')
            sc.embed(g, w, 'L_VM/L_disk.%s' % ('vhd' if as_vhd else 'img'), as_vhd=as_vhd)
    accident(sc, h)
    gk = 'other' if container_first else 'embedded'
    s, e = whole(sc, DESCRIPTION, {'lost': 'main', 'guest': gk}, 'lost',
                 validated={'lost': True}, protected={'lost': True})
    sc.finish([s], {'whole': e})
