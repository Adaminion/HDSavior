#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
check-recovery.py - check a recovery of a fixture against its ground truth.

Given a fixture manifest (make-fixtures.sh), the folder a recovery wrote to
and the recovery report (ALGORITHMS.md H10 / H11), this checks the properties
of ARCHITECTURE.md section 7 item 3:

  (a) every file whose MFT record and content survived the accident is in the
      output with an identical SHA-256: at its original path when that path
      can be rebuilt (manifest: after.ancestry.path_reconstructible),
      otherwise somewhere in the output with the surviving part of its
      ancestry as path suffix (after.ancestry.surviving_suffix);
  (b) no file reported ok or ok_unverified differs from the manifest's hash;
  (c) no file whose content is known to be damaged is reported ok or
      ok_unverified;
  (d) names that cannot exist unchanged on the local file system (manifest:
      name_issues) are compared after applying the sanitizing rules of
      ALGORITHMS.md H4.

It prints a summary per category and every violation, and exits with 1 when
there is a violation (2 on a usage error).  Report rows are tied to manifest
entries by the absolute record offset (record_offset column) or, for carved
files, by the absolute offset of the first data byte (first_data_offset).

    check-recovery.py --manifest fixtures-out/basic.manifest.json --output /tmp/rescue
    check-recovery.py --manifest M --output DIR --report DIR/HDSavior-report-20261124-101500.csv
    check-recovery.py ... --main-root "My Disk"           files of the main file system are below DIR/My Disk
    check-recovery.py ... --fs-root "g1=[Older file system]"
    check-recovery.py ... --include-deleted               deleted files were recovered too (H2 include_deleted)
    check-recovery.py ... --scan-range whole              the whole image was scanned, not the manifest's range
    check-recovery.py ... --allow "names/long/*"          tolerate violations for these paths (glob, repeatable)
    check-recovery.py ... --exceptions FILE               one glob per line, "#" comments

Which files are expected (a):
  - type file, after.record_intact and after.content_intact;
  - not deleted, unless --include-deleted;
  - not compressed or encrypted (ondisk.data_flags) and not inside an
    embedded file system: those are "unsupported" and never written;
  - all of its data inside the scan range (after.data_inside_scan_range),
    unless --scan-range whole.
Files of an "older" file system are accepted below the main root or below
their own root (--fs-root NAME=FOLDER, default "[Older file system]"): the
contract keeps them in the main tree when nothing proves a second file system.

Tolerated without an option: a row reported ok_unverified whose content
differs only because of an overwrite nothing on the disk shows
(after.damage_evidence == ["none"], ARCHITECTURE.md section 8: files written
and deleted after the accident).  Such rows are listed separately.

Shares no code with HDSavior or with the fixture builder.
"""

import argparse
import csv
import fnmatch
import hashlib
import io
import json
import os
import sys

OK_STATUSES = ("ok", "ok_unverified")
COLUMNS = ["status", "flags", "size", "bytes_written", "output_path", "original_path", "mft_record",
           "record_offset", "first_data_offset", "modified_utc", "detail"]

# ---------------------------------------------------------------------------
# ALGORITHMS.md H4: output names
# ---------------------------------------------------------------------------

RESERVED = set(["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"]
               + ["COM%d" % i for i in range(10)] + ["LPT%d" % i for i in range(10)]
               + ["COM¹", "COM²", "COM³", "LPT¹", "LPT²", "LPT³"])


def utf16_units(name):
    return len(name.encode("utf-16-le")) // 2


def sanitize(name):
    """Steps 1-4 of H4 for one name component (steps 5 and 6 depend on the folder)."""
    out = []
    for ch in name:
        if ch in '<>:"/\\|?*' or ord(ch) < 0x20:
            out.append("_")
        elif 0xD800 <= ord(ch) <= 0xDFFF:           # a lone surrogate (Python keeps them as such)
            out.append("�")
        else:
            out.append(ch)
    name = "".join(out).rstrip(". ")
    if not name:
        name = "_"
    base = "".join(c.upper() if "a" <= c <= "z" else c for c in name.split(".", 1)[0].rstrip(" "))
    if base in RESERVED:                         # ASCII case-insensitive
        name = "_" + name
    elif name.startswith("HDSavior-report-") or (fnmatch.fnmatchcase(name, "~hds-*.part")):
        name = "_" + name
    return name


def split_ext(name):
    """(stem, ".ext") with the extension after the last dot; a leading dot is not an extension."""
    i = name.rfind(".")
    if i <= 0:
        return name, ""
    return name[:i], name[i:]


def numbered(name, n):
    stem, ext = split_ext(name)
    return "%s (%d)%s" % (stem, n, ext)


def acceptable_names(name, issues, record):
    """Every output name the H4 rules allow for a manifest name; the exact one first."""
    clean = sanitize(name)
    names = [clean]
    if "case-collision" in issues:
        names += [numbered(clean, n) for n in range(2, 10)]
    return names


def name_matches(out_name, expected_names, record, limit_bytes):
    """Does an output name follow H4 for this entry?  Step 6 (shortening) and the ~record retry
    are accepted when the expected name is longer than the destination's component limit."""
    if out_name in expected_names:
        return True
    for exp in expected_names:
        if len(exp.encode("utf-8")) <= limit_bytes and utf16_units(exp) <= 255:
            continue
        stem, ext = split_ext(exp)
        ostem, oext = split_ext(out_name)
        if oext != ext:
            continue
        if ostem.endswith("~%d" % record):
            ostem = ostem[:-len("~%d" % record)]
        if ostem and stem.startswith(ostem):
            return True
    return False


