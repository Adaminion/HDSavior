"""Sensitivity (D7 day tolerance): the guest's clock ran 3 days ahead of the host's when the image was
copied, so the guest's newest change time is 3 days after the container's MFT-change time."""
from embed_big import build as _b
DESCRIPTION = 'image file, guest clock 3 days ahead of the host'
RULES = 'D7'


def build(sc):
    _b(sc, guest_files=800, host_files=600, skew_days=3.0)
