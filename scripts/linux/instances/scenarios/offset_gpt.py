"""Plain case: NTFS partition at 16 MiB of a GPT disk, behind a Microsoft reserved partition."""
from offset_mbr import build as _b
DESCRIPTION = 'volume at 16 MiB of a GPT disk'
RULES = 'C1 F2 F5 D8'


def build(sc):
    _b(sc, start=32768, gpt=True)