# ---------------------------------------------------------------------------
# inputs
# ---------------------------------------------------------------------------

def read_report(path):
    with open(path, "rb") as f:
        raw = f.read()
    if not raw.startswith(b"\xef\xbb\xbf"):
        raise SystemExit("check-recovery: %s: the report does not start with a UTF-8 BOM (H10)" % path)
    text = raw[3:].decode("utf-8")
    rows = list(csv.reader(io.StringIO(text, newline="")))
    if not rows or rows[0] != COLUMNS:
        raise SystemExit("check-recovery: %s: unexpected header %r" % (path, rows[0] if rows else None))
    out = []
    for r in rows[1:]:
        if not r:
            continue
        cells = []
        for c in r:
            # H10: a cell starting with = + - @ or TAB carries an apostrophe in front
            if len(c) >= 2 and c[0] == "'" and c[1] in "=+-@\t":
                c = c[1:]
            cells.append(c)
        out.append(dict(zip(COLUMNS, cells)))
    return out


def find_report(output):
    found = sorted(n for n in os.listdir(output) if n.startswith("HDSavior-report-") and n.endswith(".csv"))
    if len(found) != 1:
        raise SystemExit("check-recovery: %d reports in %s; name one with --report" % (len(found), output))
    return os.path.join(output, found[0])


def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while True:
            b = f.read(1 << 20)
            if not b:
                return h.hexdigest()
            h.update(b)


def relative_output(path, output, report_root):
    """Report output path -> path relative to the output folder, "/" separators."""
    p = path.replace("\\", "/")
    for root in (report_root, output):
        if root:
            r = root.replace("\\", "/").rstrip("/") + "/"
            if p.startswith(r):
                return p[len(r):]
    if p.startswith("/") or (len(p) > 2 and p[1] == ":"):
        return None
    return p


def first_data_offset(entry, fs):
    od = entry["ondisk"]
    if od.get("resident") is not False or fs.get("offset") is None:
        return None
    for lcn, n in od.get("runs", []):
        if lcn is not None:
            return fs["offset"] + lcn * fs["cluster_size"]
    return None


# ---------------------------------------------------------------------------
# the check
# ---------------------------------------------------------------------------

class Result:
    def __init__(self, patterns):
        self.patterns = patterns
        self.violations = []
        self.tolerated = []
        self.counts = {}

    def count(self, cat, key, n=1):
        self.counts.setdefault(cat, {})
        self.counts[cat][key] = self.counts[cat].get(key, 0) + n

    def violate(self, cat, path, msg):
        if path is not None and any(fnmatch.fnmatchcase(path, p) for p in self.patterns):
            self.tolerated.append((cat, path, msg + " (tolerated by --allow / --exceptions)"))
            self.count(cat, "tolerated")
        else:
            self.violations.append((cat, path, msg))
            self.count(cat, "violations")


