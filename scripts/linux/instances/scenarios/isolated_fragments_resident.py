"""Like isolated_fragments, but every file is tiny (resident data): no content check is possible, so no
rule can tie the fragments to main.  Documents a limit: the lost volume shows as main + other."""
from isolated_fragments import build as _b
DESCRIPTION = '$MFT fragments with no surviving parent and only resident files (limit case)'
RULES = 'D4 D9'


def build(sc):
    _b(sc, resident=True)
