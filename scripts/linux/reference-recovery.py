#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
reference-recovery.py - an independent, deliberately simple recovery of a fixture image.

It exists to prove check-recovery.py (and the manifests' promises about what
a correct recovery looks like) without the engine: it reads only the image,
never the manifest, and follows the rules of docs/ALGORITHMS.md as far as the
fixtures need them:

  C1-C5  current volumes from the partition table (or a boot sector at the
         scan start); live / post-format / old records; overwritten set =
         live-allocated clusters, clusters of post-format records, and the
         current volumes' backup boot sectors
  D      fragments by MFT origin; joining by references (D4 a) when record
         numbers do not intersect; generation cuts (D2) with changepoints and
         contradictions (shared clusters, parent slots that do not fit);
         fragments that join nothing are left out (an embedded volume, strays)
  E      base/extension groups, names, parents, lost folders named from usable
         index entries (INDX blocks outside live-allocated clusters, index
         roots of the instance's records, on the instance's side of a cut)
  F1     cluster size from the attributes; volume offset = the current
         volume's offset (the fixtures never move the volume start)
  G      flags and ratings, overlap conflicts between instances
  H      output paths with the H4 sanitizing rules, contents (initialized
         size, holes, zeros outside the scan range), signature check, the
         report of H10 / H11

    reference-recovery.py IMAGE --dest DIR [--start BYTE --length BYTES] [--include-deleted]

Output: DIR/<main tree>, DIR/[Older file system]/..., DIR/[Lost folders]/...,
DIR/HDSavior-report-<UTC>.csv.  The default scan range is the first NTFS
partition, or the whole image when it has no partition table.