def check(args):
    with open(args.manifest, "r", encoding="ascii") as f:
        m = json.load(f)
    output = os.path.abspath(args.output)
    report_path = args.report or find_report(output)
    rows = read_report(report_path)
    patterns = list(args.allow)
    if args.exceptions:
        with open(args.exceptions, "r", encoding="utf-8") as f:
            for line in f:
                line = line.split("#", 1)[0].strip()
                if line:
                    patterns.append(line)
    res = Result(patterns)

    fss = {fs["name"]: fs for fs in m.get("file_systems", [])}
    main_name = [fs["name"] for fs in m.get("file_systems", []) if fs["kind"] == "main"]
    main_name = main_name[0] if main_name else "main"
    if not fss:
        fss = {main_name: dict(m["old_volume"], name=main_name, kind="main")}
    fs_roots = {}
    for spec in args.fs_root:
        name, _, folder = spec.partition("=")
        fs_roots[name] = folder
    entries = [(e.get("file_system", main_name), e) for e in m["entries"] + m.get("other_entries", [])]

    def roots_of(fsname):
        kind = fss.get(fsname, {}).get("kind", "main")
        if kind == "main":
            return [args.main_root]
        return [args.main_root, fs_roots.get(fsname, "[Older file system]")]

    # report rows -> entries
    by_record_offset = {}
    by_data_offset = {}
    for fsname, e in entries:
        ro = e["ondisk"].get("record_offset")
        if ro is not None and e["after"]["record_intact"]:
            by_record_offset[ro] = (fsname, e)
        do = first_data_offset(e, fss.get(fsname, {}))
        if do is not None and e["type"] == "file" and not e["deleted"]:
            by_data_offset.setdefault(do, (fsname, e))
    row_of = {}
    unmatched = []
    for r in rows:
        hit = None
        if r["record_offset"]:
            hit = by_record_offset.get(int(r["record_offset"]))
        elif r["first_data_offset"]:
            hit = by_data_offset.get(int(r["first_data_offset"]))
        if hit is None:
            unmatched.append(r)
            continue
        r["_entry"] = hit
        row_of.setdefault(id(hit[1]), []).append(r)
    res.count("report", "rows", len(rows))
    res.count("report", "rows tied to manifest entries", len(rows) - len(unmatched))

    whole = args.scan_range == "whole"

    # ---- (a) every intact file is there --------------------------------------
    for fsname, e in entries:
        if e["type"] != "file" or not e["after"]["record_intact"] or not e["after"]["content_intact"]:
            continue
        if e["deleted"] and not args.include_deleted:
            continue
        flags = e["ondisk"].get("data_flags", [])
        kind = fss.get(fsname, {}).get("kind", "main")
        if "compressed" in flags or "encrypted" in flags or kind == "embedded":
            res.count("a", "not expected: unsupported")
            continue
        if not whole and e["after"].get("data_inside_scan_range") is False:
            res.count("a", "not expected: data outside the scan range")
            continue
        res.count("a", "expected")
        anc = ancestry_of(e, whole)
        candidates = [r for r in row_of.get(id(e), []) if r["output_path"]]
        located = None
        for r in candidates:
            rel = relative_output(r["output_path"], output, args.report_root)
            if rel is None:
                res.violate("report", e["path"], "output path %r lies outside the output folder" % r["output_path"])
                continue
            if os.path.isfile(os.path.join(output, rel)):
                located = (rel, r)
                break
        if located is None:
            want = expected_paths(e, m, fsname, entries, roots_of(fsname), args)
            for rel in want:
                if rel and os.path.isfile(os.path.join(output, rel)):
                    located = (rel, None)
                    res.violate("report", e["path"], "recovered to %r but the report has no row for it" % rel)
                    break
        if located is None:
            res.violate("a", e["path"], "missing: no output file for this intact file (record %d)"
                        % e["mft_record"])
            continue
        rel, row = located
        if sha256_file(os.path.join(output, rel)) != e["sha256"]:
            res.violate("a", e["path"], "%r differs from the original (SHA-256)" % rel)
            continue
        if path_ok(rel, e, m, fsname, entries, roots_of(fsname), anc, args, res):
            res.count("a", "present, identical, right place")
        else:
            res.violate("a", e["path"], "found at %r; expected %s" % (rel, describe_expectation(e, anc, roots_of(fsname))))

    # ---- (b) and (c): what the report calls ok --------------------------------
    for r in rows:
        if "_entry" in r and r["_entry"][1]["type"] == "file" and r["_entry"][1]["after"]["content_intact"] is False:
            res.count("c", "damaged files in the report (%s)" % r["status"].split(":")[0])
        if r["status"] not in OK_STATUSES:
            res.count("b", "rows not ok (%s)" % r["status"].split(":")[0])
            continue
        res.count("b", "ok rows")
        if "_entry" not in r:
            rel = relative_output(r["output_path"], output, args.report_root)
            if rel and os.path.isfile(os.path.join(output, rel)):
                digest = sha256_file(os.path.join(output, rel))
                if any(e["sha256"] == digest for _, e in entries if e["type"] == "file"):
                    res.count("b", "ok rows not tied to an entry, content equals a manifest file")
                    continue
            res.violate("b", r["original_path"] or r["output_path"],
                        "reported %s but tied to no manifest file and equal to none" % r["status"])
            continue
        fsname, e = r["_entry"]
        rel = relative_output(r["output_path"], output, args.report_root)
        if rel is None or not os.path.isfile(os.path.join(output, rel)):
            res.violate("b", e["path"], "reported %s but %r does not exist" % (r["status"], r["output_path"]))
            continue
        same = e["type"] == "file" and sha256_file(os.path.join(output, rel)) == e["sha256"]
        if e["type"] == "file" and e["after"]["content_intact"] is False:
            evidence = e["after"].get("damage_evidence", [])
            if same:
                res.count("c", "damaged per manifest but the output equals the original")
            elif r["status"] == "ok_unverified" and evidence == ["none"]:
                res.tolerated.append(("c", e["path"], "reported ok_unverified; overwritten by data nothing on "
                                      "the disk accounts for (ARCHITECTURE.md section 8)"))
                res.count("c", "tolerated: undetectable overwrite")
                continue
            else:
                res.violate("c", e["path"], "known to be damaged (evidence: %s) but reported %s"
                            % (", ".join(evidence) or "-", r["status"]))
        if not same:
            res.violate("b", e["path"], "reported %s but %r differs from the original" % (r["status"], rel))
        else:
            res.count("b", "ok rows identical to the original")

    # ---- (d) names that need sanitizing --------------------------------------
    for fsname, e in entries:
        if not e.get("name_issues"):
            continue
        res.count("d", "entries with name issues")
        for r in row_of.get(id(e), []):
            rel = relative_output(r["output_path"], output, args.report_root) if r["output_path"] else None
            if rel is None:
                continue
            leaf = rel.rsplit("/", 1)[-1]
            name = e["path"].rsplit("/", 1)[-1]
            if name_matches(leaf, acceptable_names(name, e["name_issues"], e["mft_record"]), e["mft_record"],
                            args.component_limit):
                res.count("d", "written under the sanitized name")
            else:
                res.violate("d", e["path"], "written as %r; H4 gives %r" % (leaf, sanitize(name)))
    return res, report_path, len(unmatched)


