#!/usr/bin/env python3
"""Measure what a Windows quick format leaves of an NTFS volume.

Used by scripts/windows/format-facts.ps1 on the Windows CI runner. The design
of HDSavior rests on facts about Windows' quick format that could not be
checked without Windows (docs/notes/quick-format-behaviour.md); this tool
measures them on volumes Windows itself wrote.

It decodes structures with scripts/linux/inspect-ntfs.py (independent of the
engine) and needs only the Python standard library.

    format_facts.py placement --raw PATH --size BYTES --partition-offset N --out F
    format_facts.py pre  --raw PATH --size BYTES --partition-offset N --out F
    format_facts.py post --raw PATH --size BYTES --partition-offset N
                         --pre F --truth F --out F
    format_facts.py find --raw PATH --size BYTES --marker HEX --out F

PATH is a raw device (\\\\.\\PhysicalDriveN, read-only) or an image file
(a fixed VHD is a raw disk followed by a 512-byte footer). Every read is
sector-aligned. Nothing is ever written to PATH.
"""

import argparse
import hashlib
import importlib.util
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location(
    "inspect_ntfs", os.path.join(HERE, "..", "linux", "inspect-ntfs.py"))
ins = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(ins)

SECTOR = 512
GIB = 1 << 30


class Raw:
    """Read-only, sector-aligned access to a device or an image file."""

    def __init__(self, path, size):
        self.f = open(path, "rb", buffering=0)
        self.size = size

    def read(self, off, n):
        if off < 0 or n < 0 or off + n > self.size:
            raise ValueError("read [%d, %d) outside [0, %d)" % (off, off + n, self.size))
        a = off // SECTOR * SECTOR
        e = min(self.size, (off + n + SECTOR - 1) // SECTOR * SECTOR)
        self.f.seek(a)
        buf = bytearray()
        while len(buf) < e - a:
            chunk = self.f.read(min(e - a - len(buf), 8 << 20))
            if not chunk:
                raise IOError("short read at %d" % (a + len(buf)))
            buf += chunk
        return bytes(buf[off - a: off - a + n])


def boot_at(raw, off):
    b = ins.parse_boot_sector(raw.read(off, SECTOR), off)
    if not b or not b.get("valid"):
        raise SystemExit("no valid NTFS boot sector at %d: %r" % (off, b and b.get("errors")))
    return b


def unnamed_data(rec):
    for s in rec.get("data", []):
        if s.get("name") == "" and s.get("lowest_vcn", 0) == 0:
            return s
    for s in rec.get("data", []):
        if s.get("name") == "":
            return s
    return None


def mft_layout(raw, poff, boot):
    """Runs and sizes of $MFT from record 0 of the volume at poff."""
    cs = boot["cluster_size"]
    rs = boot["mft_record_size"]
    off = poff + boot["mft_lcn"] * cs
    rec = ins.parse_file_record(raw.read(off, rs), off)
    if not rec or not rec.get("valid"):
        raise SystemExit("record 0 of the volume at %d does not parse" % poff)
    d = unnamed_data(rec)
    if not d or not d.get("non_resident"):
        raise SystemExit("$MFT has no non-resident $DATA")
    has_list = any(a.get("type") == 0x20 for a in rec.get("attributes", []))
    runs = [(r["vcn"], r["lcn"], r["length"]) for r in d.get("runs", [])]
    return {
        "cluster_size": cs, "record_size": rs, "mft_lcn": boot["mft_lcn"],
        "runs": runs, "data_size": d.get("data_size"),
        "initialized_size": d.get("initialized_size"),
        "allocated_size": d.get("allocated_size"),
        "attribute_list": has_list,
    }


def record_offset(poff, lay, n):
    cs, rs = lay["cluster_size"], lay["record_size"]
    byte = n * rs
    vcn, within = divmod(byte, cs)
    for rv, lcn, length in lay["runs"]:
        if lcn is not None and rv <= vcn < rv + length:
            return poff + (lcn + vcn - rv) * cs + within
    return None


def si_times(rec):
    for a in rec.get("attributes", []):
        if a.get("type") == 0x10 and a.get("standard_information"):
            return a["standard_information"]
    return None


def summary(rec):
    d = unnamed_data(rec)
    data = None
    if d is not None:
        if d.get("non_resident"):
            data = {"runs": [(r["vcn"], r["lcn"], r["length"]) for r in d.get("runs", [])],
                    "data_size": d.get("data_size"), "initialized_size": d.get("initialized_size"),
                    "sparse": d.get("sparse"), "compressed": d.get("compressed")}
        else:
            data = {"resident_length": d.get("value_length")}
    attr_list = None
    for a in rec.get("attributes", []):
        if a.get("type") == 0x20:
            attr_list = {"non_resident": a.get("non_resident"),
                         "entries": len(a.get("attribute_list") or [])}
    return {
        "number": rec.get("record_number"), "sequence": rec.get("sequence"),
        "in_use": rec.get("in_use"), "directory": rec.get("is_directory"),
        "base": [rec.get("base_record"), rec.get("base_sequence")],
        "fixup_ok": rec.get("fixup_ok"), "attributes": len(rec.get("attributes", [])),
        "names": [[n["name"], n["parent_record"], n["parent_sequence"], n["namespace"]]
                  for n in rec.get("names", [])],
        "si": si_times(rec), "data": data, "attribute_list": attr_list,
    }


def cmd_placement(a):
    raw = Raw(a.raw, a.size)
    boot = boot_at(raw, a.partition_offset)
    lay = mft_layout(raw, a.partition_offset, boot)
    vol = boot["volume_size"]
    probe = a.partition_offset + min(3 * GIB, vol // 3) // 4096 * 4096
    out = {
        "volume_size": vol, "cluster_size": boot["cluster_size"],
        "mft_record_size": boot["mft_record_size"], "index_block_size": boot["index_block_size"],
        "mft_lcn": boot["mft_lcn"], "mftmirr_lcn": boot["mftmirr_lcn"],
        "mft_offset": a.partition_offset + boot["mft_lcn"] * boot["cluster_size"],
        "a2_probe_offset": probe,
        "mft_offset_minus_partition_over_volume": (boot["mft_lcn"] * boot["cluster_size"]) / vol,
        "mft_initialized_size": lay["initialized_size"], "mft_runs": lay["runs"],
    }
    json.dump(out, open(a.out, "w", encoding="utf-8"), indent=1)
    print(json.dumps(out))


def cmd_pre(a):
    raw = Raw(a.raw, a.size)
    boot = boot_at(raw, a.partition_offset)
    lay = mft_layout(raw, a.partition_offset, boot)
    rs = lay["record_size"]
    records = {}
    for n in range(lay["initialized_size"] // rs):
        off = record_offset(a.partition_offset, lay, n)
        if off is None:
            continue
        b = raw.read(off, rs)
        rec = ins.parse_file_record(b, off)
        if rec and rec.get("valid") and rec.get("attributes"):
            s = summary(rec)
            s["offset"] = off
            s["raw_sha1"] = hashlib.sha1(b).hexdigest()
            records[str(n)] = s
    out = {"boot": boot, "mft": lay, "records": records}
    json.dump(out, open(a.out, "w", encoding="utf-8"))
    print("old volume: %d records with attributes below the initialized size %d; MFT at LCN %d, runs %s"
          % (len(records), lay["initialized_size"], lay["mft_lcn"], lay["runs"]))


def classify(raw, off, rs, old):
    """intact = the record's bytes are exactly what they were before the format."""
    b = raw.read(off, rs)
    if hashlib.sha1(b).hexdigest() == old["raw_sha1"]:
        return "intact", ins.parse_file_record(b, off)
    if not any(b):
        return "zeroed", None
    rec = ins.parse_file_record(b, off)
    if rec is None:
        return "not_a_record", None
    if not rec.get("attributes"):
        return "blank_record", rec
    names = [[n["name"], n["parent_record"], n["parent_sequence"], n["namespace"]]
             for n in rec.get("names", [])]
    if (rec.get("record_number") == old["number"] and rec.get("sequence") == old["sequence"]
            and names == old["names"]):
        return "rewritten_same_identity", rec
    return "other_record", rec


def content_hash(raw, poff, cs, data):
    """SHA-256 of a non-resident stream read through its runs (initialized-size rule)."""
    h = hashlib.sha256()
    size, init = data["data_size"], data["initialized_size"]
    pos = 0
    for vcn, lcn, length in data["runs"]:
        for c in range(length):
            if pos >= size:
                break
            take = min(cs, size - pos)
            if lcn is None:
                chunk = bytes(take)
            else:
                chunk = raw.read(poff + (lcn + c) * cs, take)
            if pos + take > init:
                keep = max(0, init - pos)
                chunk = chunk[:keep] + bytes(take - keep)
            h.update(chunk)
            pos += take
    if pos < size:
        return None
    return h.hexdigest()


def frn(number, sequence):
    return (sequence << 48) | number


def cmd_post(a):
    raw = Raw(a.raw, a.size)
    pre = json.load(open(a.pre, encoding="utf-8-sig"))
    truth = json.load(open(a.truth, encoding="utf-8-sig"))
    poff = a.partition_offset
    new_boot = boot_at(raw, poff)
    new = mft_layout(raw, poff, new_boot)
    old = pre["mft"]
    rs = old["record_size"]
    new_init_records = new["initialized_size"] // new["record_size"]
    facts = {
        "old_mft": {k: old[k] for k in ("mft_lcn", "cluster_size", "record_size", "runs",
                                         "initialized_size", "data_size")},
        "new_mft": {k: new[k] for k in ("mft_lcn", "cluster_size", "record_size", "runs",
                                         "initialized_size", "data_size")},
        "same_mft_location": old["mft_lcn"] == new["mft_lcn"] and old["cluster_size"] == new["cluster_size"],
        "new_initialized_records": new_init_records,
    }
    # 1. What happened to every old record.
    by_status = {}
    status_of = {}
    old_records_below_new_init = []
    for key, rec in pre["records"].items():
        n = int(key)
        st, now = classify(raw, rec["offset"], rs, rec)
        status_of[n] = (st, now)
        by_status.setdefault(st, []).append(n)
        if st == "intact" and n < new_init_records:
            old_records_below_new_init.append(n)
    facts["old_records"] = {st: {"count": len(ns), "min": min(ns), "max": max(ns)}
                            for st, ns in sorted(by_status.items())}
    destroyed = sorted(n for st, ns in by_status.items() if st != "intact" for n in ns)
    intact = sorted(by_status.get("intact", []))
    facts["highest_destroyed_record"] = destroyed[-1] if destroyed else None
    facts["lowest_intact_record"] = intact[0] if intact else None
    facts["intact_records_below_new_initialized_size"] = old_records_below_new_init
    facts["every_slot_below_new_initialized_size_rewritten"] = not old_records_below_new_init
    user = [n for n in pre["records"] if int(n) >= 24]
    facts["old_user_records"] = len(user)
    facts["old_user_records_destroyed"] = sum(1 for n in user if status_of[int(n)][0] != "intact")
    # What the destroyed slots hold now (sample of each kind with its record number).
    facts["destroyed_slot_examples"] = {
        st: ns[:5] for st, ns in sorted(by_status.items()) if st != "intact"}

    # 2. Content of every old file whose record survived.
    cs = old["cluster_size"]
    sha_by_frn = {e["frn"]: e for e in truth["entries"] if e.get("sha256")}
    checked = same = differs = no_truth = 0
    differing = []
    for n, (st, now) in status_of.items():
        if st != "intact":
            continue
        rec = pre["records"][str(n)]
        e = sha_by_frn.get(frn(n, rec["sequence"]))
        if e is None or rec["data"] is None or "runs" not in rec["data"]:
            no_truth += 1
            continue
        checked += 1
        h = content_hash(raw, poff, cs, rec["data"])
        if h == e["sha256"]:
            same += 1
        else:
            differs += 1
            differing.append(e["path"])
    facts["content_of_intact_records"] = {"checked": checked, "same": same, "differs": differs,
                                          "examples_differing": differing[:10]}

    # 3. Truth entries by phase: survival of their records.
    phases = {}
    for e in truth["entries"]:
        number, seq = e["frn"] & ((1 << 48) - 1), e["frn"] >> 48
        st = status_of.get(number, ("not_in_old_mft", None))[0]
        if st == "intact" and pre["records"][str(number)]["sequence"] != seq:
            st = "other_sequence"
        p = phases.setdefault(e["phase"], {})
        p[st] = p.get(st, 0) + 1
    facts["truth_by_phase"] = phases

    # 4. Timestamps after robocopy / Move-Item / Copy-Item from another volume.
    ts = {}
    for e in truth["entries"]:
        if e["phase"] not in ("robocopy", "moved", "copyitem") or e.get("directory"):
            continue
        number = e["frn"] & ((1 << 48) - 1)
        rec = pre["records"].get(str(number))
        if not rec or not rec.get("si"):
            continue
        si = rec["si"]
        t = ts.setdefault(e["phase"], {"files": 0, "mft_changed_after_copy_start": 0,
                                       "created_preserved": 0, "modified_preserved": 0,
                                       "mft_changed_preserved": 0})
        t["files"] += 1
        if si["mft_changed"] >= e["copy_started"]:
            t["mft_changed_after_copy_start"] += 1
        if si["created"] == e.get("source_created"):
            t["created_preserved"] += 1
        if si["modified"] == e.get("source_modified"):
            t["modified_preserved"] += 1
        if e.get("source_mft_changed") is not None and si["mft_changed"] == e["source_mft_changed"]:
            t["mft_changed_preserved"] += 1
        os_t = e.get("os_times") or {}
        if os_t.get("mft_changed") is not None and os_t["mft_changed"] >= e["copy_started"]:
            t["os_mft_changed_after_copy_start"] = t.get("os_mft_changed_after_copy_start", 0) + 1
        if os_t.get("mft_changed") is not None and os_t["mft_changed"] == si["mft_changed"]:
            t["raw_si_equals_os"] = t.get("raw_si_equals_os", 0) + 1
        if len(t.setdefault("examples", [])) < 3:
            t["examples"].append({"copy_started": e["copy_started"], "raw_si": si, "os": os_t,
                                  "source": {"created": e.get("source_created"),
                                             "modified": e.get("source_modified"),
                                             "mft_changed": e.get("source_mft_changed")}})
    facts["timestamps_after_copy"] = ts

    # 5. Slot reuse: deleted files and the files created right after them.
    deleted = sorted(e["frn"] & ((1 << 48) - 1) for e in truth["entries"] if e["phase"] == "deleted")
    reused = sorted(e["frn"] & ((1 << 48) - 1) for e in truth["entries"] if e["phase"] == "reuse")
    facts["slot_reuse"] = {"deleted_records": deleted, "new_records": reused,
                           "new_took_freed_slots": sorted(set(deleted) & set(reused))}

    # 6. The attribute list of the file with thousands of runs.
    for e in truth["entries"]:
        if e["phase"] != "many_runs":
            continue
        number = e["frn"] & ((1 << 48) - 1)
        rec = pre["records"].get(str(number))
        ext = [int(k) for k, r in pre["records"].items() if r["base"][0] == number]
        facts["many_runs_file"] = {"record": number, "attribute_list": rec and rec["attribute_list"],
                                   "extension_records": sorted(ext),
                                   "extension_status_after": sorted({status_of[x][0] for x in ext})}

    # 7. Files written after the format.
    facts["after_format"] = [{"path": e["path"], "frn": e["frn"]} for e in truth["entries"]
                             if e["phase"] in ("new", "new_deleted")]
    facts["format_time"] = truth.get("format_time")
    facts["last_old_write"] = truth.get("last_old_write")
    json.dump(facts, open(a.out, "w", encoding="utf-8"), indent=1)
    print(json.dumps(facts, indent=1))


def cmd_find(a):
    raw = Raw(a.raw, a.size)
    marker = bytes.fromhex(a.marker)
    hits = 0
    pos = 0
    step = 8 << 20
    while pos < a.size:
        n = min(step, a.size - pos)
        buf = raw.read(pos, n)
        hits += buf.count(marker)
        pos += n
    out = {"marker_hits": hits}
    json.dump(out, open(a.out, "w", encoding="utf-8"))
    print(json.dumps(out))


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    for name in ("placement", "pre", "post", "find"):
        p = sub.add_parser(name)
        p.add_argument("--raw", required=True)
        p.add_argument("--size", type=int, required=True)
        p.add_argument("--out", required=True)
        if name != "find":
            p.add_argument("--partition-offset", type=int, required=True)
        if name == "post":
            p.add_argument("--pre", required=True)
            p.add_argument("--truth", required=True)
        if name == "find":
            p.add_argument("--marker", required=True)
    a = ap.parse_args()
    {"placement": cmd_placement, "pre": cmd_pre, "post": cmd_post, "find": cmd_find}[a.cmd](a)


if __name__ == "__main__":
    main()
