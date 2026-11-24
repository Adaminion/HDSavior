"""Instance separation: a newer generation of 20 records over a large older one, plain mkntfs re-formats
(no wipe: every record of both generations survives)."""
from gen_tiny_new import build as _b
DESCRIPTION = 'newer generation of 20 records, plain mkntfs (no wipe)'
RULES = 'D2 D6 D8 D9'


def build(sc):
    _b(sc, new_files=18, windows=None, validated=None)