def ancestry_of(e, whole):
    """What the manifest says survives of the entry's path, for the scan range used."""
    a = e["after"]
    if whole and "ancestry_whole_image" in a:
        return a["ancestry_whole_image"]
    return a.get("ancestry") or {"path_reconstructible": True, "surviving_suffix": e["path"]}


def components(path):
    return path.split("/")


def expected_components(path_parts, entries_by_path, fsname):
    """Sanitized names for each component of a path inside one file system."""
    out = []
    for i in range(len(path_parts)):
        p = "/".join(path_parts[:i + 1])
        e = entries_by_path.get((fsname, p))
        issues = e.get("name_issues", []) if e else []
        rec = e["mft_record"] if e else 0
        out.append(acceptable_names(path_parts[i], issues, rec))
    return out


_index_cache = {}


def entries_index(entries):
    key = id(entries)
    if key not in _index_cache:
        _index_cache[key] = {(fs, e["path"]): e for fs, e in entries}
    return _index_cache[key]


def expected_paths(e, m, fsname, entries, roots, args):
    """Exact candidate locations (first acceptable name per component), for the report-less search."""
    idx = entries_index(entries)
    anc = ancestry_of(e, args.scan_range == "whole")
    if not anc["path_reconstructible"]:
        return []
    names = [c[0] for c in expected_components(components(e["path"]), idx, fsname)]
    return ["/".join([r for r in [root] if r] + names) for root in roots]