It shares no code with HDSavior; NTFS structures are decoded by
inspect-ntfs.py next to it.  It is a test tool: it holds everything in
memory, writes with ordinary file calls and is not hardened against hostile
images.
"""

import argparse
import bisect
import csv
import importlib.util
import io
import os
import struct
import sys
import time

sys.dont_write_bytecode = True
HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location("inspect_ntfs", os.path.join(HERE, "inspect-ntfs.py"))
I = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(I)

EPOCH_DIFF = 116444736000000000
NOW = int(time.time() * 10000000) + EPOCH_DIFF
FT_1980 = 119600064000000000


def nxt(seq):
    return 1 if seq == 0xFFFF else seq + 1


# ---------------------------------------------------------------------------
# partitions and current volumes (C1, C2)
# ---------------------------------------------------------------------------

def partition_starts(fd, size):
    """Byte offsets of the partitions of an MBR or GPT, [] without a table."""
    mbr = os.pread(fd, 512, 0)
    if mbr[510:512] != b"\x55\xaa" or mbr[3:11] == b"NTFS    ":
        return []
    entries = [mbr[446 + 16 * i:462 + 16 * i] for i in range(4)]
    if entries[0][4] == 0xEE:
        hdr = os.pread(fd, 92, 512)
        if hdr[:8] != b"EFI PART":
            return []
        first_lba, count, esize = struct.unpack_from("<QII", hdr, 72)
        table = os.pread(fd, count * esize, first_lba * 512)
        out = []
        for i in range(count):
            e = table[i * esize:(i + 1) * esize]
            if e[:16] != bytes(16):
                out.append((struct.unpack_from("<Q", e, 32)[0] * 512,
                            (struct.unpack_from("<Q", e, 40)[0] + 1 - struct.unpack_from("<Q", e, 32)[0]) * 512))
        return out
    return [(struct.unpack_from("<I", e, 8)[0] * 512, struct.unpack_from("<I", e, 12)[0] * 512)
            for e in entries if e[4]]


class Image:
    def __init__(self, path, lo, hi):
        self.fd = os.open(path, os.O_RDONLY)
        self.size = os.fstat(self.fd).st_size
        self.lo, self.hi = lo, hi

    def read(self, off, n):
        """Bytes [off, off+n); zeros for anything outside the scan range (S2a)."""
        out = bytearray(n)
        a, b = max(off, self.lo), min(off + n, self.hi)
        if a < b:
            out[a - off:b - off] = os.pread(self.fd, b - a, a)
        return bytes(out)


def fixed(img, off, size):
    buf = bytearray(img.read(off, size))
    head = I.parse_file_record(bytes(buf), 0)
    if head and head.get("usn") is not None:
        I.apply_fixups(buf, head["usa_offset"], head["usa_count"])
    return buf


def si_of(r):
    for a in r["attributes"]:
        if a["type"] == 0x10 and "standard_information" in a:
            return a["standard_information"]
    return None


class Current:
    """A current volume (C1) and its snapshot (C2), in absolute image offsets."""

    def __init__(self, img, off):
        self.off = off
        b = I.parse_boot_sector(img.read(off, 512), off)
        if not b or not b["valid"]:
            raise ValueError("no boot sector")
        self.boot = b
        self.cs, self.rs, self.bps = b["cluster_size"], b["mft_record_size"], b["bytes_per_sector"]
        r0 = I.parse_file_record(img.read(off + b["mft_lcn"] * self.cs, self.rs), 0)
        if not r0 or not r0["valid"] or not r0["in_use"] or r0["record_number"] != 0 or \
                not any(n["name"] == "$MFT" for n in r0["names"]):
            raise ValueError("no $MFT")
        d = [x for x in r0["data"] if x["name"] == "" and x["non_resident"] and x["lowest_vcn"] == 0][0]
        self.mft_runs = [(r["lcn"], r["length"]) for r in d["runs"]]
        self.initialized = d["initialized_size"]
        self.slots = {}                          # absolute offset -> record number, below the initialized size
        n = 0
        for lcn, length in self.mft_runs:
            for k in range(length * self.cs // self.rs):
                if n * self.rs >= self.initialized:
                    break
                self.slots[off + lcn * self.cs + k * self.rs] = n
                n += 1
        mbit = [a for a in r0["attributes"] if a["type"] == 0xB0]
        if mbit and not mbit[0]["non_resident"]:
            self.mft_bitmap = bytes.fromhex(mbit[0]["bitmap_hex"])
        elif mbit:
            self.mft_bitmap = self.read_runs(img, [(r["lcn"], r["length"]) for r in mbit[0]["runs"]],
                                             mbit[0]["data_size"])
        else:
            self.mft_bitmap = b""
        rec6 = self.record(img, 6)
        d6 = [x for x in rec6["data"] if x["name"] == "" and x["non_resident"]][0]
        self.bitmap = self.read_runs(img, [(r["lcn"], r["length"]) for r in d6["runs"]], d6["data_size"])
        self.format_time = si_of(self.record(img, 3))["created"]
        self.total = b["total_sectors"] * self.bps
        self.backup_boot = (off + self.total, self.bps)

    def record(self, img, n):
        for o, k in self.slots.items():
            if k == n:
                return I.parse_file_record(img.read(o, self.rs), 0)
        raise ValueError("record %d not found" % n)

    def read_runs(self, img, runs, size):
        out = bytearray()
        for lcn, length in runs:
            out += bytes(length * self.cs) if lcn is None else img.read(self.off + lcn * self.cs, length * self.cs)
        return bytes(out[:size])

    def in_use_slot(self, n):
        return n // 8 < len(self.mft_bitmap) and self.mft_bitmap[n // 8] >> (n % 8) & 1

    def allocated_ranges(self):
        """Live-allocated ranges, absolute."""
        out = []
        c, total = 0, len(self.bitmap) * 8
        while c < total:
            if self.bitmap[c // 8] >> (c % 8) & 1:
                s = c
                while c < total and self.bitmap[c // 8] >> (c % 8) & 1:
                    c += 1
                out.append((self.off + s * self.cs, (c - s) * self.cs))
            else:
                c += 1
        return out


class Ranges:
    def __init__(self, ranges):
        merged = []
        for o, n in sorted(r for r in ranges if r[1] > 0):
            if merged and o <= merged[-1][1]:
                merged[-1][1] = max(merged[-1][1], o + n)
            else:
                merged.append([o, o + n])
        self.s = [m[0] for m in merged]
        self.e = [m[1] for m in merged]

    def hits(self, o, n):
        i = bisect.bisect_right(self.s, o + n - 1) - 1
        return i >= 0 and self.e[i] > o


# ---------------------------------------------------------------------------
# records
# ---------------------------------------------------------------------------

class Rec:
    __slots__ = ("off", "num", "seq", "in_use", "is_dir", "base", "names", "si", "data", "attrs", "rs", "fix_ok",
                 "index_root", "lsn", "has_attrs")

    def __init__(self, r):
        self.off = r["offset"]
        self.num = r["record_number"]
        self.seq = r["sequence"]
        self.in_use = r["in_use"]
        self.is_dir = r["is_directory"]
        self.base = (r["base_record"], r["base_sequence"])
        self.names = r["names"]
        self.si = si_of(r)
        self.data = [d for d in r["data"] if d["name"] == ""]
        self.attrs = r["attributes"]
        self.rs = r["record_size"]
        self.fix_ok = r["fixup_ok"]
        self.lsn = r["lsn"]
        self.has_attrs = bool(r["attributes"])
        self.index_root = []
        for a in r["attributes"]:
            if a["type"] == 0x90 and a["name"] == "$I30" and "index_root" in a:
                self.index_root = a["index_root"]["entries"]

    def timed(self):
        return self.si is not None and FT_1980 <= self.si["mft_changed"] <= NOW + 864000000000


def scan(img, currents):
    recs, blocks = [], []
    for item in I.scan_image(img.path, img.lo, img.hi - img.lo, kinds=("FILE", "INDX")):
        if not item["valid"]:
            continue
        if item["kind"] == "INDX":
            blocks.append(item)
            continue
        if not item["in_use"] and not item["attributes"]:
            continue                                        # never used
        nsect = item["record_size"] // 512
        if len(item.get("fixup_mismatch_sectors", [])) >= nsect:
            continue                                        # a $LogFile copy
        recs.append(Rec(item))
    old, post_format = [], []
    for r in recs:
        cur = [c for c in currents if r.off in c.slots]
        if not cur:
            old.append(r)
            continue
        c = cur[0]
        if c.in_use_slot(c.slots[r.off]):
            continue                                        # live
        if r.si is not None:
            (post_format if r.si["mft_changed"] >= c.format_time else old).append(r)
        elif r.in_use:
            old.append(r)
        else:
            post_format.append(r)
    return old, post_format, blocks


# ---------------------------------------------------------------------------
# instances (D)
# ---------------------------------------------------------------------------

def changepoints(recs):
    timed = sorted((r.num, r.si["mft_changed"]) for r in recs if r.timed() and r.num is not None)
    out = []
    pre_min = []
    m = None
    for _, t in timed:
        m = t if m is None else min(m, t)
        pre_min.append(m)
    suf_max = [0] * len(timed)
    m = None
    for i in range(len(timed) - 1, -1, -1):
        m = timed[i][1] if m is None else max(m, timed[i][1])
        suf_max[i] = m
    for i in range(1, len(timed)):
        if suf_max[i] < pre_min[i - 1] and timed[i][0] != timed[i - 1][0]:
            out.append((timed[i][0], suf_max[i], pre_min[i - 1]))     # (p, newest above, oldest below)
    return out


def contradictions(recs, cs):
    by_num = {}
    for r in recs:
        if r.num is not None:
            by_num.setdefault(r.num, []).append(r)
    pairs = {}
    live = [r for r in recs if r.in_use and r.fix_ok and r.num is not None]
    # (B) a parent or base reference to a slot whose record does not fit
    for r in live:
        refs = [(fn["parent_record"], fn["parent_sequence"], True) for fn in r.names]
        if r.base != (0, 0):
            refs.append((r.base[0], r.base[1], False))
        for p, s, want_dir in refs:
            if p >= r.num or p in (5, 11):
                continue
            for t in by_num.get(p, []):
                if t.in_use and t.fix_ok and (t.seq != s or (want_dir and not t.is_dir)):
                    pairs[r.num] = max(pairs.get(r.num, -1), p)
    # (A) the same cluster in two in-use records
    ext = []
    for r in live:
        for d in r.data:
            if d["non_resident"]:
                for run in d["runs"]:
                    if run["lcn"] is not None:
                        ext.append((run["lcn"], run["lcn"] + run["length"], r.num))
    ext.sort()
    active = []
    for s, e, n in ext:
        active = [a for a in active if a[0] > s]
        for ae, an in active:
            if an != n:
                lo, hi = min(an, n), max(an, n)
                pairs[hi] = max(pairs.get(hi, -1), lo)
        active.append((e, n))
    return sorted((lo, hi) for hi, lo in pairs.items())


def split_generations(recs, cs):
    """D2 on one fragment: returns (segments ordered newest first, cut info)."""
    cps = changepoints(recs)
    chosen = []
    support = {}
    for lo, hi in contradictions(recs, cs):
        if any(lo < c <= hi for c in chosen):
            for c in chosen:
                if lo < c <= hi:
                    support.setdefault(c, set()).add(hi)
            continue
        cand = [p for p, _, _ in cps if lo < p <= hi]
        if cand:
            c = max(cand)
            chosen.append(c)
            support.setdefault(c, set()).add(hi)
    kept = sorted(c for c in chosen if len(support.get(c, ())) >= 2)
    bounds = [None] + kept + [None]
    segs = []
    for i in range(len(bounds) - 1):
        a, b = bounds[i], bounds[i + 1]
        segs.append([r for r in recs if (a is None or r.num >= a) and (b is None or r.num < b)])
    cut_times = {}
    for c in kept:
        for p, newest_above, oldest_below in cps:
            if p == c:
                cut_times[c] = newest_above
    return segs, kept, cut_times


def join_by_references(a, b):
    """D4 (a) between two record sets; None if numbers intersect."""
    na = set(r.num for r in a)
    nb = set(r.num for r in b)
    if na & nb:
        return False
    counted = resolved = 0
    for src, dst_nums, dst in ((a, nb, b), (b, na, a)):
        by = {}
        for r in dst:
            by.setdefault(r.num, []).append(r)
        for r in src:
            for fn in r.names:
                p, s = fn["parent_record"], fn["parent_sequence"]
                if p in (5, 11) or p not in dst_nums:
                    continue
                counted += 1
                if any(t.is_dir and ((t.in_use and t.seq == s) or (not t.in_use and t.seq == nxt(s)))
                       for t in by[p]):
                    resolved += 1
    return resolved >= 1 and resolved * 10 >= counted * 9


def instances(old, currents):
    frags = {}
    for r in old:
        if r.num is None:
            continue
        frags.setdefault((r.rs, r.off - r.num * r.rs), []).append(r)
    origins = set(c.off + c.mft_runs[0][0] * c.cs for c in currents)
    anchor = [k for k in frags if k[1] in origins]
    order = sorted(frags, key=lambda k: (k not in anchor, -len(frags[k])))
    if not order:
        return None, [], {}
    main_key = order[0]
    segs, kept, cut_times = split_generations(frags[main_key], 0)
    main = list(segs[0])
    older = [list(s) for s in segs[1:]]
    rest = order[1:]
    joined = True
    while joined:                                   # until nothing changes (D5 step 2)
        joined = False
        for k in list(rest):
            if join_by_references(main, frags[k]):
                main += frags[k]
                rest.remove(k)
                joined = True
    return main, older, cut_times


# ---------------------------------------------------------------------------
# trees (E)
# ---------------------------------------------------------------------------

class Node:
    def __init__(self, name, parent, rec=None, is_dir=True, virtual=False):
        self.name, self.parent, self.rec, self.is_dir, self.virtual = name, parent, rec, is_dir, virtual
        self.children = []
        self.group = []
        if parent is not None:
            parent.children.append(self)


def best_name(names):
    for pref in ((1, 3), (0,), (2,)):
        cand = [n for n in names if n["namespace"] in pref]
        if cand:
            return cand[0]
    return None


def resolves(r, s):
    return (r.in_use and r.seq == s) or (not r.in_use and r.seq == nxt(s) and r.has_attrs)


def build_tree(label, recs, index_entries, cut_side, all_recs_by_off):
    root = Node(label, None)
    by_num = {}
    for r in recs:
        by_num.setdefault(r.num, []).append(r)
    groups = {}
    for r in recs:
        if r.base == (0, 0):
            groups[id(r)] = [r]
    for r in recs:
        if r.base == (0, 0):
            continue
        for b in by_num.get(r.base[0], []):
            if b.base != (0, 0):
                continue
            if (r.in_use and b.in_use and b.seq == r.base[1]) or \
                    (not r.in_use and not b.in_use and b.has_attrs and b.seq in (r.base[1], nxt(r.base[1]))):
                groups[id(b)].append(r)
    dirs = {}
    for g in groups.values():
        b = g[0]
        if b.is_dir and b.num not in (5,):
            dirs.setdefault(b.num, []).append(g)
    nodes = {}
    lost = {}
    lost_root = [None]

    def lost_parent():
        if lost_root[0] is None:
            lost_root[0] = Node("[Lost folders]", root, virtual=True)
        return lost_root[0]

    def entry_for(P, s):
        cand = [x for x in index_entries if (x["record"], x["sequence"]) == (P, s) and cut_side(x)]
        if not cand:
            return None
        if any(x["namespace"] != 2 for x in cand):
            cand = [x for x in cand if x["namespace"] != 2]
        return max(cand, key=lambda x: x["lsn"])

    def folder(P, s, depth=0):
        if P == 5:
            return root
        if P == 11 or depth > 64:
            return None
        for g in dirs.get(P, []):
            if resolves(g[0], s):
                return node_of(g, depth + 1)
        key = (P, s)
        if key not in lost:
            e = entry_for(P, s)
            if e is not None:
                parent = folder(e["parent_record"], e["parent_sequence"], depth + 1)
                if parent is None:
                    return None
                lost[key] = Node(e["name"], parent)
            else:
                n_seqs = len(set(k[1] for k in lost if k[0] == P)) + 1
                lost[key] = Node("Folder #%d" % P if n_seqs == 1 else "Folder #%d.%d" % (P, s), lost_parent())
        return lost[key]

    def node_of(g, depth=0):
        b = g[0]
        if id(b) in nodes:
            return nodes[id(b)]
        names = [n for r in g for n in r.names]
        nm = best_name(names)
        if nm is None:
            nodes[id(b)] = None
            return None
        if b.num < 16 or (b.num < 24 and not names):
            nodes[id(b)] = None
            return None
        nodes[id(b)] = "pending"
        parent = folder(nm["parent_record"], nm["parent_sequence"], depth + 1)
        if parent == "pending":                             # a cycle
            parent = lost_parent()
        if parent is None:
            nodes[id(b)] = None
            return None
        n = Node(nm["name"], parent, rec=b, is_dir=b.is_dir)
        n.group = g
        nodes[id(b)] = n
        return n

    for g in groups.values():
        if g[0].num is not None and g[0].num >= 16:
            node_of(g)
    return root, [n for n in nodes.values() if isinstance(n, Node)]


# ---------------------------------------------------------------------------
# content and health (F1, G, H7, H9)
# ---------------------------------------------------------------------------

SIGS = {
    "jpg": lambda h: h[:3] == b"\xff\xd8\xff", "jpeg": lambda h: h[:3] == b"\xff\xd8\xff",
    "png": lambda h: h[:8] == b"\x89PNG\r\n\x1a\n", "gif": lambda h: h[:6] in (b"GIF87a", b"GIF89a"),
    "bmp": lambda h: h[:2] == b"BM", "tif": lambda h: h[:4] in (b"II*\x00", b"MM\x00*"),
    "tiff": lambda h: h[:4] in (b"II*\x00", b"MM\x00*"), "pdf": lambda h: h[:5] == b"%PDF-",
    "zip": lambda h: h[:4] == b"PK\x03\x04", "docx": lambda h: h[:4] == b"PK\x03\x04",
    "xlsx": lambda h: h[:4] == b"PK\x03\x04", "pptx": lambda h: h[:4] == b"PK\x03\x04",
    "odt": lambda h: h[:4] == b"PK\x03\x04", "epub": lambda h: h[:4] == b"PK\x03\x04",
    "7z": lambda h: h[:6] == b"7z\xbc\xaf\x27\x1c", "rar": lambda h: h[:7] == b"Rar!\x1a\x07\x00" or h[:8] == b"Rar!\x1a\x07\x01\x00",
    "wav": lambda h: h[:4] == b"RIFF" and h[8:12] == b"WAVE", "avi": lambda h: h[:4] == b"RIFF" and h[8:12] == b"AVI ",
    "webp": lambda h: h[:4] == b"RIFF" and h[8:12] == b"WEBP",
    "mp4": lambda h: h[4:8] == b"ftyp", "mov": lambda h: h[4:8] in (b"ftyp", b"moov", b"wide", b"mdat"),
    "m4a": lambda h: h[4:8] == b"ftyp", "3gp": lambda h: h[4:8] == b"ftyp",
    "mkv": lambda h: h[:4] == b"\x1a\x45\xdf\xa3", "webm": lambda h: h[:4] == b"\x1a\x45\xdf\xa3",
    "mp3": lambda h: h[:3] == b"ID3" or (h[0] == 0xFF and h[1] & 0xE0 == 0xE0),
}


def signature(name, head):
    ext = name.rsplit(".", 1)[-1].lower() if "." in name else ""
    if ext not in SIGS or len(head) < 12:
        return None
    return SIGS[ext](head)


def cluster_size_of(recs):
    votes = {}
    for r in recs:
        for d in r.data:
            if d["non_resident"] and d["lowest_vcn"] == 0 and d["highest_vcn"] >= 0:
                v = d["allocated_size"] // (d["highest_vcn"] + 1)
                if v >= 512 and v & (v - 1) == 0 and v <= 2 << 20 and d["allocated_size"] % (d["highest_vcn"] + 1) == 0:
                    votes[v] = votes.get(v, 0) + 1
    return max(votes, key=votes.get) if votes else 4096


def stream_of(group):
    pieces = sorted([d for r in group for d in r.data], key=lambda d: d.get("lowest_vcn", 0))
    if not pieces:
        return None
    if not pieces[0]["non_resident"]:
        holder = [r for r in group if pieces[0] in r.data][0]
        return {"resident": True, "holder": holder, "piece": pieces[0], "size": pieces[0]["value_length"],
                "flags": pieces[0]["flags"]}
    runs, nextv, broken = [], 0, False
    for d in pieces:
        if d["lowest_vcn"] != nextv or not d["runlist_ok"]:
            broken = True
            break
        runs += [(x["lcn"], x["length"]) for x in d["runs"]]
        nextv = d["highest_vcn"] + 1
    first = pieces[0]
    return {"resident": False, "runs": runs, "size": first["data_size"], "init": first["initialized_size"],
            "flags": first["flags"], "broken": broken or first["lowest_vcn"] != 0}


def read_content(img, s, vol_off, cs):
    if s["resident"]:
        buf = fixed(img, s["holder"].off, s["holder"].rs)
        o = s["piece"]["value_offset"]
        return bytes(buf[o:o + s["size"]])
    out = bytearray()
    for lcn, n in s["runs"]:
        if len(out) >= s["size"]:
            break
        out += bytes(n * cs) if lcn is None else img.read(vol_off + lcn * cs, n * cs)
    out = out[:s["size"]].ljust(s["size"], b"\x00")
    init = min(s["init"], s["size"])
    out[init:] = bytes(s["size"] - init)
    return bytes(out)


# ---------------------------------------------------------------------------
# H4 names
# ---------------------------------------------------------------------------

RESERVED = set(["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"] + ["COM%d" % i for i in range(10)]
               + ["LPT%d" % i for i in range(10)] + ["COM¹", "COM²", "COM³", "LPT¹",
                                                     "LPT²", "LPT³"])


def ascii_upper(s):
    return "".join(c.upper() if "a" <= c <= "z" else c for c in s)


def sanitize(name, limit=255):
    out = []
    for ch in name:
        if ch in '<>:"/\\|?*' or ord(ch) < 0x20:
            out.append("_")
        elif 0xD800 <= ord(ch) <= 0xDFFF:
            out.append("�")
        else:
            out.append(ch)
    name = "".join(out).rstrip(". ") or "_"
    if ascii_upper(name.split(".", 1)[0].rstrip(" ")) in RESERVED or name.startswith("HDSavior-report-") or \
            (name.startswith("~hds-") and name.endswith(".part")):
        name = "_" + name
    return name


def split_ext(name):
    i = name.rfind(".")
    return (name, "") if i <= 0 else (name[:i], name[i:])


def shorten(name, limit):
    stem, ext = split_ext(name)
    while len((stem + ext).encode("utf-8")) > limit and stem:
        stem = stem[:-1]
    return stem + ext


def assign_names(parent, limit):
    """H4 steps 5 and 6 for the children of one folder: in-use first, then by record number."""
    kids = sorted(parent.children, key=lambda n: (0 if n.rec is None or n.rec.in_use else 1,
                                                  n.rec.num if n.rec is not None else -1, n.name))
    used = set()
    for k in kids:
        base = shorten(sanitize(k.name), limit)
        name, n = base, 1
        while name.lower() in used:
            n += 1
            stem, ext = split_ext(base)
            name = shorten("%s (%d)%s" % (stem, n, ext), limit)
        used.add(name.lower())
        k.out = name
        if k.is_dir:
            assign_names(k, limit)


# ---------------------------------------------------------------------------
# recovery
# ---------------------------------------------------------------------------

def run(args):
    fd = os.open(args.image, os.O_RDONLY)
    size = os.fstat(fd).st_size
    parts = partition_starts(fd, size)
    if args.length:
        lo, hi = args.start, args.start + args.length
    elif parts:
        ntfs = [p for p in parts if os.pread(fd, 11, p[0])[3:11] == b"NTFS    "]
        lo, hi = (ntfs or parts)[0][0], (ntfs or parts)[0][0] + (ntfs or parts)[0][1]
    else:
        lo, hi = 0, size
    os.close(fd)
    img = Image(args.image, lo, hi)
    img.path = args.image
    currents = []
    for off in sorted(set([lo] + [p[0] for p in parts if lo <= p[0] < hi])):
        try:
            currents.append(Current(img, off))
        except (ValueError, IndexError, KeyError, TypeError):
            pass
    if not currents:
        sys.exit("reference-recovery: no current NTFS volume in the scan range")
    format_time = min(c.format_time for c in currents)
    old, post_format, blocks = scan(img, currents)
    overwritten = []
    for c in currents:
        overwritten += c.allocated_ranges()
        overwritten.append(c.backup_boot)
        for r in post_format:
            for d in r.data:
                if d["non_resident"]:
                    overwritten += [(c.off + x["lcn"] * c.cs, x["length"] * c.cs) for x in d["runs"]
                                    if x["lcn"] is not None and r.off in c.slots]
    allocated = Ranges([x for c in currents for x in c.allocated_ranges()])
    overwritten = Ranges(overwritten)
    main, older, cut_times = instances(old, currents)
    if main is None:
        sys.exit("reference-recovery: no old records found")
    cs = cluster_size_of(main)
    vol_off = currents[0].off
    cut = min(cut_times.values()) if cut_times else None

    # usable index entries (C4, E6)
    entries = []
    for blk in blocks:
        if allocated.hits(blk["offset"], blk["block_size"]):
            continue
        for x in blk["entries"]:
            fn = x["file_name"]
            if fn and fn["file_attributes"] & 0x10000000 and fn["mft_changed"] < format_time:
                entries.append({"record": x["record"], "sequence": x["sequence"], "name": fn["name"],
                                "namespace": fn["namespace"], "parent_record": fn["parent_record"],
                                "parent_sequence": fn["parent_sequence"], "lsn": blk["lsn"],
                                "time": fn["mft_changed"], "root_of": None})
    for r in main + [x for s in older for x in s]:
        for x in r.index_root:
            fn = x["file_name"]
            if fn and x["record"] != r.num and fn["file_attributes"] & 0x10000000 and fn["mft_changed"] < format_time:
                entries.append({"record": x["record"], "sequence": x["sequence"], "name": fn["name"],
                                "namespace": fn["namespace"], "parent_record": fn["parent_record"],
                                "parent_sequence": fn["parent_sequence"], "lsn": r.lsn, "time": fn["mft_changed"],
                                "root_of": r.off})
    main_offs = set(r.off for r in main)

    def main_side(x):
        if x["root_of"] is not None:
            return x["root_of"] in main_offs
        return cut is None or x["time"] > cut

    trees = [("", build_tree("", main, entries, main_side, None), "main")]
    for i, seg in enumerate(older):
        offs = set(r.off for r in seg)
        label = "[Older file system]" if i == 0 else "[Older file system %d]" % (i + 1)
        side = (lambda offs: lambda x: x["root_of"] in offs if x["root_of"] is not None else x["time"] <= cut)(offs)
        trees.append((label, build_tree(label, seg, entries, side, None), "older"))

    # files, their extents, overlap conflicts (G2)
    files = []
    for label, (root, nodes), kind in trees:
        for n in nodes:
            if n.is_dir or n.rec is None:
                continue
            s = stream_of(n.group)
            ext = []
            if s and not s["resident"]:
                ext = [(vol_off + lcn * cs, k * cs) for lcn, k in s["runs"] if lcn is not None]
            files.append({"node": n, "stream": s, "ext": ext, "kind": kind,
                          "time": n.rec.si["mft_changed"] if n.rec.si else 0, "flags": set()})
    rank = {"main": 0, "older": 1}
    pieces = sorted((o, o + k, i) for i, f in enumerate(files) for o, k in f["ext"])
    active = []
    for s0, e0, i in pieces:
        active = [a for a in active if a[0] > s0]
        for ae, j in active:
            if i == j:
                continue
            a, b = files[i], files[j]
            ka = (rank[a["kind"]], 0 if a["node"].rec.in_use else 1, -a["time"])
            kb = (rank[b["kind"]], 0 if b["node"].rec.in_use else 1, -b["time"])
            if ka == kb:
                a["flags"].add("overlap")
                b["flags"].add("overlap")
            else:
                loser, winner = (a, b) if ka > kb else (b, a)
                loser["flags"].add("overwritten")
                if loser["kind"] == winner["kind"] and loser["node"].rec.in_use and winner["node"].rec.in_use:
                    winner["flags"].add("overlap")
        active.append((e0, i))

    # names and folders
    os.makedirs(args.dest, exist_ok=True)
    report_name = "HDSavior-report-%s.csv" % time.strftime("%Y%m%d-%H%M%S", time.gmtime())
    rows = []
    for label, (root, nodes), kind in trees:
        assign_names(root, args.component_limit)
    for f in files:
        n = f["node"]
        r = n.rec
        if not r.in_use and not args.include_deleted:
            continue
        if any(not (a.rec is None or a.rec.in_use) for a in ancestors(n)) and not args.include_deleted:
            continue
        s = f["stream"]
        flags = set(f["flags"])
        if not r.in_use:
            flags.add("deleted")
        if f["kind"] == "older":
            flags.add("older_generation")
        if not all(x.fix_ok for x in n.group):
            flags.add("torn_record")
        path_parts = [a.out for a in reversed(ancestors(n)) if a.parent is not None] + [n.out]
        label = trees_label(n)
        rel = "/".join([p for p in [label] if p] + path_parts)
        original = "/".join([a.name for a in reversed(ancestors(n)) if a.parent is not None] + [n.name])
        status, written = None, 0
        first = None
        if s is None:
            flags.add("no_data")
            data = b""
        elif s["flags"] & 0x00FF or s["flags"] & 0x4000:
            status = "skipped:compressed" if s["flags"] & 0xFF else "skipped:encrypted"
            data = None
        else:
            if not s["resident"]:
                if s["broken"]:
                    flags.add("missing_fragments")
                if any(lcn is None for lcn, _ in s["runs"]) or s["flags"] & 0x8000:
                    flags.add("sparse")
                for o, k in f["ext"]:
                    if o < lo or o + k > hi:
                        flags.add("out_of_bounds")
                    if overwritten.hits(o, k):
                        flags.add("overwritten")
                first = f["ext"][0][0] if f["ext"] else None
            else:
                first = s["holder"].off
            data = read_content(img, s, vol_off, cs)
        if data is not None:
            sig = signature(n.name, data[:64]) if data else None
            if sig is True:
                flags.add("signature_ok")
            elif sig is False:
                flags.add("signature_mismatch")
            bad = flags & {"missing_fragments", "out_of_bounds", "overwritten", "bad_sectors", "signature_mismatch"}
            unsure = flags & {"deleted", "overlap", "torn_record", "no_data", "older_generation", "stray"}
            if bad or unsure:
                status = "warning:" + "|".join(sorted(flags - {"signature_ok", "sparse"}))
            else:
                status = "ok" if sig is True else "ok_unverified"
            out = os.path.join(args.dest, *rel.split("/"))
            os.makedirs(os.path.dirname(out), exist_ok=True)
            part = os.path.join(os.path.dirname(out), "~hds-%d.part" % len(rows))
            with open(part, "xb") as fo:
                fo.write(data)
            if os.path.exists(out):
                sys.exit("reference-recovery: %s exists (two files got the same name)" % out)
            os.rename(part, out)
            si = r.si
            if si:
                try:
                    os.utime(out, ns=((si["accessed"] - EPOCH_DIFF) * 100, (si["modified"] - EPOCH_DIFF) * 100))
                except (OverflowError, OSError, ValueError):
                    pass
            written = len(data)
        rows.append([status, "|".join(sorted(flags)), s["size"] if s else 0, written,
                     rel if data is not None else "", original, r.num, n.group[0].off,
                     "" if first is None else first,
                     time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(max(0, (r.si["modified"] - EPOCH_DIFF) // 10000000)))
                     if r.si else "", ""])
    write_report(os.path.join(args.dest, report_name), rows)
    counts = {}
    for row in rows:
        k = row[0].split(":")[0]
        counts[k] = counts.get(k, 0) + 1
    print("reference-recovery: scan range %d..%d, %d current volume(s), %d old records, %d post-format, "
          "%d older segment(s); cluster size %d; %s; report %s"
          % (lo, hi, len(currents), len(old), len(post_format), len(older), cs,
             ", ".join("%s %d" % kv for kv in sorted(counts.items())), report_name))


def ancestors(n):
    out = []
    p = n.parent
    while p is not None:
        out.append(p)
        p = p.parent
    return out


def trees_label(n):
    p = n
    while p.parent is not None:
        p = p.parent
    return p.name


def write_report(path, rows):
    buf = io.StringIO(newline="")
    w = csv.writer(buf, lineterminator="\r\n")
    w.writerow(["status", "flags", "size", "bytes_written", "output_path", "original_path", "mft_record",
                "record_offset", "first_data_offset", "modified_utc", "detail"])
    for row in rows:
        w.writerow([("'" + c) if isinstance(c, str) and c[:1] in ("=", "+", "-", "@", "\t") else c for c in row])
    with open(path, "xb") as f:
        f.write(b"\xef\xbb\xbf" + buf.getvalue().encode("utf-8"))


def main():
    ap = argparse.ArgumentParser(description="Independent reference recovery of a fixture image.")
    ap.add_argument("image")
    ap.add_argument("--dest", required=True)
    ap.add_argument("--start", type=int, default=0)
    ap.add_argument("--length", type=int, default=0)
    ap.add_argument("--include-deleted", action="store_true")
    ap.add_argument("--component-limit", type=int, default=255, help="longest output name in UTF-8 bytes")
    run(ap.parse_args())


if __name__ == "__main__":
    main()
