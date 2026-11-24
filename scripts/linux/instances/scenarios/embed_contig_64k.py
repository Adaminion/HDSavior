"""Instance separation (reviewer E1c): a contiguous image file (4 KiB clusters inside) on a host with
64 KiB clusters; the image's own index blocks vote strongly for its own offset."""
from embed_big import build as _b
DESCRIPTION = 'contiguous image file on a 64 KiB-cluster host'
RULES = 'F2 F4 F6 D7 D8'


def build(sc):
    _b(sc, guest_files=1500, host_files=300, host_cluster=65536, guest_mib=100, host_mib=400)
