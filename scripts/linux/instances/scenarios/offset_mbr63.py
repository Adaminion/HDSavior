"""Plain case: NTFS partition at sector 63 (Windows XP layout; nothing 1 KiB-aligned)."""
from offset_mbr import build as _b
DESCRIPTION = 'volume at sector 63 (MBR)'
RULES = 'C1 F2 F5 D8'


def build(sc):
    _b(sc, start=63)