def path_ok(rel, e, m, fsname, entries, roots, anc, args, res):
    idx = entries_index(entries)
    parts = rel.split("/")
    if anc["path_reconstructible"]:
        want = expected_components(components(e["path"]), idx, fsname)
        for root in roots:
            pre = [x for x in root.split("/") if x] if root else []
            if parts[:len(pre)] != pre or len(parts) - len(pre) != len(want):
                continue
            if all(name_matches(p, w, idx.get((fsname, "/".join(components(e["path"])[:i + 1])), {"mft_record": 0})
                                ["mft_record"], args.component_limit)
                   for i, (p, w) in enumerate(zip(parts[len(pre):], want))):
                return True
        return False
    suffix = anc["surviving_suffix"].split("/")
    full = components(e["path"])
    start = len(full) - len(suffix)
    want = expected_components(full, idx, fsname)[start:]
    if len(parts) < len(want):
        return False
    tail = parts[len(parts) - len(want):]
    return all(name_matches(p, w, idx.get((fsname, "/".join(full[:start + i + 1])), {"mft_record": 0})["mft_record"],
                            args.component_limit)
               for i, (p, w) in enumerate(zip(tail, want)))


def describe_expectation(e, anc, roots):
    if anc["path_reconstructible"]:
        return "%r below %s" % (e["path"], " or ".join(repr(r or ".") for r in roots))
    return "a path ending in %r" % anc["surviving_suffix"]


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0],
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--manifest", required=True)
    ap.add_argument("--output", required=True, help="the folder the recovery wrote to")
    ap.add_argument("--report", default="", help="the report CSV (default: the one HDSavior-report-*.csv in --output)")
    ap.add_argument("--report-root", default="", help="the output folder as the report spells it, if absolute")
    ap.add_argument("--main-root", default="", help="folder below --output that holds the main file system")
    ap.add_argument("--fs-root", action="append", default=[], metavar="NAME=FOLDER",
                    help="folder below --output for another file system of the manifest")
    ap.add_argument("--include-deleted", action="store_true", help="deleted files were recovered too")
    ap.add_argument("--scan-range", choices=("manifest", "whole"), default="manifest")
    ap.add_argument("--component-limit", type=int, default=255,
                    help="longest name, in UTF-8 bytes, the output file system accepts (default 255)")
    ap.add_argument("--allow", action="append", default=[], metavar="GLOB",
                    help="tolerate violations for manifest paths matching GLOB")
    ap.add_argument("--exceptions", default="", help="file with one GLOB per line")
    ap.add_argument("--max-list", type=int, default=200, help="list at most this many violations")
    args = ap.parse_args()
    res, report_path, unmatched = check(args)
    print("check-recovery: %s" % args.manifest)
    print("  report: %s (%d rows, %d not tied to a manifest entry)"
          % (report_path, res.counts.get("report", {}).get("rows", 0), unmatched))
    titles = {"a": "(a) intact files recovered identical, at the right place",
              "b": "(b) nothing reported ok differs from the original",
              "c": "(c) damaged files are not reported ok",
              "d": "(d) names compared after H4 sanitizing",
              "report": "report"}
    for cat in ("a", "b", "c", "d", "report"):
        c = res.counts.get(cat, {})
        print("  %s" % titles[cat])
        for k in sorted(c):
            print("      %-58s %d" % (k, c[k]))
    if res.tolerated:
        print("  tolerated (%d):" % len(res.tolerated))
        for cat, path, msg in res.tolerated[:args.max_list]:
            print("      [%s] %s: %s" % (cat, path, msg))
    if res.violations:
        print("  VIOLATIONS (%d):" % len(res.violations))
        for cat, path, msg in res.violations[:args.max_list]:
            print("      [%s] %s: %s" % (cat, path, msg))
        if len(res.violations) > args.max_list:
            print("      ... and %d more" % (len(res.violations) - args.max_list))
        sys.exit(1)
    print("  no violations")


if __name__ == "__main__":
    main()
