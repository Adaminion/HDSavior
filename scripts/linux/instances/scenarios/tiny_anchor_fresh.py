"""Red team: a nearly empty lost file system on a new disk (an external drive that held about 200
documents and a folder of eight videos, then quick-formatted by accident).  Only the video folder's
records (numbers above 255) survive the Windows-like rewrite.  Expected: they are main (anchor), not
strays."""
from tiny_anchor import build as _b
DESCRIPTION = 'nearly empty lost volume (9 surviving records) on a new disk'
RULES = 'D5 D6 D8'


def build(sc):
    _b(sc, older=False)
