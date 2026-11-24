"""Instance separation: the image file was the first thing copied to the volume, so the format destroyed
its MFT record.  Nothing proves containment; the image's file system must stay apart (other), never mixed."""
from embed_big import build as _b
DESCRIPTION = "image file whose container record was destroyed"
RULES = 'D4 D7 D8 D9'


def build(sc):
    _b(sc, guest_files=1500, host_files=1200, container_first=True)
