"""Instance separation: an image file with FEWER records than its host, both full of sequence-1
records at overlapping numbers (D6's copy-set test compares number and sequence only)."""
from embed_big import build as _b
DESCRIPTION = 'image file with fewer records than its host, overlapping record numbers'
RULES = 'D6 D7'


def build(sc):
    _b(sc, guest_files=3000, host_files=5000, guest_mib=96, host_mib=360, host_avg=6, guest_avg=4)
