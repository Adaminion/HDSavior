"""Instance separation (reviewer E3): a dynamic VHD whose file system has more records than the host;
its $MFT appears as 2 MiB blocks whose origins differ by the 512-byte block bitmaps."""
from embed_big import build as _b
DESCRIPTION = 'dynamic VHD with more records than its host'
RULES = 'D1 D4(a) D7 D8'


def build(sc):
    _b(sc, guest_files=3000, host_files=600, as_vhd=True, guest_mib=160, guest_avg=6)
