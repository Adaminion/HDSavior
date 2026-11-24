#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
test-check-recovery.py - prove check-recovery.py with an independent recovery and planted faults.

For every scenario given, reference-recovery.py recovers the fixture image
(it reads only the image, never the manifest) and check-recovery.py must
accept the result.  Then faults are planted, each in a fresh copy of one
recovery, and the checker must reject each copy with a violation of the
expected category:

  flipped-byte   one byte of a file reported "ok" is changed        -> (b) and (a)
  missing        an intact file is deleted from the output           -> (a) missing
  wrong-path     an intact file is moved to another folder (and the
                 report row follows it)                              -> (a) wrong place
  lost-suffix    a file below a lost folder is renamed (report too)  -> (a) wrong place
  unsanitized    a name that H4 sanitizes is written differently     -> (d)
  damaged-ok     a damaged file's row is changed to ok_unverified    -> (c)
  extra-ok       a row claims ok for a file that matches nothing     -> (b)

    test-check-recovery.py --fixtures fixtures-out [--work DIR] [--scenarios basic,post-write,...]

Exit status 0 when every expectation held.
"""

import argparse
import csv
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT = "basic,wiped,fragmented,cluster-change,offset-mbr63,post-write,live-collision,generations,embedded,repartition"


def run(cmd):
    p = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, universal_newlines=True)
    return p.returncode, p.stdout


def check(manifest, out, extra=()):
    return run([sys.executable, "-B", os.path.join(HERE, "check-recovery.py"), "--manifest", manifest,
                "--output", out, "--max-list", "1000"] + list(extra))


def report_of(out):
    return [os.path.join(out, n) for n in os.listdir(out) if n.startswith("HDSavior-report-")][0]


def read_rows(path):
    with open(path, "rb") as f:
        text = f.read()[3:].decode("utf-8")
    return list(csv.reader(io.StringIO(text, newline="")))


def write_rows(path, rows):
    buf = io.StringIO(newline="")
    csv.writer(buf, lineterminator="\r\n").writerows(rows)
    with open(path, "wb") as f:
        f.write(b"\xef\xbb\xbf" + buf.getvalue().encode("utf-8"))


class Case:
    def __init__(self, base, work, name):
        self.dir = os.path.join(work, name)
        shutil.copytree(base, self.dir)
        self.report = report_of(self.dir)
        self.rows = read_rows(self.report)
        self.head = self.rows[0]
        self.col = {c: i for i, c in enumerate(self.head)}

    def find(self, pred):
        for r in self.rows[1:]:
            if pred(dict(zip(self.head, r))):
                return r
        return None

    def save(self):
        write_rows(self.report, self.rows)


def entries_by_offset(manifest):
    with open(manifest, "r", encoding="ascii") as f:
        m = json.load(f)
    out = {}
    for e in m["entries"]:
        ro = e["ondisk"].get("record_offset")
        if ro is not None:
            out[str(ro)] = e
    return out


def plant(kind, base, work, manifest):
    """Plant one fault; returns (case directory, expected category, expected text) or None if not possible."""
    c = Case(base, work, kind)
    ents = entries_by_offset(manifest)

    def entry(r):
        return ents.get(r["record_offset"])

    def intact(r):
        e = entry(r)
        return e is not None and e["after"]["record_intact"] and e["after"]["content_intact"] and not e["deleted"]

    if kind == "flipped-byte":
        r = c.find(lambda r: r["status"] == "ok" and int(r["size"]) > 100 and intact(r))
        if r is None:
            return None
        p = os.path.join(c.dir, r[c.col["output_path"]])
        with open(p, "r+b") as f:
            f.seek(int(r[c.col["size"]]) // 2)
            b = f.read(1)
            f.seek(-1, 1)
            f.write(bytes([b[0] ^ 0x5A]))
        return c.dir, "b", "differs from the original"
    if kind == "missing":
        r = c.find(lambda r: r["status"].startswith("ok") and intact(r))
        os.remove(os.path.join(c.dir, r[c.col["output_path"]]))
        return c.dir, "a", "missing"
    if kind == "wrong-path":
        r = c.find(lambda r: r["status"].startswith("ok") and intact(r) and "/" in r["output_path"]
                   and entry(r)["after"]["ancestry"]["path_reconstructible"])
        old = r[c.col["output_path"]]
        new = "misplaced/" + old.rsplit("/", 1)[-1]
        os.makedirs(os.path.join(c.dir, "misplaced"), exist_ok=True)
        os.rename(os.path.join(c.dir, old), os.path.join(c.dir, new))
        r[c.col["output_path"]] = new
        c.save()
        return c.dir, "a", "found at"
    if kind == "lost-suffix":
        r = c.find(lambda r: r["status"].startswith("ok") and intact(r)
                   and not entry(r)["after"]["ancestry"]["path_reconstructible"])
        if r is None:
            return None
        old = r[c.col["output_path"]]
        new = old.rsplit("/", 1)[0] + "/renamed-" + old.rsplit("/", 1)[-1]
        os.rename(os.path.join(c.dir, old), os.path.join(c.dir, new))
        r[c.col["output_path"]] = new
        c.save()
        return c.dir, "a", "found at"
    if kind == "unsanitized":
        r = c.find(lambda r: entry(r) is not None and "win32-illegal-char" in entry(r).get("name_issues", [])
                   and r["output_path"])
        if r is None:
            return None
        old = r[c.col["output_path"]]
        new = old.rsplit("/", 1)[0] + "/" + old.rsplit("/", 1)[-1].replace("_", "-")
        os.rename(os.path.join(c.dir, old), os.path.join(c.dir, new))
        r[c.col["output_path"]] = new
        c.save()
        return c.dir, "d", "H4 gives"
    if kind == "damaged-ok":
        r = c.find(lambda r: entry(r) is not None and entry(r)["type"] == "file"
                   and entry(r)["after"]["content_intact"] is False and r["output_path"]
                   and entry(r)["after"].get("damage_evidence") != ["none"])
        if r is None:
            return None
        r[c.col["status"]] = "ok_unverified"
        c.save()
        return c.dir, "c", "known to be damaged"
    if kind == "extra-ok":
        with open(os.path.join(c.dir, "invented.bin"), "wb") as f:
            f.write(b"no file of the old volume has these bytes\n")
        c.rows.append(["ok", "", "42", "42", "invented.bin", "invented.bin", "", "", "", "", ""])
        c.save()
        return c.dir, "b", "tied to no manifest file"
    raise ValueError(kind)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--fixtures", required=True)
    ap.add_argument("--work", default="")
    ap.add_argument("--scenarios", default=DEFAULT)
    ap.add_argument("--faults-in", default="basic,post-write,generations,repartition",
                    help="scenarios in which faults are planted")
    args = ap.parse_args()
    work = args.work or tempfile.mkdtemp(prefix="check-recovery-test-")
    failures = []
    passed = 0
    for s in args.scenarios.split(","):
        img = os.path.join(args.fixtures, s + ".img")
        manifest = os.path.join(args.fixtures, s + ".manifest.json")
        base = os.path.join(work, s, "recovered")
        shutil.rmtree(os.path.join(work, s), ignore_errors=True)
        os.makedirs(os.path.join(work, s))
        rc, out = run([sys.executable, "-B", os.path.join(HERE, "reference-recovery.py"), img, "--dest", base])
        if rc != 0:
            failures.append("%s: reference recovery failed:\n%s" % (s, out))
            continue
        rc, out = check(manifest, base)
        if rc != 0:
            failures.append("%s: the checker rejects a correct recovery:\n%s" % (s, out))
            continue
        passed += 1
        print("%-22s correct recovery accepted" % s)
        if s not in args.faults_in.split(","):
            continue
        for kind in ("flipped-byte", "missing", "wrong-path", "lost-suffix", "unsanitized", "damaged-ok", "extra-ok"):
            planted = plant(kind, base, os.path.join(work, s), manifest)
            if planted is None:
                print("%-22s %-13s (not applicable)" % ("", kind))
                continue
            d, cat, text = planted
            rc, out = check(manifest, d)
            hit = [line for line in out.splitlines() if line.strip().startswith("[%s]" % cat) and text in line]
            if rc == 1 and hit:
                passed += 1
                print("%-22s %-13s caught: %s" % ("", kind, hit[0].strip()[:110]))
            else:
                failures.append("%s / %s: expected a [%s] violation containing %r, exit %d:\n%s"
                                % (s, kind, cat, text, rc, out[-3000:]))
            shutil.rmtree(d)
    print("%d expectations held, %d failed (work directory %s)" % (passed, len(failures), work))
    for f in failures:
        print("FAILED: " + f)
    if not failures and not args.work:
        shutil.rmtree(work)
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
