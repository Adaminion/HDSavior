#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
fixture_helper.py - worker behind make-fixtures.sh.

make-fixtures.sh runs the real NTFS tools (mkntfs, ntfs-3g, umount, sfdisk)
and calls this helper for everything in between.  Each scenario keeps its
working data in one JSON "state" file; the sub-commands add to it:

  samples     build the media / document samples once per run
  init        start a scenario (description, expectations)
  format      record how a volume was formatted (old or new)
  populate    create the old volume's content below a mounted directory
  capture     ground truth through the mounted driver (read-only mount)
  analyze     read the same volume raw, cross-check it against the driver's
              ground truth and against the scenario's promises
  crosscheck  compare inspect-ntfs.py with "ntfsinfo -v" on a sample of records
  wipe        overwrite old MFT record slots the way a Windows format would
  newfiles    write new data onto the re-formatted volume (post-write)
  capture-new ground truth for that new data
  parttable   read a partition table back from a whole-disk image
  finalize    compare before / after and write the manifest

The raw reading is done with inspect-ntfs.py's parser, never with the driver,
so every manifest is checked by two independent readers of the same bytes.
The manifest schema is documented in make-fixtures.sh and in README.md.
"""

import argparse
import calendar
import errno
import hashlib
import importlib.util
import json
import os
import re
import stat
import struct
import subprocess
import sys
import time

sys.dont_write_bytecode = True          # no __pycache__ next to the scripts
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import fixture_content as C  # noqa: E402


def _load_inspector():
    spec = importlib.util.spec_from_file_location("inspect_ntfs", os.path.join(HERE, "inspect-ntfs.py"))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


I = _load_inspector()

MANIFEST_VERSION = 1
EPOCH_DIFF = 116444736000000000        # FILETIME of 1970-01-01
ROOT_RECORD = 5
FIRST_USER_RECORD = 24                 # records below this are NTFS metafiles
NTFS3G_FIRST_USER_RECORD = 64          # the first record ntfs-3g gives a file on a fresh volume


class Fail(Exception):
    """A promise of a scenario does not hold; make-fixtures.sh must stop."""


def fail(msg):
    raise Fail(msg)


def log(msg):
    print("      " + msg, flush=True)


def load_state(path):
    with open(path, "r", encoding="utf-8") as f:
        return json.load(f)


def save_state(path, state):
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(state, f, ensure_ascii=True)
    os.replace(tmp, path)


def filetime(y, mo, d, h=0, mi=0, s=0, ticks=0):
    """FILETIME (100 ns ticks since 1601-01-01 UTC) of a UTC calendar time."""
    return calendar.timegm((y, mo, d, h, mi, s)) * 10000000 + EPOCH_DIFF + ticks


def unix_ms(ft):
    """FILETIME -> Unix milliseconds, rounded towards minus infinity."""
    return (ft - EPOCH_DIFF) // 10000


def sha256_hex(data):
    return hashlib.sha256(data).hexdigest()


def parent_of(path):
    return path.rsplit("/", 1)[0] if "/" in path else ""


def base_of(path):
    return path.rsplit("/", 1)[-1]


# ==========================================================================
# 1. Populating a mounted volume
# ==========================================================================

class Tree:
    """Creates files below a mounted volume and remembers what it created."""

    def __init__(self, root, samples_dir, cluster_size, record_size):
        self.root = root
        self.cs = cluster_size
        self.rs = record_size
        self.samples_dir = samples_dir
        with open(os.path.join(samples_dir, "index.json"), "r", encoding="utf-8") as f:
            self.samples = {s["name"]: s for s in json.load(f)["samples"]}
        self.live = {}            # path -> entry, in creation order
        self.deleted = {}         # path -> entry with its last known state
        self.fixed_times = {}     # path -> (created, modified, accessed) overrides
        self.notes = []
        # creation times are spread over [start year, start year + span years)
        self.time_window = (2012, 10)

    def real(self, path):
        return self.root + "/" + path

    def dump(self):
        return {"live": list(self.live.values()), "deleted": list(self.deleted.values()),
                "fixed_times": self.fixed_times, "notes": self.notes, "time_window": list(self.time_window)}

    def restore(self, saved):
        self.live = {e["path"]: e for e in saved["live"]}
        self.deleted = {e["path"]: e for e in saved["deleted"]}
        self.fixed_times = {p: tuple(v) for p, v in saved["fixed_times"].items()}
        self.notes = saved["notes"]
        self.time_window = tuple(saved["time_window"])

    # ---- creation --------------------------------------------------------
    def _add(self, path, typ, size=None, sha=None, ctype=None, tags=()):
        if path in self.live or path in self.deleted:
            fail("path %r created twice" % path)
        st = os.lstat(self.real(path))
        e = {"path": path, "type": typ, "size": size, "sha256": sha, "mft_record": st.st_ino,
             "content_type": ctype, "tags": sorted(set(tags))}
        self.live[path] = e
        return e

    def mkdir(self, path, tags=()):
        os.mkdir(self.real(path))
        return self._add(path, "dir", tags=tags)

    def mkdirs(self, path):
        parts = path.split("/")
        for i in range(1, len(parts) + 1):
            p = "/".join(parts[:i])
            if p not in self.live:
                self.mkdir(p)

    def write(self, path, data, ctype=None, tags=()):
        with open(self.real(path), "wb") as f:
            f.write(data)
        return self._add(path, "file", len(data), sha256_hex(data), ctype, tags)

    def blob(self, path, size, tags=()):
        """Arbitrary bytes (no file signature)."""
        return self.write(path, C.rnd(path, size), None, tags)

    def text(self, path, size, tags=()):
        return self.write(path, C.gen_text(path, size), "text", tags)

    def typed(self, path, size, tags=()):
        """A valid file of the type its extension names."""
        gen, ctype = C.TYPED[path.rsplit(".", 1)[-1].lower()]
        return self.write(path, gen(path, size), ctype, tags)

    def has_sample(self, name):
        return name in self.samples

    def sample(self, path, name, tags=()):
        with open(os.path.join(self.samples_dir, name), "rb") as f:
            data = f.read()
        return self.write(path, data, self.samples[name]["content_type"], tuple(tags) + ("sample",))

    def record_stat(self, path, data_sha, size, ctype=None, tags=()):
        """Register a file that was written by special means (sparse writes, truncation ...)."""
        return self._add(path, "file", size, data_sha, ctype, tags)

    def rename(self, path, new_path):
        """Move an entry (and everything below it) to another folder; it keeps its MFT record."""
        if new_path in self.live or new_path in self.deleted:
            fail("rename target %r exists" % new_path)
        ino = os.lstat(self.real(path)).st_ino
        os.rename(self.real(path), self.real(new_path))
        if os.lstat(self.real(new_path)).st_ino != ino:
            fail("%r changed its MFT record when it was moved" % path)
        moved = {}
        for p in list(self.live):
            if p == path or p.startswith(path + "/"):
                e = self.live.pop(p)
                e["path"] = new_path + p[len(path):]
                moved[e["path"]] = e
        self.live.update(moved)

    # ---- timestamps ------------------------------------------------------
    def times_for(self, path):
        if path in self.fixed_times:
            return self.fixed_times[path]
        start, span = self.time_window
        base = filetime(start, 1, 1)
        year = 365 * 86400 * 10000000
        cr = base + C.rint("time/created/" + path, 0, span * year)
        mt = cr + C.rint("time/modified/" + path, 0, 3 * year)
        at = mt + C.rint("time/accessed/" + path, 0, year)
        return cr, mt, at

    def _stamp(self, e):
        cr, mt, at = self.times_for(e["path"])
        os.setxattr(self.real(e["path"]), "system.ntfs_times", struct.pack("<qqq", cr, mt, at))
        e["created"], e["modified"], e["accessed"] = cr, mt, at

    def stamp_all(self):
        """Give every live entry its deterministic times; children before their parents."""
        for path in sorted(self.live, key=lambda p: (-p.count("/"), p)):
            self._stamp(self.live[path])

    # ---- deletion --------------------------------------------------------
    def delete(self, path, phase):
        """Delete a file, or a directory with everything below it.

        The last state seen through the driver (content hash verified by
        reading the file back, record number, times) is kept for the manifest.
        """
        if self.live[path]["type"] == "dir":
            below = [p for p in self.live if p.startswith(path + "/")]
            for p in sorted(below, key=lambda p: (-p.count("/"), p)):
                self._delete_one(p, phase)
        self._delete_one(path, phase)

    def _delete_one(self, path, phase):
        e = self.live.pop(path)
        real = self.real(path)
        self._stamp(e)
        st = os.lstat(real)
        if st.st_ino != e["mft_record"]:
            fail("%r changed its MFT record before deletion" % path)
        if e["type"] == "file":
            with open(real, "rb") as f:
                data = f.read()
            if len(data) != e["size"] or sha256_hex(data) != e["sha256"]:
                fail("%r does not read back as written (before deletion)" % path)
            os.remove(real)
        else:
            os.rmdir(real)
        e["deleted"] = True
        e["tags"] = sorted(set(e["tags"]) | {"deleted-" + phase})
        self.deleted[path] = e


def join(prefix, *parts):
    return "/".join([p for p in (prefix,) + parts if p])


# ---- building blocks ------------------------------------------------------

SAMPLE_PLACEMENT = [
    ("Photos", [("holiday.jpg", "photo.jpg"), ("portrait-exif.jpg", "photo-exif.jpg"),
                ("progressive.jpg", "photo-progressive.jpg"), ("diagram.png", "picture.png"),
                ("animation.gif", "animation.gif"), ("scan.bmp", "picture.bmp"),
                ("scan.tif", "picture.tif"), ("web.webp", "picture.webp")]),
    ("Music", [("song.mp3", "song.mp3"), ("song-noid3.mp3", "song-noid3.mp3"), ("tone.wav", "sound.wav"),
               ("voice.m4a", "voice.m4a"), ("track.flac", "track.flac"), ("track.ogg", "track.ogg")]),
    ("Videos", [("clip.mp4", "clip.mp4"), ("clip-faststart.mp4", "clip-faststart.mp4"),
                ("clip.mov", "clip.mov"), ("clip.3gp", "clip.3gp"), ("clip.avi", "clip.avi"),
                ("clip.mkv", "clip.mkv"), ("clip.webm", "clip.webm")]),
    ("Documents", [("report.pdf", "report.pdf"), ("report-updated.pdf", "report-updated.pdf"),
                   ("letter.docx", "document.docx"), ("budget.xlsx", "sheet.xlsx"),
                   ("slides.pptx", "slides.pptx"), ("notes.odt", "text.odt"), ("book.epub", "book.epub"),
                   ("backup.zip", "archive.zip"), ("backup.7z", "archive.7z"),
                   ("backup.rar", "archive.rar")]),
]

PROJECTS = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india",
            "juliett", "kilo", "lima", "mike", "november", "oscar", "papa"]

# Enough top-level folders to make the root directory's index grow beyond its
# first INDX block.  That block sits where mkntfs put it and is overwritten by
# a same-geometry re-format; the blocks added later lie elsewhere and survive,
# so some of these names can still be found after the accident and some cannot.
TOP_FOLDERS = ["Accounting 2019", "Backups", "Camera Uploads", "Downloads", "Ebooks", "Family", "Games",
               "Holiday Plans", "Invoices", "Journal", "Kitchen Recipes", "Letters", "Maps", "Notes",
               "Old Laptop", "Presentations", "Quotes", "Receipts", "Scans", "Taxes", "University",
               "Vacation 2018", "Wallpapers", "Xmas", "Yearbook", "Zipped", "archive (old)", "bin",
               "contracts", "drafts", "exports", "fonts", "garden", "household", "insurance",
               "job applications", "keys", "lectures", "misc", "new folder", "office", "printer drivers",
               "quarterly reports", "recordings", "software", "templates", "utilities", "work"]

UNICODE_FILES = [
    "Café déjà vu – naïve façade.txt",          # accented Latin, en dash
    "Zażółć gęślą jaźń.txt",                    # Polish
    "ŁÓDŹ ŚWIĘTOKRZYSKA.TXT",
    "日本語のファイル名.txt",                     # CJK
    "简体中文文件.txt",
    "한국어 파일.txt",
    "😀 emoji 🚀 name 🎉.txt",                  # outside the BMP: surrogate pairs in UTF-16
    "Ελληνικά.txt",
    "Кириллица.txt",
    "עברית.txt",
    "العربية.txt",
    "nfc-é.txt",                           # precomposed e-acute
    "nfd-é.txt",                          # e + combining acute: a different NTFS name
    "mixed 文件 – ß ÿ Ω.bin",
]
UNICODE_DIRS = ["Zdjęcia z wakacji", "写真フォルダ", "📷 Fotos 🎉"]

SPACE_DOT_NAMES = ["trailing dot.", "trailing space ", " leading space", ".leading dot", "...",
                   "two  spaces.txt", "dots...inside.txt", "dot and space. "]
WIN32_ILLEGAL_NAMES = ["colon:name.txt", "question?.txt", "star*.txt", "pipe|name.txt", 'quote"name.txt',
                       "less<greater>.txt", "back\\slash.txt", "ctrl\x01char.txt", "tab\tname.txt",
                       "CON", "nul.txt", "COM1", "LPT1.txt", "AUX.tar.gz"]


def add_samples(t, prefix, tags=()):
    """Copy every available sample into four folders below prefix."""
    for folder, files in SAMPLE_PLACEMENT:
        t.mkdirs(join(prefix, folder))
        for target, name in files:
            if t.has_sample(name):
                t.sample(join(prefix, folder, target), name, tags)


def add_prelude(t, boundary):
    """The very first user files of a volume.

    Samples first (so their MFT records are the lowest user records), then the
    directories whose records an accident destroys while their children,
    created after record number `boundary`, survive.  The Windows-like wipe
    destroys old records 0..boundary (255 when the new $MFT starts where the
    old one did).

    How the destroyed folders can still be named afterwards:
      - the 48 extra top-level folders make the root index three INDX blocks
        long; the first block is overwritten, the later ones survive;
      - Projects/p01-alpha ... are listed in Projects' INDX block, which lies
        in a data cluster and survives although Projects' record does not;
      - Projects/p01-alpha/src and small-parent/inner are listed only in the
        resident index root of a parent whose record is destroyed: nameless;
      - "Moved Early" is created early (low record) and later moved into
        "Later Parent", which is created after the boundary: its record
        survives and its resident index root still names "Moved Early".
    """
    add_samples(t, "", tags=("contiguous",))
    t.mkdir("Projects")
    for i, name in enumerate(PROJECTS):
        t.mkdir("Projects/p%02d-%s" % (i + 1, name))
    for i in range(4):
        t.mkdir("Projects/p%02d-%s/src" % (i + 1, PROJECTS[i]))
    t.mkdir("small-parent")
    t.mkdir("small-parent/inner")
    t.mkdir("Moved Early")
    for name in TOP_FOLDERS:
        t.mkdir(name)
    t.mkdir("early-junk")
    i = 0
    while True:
        e = t.blob("early-junk/junk-%04d.bin" % i, 3000 + 1500 * (i % 7))
        i += 1
        if e["mft_record"] >= boundary + 4:
            break
        if i > 4000:
            fail("could not push the MFT past record %d" % boundary)
    # everything below gets a record number above the boundary
    t.mkdir("Photos/2019-summer")
    for k in range(5):
        t.typed("Photos/2019-summer/img-%02d.png" % k, 30000 + 7000 * k)
    t.sample("Photos/copy-of-holiday.jpg", "photo.jpg")
    for i, name in enumerate(PROJECTS):
        d = "Projects/p%02d-%s" % (i + 1, name)
        t.text(d + "/README.txt", 800 + 100 * i)
        t.blob(d + "/build.bin", 20000 + 3000 * i)
    for i in range(4):
        d = "Projects/p%02d-%s/src" % (i + 1, PROJECTS[i])
        t.text(d + "/main.c", 2500 + 300 * i)
        t.text(d + "/util.h", 500)
    for i, name in enumerate(TOP_FOLDERS):
        t.text(name + "/about.txt", 300 + 17 * i)
    t.text("small-parent/inner/deep-note.txt", 1200)
    t.blob("small-parent/inner/payload.bin", 50000)
    t.text("small-parent/sibling.txt", 400)
    t.mkdir("Later Parent")
    t.rename("Moved Early", "Later Parent/Moved Early")
    t.text("Later Parent/Moved Early/kept-note.txt", 1500)
    t.typed("Later Parent/Moved Early/kept-scan.png", 45000)
    t.text("Later Parent/readme.txt", 210)
    t.typed("Music/extra-tone.wav", 90000)
    t.sample("Videos/second-copy.mp4", "clip.mp4")
    t.typed("Documents/manual.pdf", 120000)
    t.typed("Documents/more.zip", 80000)


def add_names(t, full):
    t.mkdirs("names/unicode")
    for i, name in enumerate(UNICODE_FILES if full else UNICODE_FILES[:7]):
        t.text("names/unicode/" + name, 150 + 37 * i)
    for i, name in enumerate(UNICODE_DIRS if full else UNICODE_DIRS[:1]):
        t.mkdir("names/unicode/" + name)
        t.text("names/unicode/%s/%s" % (name, UNICODE_FILES[i]), 900 + i)
        t.typed("names/unicode/%s/zdjęcie-%d.png" % (name, i), 9000)
    t.mkdir("names/long")
    t.text("names/long/long-" + "L" * 225 + ".txt", 700)                 # 234 characters
    if full:
        t.text("names/long/" + "x" * 251 + ".bin", 300)                  # 255: the NTFS maximum
        t.text("names/long/" + "長" * 251 + ".txt", 300)                 # 255 UTF-16 units, 757 UTF-8 bytes
        t.text("names/long/" + "😀" * 125 + ".txt", 300)                 # 254 UTF-16 units, 125 characters
        d = "names/long/" + "directory-with-a-very-long-name-" * 7       # 224 characters
        t.mkdir(d)
        t.text(d + "/inside-" + "y" * 200 + ".txt", 500)
        t.blob(d + "/data.bin", 12000)
    t.mkdir("names/case")
    for name in ("Readme.txt", "README.TXT", "readme.txt"):
        t.text("names/case/" + name, 100 + len(name) + ord(name[1]))
    if full:
        for name in ("Folder", "FOLDER", "folder"):
            t.mkdir("names/case/" + name)
            t.text("names/case/%s/inside.txt" % name, 200 + ord(name[1]))
    t.mkdir("names/spaces-dots")
    for name in (SPACE_DOT_NAMES if full else SPACE_DOT_NAMES[:4]):
        t.text("names/spaces-dots/" + name, 64 + len(name))
    if full:
        t.mkdir("names/spaces-dots/folder with trailing dot.")
        t.text("names/spaces-dots/folder with trailing dot./file.txt", 333)
        t.mkdir("names/spaces-dots/folder with trailing space ")
        t.blob("names/spaces-dots/folder with trailing space /file.bin", 7000)
    t.mkdir("names/win32-illegal")
    for name in (WIN32_ILLEGAL_NAMES if full else WIN32_ILLEGAL_NAMES[:5]):
        t.text("names/win32-illegal/" + name, 80 + len(name))
    # Windows-style names: ntfs-3g writes POSIX-namespace names unless a DOS
    # name is attached, which turns the long name into a WIN32 one.
    t.mkdir("names/dos")
    dos = [("Program Files", "PROGRA~1", "dir"), ("Long Document Name.docx", "LONGDO~1.DOC", "file"),
           ("short.txt", "SHORT.TXT", "file"), ("Übergrößenträger.txt", "BERGRE~1.TXT", "file")]
    for name, short, typ in dos if full else dos[:3]:
        p = "names/dos/" + name
        if typ == "dir":
            t.mkdir(p, tags=("dos-name",))
            t.text(p + "/inside.txt", 250)
        elif name.endswith(".docx") and t.has_sample("document.docx"):
            t.sample(p, "document.docx", tags=("dos-name",))
        else:
            t.text(p, 5000 if name.startswith("Ü") else 40, tags=("dos-name",))
        os.setxattr(t.real(p), "system.ntfs_dos_name", short.encode("utf-8"))
        t.live[p]["dos_name"] = short


def resident_limit(record_size):
    """Largest resident file size expected for a 14-character POSIX name.

    record header (56 bytes with 1024-byte records, 72 with 4096-byte ones)
    + $STANDARD_INFORMATION 72 + $FILE_NAME 120 + $SECURITY_DESCRIPTOR 104
    + $DATA header 24 + end marker 8.  analyze() checks where the real limit is.
    """
    usa = 2 * (record_size // 512 + 1)
    header = (48 + usa + 7) & ~7
    return record_size - (header + 72 + 120 + 104 + 24 + 8)


def add_sizes(t, big):
    cs = t.cs
    t.mkdir("sizes")
    t.write("sizes/empty.bin", b"")
    for n in (10, 300, 600):
        t.blob("sizes/resident-%04d.bin" % n, n, tags=("resident",))
    t.mkdir("sizes/limit")
    lim = resident_limit(t.rs)
    for n in range(lim - 24, lim + 28, 4):
        t.blob("sizes/limit/limit-%04d.bin" % n, n, tags=("limit",))
    t.blob("sizes/cluster-minus-1.bin", cs - 1)
    t.blob("sizes/cluster-exact.bin", cs)
    t.blob("sizes/cluster-plus-1.bin", cs + 1)
    t.blob("sizes/two-clusters-minus-1.bin", 2 * cs - 1)
    t.blob("sizes/two-clusters-exact.bin", 2 * cs)
    t.blob("sizes/two-clusters-plus-1.bin", 2 * cs + 1)
    for label, n in big:
        t.blob("sizes/%s.bin" % label, n)


def add_many(t, count):
    t.mkdir("many")
    for i in range(count):
        size = 5000 if i % 250 == 249 else (i * 37) % 700
        t.text("many/f%04d.txt" % i, size)


def add_typed(t, count):
    t.mkdir("typed")
    kinds = ["png", "bmp", "wav", "tif", "pdf", "zip", "txt"]
    sizes = [6000, 15000, 40000, 90000, 150000, 260000]
    for i in range(count):
        ext = kinds[i % len(kinds)]
        t.typed("typed/%03d-%s.%s" % (i, C.gen_text("typed/name/%d" % i, 9).decode("ascii").replace(" ", "-")
                                      .replace("\n", "-"), ext),
                sizes[(i // len(kinds) + i) % len(sizes)] + 13 * i)


def add_deep(t, levels):
    path = "deep"
    t.mkdir(path)
    for i in range(1, levels + 1):
        path += "/level-%02d-%s" % (i, "abcdefghijklmnopqrstuvwxyz"[i % 26] * 22)
        t.mkdir(path)
        t.text(path + "/note-%02d.txt" % i, 100 + 50 * i)
    t.blob(path + "/bottom.bin", 33333)          # full path: about 30 characters per level


def add_special(t, full, compression):
    t.mkdirs("special/streams")
    p = "special/streams/download.exe"
    t.write(p, b"MZ" + C.rnd(p, 9000), None, tags=("ads",))
    os.setxattr(t.real(p), "user.Zone.Identifier", b"[ZoneTransfer]\r\nZoneId=3\r\n")
    if full:
        p = "special/streams/two-streams.txt"
        t.text(p, 2000, tags=("ads",))
        os.setxattr(t.real(p), "user.small", b"a small named stream")
        os.setxattr(t.real(p), "user.large", C.rnd(p + ":large", 20000))
    t.mkdir("special/times")
    edge = {
        "before-unix-epoch.txt": (filetime(1965, 3, 1, 12, 0, 0, 1234567), filetime(1969, 12, 31, 23, 59, 59, 9999999),
                                  filetime(1969, 12, 31, 23, 59, 59, 9999999)),
        "dos-epoch.txt": (filetime(1980, 1, 1), filetime(1980, 1, 1), filetime(1980, 1, 1)),
        "y2038.txt": (filetime(2038, 1, 19, 3, 14, 7), filetime(2038, 1, 19, 3, 14, 8, 5000000),
                      filetime(2038, 1, 19, 3, 14, 9)),
        "year-2100.txt": (filetime(2100, 2, 28, 23, 59, 59, 9999999), filetime(2100, 3, 1), filetime(2100, 3, 1, 0, 0, 1)),
        "whole-seconds.txt": (filetime(2015, 6, 30, 23, 59, 59), filetime(2016, 12, 31, 23, 59, 59),
                              filetime(2017, 1, 1)),
        "one-tick.txt": (filetime(2020, 2, 29, 12, 0, 0, 1), filetime(2020, 2, 29, 12, 0, 0, 2),
                         filetime(2020, 2, 29, 12, 0, 0, 3)),
    }
    for name in sorted(edge):
        t.text("special/times/" + name, 120)
        t.fixed_times["special/times/" + name] = edge[name]
    if compression and t.cs <= 4096:
        d = "special/compressed"
        t.mkdir(d)
        attrib = struct.unpack("<I", os.getxattr(t.real(d), "system.ntfs_attrib"))[0]
        os.setxattr(t.real(d), "system.ntfs_attrib", struct.pack("<I", attrib | 0x800))
        t.write(d + "/compressible.txt", C.gen_text(d + "/c", 3000) * 45, "text", tags=("compressed",))
        t.blob(d + "/incompressible.bin", 200000, tags=("compressed",))
        t.write(d + "/tiny.txt", b"resident data in a compressed folder", "text",
                tags=("compressed", "resident"))


def add_trash(t):
    """Files and folders that are deleted before the accident (see finish_trash)."""
    t.mkdir("trash")
    t.text("trash/survivor.txt", 500)
    t.blob("trash/early-1.bin", 20000)
    t.text("trash/early-2.txt", 200)
    t.mkdir("trash/early-folder")
    t.blob("trash/early-folder/e-a.bin", 30000)
    t.text("trash/early-folder/e-b.txt", 120)
    t.blob("trash/early-folder/e-c.bin", 9000)
    t.blob("trash/late-1.bin", 50000)
    t.text("trash/late-2.txt", 300)
    t.typed("trash/late-3.png", 40000)
    t.mkdir("trash/late-folder")
    t.blob("trash/late-folder/l-a.bin", 25000)
    t.text("trash/late-folder/l-b.txt", 100)
    t.mkdir("trash/late-folder/sub")
    t.blob("trash/late-folder/sub/l-c.bin", 12000)


def trash_early(t):
    """Last step of populate phase 1: delete the early victims."""
    for p in ("trash/early-1.bin", "trash/early-2.txt", "trash/early-folder"):
        t.delete(p, "early")


def trash_late(t):
    """Populate phase 2, after a remount.

    A freshly mounted ntfs-3g searches the MFT bitmap from the start, so the
    four files created first reuse the four lowest freed records: those of
    early-1, early-2, early-folder and early-folder's first child.  (Within
    one mount it would have continued behind the last record it handed out.)
    The folder's other two children stay behind as deleted records whose
    parent record now belongs to an unrelated file.
    """
    for i in range(4):
        t.blob("trash/reuser-%d.bin" % i, 15000 + 4096 * i)
    # Late: nothing is created afterwards, so records and clusters stay intact.
    for p in ("trash/late-1.bin", "trash/late-2.txt", "trash/late-3.png", "trash/late-folder"):
        t.delete(p, "late")


# ---- profiles -------------------------------------------------------------

# Every profile runs in two phases with an unmount / mount in between (see
# trash_late for why).  Phase 1 creates the tree, phase 2 finishes it.

def profile_full(t, opts, phase):
    if phase == 2:
        return trash_late(t)
    if opts["prelude"]:
        add_prelude(t, opts["prelude"])
    add_samples(t, "media")
    add_typed(t, 140)
    add_names(t, True)
    add_sizes(t, [("big-1MiB", 1 << 20), ("big-3MiB-odd", (3 << 20) + 12345), ("big-5MiB", 5 << 20)])
    add_deep(t, 14)
    add_many(t, 3000)
    add_special(t, True, opts["compression"])
    add_trash(t)
    trash_early(t)


def profile_standard(t, opts, phase):
    """A medium-sized general tree (cluster-change, post-write, live-collision)."""
    if phase == 2:
        return trash_late(t)
    if opts["prelude"]:
        add_prelude(t, opts["prelude"])
    add_samples(t, "media")
    add_typed(t, 70)
    add_names(t, False)
    add_sizes(t, [("big-1MiB", 1 << 20), ("big-4MiB-odd", (4 << 20) + 4321)])
    add_deep(t, 8)
    add_many(t, 1200)
    add_special(t, False, opts["compression"])
    add_trash(t)
    trash_early(t)


def profile_tiny(t, opts, phase):
    """A small tree for the cluster-size, record-size and partition-offset scenarios."""
    if phase == 2:
        return trash_late(t)
    if opts["prelude"]:
        add_prelude(t, opts["prelude"])
    add_samples(t, "media")
    add_typed(t, 14)
    add_names(t, False)
    add_sizes(t, [("big-1MiB", 1 << 20), ("big-2MiB-odd", (2 << 20) + 777)])
    add_deep(t, 6)
    add_many(t, 400)
    add_special(t, False, opts["compression"])
    add_trash(t)
    trash_early(t)


def write_until_full(t, path, tag):
    """Fill the volume completely with one file; returns the bytes written."""
    chunk = C.rnd(tag, 1 << 20)
    h = hashlib.sha256()
    total = 0
    with open(t.real(path), "wb", buffering=0) as f:
        for size in (1 << 20, 65536, 4096):
            while True:
                try:
                    n = f.write(chunk[:size])
                except OSError as e:
                    if e.errno != errno.ENOSPC:
                        raise
                    n = 0
                if n:
                    h.update(chunk[:n])
                    total += n
                if n < size:
                    break
    if os.path.getsize(t.real(path)) != total:
        fail("ballast size %d differs from the %d bytes written" % (os.path.getsize(t.real(path)), total))
    t.record_stat(path, h.hexdigest(), total)
    return total


def profile_fragmented(t, opts, phase):
    if phase == 2:
        return          # nothing may be created after the ballast is deleted: it would reuse its record
    cs = t.cs
    if opts["prelude"]:
        add_prelude(t, opts["prelude"])
    add_typed(t, 14)
    add_names(t, False)
    add_sizes(t, [("big-1MiB", 1 << 20)])

    # --- sparse files: written with seeks, so ntfs-3g leaves real holes -----
    t.mkdir("sparse")
    a, b, c = C.rnd("sparse/a", 8192), C.rnd("sparse/b", 8192), C.rnd("sparse/c", 100)
    p = "sparse/holes-in-the-middle.bin"
    with open(t.real(p), "wb") as f:
        f.write(a)
        f.seek(1 << 20)
        f.write(b)
        f.seek(3 << 20)
        f.write(c)
    content = a + bytes((1 << 20) - 8192) + b + bytes((2 << 20) - 8192) + c
    t.record_stat(p, sha256_hex(content), len(content), tags=("sparse",))
    p = "sparse/hole-at-start.bin"
    z = C.rnd("sparse/z", 10000)
    with open(t.real(p), "wb") as f:
        f.seek(200000)
        f.write(z)
    t.record_stat(p, sha256_hex(bytes(200000) + z), 210000, tags=("sparse",))
    p = "sparse/extended-by-truncate.bin"
    y = C.rnd("sparse/y", 5000)
    with open(t.real(p), "wb") as f:
        f.write(y)
        f.truncate(500000)
    t.record_stat(p, sha256_hex(y + bytes(495000)), 500000, tags=("sparse", "initialized-short"))

    # --- valid data shorter than the file, with stale bytes behind it -------
    # write 20000 bytes, cut the file to 5000, grow it to 60000: the driver
    # keeps initialized_size at 5000 and returns zeros behind it, but the
    # second cluster still holds bytes 5000..8191 of the first write.
    t.mkdir("valid-data")
    p = "valid-data/stale-bytes-after-initialized-size.bin"
    x = C.rnd("vdl/x", 20000)
    with open(t.real(p), "wb") as f:
        f.write(x)
        f.flush()
        f.truncate(5000)
        f.flush()
        f.truncate(60000)
    t.record_stat(p, sha256_hex(x[:5000] + bytes(55000)), 60000, tags=("initialized-short", "stale-tail"))

    # --- the classic fragmentation recipe -----------------------------------
    t.blob("spacer.bin", 4 << 20)
    t.mkdir("filler")
    count = opts.get("filler", 2400)
    for i in range(count):
        t.blob("filler/s%05d.dat" % i, cs)
    total = write_until_full(t, "ballast.bin", "ballast")
    log("volume filled: ballast.bin has %d bytes" % total)
    for i in range(1, count, 2):
        t.delete("filler/s%05d.dat" % i, "early")
    t.mkdir("frag")
    free = os.statvfs(t.root).f_bfree
    clusters = min(count // 2, free) - 24
    t.blob("frag/big-fragmented.bin", clusters * cs, tags=("fragmented",))
    log("frag/big-fragmented.bin: %d clusters written into the holes" % clusters)

    # --- fragment the MFT ----------------------------------------------------
    # The ballast occupies what is left of the MFT zone.  Freeing the spacer
    # gives room elsewhere; creating more files than there are free records
    # forces the MFT to grow there, in pieces.
    t.delete("spacer.bin", "early")
    t.mkdir("late")
    n_late = count // 2 + 500
    for i in range(n_late):
        t.text("late/l%04d.txt" % i, 20 + i % 300)
    t.delete("ballast.bin", "late")
    t.notes.append("ballast.bin (%d bytes) filled the volume and was deleted last; its record and "
                   "content are still intact before the accident" % total)


# ---- profiles for the scenarios with more than one file system ---------------

G1_FOLDERS = ["G1 Archive %02d" % i for i in range(24)]


def profile_gen1(t, opts, phase):
    """g1: the file system formatted over by g2 (generations).  A few thousand files.

    Its folders come first, so their records are among the lowest (the format
    over g1 destroys them).  Six "G1 Late" folders are created when the MFT
    has reached about record 400: g2 later puts its own records there, while
    their children, created last, survive behind g2's records and name those
    slots as their parent.  Creation times 2008-2013.
    """
    if phase == 2:
        return
    t.time_window = (2008, 6)
    for name in G1_FOLDERS:
        t.mkdir(name)
    t.mkdir("G1 Mail")
    t.mkdir("G1 Mail/Inbox")
    kinds = ["png", "pdf", "bmp", "zip", "wav"]

    def archive_file(i, k):
        p = "%s/g1-%02d-%03d" % (G1_FOLDERS[i], i, k)
        if k % 10 == 3:
            t.typed(p + "." + kinds[(i + k) % len(kinds)], 9000 + 1700 * ((i * 7 + k) % 23))
        elif k % 3 == 0:
            t.text(p + ".txt", 300 + 97 * ((i + k) % 40))
        else:
            t.blob(p + ".bin", 2000 + 611 * ((i * 13 + k) % 50))

    for i in range(len(G1_FOLDERS)):
        for k in range(13):
            archive_file(i, k)
    late = ["G1 Late %d" % i for i in range(6)]
    for name in late:
        t.mkdir(name)
    for i in range(len(G1_FOLDERS)):
        for k in range(13, 100):
            archive_file(i, k)
    for k in range(150):
        t.text("G1 Mail/Inbox/message-%04d.eml" % k, 900 + 31 * k)
    t.typed("G1 Mail/attachment-large.pdf", 2 << 20)
    for i, name in enumerate(late):
        for k in range(8):
            t.text("%s/late-%d-%d.txt" % (name, i, k), 1000 + 77 * k)


def profile_gen2(t, opts, phase):
    """g2: the lost file system of the generations scenarios (a few hundred entries).

    Same prelude as the other scenarios (its first records are destroyed by
    the accident), then a few folders of its own.  Creation times 2019-2022,
    or, with opts["restored"], the same spread as g1 (2008-2013): files
    restored from a backup keep their old creation times.
    """
    if phase == 2:
        return trash_late(t)
    t.time_window = (2008, 6) if opts["restored"] else (2019, 4)
    if opts["prelude"]:
        add_prelude(t, opts["prelude"])
    t.mkdir("G2 Documents")
    for k in range(90):
        p = "G2 Documents/report-%03d" % k
        if k % 6 == 0:
            t.typed(p + ".pdf", 20000 + 900 * k)
        else:
            t.text(p + ".txt", 400 + 53 * k)
    t.mkdir("G2 Photos")
    for k in range(30):
        t.typed("G2 Photos/photo-%03d.png" % k, 30000 + 2100 * k)
    t.blob("G2 Photos/album.bin", 3 << 20)
    add_trash(t)
    trash_early(t)


def profile_inner(t, opts, phase):
    """The volume inside a disk-image file (embedded): more entries than its host."""
    if phase == 2:
        return
    t.time_window = (2016, 4)
    t.mkdir("Guest Documents")
    for k in range(320):
        t.text("Guest Documents/guest-note-%04d.txt" % k, 200 + 37 * (k % 60))
    t.mkdir("Guest Pictures")
    for k in range(120):
        t.typed("Guest Pictures/guest-pic-%03d.png" % k, 7000 + 450 * (k % 30))
    t.mkdir("Guest Projects")
    for i in range(10):
        d = "Guest Projects/guest-proj-%02d" % i
        t.mkdir(d)
        for k in range(25):
            t.text("%s/guest-src-%02d.c" % (d, k), 800 + 70 * k)
    t.blob("Guest Projects/guest-build.bin", 1 << 20)


def write_container(t, path, source, companion, tags):
    """Copy a raw volume image into a file of the mounted host volume.

    Zero 64 KiB blocks are skipped with a seek, so the driver leaves holes
    (sparse runs); after every MiB of the image, 256 KiB are appended to a
    companion file, so the two files take turns at the allocation cursor and
    the container ends up in several fragments.
    """
    block = 65536
    h = hashlib.sha256()
    size = os.path.getsize(source)
    with open(source, "rb") as src, open(t.real(path), "wb") as dst, open(t.real(companion), "wb") as comp:
        pos = 0
        while pos < size:
            data = src.read(block)
            h.update(data)
            if data.count(0) != len(data):
                dst.seek(pos)
                dst.write(data)
                dst.flush()
            pos += len(data)
            if pos % (1 << 20) == 0:
                comp.write(C.rnd("%s/%d" % (companion, pos), 1 << 18))
                comp.flush()
        dst.truncate(size)
    t.record_stat(path, h.hexdigest(), size, tags=tags)
    with open(t.real(companion), "rb") as f:
        data = f.read()
    t.record_stat(companion, sha256_hex(data), len(data))


def profile_host(t, opts, phase):
    """The host of embedded: a few hundred entries and one disk-image file (opts["source"])."""
    if phase == 2:
        return trash_late(t)
    if opts["prelude"]:
        add_prelude(t, opts["prelude"])
    t.mkdir("Host Documents")
    for k in range(120):
        t.text("Host Documents/host-letter-%03d.txt" % k, 500 + 41 * k)
    add_typed(t, 14)
    t.mkdir("Virtual Machines")
    write_container(t, "Virtual Machines/old-laptop.img", opts["source"], "Virtual Machines/old-laptop.swap",
                    tags=("container", "sparse", "split"))
    t.text("Virtual Machines/notes.txt", 700)
    add_trash(t)
    trash_early(t)


def profile_repart(t, opts, phase):
    """The large old volume of repartition: data all over the volume."""
    if phase == 2:
        return trash_late(t)
    if opts["prelude"]:
        add_prelude(t, opts["prelude"])
    add_samples(t, "media")
    add_typed(t, 70)
    add_names(t, False)
    add_many(t, 800)
    t.mkdir("big")
    for i in range(10):
        t.blob("big/chunk-%02d.bin" % i, (3 << 20) + 77777 * i)
    t.mkdir("late")
    for i in range(200):
        t.typed("late/late-%03d.%s" % (i, ["png", "pdf", "zip", "txt"][i % 4]), 30000 + 1300 * (i % 50))
    add_trash(t)
    trash_early(t)


PROFILES = {
    "full": profile_full,
    "standard": profile_standard,
    "tiny": profile_tiny,
    "fragmented": profile_fragmented,
    "gen1": profile_gen1,
    "gen2": profile_gen2,
    "inner": profile_inner,
    "host": profile_host,
    "repart": profile_repart,
}


def cmd_populate(args):
    state = load_state(args.state)
    old = state["old_volume"]
    t = Tree(args.root, args.samples, old["cluster_size"], old["mft_record_size"])
    opts = {"compression": args.compression, "prelude": args.prelude, "restored": args.restored,
            "source": args.source}
    if args.phase == 2:
        t.restore(state.pop("tree"))
    PROFILES[args.profile](t, opts, args.phase)
    if args.phase == 1:
        state["tree"] = t.dump()
        save_state(args.state, state)
        return
    t.stamp_all()
    state["profile"] = args.profile
    state["expected"] = list(t.live.values())
    state["deleted"] = list(t.deleted.values())
    state["notes"].extend(t.notes)
    save_state(args.state, state)
    files = sum(1 for e in t.live.values() if e["type"] == "file")
    log("populated: %d files, %d folders, %d deleted entries" % (files, len(t.live) - files, len(t.deleted)))


# ==========================================================================
# 2. Ground truth through the mounted driver
# ==========================================================================

def walk_mounted(root):
    """Every file and directory below root as the driver shows it."""
    found = {}

    def visit(rel):
        with os.scandir(root + "/" + rel if rel else root) as it:
            names = sorted(e.name for e in it)
        for name in names:
            path = rel + "/" + name if rel else name
            full = root + "/" + path
            st = os.lstat(full)
            cr, mt, at, _ = struct.unpack("<4q", os.getxattr(full, "system.ntfs_times"))
            e = {"path": path, "mft_record": st.st_ino, "created": cr, "modified": mt, "accessed": at,
                 "nlink": st.st_nlink}
            # stat() must tell the same story as the raw FILETIME values.  (Only
            # from 1970 on: the kernel clamps earlier times of a FUSE file
            # system to 0, which is why the xattr is the source of truth.)
            for ft, ns in ((mt, st.st_mtime_ns), (at, st.st_atime_ns)):
                if ft >= EPOCH_DIFF and ns != (ft - EPOCH_DIFF) * 100:
                    fail("%r: stat times disagree with system.ntfs_times" % path)
            try:
                e["dos_name"] = os.getxattr(full, "system.ntfs_dos_name").decode("utf-8")
            except OSError:
                pass
            streams = sorted(x[5:] for x in os.listxattr(full) if x.startswith("user."))
            if streams:
                e["streams"] = streams
            if stat.S_ISDIR(st.st_mode):
                e["type"] = "dir"
                e["size"] = None
                e["sha256"] = None
                found[path] = e
                visit(path)
            elif stat.S_ISREG(st.st_mode):
                h = hashlib.sha256()
                n = 0
                with open(full, "rb") as f:
                    while True:
                        b = f.read(1 << 20)
                        if not b:
                            break
                        h.update(b)
                        n += len(b)
                if n != st.st_size:
                    fail("%r: read %d bytes but stat says %d" % (path, n, st.st_size))
                e["type"] = "file"
                e["size"] = n
                e["sha256"] = h.hexdigest()
                found[path] = e
            else:
                fail("%r is neither a file nor a directory" % path)

    visit("")
    return found


def cmd_capture(args):
    """Read everything back through a read-only mount and compare with what populate wrote."""
    state = load_state(args.state)
    expected = {e["path"]: e for e in state["expected"]}
    found = walk_mounted(args.root)
    missing = sorted(set(expected) - set(found))
    extra = sorted(set(found) - set(expected))
    if missing or extra:
        fail("the mounted tree differs from what was written: missing %r, unexpected %r"
             % (missing[:5], extra[:5]))
    for path, e in expected.items():
        f = found[path]
        for key in ("type", "size", "sha256", "mft_record", "created", "modified", "accessed"):
            if f[key] != e[key]:
                fail("%r: %s is %r through the driver, expected %r" % (path, key, f[key], e[key]))
        if f.get("dos_name") != e.get("dos_name"):
            fail("%r: DOS name %r, expected %r" % (path, f.get("dos_name"), e.get("dos_name")))
        e["nlink"] = f["nlink"]
        if "streams" in f:
            e["streams"] = f["streams"]
    state["captured"] = True
    save_state(args.state, state)
    log("captured: %d entries read back through the driver match what was written" % len(found))


# ==========================================================================
# 3. Reading a volume raw
# ==========================================================================

class RawVolume:
    """An NTFS volume read without a driver, on top of inspect-ntfs.py's parsers."""

    def __init__(self, path, offset=0):
        self.path = path
        self.base = offset
        self.fd = os.open(path, os.O_RDONLY)
        self.boot = I.parse_boot_sector(os.pread(self.fd, 512, offset), 0)
        if not self.boot or not self.boot["valid"]:
            fail("%s: no valid NTFS boot sector at byte %d" % (path, offset))
        self.cs = self.boot["cluster_size"]
        self.rs = self.boot["mft_record_size"]
        self.sector = self.boot["bytes_per_sector"]
        self._records = {}
        self.ext = {}
        self._load_mft()

    def close(self):
        os.close(self.fd)

    def pread(self, off, n):
        """n bytes at volume-relative offset off."""
        data = os.pread(self.fd, n, self.base + off)
        if len(data) != n:
            fail("%s: short read at %d" % (self.path, off))
        return data

    # ---- $MFT ------------------------------------------------------------
    def _load_mft(self):
        off0 = self.boot["mft_lcn"] * self.cs
        rec0 = I.parse_file_record(self.pread(off0, self.rs), off0)
        if not rec0 or not rec0["valid"] or rec0["record_number"] != 0 or not rec0["in_use"]:
            fail("%s: record 0 at the boot sector's MFT position is not a valid $MFT record" % self.path)
        pieces = [d for d in rec0["data"] if d["name"] == "" and d["non_resident"]]
        if len(pieces) != 1 or pieces[0]["lowest_vcn"] != 0 or not pieces[0]["runlist_ok"]:
            fail("%s: unexpected $MFT $DATA attribute" % self.path)
        self.mft_runs = [(r["lcn"], r["length"]) for r in pieces[0]["runs"]]
        self.mft_data_size = pieces[0]["data_size"]
        self.mft_initialized = pieces[0]["initialized_size"]
        self._records[0] = rec0
        # A $MFT so fragmented that its own runlist needs extension records:
        # the further pieces are named by its attribute list and lie in the
        # part of the MFT that is already mapped.
        if any(a["type"] == 0x20 for a in rec0["attributes"]):
            next_vcn = pieces[0]["highest_vcn"] + 1
            for e in sorted(self.attribute_list(0), key=lambda e: e["lowest_vcn"]):
                if e["type"] != 0x80 or e["name"] or e["lowest_vcn"] == 0:
                    continue
                holder = self.record(e["record"])
                for d in holder["data"]:
                    if d["name"] == "" and d["non_resident"] and d["lowest_vcn"] == next_vcn:
                        self.mft_runs += [(r["lcn"], r["length"]) for r in d["runs"]]
                        next_vcn = d["highest_vcn"] + 1
        if any(lcn is None for lcn, _ in self.mft_runs):
            fail("%s: the $MFT has a sparse run" % self.path)
        self.nrecords = self.mft_initialized // self.rs
        self.mft_bitmap = None

    def record_offset(self, n):
        """Volume-relative byte offset of MFT record n, or None beyond the MFT."""
        vcn, within = divmod(n * self.rs, self.cs)
        pos = 0
        for lcn, length in self.mft_runs:
            if vcn < pos + length:
                if within + self.rs > (pos + length - vcn) * self.cs:
                    fail("MFT record %d straddles two runs" % n)
                return (lcn + vcn - pos) * self.cs + within
            pos += length
        return None

    def mft_run_of(self, n):
        """Index of the $MFT data run that holds record n."""
        vcn = n * self.rs // self.cs
        pos = 0
        for i, (_, length) in enumerate(self.mft_runs):
            if vcn < pos + length:
                return i
            pos += length
        return None

    def raw_record(self, n):
        return self.pread(self.record_offset(n), self.rs)

    def record(self, n):
        if n not in self._records:
            off = self.record_offset(n)
            self._records[n] = None if off is None else I.parse_file_record(self.pread(off, self.rs), off)
        return self._records[n]

    def load_all(self):
        """Parse every record and find extension records through their base references."""
        self.ext = {}
        for n in range(self.nrecords):
            r = self.record(n)
            if r is None or not r["valid"]:
                continue
            if r["in_use"] and (r["base_record"], r["base_sequence"]) != (0, 0):
                self.ext.setdefault(r["base_record"], []).append(n)

    # ---- attributes of one file ---------------------------------------------
    def gather(self, n, extensions=None):
        """[(holder record number, attribute)] of the base record and its extension records."""
        base = self.record(n)
        out = [(n, a) for a in base["attributes"]]
        for x in (self.ext.get(n, []) if extensions is None else extensions):
            xr = self.record(x)
            out += [(x, a) for a in xr["attributes"]]
        return out

    def attribute_list(self, n):
        """Entries of record n's $ATTRIBUTE_LIST (resident or not), or None without one."""
        for a in self.record(n)["attributes"]:
            if a["type"] != 0x20:
                continue
            if not a["non_resident"]:
                return a["attribute_list"]
            value = self.read_runs([(r["lcn"], r["length"]) for r in a["runs"]], a["data_size"])
            return I.parse_attribute_list_value(value)
        return None

    def read_runs(self, runs, size):
        out = bytearray()
        for lcn, length in runs:
            if len(out) >= size:
                break
            if lcn is None:
                out += bytes(length * self.cs)
            else:
                out += self.pread(lcn * self.cs, length * self.cs)
        if len(out) < size:
            fail("runs cover %d bytes, %d needed" % (len(out), size))
        return bytes(out[:size])

    def stream(self, n, name="", attrs=None):
        """One $DATA stream of file n put together from all its pieces, or None."""
        parts = [(h, a) for h, a in (attrs or self.gather(n)) if a["type"] == 0x80 and a["name"] == name]
        if not parts:
            return None
        if not parts[0][1]["non_resident"]:
            if len(parts) != 1:
                fail("record %d: resident $DATA in several pieces" % n)
            h, a = parts[0]
            return {"resident": True, "holders": [h], "value_offset": a["value_offset"],
                    "size": a["value_length"], "flags": a["flags"]}
        parts.sort(key=lambda p: p[1]["lowest_vcn"])
        first = parts[0][1]
        runs = []
        next_vcn = 0
        for h, a in parts:
            if a["lowest_vcn"] != next_vcn or not a["runlist_ok"]:
                fail("record %d: $DATA pieces do not line up at VCN %d" % (n, next_vcn))
            runs += [(r["lcn"], r["length"]) for r in a["runs"]]
            next_vcn = a["highest_vcn"] + 1
        return {"resident": False, "holders": [h for h, _ in parts], "runs": runs,
                "size": first["data_size"], "allocated": first["allocated_size"],
                "initialized": first["initialized_size"], "flags": first["flags"],
                "compression_unit": first["compression_unit"], "compressed_size": first["compressed_size"]}

    def fixed_record(self, n):
        """Bytes of record n after undoing the fixups (as they are if it is no sane FILE record)."""
        buf = bytearray(self.raw_record(n))
        head = I.parse_file_record(bytes(buf), 0)
        if head is not None and head["usn"] is not None:
            I.apply_fixups(buf, head["usa_offset"], head["usa_count"])
        return buf

    def read_compressed(self, s):
        """Decode an LZNT1-compressed stream, compression unit by compression unit."""
        unit = 1 << s["compression_unit"]                   # clusters per compression unit
        lcns = []
        for lcn, length in s["runs"]:
            lcns += [None] * length if lcn is None else list(range(lcn, lcn + length))
        out = bytearray()
        for u in range(0, len(lcns), unit):
            if len(out) >= s["size"]:
                break
            stored = [x for x in lcns[u:u + unit] if x is not None]
            raw = b"".join(self.pread(x * self.cs, self.cs) for x in stored)
            if not stored:
                out += bytes(unit * self.cs)                # an all-zero unit takes no clusters
            elif len(stored) == len(lcns[u:u + unit]):
                out += raw                                  # a unit that did not shrink is stored raw
            else:
                out += lznt1_decompress(raw).ljust(unit * self.cs, b"\x00")
        return bytes(out)

    def read_stream(self, s):
        """Content of a stream as an application would see it."""
        if s["resident"]:
            buf = self.fixed_record(s["holders"][0])
            return bytes(buf[s["value_offset"]:s["value_offset"] + s["size"]])
        if s["flags"] & 0x00FF:
            data = bytearray(self.read_compressed(s)[:s["size"]])
        else:
            data = bytearray(self.read_runs(s["runs"], s["size"]))
        init = min(s["initialized"], s["size"])
        if init < s["size"]:
            data[init:] = bytes(s["size"] - init)       # bytes behind the valid data read as zero
        return bytes(data)

    def extents(self, s):
        """Allocated byte ranges [(volume-relative offset, length)] of a non-resident stream."""
        out = []
        for lcn, length in s["runs"]:
            if lcn is None:
                continue
            if out and out[-1][0] + out[-1][1] == lcn * self.cs:
                out[-1] = (out[-1][0], out[-1][1] + length * self.cs)
            else:
                out.append((lcn * self.cs, length * self.cs))
        return out

    # ---- directories -----------------------------------------------------
    def directory(self, n):
        """Entries of directory n from its index root and all in-use INDX blocks.

        Returns (entries, block offsets).  Checks along the way that every
        block carries the VCN its position demands: block k is at byte k *
        index_block_size, and the VCN counts clusters when a cluster is not
        larger than an index block, 512-byte units otherwise.
        """
        attrs = self.gather(n)
        root = [a for _, a in attrs if a["type"] == 0x90 and a["name"] == "$I30"]
        if len(root) != 1 or "index_root" not in root[0]:
            fail("record %d has no $INDEX_ROOT:$I30" % n)
        ir = root[0]["index_root"]
        entries = [e for e in ir["entries"] if e["file_name"]]
        alloc = sorted([a for _, a in attrs if a["type"] == 0xA0 and a["name"] == "$I30"],
                       key=lambda a: a["lowest_vcn"])
        offsets = []
        if alloc:
            runs = []
            for a in alloc:
                runs += [(r["lcn"], r["length"]) for r in a["runs"]]
            ibs = ir["index_block_size"]
            bitmap = [a for _, a in attrs if a["type"] == 0xB0 and a["name"] == "$I30"]
            if len(bitmap) != 1:
                fail("record %d: $INDEX_ALLOCATION without $BITMAP" % n)
            if bitmap[0]["non_resident"]:
                bits = self.read_runs([(r["lcn"], r["length"]) for r in bitmap[0]["runs"]],
                                      bitmap[0]["data_size"])
            else:
                bits = bytes.fromhex(bitmap[0]["bitmap_hex"])
            for k in range(alloc[0]["data_size"] // ibs):
                if not (k // 8 < len(bits) and bits[k // 8] >> (k % 8) & 1):
                    continue
                off = self.map_offset(runs, k * ibs)
                blk = I.parse_index_block(self.pread(off, ibs), off)
                if blk is None or not blk["valid"]:
                    fail("record %d: index block %d at byte %d is not a valid INDX block" % (n, k, off))
                want = k * ibs // self.cs if self.cs <= ibs else k * ibs // 512
                if blk["vcn"] != want:
                    fail("record %d: index block %d carries VCN %d, expected %d" % (n, k, blk["vcn"], want))
                offsets.append(off)
                entries += [e for e in blk["entries"] if e["file_name"]]
        return entries, offsets

    def map_offset(self, runs, byte):
        """Volume-relative offset of byte `byte` of an attribute with these runs."""
        vcn, within = divmod(byte, self.cs)
        pos = 0
        for lcn, length in runs:
            if vcn < pos + length:
                return (lcn + vcn - pos) * self.cs + within
            pos += length
        fail("byte %d lies beyond the runlist" % byte)

    def label(self):
        for a in self.record(3)["attributes"]:
            if a["type"] == 0x60:
                return a.get("volume_name", "")
        return ""

    def geometry(self):
        b = self.boot
        return {"sector_size": b["bytes_per_sector"], "cluster_size": b["cluster_size"],
                "mft_record_size": b["mft_record_size"], "index_block_size": b["index_block_size"],
                "total_sectors": b["total_sectors"], "hidden_sectors": b["hidden_sectors"],
                "mft_lcn": b["mft_lcn"], "mftmirr_lcn": b["mftmirr_lcn"], "serial": b["serial"],
                "label": self.label(),
                "mft_runs": [[lcn, length] for lcn, length in self.mft_runs],
                "mft_fragments": len(self.mft_runs),
                "mft_records": self.nrecords}


def lznt1_decompress(buf):
    """LZNT1 (NTFS compression): a sequence of chunks of up to 4096 uncompressed bytes.

    Each chunk starts with a le16 header: bits 0-11 = size of the chunk data
    minus 1, bit 15 = compressed.  A compressed chunk is a series of flag bytes,
    each followed by eight items: a literal byte (flag bit 0) or a le16 token
    (flag bit 1) holding a back reference whose split between offset and length
    bits depends on how much of the chunk has been produced so far.
    Only used to check the compressed fixture files against the driver.
    """
    out = bytearray()
    pos = 0
    while pos + 2 <= len(buf):
        header = I.u16(buf, pos)
        pos += 2
        if header == 0:
            break
        chunk = buf[pos:pos + (header & 0x0FFF) + 1]
        pos += len(chunk)
        if not header & 0x8000:
            out += chunk
            continue
        start = len(out)
        cpos = 0
        while cpos < len(chunk):
            flags = chunk[cpos]
            cpos += 1
            for bit in range(8):
                if cpos >= len(chunk):
                    break
                if not flags >> bit & 1:
                    out.append(chunk[cpos])
                    cpos += 1
                    continue
                token = I.u16(chunk, cpos)
                cpos += 2
                shift = 12
                i = len(out) - start - 1
                while i >= 0x10:
                    shift -= 1
                    i >>= 1
                length = (token & ((1 << shift) - 1)) + 3
                offset = (token >> shift) + 1
                for _ in range(length):
                    out.append(out[-offset])
    return bytes(out)


def names_of(vol, n):
    """Every $FILE_NAME of file n with the record that holds it."""
    out = []
    for h, a in vol.gather(n):
        if a["type"] == 0x30 and a.get("file_name"):
            fn = dict(a["file_name"])
            fn["holder"] = h
            out.append(fn)
    return out


def standard_information(vol, n):
    for a in vol.record(n)["attributes"]:
        if a["type"] == 0x10 and "standard_information" in a:
            return a["standard_information"]
    return None


def flag_names(flags):
    out = []
    if flags & 0x00FF:
        out.append("compressed")
    if flags & 0x4000:
        out.append("encrypted")
    if flags & 0x8000:
        out.append("sparse")
    return out


def describe_file(vol, n, attrs):
    """The "ondisk" facts of a file's unnamed data stream and what is needed to re-read it."""
    s = vol.stream(n, "", attrs)
    if s is None:
        fail("record %d has no unnamed $DATA" % n)
    d = {"resident": s["resident"], "data_flags": flag_names(s["flags"])}
    if s["resident"]:
        d["fragments"] = 0
    else:
        d["runs"] = [[lcn, length] for lcn, length in s["runs"]]
        d["fragments"] = len(vol.extents(s))
        d["allocated_size"] = s["allocated"]
        d["initialized_size"] = s["initialized"]
        if s["compressed_size"] is not None:
            d["compressed_size"] = s["compressed_size"]
            d["compression_unit"] = s["compression_unit"]
    return s, d


def analyze_volume(state, vol):
    """Cross-check the driver's ground truth against the raw volume; returns notes.

    Adds "mft_sequence" and "ondisk" to every entry of state["expected"] and
    state["deleted"].
    """
    vol.load_all()
    notes = []
    live = {e["path"]: e for e in state["expected"]}
    by_record = {}
    for e in state["expected"]:
        if e["mft_record"] in by_record:
            fail("%r and %r share MFT record %d" % (e["path"], by_record[e["mft_record"]]["path"], e["mft_record"]))
        by_record[e["mft_record"]] = e
    children = {}
    for e in state["expected"]:
        children.setdefault(parent_of(e["path"]), []).append(e)
    verified = 0
    decoded = []
    used_as_extension = set()

    # mkntfs quirk: note records whose self-number does not match their slot
    odd = [n for n in range(vol.nrecords)
           if vol.record(n) and vol.record(n)["valid"] and vol.record(n)["record_number"] != n]
    if odd:
        in_use = [n for n in odd if vol.record(n)["in_use"]]
        if in_use:
            fail("in-use records with a wrong self-number: %r" % in_use[:10])
        notes.append("old MFT slots %s hold unused FILE records whose record-number field (0x2C) reads %s: "
                     "mkntfs gives the unused records of its initial MFT allocation the number 0 (slots 16-23, "
                     "and with clusters larger than 27 KiB also the slots from 27 to the end of the first cluster)"
                     % (ranges(odd), sorted(set(vol.record(n)["record_number"] for n in odd))))

    for e in state["expected"]:
        n = e["mft_record"]
        path = e["path"]
        r = vol.record(n)
        if r is None or not r["valid"] or not r["in_use"] or r["record_number"] != n:
            fail("%r: MFT record %d is not a valid in-use record" % (path, n))
        if (r["base_record"], r["base_sequence"]) != (0, 0):
            fail("%r: MFT record %d is an extension record" % (path, n))
        if r["is_directory"] != (e["type"] == "dir"):
            fail("%r: directory flag of record %d contradicts the driver" % (path, n))
        exts = vol.ext.get(n, [])
        for x in exts:
            if vol.record(x)["base_sequence"] != r["sequence"]:
                fail("%r: extension record %d names base sequence %d, base has %d"
                     % (path, x, vol.record(x)["base_sequence"], r["sequence"]))
            used_as_extension.add(x)
        attrs = vol.gather(n)
        # The attribute list must name exactly the extension records that the
        # base references lead to (the two ways of finding them must agree).
        al = vol.attribute_list(n)
        if (al is None) != (not exts):
            fail("%r: attribute list %s but %d extension records" % (path, "absent" if al is None else "present", len(exts)))
        if al is not None:
            listed = sorted(set(x["record"] for x in al if x["record"] != n))
            if listed != sorted(exts):
                fail("%r: attribute list names records %r, base references give %r" % (path, listed, sorted(exts)))
            for x in al:
                if vol.record(x["record"])["sequence"] != x["sequence"]:
                    fail("%r: attribute list entry has a stale sequence number" % path)
                if x["type"] == 0x80 and not any(
                        h == x["record"] and a["type"] == 0x80 and a["name"] == x["name"]
                        and (not a["non_resident"] or a["lowest_vcn"] == x["lowest_vcn"]) for h, a in attrs):
                    fail("%r: attribute list names a $DATA piece at VCN %d that record %d lacks"
                         % (path, x["lowest_vcn"], x["record"]))
        # name and parent
        names = names_of(vol, n)
        parent = parent_of(path)
        parent_rec = live[parent]["mft_record"] if parent else ROOT_RECORD
        parent_seq = vol.record(parent_rec)["sequence"]
        mine = [fn for fn in names if fn["name"] == base_of(path) and fn["namespace"] != 2]
        if len(mine) != 1:
            fail("%r: record %d holds names %r" % (path, n, [fn["name"] for fn in names]))
        for fn in names:
            if (fn["parent_record"], fn["parent_sequence"]) != (parent_rec, parent_seq):
                fail("%r: parent reference %d/%d, expected %d/%d"
                     % (path, fn["parent_record"], fn["parent_sequence"], parent_rec, parent_seq))
        dos = [fn["name"] for fn in names if fn["namespace"] == 2]
        if "dos_name" in e:
            want_ns = 3 if e["dos_name"] == base_of(path).upper() and not dos else 1
            if mine[0]["namespace"] != want_ns or (want_ns == 1 and dos != [e["dos_name"]]):
                fail("%r: namespaces %r do not match DOS name %r"
                     % (path, [(fn["name"], fn["namespace"]) for fn in names], e["dos_name"]))
        elif dos or mine[0]["namespace"] != 0:
            fail("%r: unexpected name namespaces %r" % (path, [(fn["name"], fn["namespace"]) for fn in names]))
        if r["link_count"] != len(names) or len(names) != (2 if dos else 1):
            fail("%r: link count %d with %d names" % (path, r["link_count"], len(names)))
        # times
        si = standard_information(vol, n)
        for key in ("created", "modified", "accessed"):
            if si[key] != e[key]:
                fail("%r: $STANDARD_INFORMATION %s is %d, the driver said %d" % (path, key, si[key], e[key]))
        # Sorted, not in on-disk order: when a file has a WIN32 and a DOS name, which of the
        # two $FILE_NAME attributes comes first in the record differs from build to build
        # (ntfs-3g orders them by comparing the attribute values, which start with timestamps).
        rank = {"WIN32": 0, "WIN32+DOS": 0, "POSIX": 1, "DOS": 2}
        ondisk = {"record_offset": r["mft_origin"] + n * vol.rs,
                  "names": [{"name": fn["name"], "namespace": fn["namespace_name"], "record": fn["holder"]}
                            for fn in sorted(names, key=lambda fn: (rank[fn["namespace_name"]], fn["name"]))],
                  "extension_records": sorted(exts),
                  "attribute_list": "none" if al is None else
                  ("non-resident" if any(a["type"] == 0x20 and a["non_resident"] for a in r["attributes"])
                   else "resident")}
        e["mft_sequence"] = r["sequence"]
        if e["type"] == "file":
            s, facts = describe_file(vol, n, attrs)
            ondisk.update(facts)
            if s["size"] != e["size"]:
                fail("%r: $DATA size %d, the driver said %d" % (path, s["size"], e["size"]))
            data = vol.read_stream(s)
            if sha256_hex(data) != e["sha256"]:
                fail("%r: content read through the runlist does not match the driver's SHA-256" % path)
            verified += 1
            if s["flags"] & 0x00FF and not s["resident"]:
                decoded.append(path)
            named = sorted(set(a["name"] for _, a in attrs if a["type"] == 0x80 and a["name"]))
            if named != e.get("streams", []):
                fail("%r: named streams %r on disk, %r through the driver" % (path, named, e.get("streams", [])))
        else:
            entries, blocks = vol.directory(n)
            got = sorted((x["record"], x["sequence"], x["file_name"]["name"]) for x in entries)
            want = []
            for c in children.get(path, []):
                cseq = vol.record(c["mft_record"])["sequence"]
                want.append((c["mft_record"], cseq, base_of(c["path"])))
                if "dos_name" in c and c["dos_name"] != base_of(c["path"]).upper():
                    want.append((c["mft_record"], cseq, c["dos_name"]))
                elif "dos_name" in c and [fn for fn in names_of(vol, c["mft_record"]) if fn["namespace"] == 2]:
                    want.append((c["mft_record"], cseq, c["dos_name"]))
            if got != sorted(want):
                fail("%r: index entries differ from the driver's listing (%d on disk, %d expected)"
                     % (path, len(got), len(want)))
            for x in entries:
                if x["file_name"]["parent_record"] != n:
                    fail("%r: index entry with a foreign parent reference" % path)
            ondisk["index"] = "non-resident" if blocks else "resident"
            ondisk["index_blocks"] = len(blocks)
        e["ondisk"] = ondisk
        check_tags(vol, e, attrs)

    # ---- deleted entries --------------------------------------------------
    deleted = {e["path"]: e for e in state["deleted"]}
    for e in state["deleted"]:
        n = e["mft_record"]
        path = e["path"]
        r = vol.record(n)
        parent = parent_of(path)
        if parent in live:
            parent_rec = live[parent]["mft_record"]
        elif parent in deleted:
            parent_rec = deleted[parent]["mft_record"]
        else:
            parent_rec = ROOT_RECORD
        state_now = "reused"
        if r is not None and r["valid"] and not r["in_use"] and (r["base_record"], r["base_sequence"]) == (0, 0):
            names = [a["file_name"] for a in r["attributes"] if a["type"] == 0x30 and a.get("file_name")]
            if any(fn["name"] == base_of(path) and fn["parent_record"] == parent_rec for fn in names) \
                    and r["is_directory"] == (e["type"] == "dir"):
                state_now = "intact"
        ondisk = {"record_state": state_now}
        e["mft_sequence"] = None
        if state_now == "intact":
            # deleting bumps the sequence number; references to the file carry the old one
            e["mft_sequence"] = (r["sequence"] - 1) & 0xFFFF
            ondisk["record_offset"] = r["mft_origin"] + n * vol.rs
            si = standard_information(vol, n)
            for key in ("created", "modified", "accessed"):
                if si[key] != e[key]:
                    fail("%r (deleted): $STANDARD_INFORMATION %s differs from the last known value" % (path, key))
            # extension records of a deleted file are no longer findable through in-use base references;
            # take the ones its attribute list names if they still point back
            exts = []
            al = vol.attribute_list(n)
            if al is not None:
                for x in sorted(set(x["record"] for x in al if x["record"] != n)):
                    xr = vol.record(x)
                    if xr and xr["valid"] and not xr["in_use"] and xr["base_record"] == n:
                        exts.append(x)
                    else:
                        state_now = ondisk["record_state"] = "extension-records-lost"
            ondisk["extension_records"] = exts
            if e["type"] == "file" and state_now == "intact":
                attrs = vol.gather(n, exts)
                s, facts = describe_file(vol, n, attrs)
                ondisk.update(facts)
                data = vol.read_stream(s) if s["size"] == e["size"] else None
                ondisk["content_intact_before_accident"] = (data is not None and sha256_hex(data) == e["sha256"])
        e["ondisk"] = ondisk

    # every in-use record of the volume must be accounted for
    for n in range(FIRST_USER_RECORD, vol.nrecords):
        r = vol.record(n)
        if r and r["valid"] and r["in_use"] and n not in by_record and n not in used_as_extension:
            if r["names"] and all(fn["parent_record"] == 11 for fn in r["names"]):
                continue                                  # $Extend\$Quota, $ObjId, $Reparse
            fail("MFT record %d (%r) is in use but unknown to the manifest"
                 % (n, [fn["name"] for fn in r["names"]]))
    msg = ("all %d live files re-read raw (resident data from the fixed-up record, the rest through the "
           "runlists) give the SHA-256 the driver gave%s"
           % (verified, "; %d of them had to be LZNT1-decompressed" % len(decoded) if decoded else ""))
    log("verified: " + msg)
    state["verified"].append(msg)
    return notes


def ranges(nums):
    """[1,2,3,7] -> '1-3, 7'"""
    nums = sorted(nums)
    out = []
    i = 0
    while i < len(nums):
        j = i
        while j + 1 < len(nums) and nums[j + 1] == nums[j] + 1:
            j += 1
        out.append(str(nums[i]) if i == j else "%d-%d" % (nums[i], nums[j]))
        i = j + 1
    return ", ".join(out)


def check_tags(vol, e, attrs):
    """Every tag is a promise about the on-disk form; break the build if one is false."""
    tags = e["tags"]
    od = e["ondisk"]
    path = e["path"]
    if "resident" in tags and not od["resident"]:
        fail("%r should be resident" % path)
    if "contiguous" in tags and not od["resident"] and od["fragments"] != 1:
        fail("%r should be stored in one piece but has %d fragments" % (path, od["fragments"]))
    if "sparse" in tags:
        if "sparse" not in od["data_flags"] or not any(lcn is None for lcn, _ in od["runs"]):
            fail("%r should be sparse (flag 0x8000 and at least one hole run)" % path)
    if "compressed" in tags and "compressed" not in od["data_flags"]:
        fail("%r should carry the compressed attribute flag" % path)
    if "initialized-short" in tags and not od["initialized_size"] < e["size"]:
        fail("%r should have initialized_size < size" % path)
    if "stale-tail" in tags:
        s = vol.stream(e["mft_record"], "", attrs)
        raw = vol.read_runs(s["runs"], min(s["allocated"], s["size"]))
        stale = sum(1 for b in raw[s["initialized"]:] if b)
        if not stale:
            fail("%r should have non-zero bytes on disk behind its initialized size" % path)
        od["stale_bytes_after_initialized_size"] = stale
    if "fragmented" in tags:
        pieces = [(h, a) for h, a in attrs if a["type"] == 0x80 and a["name"] == ""]
        if od["attribute_list"] == "none" or len(pieces) < 2 or not od["extension_records"]:
            fail("%r should need an $ATTRIBUTE_LIST and extension records (it has %d $DATA piece(s))"
                 % (path, len(pieces)))
        od["data_pieces"] = len(pieces)
        od["data_runs"] = len(od["runs"])
        od["data_extension_records"] = sorted(set(h for h, _ in pieces if h != e["mft_record"]))
    if "ads" in tags and not e.get("streams"):
        fail("%r should have a named data stream" % path)
    if "dos-name" in tags and "dos_name" not in e:
        fail("%r should have a DOS name" % path)
    if "split" in tags and (od["resident"] or od["fragments"] < 2):
        fail("%r should be stored in at least two fragments" % path)


EXPECT_PRE = {}
EXPECT_POST = {}


def expect_pre(name):
    def deco(fn):
        EXPECT_PRE[name] = fn
        return fn
    return deco


def expect_post(name):
    def deco(fn):
        EXPECT_POST[name] = fn
        return fn
    return deco


@expect_pre("resident-limit")
def _(state, vol):
    lim = [e for e in state["expected"] if "limit" in e["tags"]]
    res = [e["size"] for e in lim if e["ondisk"]["resident"]]
    non = [e["size"] for e in lim if not e["ondisk"]["resident"]]
    if not res or not non or max(res) > min(non):
        fail("the files in sizes/limit do not straddle the resident limit: resident %r, non-resident %r" % (res, non))
    return "largest resident file in sizes/limit: %d bytes; smallest non-resident one: %d bytes" % (max(res), min(non))


@expect_pre("indx")
def _(state, vol):
    best = max((e["ondisk"]["index_blocks"], e["path"]) for e in state["expected"] if e["type"] == "dir")
    if best[0] < 2:
        fail("no directory has more than one INDX block")
    total = sum(e["ondisk"]["index_blocks"] for e in state["expected"] if e["type"] == "dir")
    return "%d INDX blocks in use, %d of them in %r" % (total, best[0], best[1])


@expect_pre("deleted")
def _(state, vol):
    d = state["deleted"]
    intact_files = [e for e in d if e["type"] == "file" and e["ondisk"]["record_state"] == "intact"
                    and e["ondisk"].get("content_intact_before_accident")]
    intact_dirs = [e for e in d if e["type"] == "dir" and e["ondisk"]["record_state"] == "intact"]
    reused = [e for e in d if e["ondisk"]["record_state"] == "reused"]
    orphans = [e for e in d if e["ondisk"]["record_state"] == "intact"
               and any(p["path"] == parent_of(e["path"]) and p["ondisk"]["record_state"] == "reused" for p in d)]
    if not intact_files or not intact_dirs or not reused or not orphans:
        fail("deleted entries: %d intact files, %d intact folders, %d reused records, %d with a reused parent; "
             "all four kinds are required" % (len(intact_files), len(intact_dirs), len(reused), len(orphans)))
    return ("deleted before the accident: %d entries; %d files with intact record and content, %d folders with "
            "intact record, %d whose record was reused by a later file, %d intact ones whose parent folder's "
            "record was reused" % (len(d), len(intact_files), len(intact_dirs), len(reused), len(orphans)))


@expect_pre("fragmented-file")
def _(state, vol):
    out = []
    for e in state["expected"]:
        if "fragmented" in e["tags"]:
            od = e["ondisk"]
            name_rec = [x["record"] for x in od["names"]]
            if name_rec != [e["mft_record"]]:
                od["name_in_extension_record"] = True
            pieces = set(vol.mft_run_of(n) for n in [e["mft_record"]] + od["extension_records"])
            od["mft_fragments_spanned"] = len(pieces)
            out.append("%s: %s $ATTRIBUTE_LIST, %d extension records (%d of them hold $DATA pieces), "
                       "%d data runs in %d $DATA pieces, %d fragments; $FILE_NAME lives in record %s "
                       "(base record %d); its %d records lie in %d different fragments of the $MFT"
                       % (e["path"], od["attribute_list"], len(od["extension_records"]),
                          len(od["data_extension_records"]), od["data_runs"], od["data_pieces"],
                          od["fragments"], name_rec, e["mft_record"], 1 + len(od["extension_records"]),
                          len(pieces)))
    if not out:
        fail("no file is tagged fragmented")
    return "; ".join(out)


@expect_pre("sparse")
def _(state, vol):
    sp = [e for e in state["expected"] if "sparse" in e["tags"]]
    short = [e for e in state["expected"] if "initialized-short" in e["tags"]]
    stale = [e for e in state["expected"] if "stale-tail" in e["tags"]]
    if not sp or not short or not stale:
        fail("sparse / initialized-short / stale-tail files are missing")
    return ("%d sparse files with hole runs (compression_unit %s, attribute flag 0x8000); %d files with "
            "initialized_size < size; %s keeps %d stale non-zero bytes on disk behind its initialized size"
            % (len(sp), sorted(set(e["ondisk"].get("compression_unit") for e in sp)), len(short),
               stale[0]["path"], stale[0]["ondisk"]["stale_bytes_after_initialized_size"]))


@expect_pre("mft-fragmented")
def _(state, vol):
    if len(vol.mft_runs) < 2:
        fail("the old $MFT is in one piece; this scenario promises a fragmented MFT")
    return "old $MFT is in %d fragments: %s" % (len(vol.mft_runs),
                                                " ".join("%d+%d" % r for r in vol.mft_runs[:12])
                                                + (" ..." if len(vol.mft_runs) > 12 else ""))


@expect_pre("deleted-ballast")
def _(state, vol):
    b = [e for e in state["deleted"] if e["path"] == "ballast.bin"]
    if not b or b[0]["ondisk"]["record_state"] != "intact" or not b[0]["ondisk"]["content_intact_before_accident"]:
        fail("ballast.bin should be a deleted file with intact record and content")
    reused = sum(1 for e in state["deleted"] if e["ondisk"]["record_state"] == "reused")
    return ("deleted before the accident: ballast.bin (%d bytes in %d fragments, record and content intact) "
            "and %d other files whose MFT records were reused by later files"
            % (b[0]["size"], b[0]["ondisk"]["fragments"], reused))


@expect_pre("mft-contiguous-256")
def _(state, vol):
    lcn, length = vol.mft_runs[0]
    if lcn != vol.boot["mft_lcn"] or length * vol.cs < 256 * vol.rs:
        fail("the first run of the old $MFT does not cover records 0..255")
    return "old MFT records 0..255 are contiguous from byte %d of the volume" % (lcn * vol.cs)


@expect_pre("compressed")
def _(state, vol):
    c = [e for e in state["expected"] if "compressed" in e["tags"]]
    if len(c) < 3:
        fail("compressed files are missing (does this ntfs-3g support the compression mount option?)")
    return ("%d files carry the compressed attribute flag: %s"
            % (len(c), ", ".join("%s (%s)" % (base_of(e["path"]),
                                              "resident, stored raw" if e["ondisk"]["resident"] else
                                              "%d of %d bytes allocated" % (e["ondisk"].get("compressed_size", 0),
                                                                           e["ondisk"]["allocated_size"]))
                                 for e in c)))


@expect_pre("samples-contiguous")
def _(state, vol):
    c = [e for e in state["expected"] if "contiguous" in e["tags"]]
    if len(c) < 18:
        fail("only %d contiguous samples" % len(c))
    return "%d samples among the first user files, each stored in one fragment" % len(c)


def cmd_analyze(args):
    state = load_state(args.state)
    if not state.get("captured"):
        fail("analyze must run after capture")
    vol = RawVolume(args.volume)
    geo = vol.geometry()
    for key in ("cluster_size", "mft_record_size", "sector_size"):
        if geo[key] != state["old_volume"][key]:
            fail("old volume %s is %d, the format step recorded %d" % (key, geo[key], state["old_volume"][key]))
    state["old_volume"].update(geo)
    state["notes"].extend(analyze_volume(state, vol))
    for name in state["expect"]:
        if name in EXPECT_PRE:
            msg = EXPECT_PRE[name](state, vol)
            log("verified: " + msg)
            state["verified"].append(msg)
        elif name not in EXPECT_POST:
            fail("unknown expectation %r" % name)
    vol.close()
    state["analyzed"] = True
    save_state(args.state, state)


# ==========================================================================
# 4. inspect-ntfs.py versus ntfsinfo
# ==========================================================================

NTFSINFO_NAMESPACE = {"POSIX": 0, "Win32": 1, "DOS": 2, "Win32 & DOS": 3}


def _num(v):
    v = v.strip().split()[0]
    return int(v, 16) if v.lower().startswith("0x") else int(v)


def _paren_hex(v):
    return int(re.search(r"\(0x([0-9a-fA-F]+)\)", v).group(1), 16)


def _quoted(v):
    v = v.strip()
    return v[1:-1] if len(v) >= 2 and v[0] == "'" and v[-1] == "'" else v


def _secs(ft):
    return (ft - EPOCH_DIFF) // 10000000


def _ctime(v):
    """'Tue Nov 24 01:37:50 2026 UTC' -> Unix seconds"""
    return calendar.timegm(time.strptime(v.strip().replace(" UTC", ""), "%a %b %d %H:%M:%S %Y"))


def parse_ntfsinfo(text):
    """Turn 'ntfsinfo -i N -v' output into {"header": {...}, "attributes": [...]}."""
    header = {}
    attrs = []
    cur = None
    group = None
    for line in text.split("\n"):
        if line.startswith("Dumping attribute "):
            m = re.match(r"Dumping attribute (\S+) \(0x([0-9a-f]+)\) from mft record (\d+)", line)
            cur = {"type": int(m.group(2), 16), "holder": int(m.group(3)), "fields": {}, "runs": None,
                   "groups": [], "bad": False}
            attrs.append(cur)
            group = None
            continue
        if line.startswith("Dumping Inode") or line.startswith("End of inode") or line.startswith("Total runs"):
            continue
        depth = len(line) - len(line.lstrip("\t"))
        body = line.strip("\t")
        if cur is not None and (line.startswith("Corrupt attribute") or line.startswith("Failed to read")):
            cur["bad"] = True
            continue
        if not body.strip():
            group = None
            continue
        if cur is not None and depth == 3 and cur["runs"] is not None and group is None:
            cols = body.split()
            if len(cols) == 3:
                # <RL_NOT_MAPPED> rows are ntfsinfo's placeholders for the VCN ranges that
                # other pieces of the attribute cover; they are not on disk
                if cols[1] != "<RL_NOT_MAPPED>":
                    cur["runs"].append((int(cols[0], 16), None if cols[1] == "<HOLE>" else int(cols[1], 16),
                                        int(cols[2], 16)))
                continue
        # "Key:<tab> value", "Key: value" or "Key<tab><tab> value"; values may contain colons
        m = re.match(r"([^:\t]+?)\s*(?::|\t)\s*(.*)$", body, re.S)
        if not m:
            continue
        key, value = m.group(1).strip(), m.group(2).strip()
        if cur is None:
            header[key] = value
        elif depth == 1:
            if key == "Runlist":
                cur["runs"] = []
            elif key.startswith("Dumping index") or key == "Dumping attribute list":
                group = None
            elif key not in cur["fields"]:
                cur["fields"][key] = value
        elif depth == 2:
            if group is None:
                group = {}
                cur["groups"].append(group)
            group[key] = value
    return {"header": header, "attributes": attrs}


def compare_name(mism, where, grp, fn, check_times=True):
    pairs = [("Parent directory", fn["parent_record"], _num), ("Allocated Size", fn["allocated_size"], _num),
             ("Data Size", fn["data_size"], _num), ("Filename Length", len(fn["name"].encode("utf-16-le")) // 2, _num),
             ("File attributes", fn["file_attributes"], _paren_hex),
             ("Namespace", fn["namespace"], lambda v: NTFSINFO_NAMESPACE[v.strip()]),
             ("Filename", fn["name"], _quoted)]
    if check_times:
        pairs += [("File Creation Time", _secs(fn["created"]), _ctime),
                  ("File Altered Time", _secs(fn["modified"]), _ctime),
                  ("MFT Changed Time", _secs(fn["mft_changed"]), _ctime),
                  ("Last Accessed Time", _secs(fn["accessed"]), _ctime)]
    for key, mine, conv in pairs:
        if key not in grp:
            mism.append("%s: ntfsinfo prints no %r" % (where, key))
            continue
        try:
            theirs = conv(grp[key])
        except (ValueError, KeyError, AttributeError, OverflowError):
            mism.append("%s: cannot read ntfsinfo's %r = %r" % (where, key, grp[key]))
            continue
        if theirs != mine:
            mism.append("%s: %s is %r in inspect-ntfs, %r in ntfsinfo" % (where, key, mine, theirs))


def compare_entries(mism, where, groups, entries):
    """Index entries: ntfsinfo prints one group per entry, the terminating end entry included."""
    groups = [g for g in groups if "Entry length" in g]
    mine = entries
    if len(groups) != len(mine):
        mism.append("%s: %d index entries in inspect-ntfs, %d in ntfsinfo" % (where, len(mine), len(groups)))
        return 0
    for i, (g, e) in enumerate(zip(groups, mine)):
        w = "%s entry %d" % (where, i)
        for key, val in (("Entry length", e["length"]), ("Key length", e["key_length"]),
                         ("Index entry flags", e["flags"])):
            if _num(g[key]) != val:
                mism.append("%s: %s is %r in inspect-ntfs, %r in ntfsinfo" % (w, key, val, _num(g[key])))
        if e["has_subnode"] and _num(g.get("Subnode VCN", "-1")) != e["subnode_vcn"]:
            mism.append("%s: sub-node VCN differs" % w)
        if e["file_name"]:
            if _num(g.get("FILE record number", "-1")) != e["record"]:
                mism.append("%s: indexed record differs" % w)
            compare_name(mism, w, g, e["file_name"])
    return len(mine)


def crosscheck_record(vol, image, n, counters):
    """Compare record n (with its extension records) between inspect-ntfs and ntfsinfo."""
    env = dict(os.environ, LC_ALL="C.UTF-8", TZ="UTC")
    p = subprocess.run(["ntfsinfo", "-i", str(n), "-v", image], stdout=subprocess.PIPE,
                       stderr=subprocess.STDOUT, env=env)
    text = p.stdout.decode("utf-8", errors="surrogateescape")
    if p.returncode != 0 or "Dumping Inode %d " % n not in text:
        return ["ntfsinfo -i %d failed: %s" % (n, text.strip()[-200:])]
    info = parse_ntfsinfo(text)
    r = vol.record(n)
    h = info["header"]
    mism = []
    where = "record %d" % n
    flags = h.get("MFT Record Flags", "").split()
    for key, mine, theirs in [
            ("usa_offset", r["usa_offset"], _num(h["Upd. Seq. Array Off."])),
            ("usa_count", r["usa_count"], _num(h["Upd. Seq. Array Count"])),
            ("usn", r["usn"], _num(h["Upd. Seq. Number"])),
            ("lsn", r["lsn"], _num(h["LogFile Seq. Number"])),
            ("sequence", r["sequence"], _num(h["MFT Record Seq. Numb."])),
            ("link_count", r["link_count"], _num(h["Number of Hard Links"])),
            ("attrs_offset", r["attrs_offset"], _num(h["Attribute Offset"])),
            ("in_use", r["in_use"], "IN_USE" in flags),
            ("is_directory", r["is_directory"], "DIRECTORY" in flags),
            ("bytes_in_use", r["bytes_in_use"], _num(h["Bytes Used"])),
            ("record_size", r["record_size"], _num(h["Bytes Allocated"])),
            ("next_attr_id", r["next_attr_id"], _num(h["Next Attribute Instance"]))]:
        counters["fields"] += 1
        if mine != theirs:
            mism.append("%s: %s is %r in inspect-ntfs, %r in ntfsinfo" % (where, key, mine, theirs))
    # ntfsinfo walks the attributes in attribute-list order; pair them per holder record
    theirs_by_holder = {}
    for a in info["attributes"]:
        theirs_by_holder.setdefault(a["holder"], []).append(a)
    for holder, tlist in theirs_by_holder.items():
        mine = vol.record(holder)["attributes"]
        mine_sorted = sorted(mine, key=lambda a: a["attr_id"])
        tl_sorted = sorted(tlist, key=lambda a: _num(a["fields"]["Attribute instance"]))
        if len(mine_sorted) != len(tl_sorted):
            mism.append("%s: %d attributes in record %d per inspect-ntfs, %d per ntfsinfo"
                        % (where, len(mine_sorted), holder, len(tl_sorted)))
            continue
        for a, t in zip(mine_sorted, tl_sorted):
            f = t["fields"]
            w = "%s %s%s (record %d)" % (where, a["type_name"], ":" + a["name"] if a["name"] else "", holder)
            checks = [("type", a["type"], t["type"]),
                      ("length", a["length"], _num(f["Attribute length"])),
                      ("non_resident", a["non_resident"], f["Resident"].strip() == "No"),
                      ("name length", len(a["name"].encode("utf-16-le")) // 2, _num(f["Name length"])),
                      ("flags", a["flags"], _num(f["Attribute flags"])),
                      ("attr_id", a["attr_id"], _num(f["Attribute instance"]))]
            if a["name"]:
                checks.append(("name", a["name"], _quoted(f.get("Attribute name", ""))))
            if a["non_resident"]:
                checks += [("lowest_vcn", a["lowest_vcn"], _num(f["Lowest VCN"])),
                           ("highest_vcn", a["highest_vcn"], _num(f["Highest VCN"])),
                           ("mapping_pairs_offset", a["mapping_pairs_offset"], _num(f["Mapping pairs offset"])),
                           ("compression_unit", a["compression_unit"], _num(f["Compression unit"])),
                           ("runs", [(x["vcn"], x["lcn"], x["length"]) for x in a["runs"]], t["runs"] or [])]
                if a["lowest_vcn"] == 0:      # ntfsinfo prints the sizes for the first piece only
                    checks += [("data_size", a["data_size"], _num(f["Data size"])),
                               ("allocated_size", a["allocated_size"], _num(f["Allocated size"])),
                               ("initialized_size", a["initialized_size"], _num(f["Initialized size"])),
                               ("compressed_size", a["compressed_size"],
                                _num(f["Compressed size"]) if "Compressed size" in f else None)]
                counters["runs"] += len(a["runs"])
            else:
                checks += [("value_length", a["value_length"], _num(f["Data size"])),
                           ("value_offset", a["value_offset"] - a["offset"], _num(f["Data offset"])),
                           ("resident_flags", a["resident_flags"], _num(f["Resident flags"]))]
            for key, m, th in checks:
                counters["fields"] += 1
                if m != th:
                    ms, ts = repr(m), repr(th)
                    mism.append("%s: %s is %s in inspect-ntfs, %s in ntfsinfo" % (w, key, ms[:120], ts[:120]))
            if a["type"] == 0x10 and "standard_information" in a:
                si = a["standard_information"]
                for key, mk in (("File Creation Time", "created"), ("File Altered Time", "modified"),
                                ("MFT Changed Time", "mft_changed"), ("Last Accessed Time", "accessed")):
                    counters["fields"] += 1
                    if _ctime(f[key]) != _secs(si[mk]):
                        mism.append("%s: %s differs" % (w, key))
                if _paren_hex(f["File attributes"]) != si["file_attributes"]:
                    mism.append("%s: file attributes differ" % w)
            elif a["type"] == 0x30 and a.get("file_name"):
                compare_name(mism, w, f, a["file_name"])
                counters["names"] += 1
            elif a["type"] == 0x20:
                entries = vol.attribute_list(holder)
                groups = [g for g in t["groups"] if "Attribute type" in g]
                if len(groups) != len(entries):
                    mism.append("%s: %d list entries in inspect-ntfs, %d in ntfsinfo" % (w, len(entries), len(groups)))
                else:
                    for g, x in zip(groups, entries):
                        counters["fields"] += 4
                        if (_num(g["Attribute type"]), _num(g["Starting VCN"]), _num(g["MFT reference"]),
                                _num(g["Instance"])) != (x["type"], x["lowest_vcn"], x["record"], x["attr_id"]):
                            mism.append("%s: attribute list entry differs: %r vs %r" % (w, g, x))
            elif a["type"] == 0x90 and "index_root" in a:
                ir = a["index_root"]
                # the same byte is labelled "512-byte Units Per Block" when a cluster is larger
                # than an index block (64 KiB clusters: it then reads 8)
                per_block = "Clusters Per Block" if "Clusters Per Block" in f else "512-byte Units Per Block"
                for key, m in (("Collation Rule", ir["collation"]), ("Index Block Size", ir["index_block_size"]),
                               (per_block, ir["clusters_per_index_block_raw"]),
                               ("Entries Offset", ir["entries_offset"]), ("Index Size", ir["index_length"]),
                               ("Allocated Size", ir["allocated_size"]), ("Index header flags", ir["node_flags"])):
                    counters["fields"] += 1
                    if _num(f[key]) != m:
                        mism.append("%s: %s is %r in inspect-ntfs, %r in ntfsinfo" % (w, key, m, _num(f[key])))
                if (f.get("Indexed Attr Type") == "DIRECTORY_I30") != (ir["indexed_type"] == 0x30):
                    mism.append("%s: indexed attribute type differs" % w)
                if a["name"] == "$I30":
                    counters["index_entries"] += compare_entries(mism, w, t["groups"], ir["entries"])
            elif a["type"] == 0xA0 and a["name"] == "$I30" and not t["bad"] and a["lowest_vcn"] == 0:
                # ntfsinfo dumps the blocks of small index allocations; libntfs-3g refuses to
                # load larger ones as a whole ("Corrupt attribute"), which sets t["bad"].
                _, offsets = vol.directory(n)
                heads = [g for g in t["groups"] if "Node VCN" in g]
                if len(heads) != len(offsets):
                    mism.append("%s: %d INDX blocks in inspect-ntfs, %d in ntfsinfo" % (w, len(offsets), len(heads)))
                else:
                    all_mine = []
                    for g, off in zip(heads, offsets):
                        blk = I.parse_index_block(vol.pread(off, vol.boot["index_block_size"]), off)
                        for key, m in (("Upd. Seq. Array Off.", blk["usa_offset"]),
                                       ("Upd. Seq. Array Count", blk["usa_count"]),
                                       ("Upd. Seq. Number", blk["usn"]), ("Node VCN", blk["vcn"]),
                                       ("Entries Offset", blk["entries_offset"]), ("Index Size", blk["index_length"]),
                                       ("Allocated Size", blk["allocated_size"]),
                                       ("Index header flags", blk["node_flags"])):
                            counters["fields"] += 1
                            if _num(g[key]) != m:
                                mism.append("%s: INDX %s is %r in inspect-ntfs, %r in ntfsinfo"
                                            % (w, key, m, _num(g[key])))
                        all_mine += blk["entries"]
                        counters["index_blocks"] += 1
                    counters["index_entries"] += compare_entries(mism, w, t["groups"], all_mine)
    counters["records"] += 1
    return mism


def cmd_crosscheck(args):
    """Compare inspect-ntfs.py with ntfsinfo -v for a sample of MFT records of the old volume."""
    state = load_state(args.state)
    vol = RawVolume(args.volume)
    vol.load_all()
    picks = list(range(0, 12)) + [24, 25, 26]
    interesting = [e for e in state["expected"]
                   if e["tags"] or e.get("streams") or e["ondisk"].get("extension_records")
                   or (e["type"] == "dir" and e["ondisk"]["index_blocks"] in (1, 2, 3))
                   or (e["type"] == "file" and e["ondisk"]["fragments"] > 1)
                   or any(ord(ch) > 127 for ch in e["path"])]
    step = max(1, len(interesting) // 40)
    picks += [e["mft_record"] for e in interesting[::step]]
    step = max(1, len(state["expected"]) // args.sample)
    picks += [e["mft_record"] for e in state["expected"][::step]]
    counters = {"records": 0, "fields": 0, "runs": 0, "names": 0, "index_entries": 0, "index_blocks": 0}
    problems = []
    seen = set()
    for n in picks:
        if n in seen:
            continue
        seen.add(n)
        problems += crosscheck_record(vol, args.volume, n, counters)
    # boot sector against ntfsinfo -m
    p = subprocess.run(["ntfsinfo", "-m", args.volume], stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                       universal_newlines=True)
    m = dict((k.strip(), v.strip()) for k, v in
             (line.split(":", 1) for line in p.stdout.split("\n") if ":" in line))
    b = vol.boot
    for key, mine in (("Sector Size", b["bytes_per_sector"]), ("Cluster Size", b["cluster_size"]),
                      ("Index Block Size", b["index_block_size"]), ("MFT Record Size", b["mft_record_size"]),
                      ("LCN of Data Attribute for FILE_MFT", b["mft_lcn"]),
                      ("LCN of Data Attribute for File_MFTMirr", b["mftmirr_lcn"]),
                      ("Volume Size in Clusters", b["total_sectors"] * b["bytes_per_sector"] // b["cluster_size"])):
        counters["fields"] += 1
        if key not in m or int(m[key]) != mine:
            problems.append("boot sector: %s is %r in inspect-ntfs, %r in ntfsinfo -m" % (key, mine, m.get(key)))
    if m.get("Volume Name") != vol.label():
        problems.append("volume label: %r in inspect-ntfs, %r in ntfsinfo -m" % (vol.label(), m.get("Volume Name")))
    try:
        uuid = subprocess.run(["blkid", "-o", "value", "-s", "UUID", args.volume], stdout=subprocess.PIPE,
                              stderr=subprocess.DEVNULL, universal_newlines=True).stdout.strip()
    except OSError:
        uuid = ""
    if uuid and uuid.upper() != b["serial"]:
        problems.append("volume serial: %s in inspect-ntfs, %s according to blkid" % (b["serial"], uuid))
    vi = [a["volume_information"] for a in vol.record(3)["attributes"] if a["type"] == 0x70]
    if not vi or m.get("Volume Version") != "%d.%d" % (vi[0]["major"], vi[0]["minor"]):
        problems.append("volume version: %r in inspect-ntfs, %r in ntfsinfo -m" % (vi, m.get("Volume Version")))
    counters["fields"] += 2
    vol.close()
    if problems:
        for line in problems[:30]:
            log("MISMATCH " + line)
        fail("inspect-ntfs.py and ntfsinfo disagree in %d places" % len(problems))
    msg = ("inspect-ntfs.py agrees with ntfsinfo -v on %(records)d MFT records: %(fields)d header and attribute "
           "fields, %(runs)d data runs, %(names)d $FILE_NAME attributes, %(index_entries)d index entries in "
           "index roots and %(index_blocks)d INDX blocks" % counters)
    log("verified: " + msg)
    state["verified"].append(msg)
    save_state(args.state, state)


# ==========================================================================
# 5. The accident
# ==========================================================================

def empty_file_record(n, record_size, lsn=0):
    """An unused, never-used MFT record as ntfs-3g and Windows 7 format one.

    Verified byte for byte against the unused records ntfs-3g writes when it
    extends the MFT (records 27.. of a fresh volume): "FILE", update sequence
    array at 0x30 with number 2, sequence number 1, flags 0, first attribute
    offset = end of the update sequence array rounded up to 8, an end marker
    as the only "attribute", and the record's own number at 0x2C.  The unused
    slots 16-255 of the Windows 7-era sample (vsstest, docs/notes) are the same
    bytes except for the LSN, which is non-zero there (logged initialisation).
    """
    usa_count = record_size // 512 + 1
    attrs = (0x30 + 2 * usa_count + 7) & ~7
    rec = bytearray(record_size)
    struct.pack_into("<4sHHQHHHHIIQHHI", rec, 0, b"FILE", 0x30, usa_count, lsn, 1, 0, attrs, 0,
                     attrs + 8, record_size, 0, 0, 0, n)
    struct.pack_into("<H", rec, 0x30, 2)
    struct.pack_into("<I", rec, attrs, 0xFFFFFFFF)
    for i in range(1, usa_count):
        struct.pack_into("<H", rec, i * 512 - 2, 2)
    check = I.parse_file_record(bytes(rec), 0)
    assert check["valid"] and check["record_number"] == n and not check["in_use"] and not check["attributes"]
    return bytes(rec)


# The LSNs of the blank records: the Windows 7-era sample has 2099858 in slot 16
# and about 19 more per slot.  Any non-zero value is as good; these are fixed so
# that builds are reproducible.
WIN7_BLANK_LSN_BASE = 0x200000
WIN7_BLANK_LSN_STEP = 19


def mft_byte_ranges(vol):
    """Volume-relative byte ranges of a volume's $MFT data runs."""
    return [(lcn * vol.cs, length * vol.cs) for lcn, length in vol.mft_runs]


def inside(ranges, off, length):
    return any(o <= off and off + length <= o + n for o, n in ranges)


def cmd_wipe(args):
    """Rewrite the new $MFT's slots up to 255 the way Windows' format and first mount do.

    Windows leaves a 256-record $MFT behind (records 0-255 written, unused
    slots zero on Windows 10/11, blank FILE records on Windows 7); mkntfs
    writes 27 records (64 with 64 KiB clusters).  Every slot from the end of
    the new $MFT's initialized size up to `last` is written here, at the place
    the new $MFT's slot has (contiguous from its first cluster, as Windows lays
    it out).  Two promises are checked first, and the build stops if either
    fails: the new volume uses none of those bytes for anything but its own
    $MFT, and the bytes held nothing of the old file system but MFT records
    (the model covers the rewrite of old MFT records, not damage to old file
    data, which Windows would make visible by allocating the slots).
    """
    state = load_state(args.state)
    new = RawVolume(args.volume)
    pre = RawVolume(args.pre_volume)
    if new.rs != pre.rs:
        fail("old and new MFT record sizes differ; the wipe models slots of one size")
    rs = new.rs
    lcn0, _ = new.mft_runs[0]
    if lcn0 != new.boot["mft_lcn"]:
        fail("the new $MFT does not begin at the boot sector's MFT cluster")
    mft_byte = lcn0 * new.cs
    first_slot = new.nrecords
    if first_slot > args.last:
        fail("the new $MFT already has %d records" % first_slot)
    bitmap = Bitmap(new)
    new_mft = mft_byte_ranges(new)
    old_mft = mft_byte_ranges(pre)
    for slot in range(first_slot, args.last + 1):
        off = mft_byte + slot * rs
        if bitmap.allocated(off, rs) and not inside(new_mft, off, rs):
            fail("new MFT slot %d (byte %d) lies in a cluster the new volume uses for something else" % (slot, off))
        if not inside(old_mft, off, rs):
            fail("new MFT slot %d (byte %d) lies outside the old $MFT: the wipe would destroy old file data"
                 % (slot, off))
    window = (mft_byte, mft_byte + (args.last + 1) * rs)
    old_in_window = [n for n in range(pre.nrecords)
                     if window[0] <= pre.record_offset(n) < window[1]]
    by_format = []
    by_wipe = []
    with open(args.pre_volume, "rb") as fpre, open(args.volume, "r+b") as img:
        before = {}
        for n in old_in_window:
            fpre.seek(pre.record_offset(n))
            before[n] = fpre.read(rs)
            img.seek(pre.record_offset(n))
            if img.read(rs) != before[n]:
                by_format.append(n)
        for slot in range(first_slot, args.last + 1):
            if args.fill == "zeros":
                rec = bytes(rs)
            else:
                rec = empty_file_record(slot, rs, WIN7_BLANK_LSN_BASE + WIN7_BLANK_LSN_STEP * slot)
            img.seek(mft_byte + slot * rs)
            img.write(rec)
        for n in old_in_window:
            if n in by_format:
                continue
            img.seek(pre.record_offset(n))
            if img.read(rs) != before[n]:
                by_wipe.append(n)
    new.close()
    pre.close()
    fills = {"zeros": "zero bytes (the Windows 10/11 form)",
             "blank": "an unused FILE record with its own slot number, sequence 1, flags 0, no attributes and "
                      "a non-zero LSN (the Windows 7 form)"}
    info = {
        "first": 0, "last": args.last, "fill": args.fill,
        "slots_written": [first_slot, args.last],
        "byte_range": [args.volume_offset + window[0] + first_slot * rs, args.volume_offset + window[1]],
        "overwritten_by_format": by_format, "overwritten_by_wipe": by_wipe,
        "method": "slots %d-%d of the new $MFT (contiguous from its first cluster, where Windows keeps a "
                  "256-record $MFT; mkntfs initialised %d) were overwritten with %s"
                  % (first_slot, args.last, first_slot, fills[args.fill]),
    }
    if args.record == "accident":
        state["accident"]["steps"].append("wipe-mft-records")
        state["accident"]["wiped_records"] = info
    else:
        state["history"][-1]["wiped_records"] = info
    save_state(args.state, state)
    log("wrote new MFT slots %d-%d (%s); old records destroyed by the format: %s, by the wipe: %s"
        % (first_slot, args.last, args.fill, ranges(by_format) or "none", ranges(by_wipe) or "none"))


def last_write(vol):
    """Latest MFT-change time in any record of a volume's $MFT (the wall clock of its last write).

    The MFT-change time is the one time a writer cannot set: ntfs-3g stamps it
    with the clock whenever a record changes, also when other times are set.
    """
    latest = 0
    for n in range(vol.nrecords):
        r = vol.record(n)
        if r is None or not r["valid"]:
            continue
        for a in r["attributes"]:
            if a["type"] == 0x10 and "standard_information" in a:
                latest = max(latest, a["standard_information"]["mft_changed"])
        for fn in r["names"]:
            latest = max(latest, fn["mft_changed"])
    now = int(time.time() * 10000000) + EPOCH_DIFF
    if latest > now + 600 * 10000000:
        fail("an MFT-change time lies in the future (%d)" % latest)
    if not latest:
        fail("no MFT-change time found")
    return latest


def cmd_lastwrite(args):
    """Record the old volume's last write before it is formatted over; print it in whole Unix seconds.

    make-fixtures.sh then waits until the clock has passed that second, so the
    format (which stamps whole seconds) is strictly later than every old write.
    """
    state = load_state(args.state)
    vol = RawVolume(args.volume)
    latest = last_write(vol)
    vol.close()
    if args.record == "accident":
        state["accident"]["last_old_write"] = latest
    else:
        state.setdefault("history", []).append({"event": "re-format", "formatted_over": args.record,
                                                "last_old_write": latest})
    save_state(args.state, state)
    print((latest - EPOCH_DIFF) // 10000000)


STASHED_KEYS = ("old_volume", "expected", "deleted", "profile", "captured", "analyzed")


def cmd_stash(args):
    """Set the file system just built aside, so that the scenario can build the next one."""
    state = load_state(args.state)
    if not state.get("analyzed"):
        fail("stash must run after analyze")
    fs = {k: state.pop(k) for k in STASHED_KEYS if k in state}
    fs.update({"name": args.name, "kind": args.kind, "pre_volume": args.pre_volume,
               "container": args.container or None})
    state.setdefault("other_fs", []).append(fs)
    save_state(args.state, state)


def cmd_newfiles(args):
    """Write new data onto the freshly formatted volume (post-write scenario)."""
    state = load_state(args.state)
    new = state["new_volume"]
    t = Tree(args.root, args.samples, new["cluster_size"], new["mft_record_size"])
    t.mkdir("new-data")
    # The first files of the new volume land where the first files of the old
    # one were.  Some of them are deleted again below: the clusters they
    # overwrote are then free in the new $Bitmap although the old content is gone.
    for i in range(6):
        t.blob("new-data/temp-%d.bin" % i, 700000 + 150000 * i, tags=("new",))
    t.mkdir("new-data/documents")
    for i in range(40):
        t.typed("new-data/documents/new-%03d.%s" % (i, ["pdf", "png", "zip", "wav"][i % 4]), 60000 + 9000 * i)
    t.mkdir("new-data/small")
    for i in range(60):
        t.text("new-data/small/note-%03d.txt" % i, 50 + 11 * i)
    t.blob("new-data/big-new.bin", 9 << 20)
    add_samples(t, "new-data/media")
    # Times as a copy onto a fresh volume leaves them: created "now" (after
    # every creation time on the old volume, which end in 2021), modified
    # either at the same moment or years earlier (a copied file keeps its
    # modification time).
    copied = filetime(2024, 6, 15, 10, 0, 0)
    for i, path in enumerate(list(t.live)):
        cr = copied + i * 3 * 10000000 + C.rint("new/created/" + path, 0, 9999999)
        mt = cr
        if i % 2:
            mt = cr - C.rint("new/modified/" + path, 30 * 86400, 6 * 365 * 86400) * 10000000
        t.fixed_times[path] = (cr, mt, cr)
    for i in range(6):
        t.delete("new-data/temp-%d.bin" % i, "new")
    t.stamp_all()
    state["new_expected"] = list(t.live.values())
    state["new_deleted"] = list(t.deleted.values())
    save_state(args.state, state)
    total = sum(e["size"] or 0 for e in t.live.values())
    log("new volume: %d new files and folders (%d bytes) written, %d of them deleted again"
        % (len(t.live) + len(t.deleted), total + sum(e["size"] or 0 for e in t.deleted.values()), len(t.deleted)))


def cmd_collide(args):
    """Write to the new volume so that new folders take the record numbers of destroyed old folders.

    ntfs-3g hands out the records of a fresh volume one after the other from
    record 64.  For every slot from 64 up to the highest old folder that the
    accident destroyed although some of its children survived, a folder is
    created when the old record in that slot was such a folder, and a small
    file otherwise.  Both file systems start record sequences at 1, so each of
    these new folders is (record N, sequence 1), exactly what the surviving
    old children name as their parent.  Each slot is checked as it is handed
    out.  Then files go into some of the new folders, a few large files are
    written, and some files are created and deleted again (deleted records of
    the new volume whose clusters overwrote old data).
    """
    state = load_state(args.state)
    new = state["new_volume"]
    boundary = state["boundary"]
    live = [e for e in state["expected"]]
    by_path = {e["path"]: e for e in live}
    targets = {}
    for e in live:
        parent = parent_of(e["path"])
        if parent and e["mft_record"] > boundary:
            d = by_path[parent]
            if FIRST_USER_RECORD <= d["mft_record"] <= boundary:
                targets[d["mft_record"]] = d
    if len(targets) < 10:
        fail("only %d destroyed old folders with surviving children" % len(targets))
    t = Tree(args.root, args.samples, new["cluster_size"], new["mft_record_size"])
    slot = NTFS3G_FIRST_USER_RECORD
    folders = []
    while slot <= max(targets):
        if slot in targets:
            e = t.mkdir("New Folder %03d" % slot, tags=("collides",))
            folders.append(e)
        else:
            e = t.text("new-file-%03d.txt" % slot, 100 + slot)
        if e["mft_record"] != slot:
            fail("the new volume handed out record %d where %d was expected" % (e["mft_record"], slot))
        slot += 1
    for i, e in enumerate(folders[::3]):
        t.text(e["path"] + "/inside-%02d.txt" % i, 700 + 13 * i)
    t.mkdir("new-big")
    for i in range(4):
        t.blob("new-big/new-blob-%d.bin" % i, (2 << 20) + 300000 * i)
    for i in range(5):
        t.blob("new-big/temp-%d.bin" % i, 900000 + 200000 * i)
    copied = filetime(2024, 6, 15, 10, 0, 0)
    for i, path in enumerate(list(t.live)):
        cr = copied + i * 3 * 10000000 + C.rint("collide/created/" + path, 0, 9999999)
        t.fixed_times[path] = (cr, cr, cr)
    for i in range(5):
        t.delete("new-big/temp-%d.bin" % i, "new")
    t.stamp_all()
    state["new_expected"] = list(t.live.values())
    state["new_deleted"] = list(t.deleted.values())
    state["collided_slots"] = sorted(targets)
    save_state(args.state, state)
    log("new volume: %d new folders took the record numbers of destroyed old folders (%s); %d entries "
        "written, %d of them deleted again"
        % (len(folders), ranges(sorted(targets)), len(t.live) + len(t.deleted), len(t.deleted)))


def cmd_capture_new(args):
    state = load_state(args.state)
    expected = {e["path"]: e for e in state["new_expected"]}
    found = walk_mounted(args.root)
    if set(found) != set(expected):
        fail("the new volume's tree differs from what was written")
    for path, e in expected.items():
        for key in ("type", "size", "sha256", "mft_record", "created", "modified", "accessed"):
            if found[path][key] != e[key]:
                fail("new file %r: %s differs through the driver" % (path, key))
    state["new_captured"] = True
    save_state(args.state, state)
    log("captured: %d new entries read back through the driver" % len(found))


# ==========================================================================
# 6. Partition tables (read back independently of sfdisk)
# ==========================================================================

def read_partition_table(path, sector=512):
    with open(path, "rb") as f:
        mbr = f.read(512)
        if mbr[510:512] != b"\x55\xaa":
            fail("%s: no MBR signature" % path)
        parts = []
        entries = [mbr[446 + 16 * i:462 + 16 * i] for i in range(4)]
        if entries[0][4] == 0xEE:
            f.seek(sector)
            hdr = f.read(92)
            if hdr[:8] != b"EFI PART":
                fail("%s: protective MBR without a GPT header" % path)
            import zlib
            crc = struct.unpack_from("<I", hdr, 16)[0]
            if zlib.crc32(hdr[:16] + b"\x00\x00\x00\x00" + hdr[20:92]) & 0xFFFFFFFF != crc:
                fail("%s: GPT header CRC mismatch" % path)
            first_lba, count, esize = struct.unpack_from("<QII", hdr, 72)
            f.seek(first_lba * sector)
            table = f.read(count * esize)
            if zlib.crc32(table) & 0xFFFFFFFF != struct.unpack_from("<I", hdr, 88)[0]:
                fail("%s: GPT partition array CRC mismatch" % path)
            import uuid
            for i in range(count):
                e = table[i * esize:(i + 1) * esize]
                if e[:16] == bytes(16):
                    continue
                first, last = struct.unpack_from("<QQ", e, 32)
                parts.append({"index": i + 1, "type": str(uuid.UUID(bytes_le=e[:16])).upper(),
                              "guid": str(uuid.UUID(bytes_le=e[16:32])).upper(),
                              "name": e[56:128].decode("utf-16-le").rstrip("\x00"),
                              "start_sector": first, "sectors": last - first + 1,
                              "offset": first * sector, "length": (last - first + 1) * sector})
            return {"type": "gpt", "disk_guid": str(uuid.UUID(bytes_le=hdr[56:72])).upper(), "partitions": parts}
        for i, e in enumerate(entries):
            ptype = e[4]
            start, count = struct.unpack_from("<II", e, 8)
            if ptype:
                parts.append({"index": i + 1, "type": "%02X" % ptype, "start_sector": start, "sectors": count,
                              "offset": start * sector, "length": count * sector})
        return {"type": "mbr", "disk_signature": "%08X" % struct.unpack_from("<I", mbr, 440)[0],
                "partitions": parts}


# ==========================================================================
# 7. After the accident: compare, verify, write the manifest
# ==========================================================================

def tool_versions():
    def first_line(cmd):
        try:
            p = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, universal_newlines=True)
            lines = [x.strip() for x in p.stdout.split("\n") if x.strip()]
            return lines[0] if lines else ""
        except OSError:
            return None
    v = {"mkntfs": first_line(["mkntfs", "--version"]), "ntfs-3g": first_line(["ntfs-3g", "--version"]),
         "ffmpeg": first_line(["ffmpeg", "-hide_banner", "-version"]),
         "python": sys.version.split()[0]}
    try:
        import PIL
        v["pillow"] = PIL.__version__
    except ImportError:
        v["pillow"] = None
    return v


# ALGORITHMS.md H4 step 4
RESERVED_DEVICE_NAMES = set(["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"]
                            + ["COM%d" % i for i in range(10)] + ["LPT%d" % i for i in range(10)]
                            + ["COM\u00b9", "COM\u00b2", "COM\u00b3", "LPT\u00b9", "LPT\u00b2", "LPT\u00b3"])


def name_issues(name, folded_siblings):
    """Why a name cannot be re-created unchanged in an ordinary Windows folder (or on Linux).

    These are the cases ALGORITHMS.md H4 sanitizes; a recovered copy
    of such an entry cannot sit at exactly the manifest's path.
    folded_siblings maps the upper-cased names of the folder to their count.
    """
    issues = []
    if any(ch in '<>:"\\|?*' for ch in name):
        issues.append("win32-illegal-char")
    if any(ord(ch) < 0x20 for ch in name):
        issues.append("control-char")
    if name[-1] in ". ":
        issues.append("trailing-dot-or-space")
    if "".join(c.upper() if "a" <= c <= "z" else c for c in name.split(".")[0].rstrip(" ")) \
            in RESERVED_DEVICE_NAMES:                         # ASCII case-insensitive
        issues.append("reserved-device-name")
    if folded_siblings[name.upper()] > 1:
        issues.append("case-collision")
    if len(name.encode("utf-8")) > 255:
        issues.append("utf8-over-255-bytes")
    return issues


class OldLayout:
    """The old volume's layout laid over another image: the one after the accident.

    Reads a stream described by the old MFT (runs, sizes, flags, the record
    that held resident data) from the bytes that are at those places now.
    """

    def __init__(self, old, fd):
        self.old = old
        self.fd = fd
        self.cs = old.cs

    def pread(self, off, n):
        return os.pread(self.fd, n, off).ljust(n, b"\x00")

    def raw_record(self, n):
        return self.pread(self.old.record_offset(n), self.old.rs)

    fixed_record = RawVolume.fixed_record
    read_runs = RawVolume.read_runs
    read_compressed = RawVolume.read_compressed
    read_stream = RawVolume.read_stream


class Bitmap:
    """The new file system's $Bitmap: which bytes of the volume it regards as allocated."""

    def __init__(self, vol):
        s = vol.stream(6)
        self.bits = vol.read_stream(s)
        self.cs = vol.cs

    def allocated(self, off, length):
        first = off // self.cs
        last = (off + length - 1) // self.cs
        for c in range(first, last + 1):
            if c // 8 < len(self.bits) and self.bits[c // 8] >> (c % 8) & 1:
                return True
        return False


def same_bytes(fd_a, fd_b, off, length, base_b=0):
    pos = 0
    while pos < length:
        n = min(4 << 20, length - pos)
        if os.pread(fd_a, n, off + pos) != os.pread(fd_b, n, base_b + off + pos):
            return False
        pos += n
    return True


def count_changed(fd_a, fd_b, off, length, unit):
    """Number of unit-sized pieces of [off, off+length) that differ between the two files."""
    changed = 0
    pos = 0
    step = max(unit, (1 << 20) // unit * unit)
    while pos < length:
        n = min(step, length - pos)
        a = os.pread(fd_a, n, off + pos)
        b = os.pread(fd_b, n, off + pos)
        if a != b:
            changed += sum(1 for i in range(0, n, unit) if a[i:i + unit] != b[i:i + unit])
        pos += n
    return changed


def _live(ctx):
    return [e for e in ctx["entries"] if not e["deleted"]]


def _written_after(state):
    return any(s in state["accident"]["steps"] for s in ("new-files-written", "live-collision"))


@expect_post("prelude-lost")
def _(state, ctx):
    """The Windows-like wipe destroyed exactly the prelude; its folders' children survive in all three ways."""
    bound = state["boundary"]
    live = _live(ctx)
    kept_low = [e["path"] for e in live if e["mft_record"] <= bound and e["after"]["record_intact"]]
    if kept_low:
        fail("live entries with records up to %d kept their record: %r" % (bound, kept_low[:5]))
    if not _written_after(state):
        lost_high = [e["path"] for e in live if e["mft_record"] > bound and not e["after"]["record_intact"]]
        if lost_high:
            fail("live entries with records above %d lost their record: %r" % (bound, lost_high[:5]))
    by_path = {e["path"]: e for e in live}
    kinds = {"indx": [], "index_root": [], "unnamed": []}
    for d in live:
        if d["type"] != "dir" or d["after"]["record_intact"]:
            continue
        if not any(c["after"]["record_intact"] for c in live if parent_of(c["path"]) == d["path"]):
            continue
        usable = [x for x in d["after"].get("named_by", []) if x["usable"]]
        if not usable:
            kinds["unnamed"].append(d["path"])
        for src in set(x["source"] for x in usable):
            kinds[src].append(d["path"])
        for x in usable:
            if x["name"] != base_of(d["path"]):
                fail("%r is named %r by a surviving index entry" % (d["path"], x["name"]))
    missing = [k for k, v in kinds.items() if not v]
    if _written_after(state):
        # new data lands first where the old volume's first data was: the INDX blocks of the
        # destroyed folders (allocated among the first old data) are overwritten
        missing = [k for k in missing if k != "indx"]
    if missing:
        fail("destroyed folders with surviving children: %s; missing kinds: %s"
             % (", ".join("%d %s" % (len(v), k) for k, v in kinds.items()), missing))
    top_named = [p for p in kinds["indx"] if "/" not in p]
    top_unnamed = [p for p in kinds["unnamed"] if "/" not in p]
    if state["old_volume"]["mft_lcn"] * state["old_volume"]["cluster_size"] == \
            state["new_volume"]["mft_lcn"] * state["new_volume"]["cluster_size"] and \
            state["old_volume"]["cluster_size"] == state["new_volume"]["cluster_size"] and \
            state["old_volume"]["total_sectors"] == state["new_volume"]["total_sectors"] and \
            not _written_after(state):
        # same geometry: the new root index block lands on the old root's first block (with clusters
        # larger than an index block, the format rewrites the whole cluster and with it the later blocks)
        if state["old_volume"]["cluster_size"] <= state["old_volume"]["index_block_size"] and \
                (len(top_named) < 3 or len(top_unnamed) < 3):
            fail("top-level lost folders: %d named by a surviving block of the old root index, %d not; several "
                 "of both kinds are required" % (len(top_named), len(top_unnamed)))
    below = [e for e in live if e["after"]["record_intact"] and not e["after"]["ancestry"]["path_reconstructible"]]
    if not below:
        fail("no surviving entry lies below a folder that cannot be named")
    del by_path
    first = dict((k, " (e.g. %s)" % v[0] if v else "") for k, v in kinds.items())
    return ("old MFT records 0-%d destroyed (%s); destroyed folders with surviving children: %d named by a "
            "surviving INDX block%s, %d by the index root of a surviving parent%s, %d not named anywhere%s; "
            "top-level: %d named by blocks of the old root index, %d not; %d surviving entries cannot get "
            "their full path back (e.g. %s, which keeps %r)"
            % (bound, state["accident"]["wiped_records"]["fill"] if state["accident"]["wiped_records"] else "-",
               len(kinds["indx"]), first["indx"], len(kinds["index_root"]), first["index_root"],
               len(kinds["unnamed"]), first["unnamed"], len(top_named), len(top_unnamed), len(below),
               below[0]["path"], below[0]["after"]["ancestry"]["surviving_suffix"]))


@expect_post("content-survives")
def _(state, ctx):
    damaged = [e["path"] for e in _live(ctx) if e["type"] == "file" and e["after"]["record_intact"]
               and not e["after"]["content_intact"]]
    if damaged:
        fail("files whose record survived lost content: %r" % damaged[:5])
    n = sum(1 for e in _live(ctx) if e["type"] == "file" and e["after"]["record_intact"])
    return "all %d live files whose record survived keep their content byte for byte" % n


@expect_post("carving-samples")
def _(state, ctx):
    carve = [e for e in _live(ctx) if "sample" in e["tags"] and not e["after"]["record_intact"]
             and e["after"]["content_intact"] and e["ondisk"]["fragments"] == 1]
    if len(carve) < 18:
        fail("only %d destroyed samples are still intact and contiguous" % len(carve))
    return "%d samples lost their record but keep intact, unfragmented content (carving)" % len(carve)


@expect_post("cluster-change")
def _(state, ctx):
    old, new = state["old_volume"], state["new_volume"]
    if old["cluster_size"] == new["cluster_size"]:
        fail("old and new cluster size are both %d" % old["cluster_size"])
    ent = _live(ctx)
    lost = [e for e in ent if not e["after"]["record_intact"]]
    kept = [e for e in ent if e["after"]["record_intact"]]
    if not lost or not kept:
        fail("cluster-change: %d live entries lost their record, %d kept it" % (len(lost), len(kept)))
    return ("old cluster size %d, new cluster size %d; the new MFT starts at byte %d (old: byte %d), so its "
            "slots 0-255 cover old records %s; %d live entries lost their record"
            % (old["cluster_size"], new["cluster_size"], new["mft_lcn"] * new["cluster_size"],
               old["mft_lcn"] * old["cluster_size"], ranges(state["destroyed_records"]), len(lost)))


@expect_post("post-write")
def _(state, ctx):
    ent = [e for e in _live(ctx) if e["type"] == "file"]
    damaged = [e for e in ent if not e["after"]["content_intact"]]
    intact = [e for e in ent if e["after"]["content_intact"] and e["after"]["record_intact"]]
    # the trap: the MFT record survived, the content did not, and the new
    # file system's $Bitmap does not show the clusters as allocated
    ghost = [e for e in damaged if e["after"]["record_intact"] and not e["after"]["overlaps_new_allocation"]]
    visible = [e for e in damaged if e["after"]["record_intact"] and e["after"]["overlaps_new_allocation"]]
    if not damaged or not intact or not ghost or not visible:
        fail("post-write: %d damaged files, %d intact, %d damaged-but-unallocated, %d damaged-and-allocated; "
             "all four kinds are required" % (len(damaged), len(intact), len(ghost), len(visible)))
    hidden = [e for e in ghost if e["after"]["damage_evidence"] == ["none"]]
    if hidden:
        fail("damage without any evidence in the new file system: %r" % [e["path"] for e in hidden[:5]])
    return ("%d old files lost content to new data, %d old files are fully intact; of the files whose record "
            "survived, %d are overwritten by data the new file system still has allocated and %d only by new "
            "files that were deleted again (free in the new $Bitmap; their deleted records in the new MFT "
            "still map the clusters, e.g. %s)" % (len(damaged), len(intact), len(visible), len(ghost),
                                                   ghost[0]["path"]))


@expect_post("partition")
def _(state, ctx):
    pt = ctx["partition_table"]
    if pt is None:
        fail("no partition table in the final image")
    part = [p for p in pt["partitions"] if p["offset"] == ctx["volume_offset"]]
    if len(part) != 1 or part[0]["length"] != ctx["volume_length"]:
        fail("no partition at byte %d with length %d" % (ctx["volume_offset"], ctx["volume_length"]))
    if state["old_volume"]["hidden_sectors"] != part[0]["start_sector"]:
        fail("the boot sector's hidden-sectors field does not equal the partition start")
    return ("%s partition table read back: NTFS partition %d at byte %d, %d bytes; the image holds the "
            "volume byte for byte at that offset" % (pt["type"].upper(), part[0]["index"], part[0]["offset"],
                                                       part[0]["length"]))


@expect_post("generations")
def _(state, ctx):
    g1 = [x for x in ctx["other_entries"] if x["file_system"] == "g1" and not x["deleted"]]
    g2 = _live(ctx)
    v1 = [v for v in ctx["views"] if v.name == "g1"][0]
    v2 = ctx["views"][0]
    if v1.pre.mft_runs[0][0] * v1.pre.cs != v2.pre.mft_runs[0][0] * v2.pre.cs:
        fail("g1's and g2's $MFT do not start at the same byte")
    survivors = [x for x in g1 if x["after"]["record_intact"]]
    top2 = max(e["mft_record"] for e in ctx["entries"])
    if not survivors or min(x["mft_record"] for x in survivors) <= top2:
        fail("g1 records must survive only behind g2's records (g2 ends at record %d)" % top2)
    ok = [x for x in survivors if x["type"] == "file" and x["after"]["content_intact"]]
    reused = [x for x in survivors if x["type"] == "file" and x["after"]["clusters_reused_by_newer"]]
    broken = [x for x in reused if not x["after"]["content_intact"]]
    if not ok or not reused or not broken:
        fail("g1: %d files intact, %d with clusters reused by g2, %d of them damaged; all three are required"
             % (len(ok), len(reused), len(broken)))
    damaged = [x for x in survivors if x["type"] == "file" and not x["after"]["content_intact"]]
    nobody = [x for x in damaged if x["after"]["damage_evidence"] == ["none"]]
    g1_changed = max(x["mft_changed"] for x in survivors)
    g2_changed = min(e["mft_changed"] for e in g2 if e["after"]["record_intact"])
    g1_last = state["history"][0]["last_old_write"]
    if not g1_changed <= g1_last < g2_changed:
        fail("MFT-change times do not separate the generations")
    taken = [x for x in g1 if "parent_reference_taken_by" in x["after"] or "parent_slot_contradiction" in x["after"]]
    if not taken:
        fail("no g1 entry names a parent slot that now holds a g2 record")
    return ("g1 (%d entries) was formatted over by g2 (%d entries) at the same MFT origin; %d g1 entries keep "
            "their record, all numbered above g2's last record %d; %d g1 files are intact, %d lost content: %d "
            "to clusters g2 still had allocated, %d to g2 files deleted before the accident (no evidence on "
            "the disk); %d g1 entries name a parent slot that g2 took over; every g1 MFT-change time is older "
            "than every g2 one"
            % (len(g1), len(g2), len(survivors), top2, len(ok), len(damaged), len(broken), len(nobody),
               len(taken)))


@expect_post("restored-times")
def _(state, ctx):
    g1 = [x for x in ctx["other_entries"] if x["file_system"] == "g1" and x["type"] == "file"]
    g2 = [e for e in _live(ctx) if e["type"] == "file"]
    c1 = sorted(x["created"] for x in g1)
    c2 = sorted(e["created"] for e in g2)
    median1 = c1[len(c1) // 2]
    older = sum(1 for c in c2 if c < median1)
    if not (c2[0] < c1[-1] and c1[0] < c2[-1]) or older < len(c2) // 4:
        fail("creation times of g2's restored files do not overlap g1's")
    return ("creation times cannot tell the generations apart: g1 files were created %s .. %s, the restored "
            "g2 files %s .. %s, and %d of %d g2 files are older than g1's median; only the MFT-change times "
            "separate them" % (time.strftime("%Y-%m-%d", time.gmtime(unix_ms(c1[0]) // 1000)),
                               time.strftime("%Y-%m-%d", time.gmtime(unix_ms(c1[-1]) // 1000)),
                               time.strftime("%Y-%m-%d", time.gmtime(unix_ms(c2[0]) // 1000)),
                               time.strftime("%Y-%m-%d", time.gmtime(unix_ms(c2[-1]) // 1000)), older, len(c2)))


@expect_post("embedded")
def _(state, ctx):
    inner = [x for x in ctx["other_entries"] if x["file_system"] == "inner"]
    host = _live(ctx)
    view = [v for v in ctx["views"] if v.name == "inner"][0]
    cont = [e for e in host if e["path"] == view.container][0]
    if not cont["after"]["record_intact"] or not cont["after"]["content_intact"]:
        fail("the container file did not survive the accident")
    if len(inner) <= len(host):
        fail("the inner volume has %d entries, its host %d" % (len(inner), len(host)))
    if any(not x["after"]["record_intact"] or (x["type"] == "file" and not x["after"]["content_intact"])
           for x in inner):
        fail("the accident damaged the inner volume")
    holes = sum(1 for lcn, _ in cont["ondisk"]["runs"] if lcn is None)
    inside_mft = inside(view.container_ranges(), view.main_point(view.pre.mft_runs[0][0] * view.pre.cs), 1)
    if not inside_mft:
        fail("the inner $MFT does not lie in the container's clusters")
    return ("%s (%d bytes, %d fragments, %d sparse holes) holds an NTFS volume with %d entries, more than the "
            "%d of its host; container and inner volume are intact after the accident"
            % (cont["path"], cont["size"], cont["ondisk"]["fragments"], holes, len(inner), len(host)))


@expect_post("repartition")
def _(state, ctx):
    pt = ctx["partition_table"]
    if pt is None or len(pt["partitions"]) != 2:
        fail("the final image should have two partitions")
    lo, hi = ctx["scan_range"]
    p1 = pt["partitions"][0]
    if (p1["offset"], p1["offset"] + p1["length"]) != (lo, hi) or lo != ctx["volume_offset"]:
        fail("the scan range is not the first new partition at the old volume's start")
    files = [e for e in _live(ctx) if e["type"] == "file" and e["after"]["record_intact"]
             and not e["ondisk"]["resident"]]
    inside_ok = [e for e in files if e["after"]["data_inside_scan_range"] and e["after"]["content_intact"]]
    outside = [e for e in files if not e["after"]["data_inside_scan_range"]]
    outside_ok = [e for e in outside if e["after"]["content_intact"]]
    outside_bad = [e for e in outside if not e["after"]["content_intact"]]
    straddle = [e for e in outside if any(lo <= ctx["volume_offset"] + o * state["old_volume"]["cluster_size"] < hi
                                          for o, _ in [(r[0], r[1]) for r in e["ondisk"]["runs"] if r[0] is not None])]
    if not inside_ok or not outside_ok or not outside_bad or not straddle:
        fail("repartition: %d intact files inside the first partition, %d intact and %d damaged files reaching "
             "beyond it, %d straddling its end; all four are required"
             % (len(inside_ok), len(outside_ok), len(outside_bad), len(straddle)))
    end = ctx["volume_offset"] + ctx["volume_length"]
    with open(ctx["image_path"], "rb") as f:
        f.seek(end - state["old_volume"]["sector_size"])
        backup = I.parse_boot_sector(f.read(512), 0)
    if not backup or not backup["valid"] or backup["total_sectors"] != state["old_volume"]["total_sectors"]:
        fail("the old volume's backup boot sector at its last sector did not survive")
    return ("old volume %d bytes at byte %d replaced by two partitions (%s); scan range = the first one; %d "
            "intact files lie wholly inside it, %d files reach beyond it (%d of them intact, %d damaged by the "
            "second partition's format, %d straddle its end); the old backup boot sector survives at byte %d"
            % (ctx["volume_length"], ctx["volume_offset"],
               ", ".join("%d+%d" % (p["offset"], p["length"]) for p in pt["partitions"]), len(inside_ok),
               len(outside), len(outside_ok), len(outside_bad), len(straddle), end - 512))


@expect_post("live-collision")
def _(state, ctx):
    hit = [e for e in ctx["entries"] if "parent_reference_taken_by" in e["after"]
           and e["after"]["parent_reference_taken_by"]["file_system"] == "new"
           and e["after"]["parent_reference_taken_by"]["in_use"]]
    folders = set(e["after"]["parent_reference_taken_by"]["path"] for e in hit)
    want = set("New Folder %03d" % n for n in state["collided_slots"])
    if folders != want:
        fail("old children whose parent reference resolves to a live new folder: %d folders, expected %d"
             % (len(folders), len(want)))
    ghosts = [e for e in _live(ctx) if e["type"] == "file" and e["after"]["record_intact"]
              and not e["after"]["content_intact"] and "post-format-record" in e["after"]["damage_evidence"]
              and not e["after"]["overlaps_new_allocation"]]
    if not ghosts:
        fail("no old file was overwritten only by a new file that was deleted again")
    named = sum(1 for d in _live(ctx) if d["mft_record"] in state["collided_slots"]
                and any(x["usable"] for x in d["after"].get("named_by", [])))
    return ("%d new folders took the (record, sequence 1) of destroyed old folders: %d surviving old entries "
            "name a live new folder as their parent (e.g. %s -> %s); %d of those old folders are still named "
            "by a surviving index entry; %d old files were overwritten only by new files deleted again"
            % (len(folders), len(hit), hit[0]["path"], hit[0]["after"]["parent_reference_taken_by"]["path"],
               named, len(ghosts)))


def scan_check(volume, entries, now, label="main"):
    """The fixture as HDSavior will meet it: recover from a blind scan of the final image.

    Every 512-byte offset is tested for a FILE record (no boot sector, no
    $MFT runlist).  For each live file a record is looked for that is in use
    and has the file's number, sequence and name; extension records are
    attached through their base references; the content is read through the
    runs found there.  A file must come back bit-identical exactly when the
    manifest says after.record_intact and after.content_intact.
    """
    bases = {}
    extensions = {}
    for r in I.scan_image(volume, kinds=("FILE",)):
        if not r["valid"] or r["record_number"] is None:
            continue
        if r["in_use"] and (r["base_record"], r["base_sequence"]) != (0, 0):
            extensions.setdefault((r["base_record"], r["base_sequence"]), []).append(r)
        else:
            bases.setdefault(r["record_number"], []).append(r)
    live = {e["path"]: e for e in entries if not e["deleted"]}
    found = good = 0
    wrong = []
    for e in entries:
        if e["type"] != "file" or e["deleted"]:
            continue
        parent = parent_of(e["path"])
        parent_rec = live[parent]["mft_record"] if parent else ROOT_RECORD
        ok = False
        for r in bases.get(e["mft_record"], []):
            if not r["in_use"] or r["sequence"] != e["mft_sequence"]:
                continue
            group = [r] + extensions.get((r["record_number"], r["sequence"]), [])
            if not any(fn["name"] == base_of(e["path"]) and fn["parent_record"] == parent_rec
                       for x in group for fn in x["names"]):
                continue
            found += 1
            try:
                stream = RawVolume.stream(None, r["record_number"], "",
                                          [(x["offset"], a) for x in group for a in x["attributes"]])
                if stream is None:
                    data = None
                elif stream["resident"]:
                    holder = [x for x in group if x["offset"] == stream["holders"][0]][0]
                    buf = bytearray(now.pread(holder["offset"], holder["record_size"]))
                    I.apply_fixups(buf, holder["usa_offset"], holder["usa_count"])
                    data = bytes(buf[stream["value_offset"]:stream["value_offset"] + stream["size"]])
                else:
                    data = now.read_stream(stream)
            except (Fail, IndexError, struct.error):
                data = None
            ok = data is not None and len(data) == e["size"] and sha256_hex(data) == e["sha256"]
            break
        good += ok
        if ok != (e["after"]["record_intact"] and e["after"]["content_intact"]):
            wrong.append(e["path"])
    if wrong:
        fail("a blind scan of the final image disagrees with the manifest for %d files, e.g. %r"
             % (len(wrong), wrong[:5]))
    return ("%s: a scan of the final image at every 512-byte offset (no boot sector, no $MFT runlist used) "
            "finds a matching in-use FILE record for %d of %d live files and reads %d of them back "
            "bit-identical: exactly the files marked record_intact and content_intact"
            % (label, found, sum(1 for e in entries if e["type"] == "file" and not e["deleted"]), good))


def write_manifest(path, manifest):
    """One top-level member per line; the long lists one element per line.

    The file is plain ASCII: json.dumps escapes every non-ASCII character
    (emoji become a \\ud83d\\ude00 style surrogate pair), so no reader can get
    the encoding wrong.
    """
    lines = []
    for key, value in manifest.items():
        if key in ("entries", "other_entries", "new_files", "verified", "notes", "samples_skipped",
                   "file_systems", "new_volumes", "history") and value:
            body = ",\n".join("  " + json.dumps(x, ensure_ascii=True, separators=(", ", ": ")) for x in value)
            lines.append(' %s: [\n%s\n ]' % (json.dumps(key), body))
        elif key in ("old_volume", "new_volume", "image", "summary", "tools", "accident"):
            body = ",\n".join("  %s: %s" % (json.dumps(k), json.dumps(v, ensure_ascii=True, separators=(", ", ": ")))
                              for k, v in value.items())
            lines.append(' %s: {\n%s\n }' % (json.dumps(key), body))
        else:
            lines.append(" %s: %s" % (json.dumps(key), json.dumps(value, ensure_ascii=True, separators=(", ", ": "))))
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="ascii") as f:
        f.write("{\n" + ",\n".join(lines) + "\n}\n")
    with open(tmp, "r", encoding="ascii") as f:
        if json.load(f) != manifest:
            fail("the manifest does not survive a JSON round trip")
    os.replace(tmp, path)


class RangeIndex:
    """A set of byte ranges with a fast "does [off, off+length) touch any of them?"."""

    def __init__(self, ranges):
        merged = []
        for off, length in sorted(r for r in ranges if r[1] > 0):
            if merged and off <= merged[-1][1]:
                merged[-1][1] = max(merged[-1][1], off + length)
            else:
                merged.append([off, off + length])
        self.starts = [m[0] for m in merged]
        self.ends = [m[1] for m in merged]

    def hits(self, off, length):
        import bisect
        i = bisect.bisect_right(self.starts, off + length - 1) - 1
        return i >= 0 and self.ends[i] > off

    def any(self, ranges):
        return any(self.hits(o, n) for o, n in ranges)


def si_times(record):
    for a in record["attributes"]:
        if a["type"] == 0x10 and "standard_information" in a:
            return a["standard_information"]
    return None


class CurrentVolume:
    """A file system the accident made, as the final image holds it (main coordinates).

    "Main coordinates" are byte offsets inside the lost file system's volume
    file (old_volume.offset in the image is main coordinate 0).
    """

    def __init__(self, path, offset_in_file, base, name):
        self.name = name
        self.vol = RawVolume(path, offset_in_file)
        self.vol.load_all()
        self.base = base
        b = self.vol.boot
        self.size = b["total_sectors"] * b["bytes_per_sector"]
        self.bitmap = Bitmap(self.vol)
        si = si_times(self.vol.record(3))
        if si is None:
            fail("%s: $Volume has no $STANDARD_INFORMATION" % name)
        self.format_time = si["created"]
        # the boot sector and its backup, the last sector of the partition, which
        # lies outside the volume and therefore outside its $Bitmap
        self.boot_sectors = [(base, b["bytes_per_sector"]), (base + self.size, b["bytes_per_sector"])]
        self.mft = [(base + o, n) for o, n in mft_byte_ranges(self.vol)]
        # C3: deleted records of the new volume (post_format) and the clusters they mapped
        self.post_format = []
        self.post_format_records = []
        for n in range(FIRST_USER_RECORD, self.vol.nrecords):
            r = self.vol.record(n)
            if r is None or not r["valid"] or r["in_use"] or (r["base_record"], r["base_sequence"]) != (0, 0):
                continue
            si = si_times(r)
            if si is None or si["mft_changed"] < self.format_time:
                continue
            self.post_format_records.append(n)
            try:
                st = self.vol.stream(n, "", self.vol.gather(n, []))
            except Fail:
                st = None                       # its extension records are gone: nothing to map
            if st is not None and not st["resident"]:
                self.post_format += [(base + o, length) for o, length in self.vol.extents(st)]

    def allocated(self, off, length):
        lo, hi = max(off, self.base), min(off + length, self.base + self.size)
        return lo < hi and self.bitmap.allocated(lo - self.base, hi - lo)

    def describe(self, vol_off):
        g = self.vol.geometry()
        g["offset"] = vol_off + self.base
        g["length"] = self.size + g["sector_size"]
        g["format_time"] = self.format_time
        g["format_time_unix_ms"] = unix_ms(self.format_time)
        return g


class FsView:
    """One old file system of the scenario: its image right before it was lost, and now.

    `pre_path` and `post_path` are files in the file system's own coordinates
    (byte 0 = its boot sector).  `runs`, for a file system stored inside a
    file of the main one, are that file's data runs: own byte X lies at main
    byte lcn * cluster + X % cluster; without runs own and main coordinates
    are the same.
    """

    def __init__(self, name, kind, pre_path, post_path, state_entries, old_volume, runs=None, run_cs=0,
                 container=None):
        self.name = name
        self.kind = kind
        self.pre_path = pre_path
        self.pre = RawVolume(pre_path)
        self.pre.load_all()
        self.post_fd = os.open(post_path, os.O_RDONLY)
        self.entries = state_entries
        self.old_volume = old_volume
        self.runs = runs
        self.run_cs = run_cs
        self.container = container
        self.now = OldLayout(self.pre, self.post_fd)
        self.by_path = {e["path"]: e for e in state_entries}

    def to_main(self, off, length):
        """[(main offset, length)] of own bytes [off, off+length); holes are left out."""
        if self.runs is None:
            return [(off, length)]
        out = []
        cs = self.run_cs
        pos = 0
        end = off + length
        for lcn, n in self.runs:
            lo, hi = max(off, pos * cs), min(end, (pos + n) * cs)
            if lo < hi and lcn is not None:
                out.append((lcn * cs + lo - pos * cs, hi - lo))
            pos += n
        return out

    def main_point(self, off):
        m = self.to_main(off, 1)
        if not m:
            fail("%s: byte %d lies in a hole of its container" % (self.name, off))
        return m[0][0]

    def container_ranges(self):
        return [r for lcn, n in self.runs if lcn is not None for r in [(lcn * self.run_cs, n * self.run_cs)]]


def resolves(record, number, sequence, want_directory):
    """ALGORITHMS reference rule: does reference (number, sequence) resolve to this record?"""
    if record is None or not record["valid"] or record["record_number"] not in (None, number):
        return False
    if want_directory and not record["is_directory"]:
        return False
    if record["in_use"]:
        return record["sequence"] == sequence
    nxt = 1 if sequence == 0xFFFF else sequence + 1
    return record["sequence"] == nxt and bool(record["attributes"])


def index_entries_of(fs, others, accident_time, current):
    """Directory entries that name something, from INDX blocks and index roots that survived.

    Returns (entries, surviving_block_triples).  An entry is "usable" in the
    sense of ALGORITHMS C4 / E6: older than the accident's format and not in a
    block the new file system has allocated.  Blocks that belong to another
    file system of the scenario (an older one at the same place, or the volume
    inside a container file) are left to that file system.
    """
    out = []
    triples = set()
    blocks = 0
    for blk in I.scan_image(fs.pre_path, kinds=("INDX",)):
        if not blk["valid"]:
            continue
        off, size = blk["offset"], blk["block_size"]
        foreign = False
        for o in others:
            if o is fs:
                continue
            if o.runs is not None and fs.runs is None and inside(o.container_ranges(), off, size):
                foreign = True
            elif o.runs is None and fs.runs is None and o.kind == "older" and fs.kind != "older" \
                    and same_bytes(o.pre.fd, fs.pre.fd, off, size):
                foreign = True
        if foreign:
            continue
        blocks += 1
        if not same_bytes(fs.pre.fd, fs.post_fd, off, size):
            continue
        main = fs.to_main(off, size)
        allocated = any(c.allocated(o, n) for c in current for o, n in main)
        for x in blk["entries"]:
            fn = x["file_name"]
            if not fn:
                continue
            triples.add((x["record"], x["sequence"], fn["name"]))
            out.append({"record": x["record"], "sequence": x["sequence"], "name": fn["name"],
                        "namespace": fn["namespace_name"], "parent_record": fn["parent_record"],
                        "parent_sequence": fn["parent_sequence"], "source": "indx",
                        "offset": fs.main_point(off),
                        "usable": not allocated and fn["mft_changed"] < accident_time,
                        "directory": bool(fn["file_attributes"] & 0x10000000)})
    return out, triples, blocks


def index_root_entries(fs, record_changed, accident_time):
    out = []
    for n in range(fs.pre.nrecords):
        r = fs.pre.record(n)
        if record_changed.get(n, True) or r is None or not r["valid"] or not r["is_directory"]:
            continue
        for a in r["attributes"]:
            if a["type"] != 0x90 or a["name"] != "$I30" or "index_root" not in a:
                continue
            for x in a["index_root"]["entries"]:
                fn = x["file_name"]
                if not fn or x["record"] == n:
                    continue
                out.append({"record": x["record"], "sequence": x["sequence"], "name": fn["name"],
                            "namespace": fn["namespace_name"], "parent_record": fn["parent_record"],
                            "parent_sequence": fn["parent_sequence"], "source": "index_root",
                            "offset": fs.main_point(fs.pre.record_offset(n)),
                            "usable": fn["mft_changed"] < accident_time,
                            "directory": bool(fn["file_attributes"] & 0x10000000)})
    return out


def file_extents(fs, e):
    """Own-coordinate byte ranges of an entry's data: its runs, or the record holding resident data."""
    od = e["ondisk"]
    if e["type"] != "file" or "resident" not in od:
        return None
    if od["resident"]:
        holder = e["mft_record"]
        return [(fs.pre.record_offset(holder), fs.pre.rs)]
    cs = fs.pre.cs
    return [(lcn * cs, n * cs) for lcn, n in od["runs"] if lcn is not None]


def assess_fs(fs, ctx):
    """The "after" facts of every entry of one old file system (first pass)."""
    pre, rs = fs.pre, fs.pre.rs
    record_changed = {}
    destroyed = []
    for n in range(pre.nrecords):
        off = pre.record_offset(n)
        changed = os.pread(pre.fd, rs, off) != os.pread(fs.post_fd, rs, off)
        record_changed[n] = changed
        if changed:
            destroyed.append(n)
    fs.record_changed = record_changed
    fs.destroyed = destroyed
    fs.index_entries, triples, fs.indx_blocks = index_entries_of(fs, ctx["fs"], ctx["accident_time"],
                                                                  ctx["current"])
    fs.indx_blocks_surviving = len(set(x["offset"] for x in fs.index_entries if x["source"] == "indx"))
    fs.index_entries += index_root_entries(fs, record_changed, ctx["accident_time"])
    siblings = {}
    for e in fs.entries:
        folded = siblings.setdefault(parent_of(e["path"]), {})
        folded[base_of(e["path"]).upper()] = folded.get(base_of(e["path"]).upper(), 0) + 1
    current = ctx["current"]
    out_entries = []
    for e in fs.entries:
        od = dict(e["ondisk"])
        out = {"path": e["path"], "type": e["type"], "size": e["size"], "sha256": e["sha256"],
               "mft_record": e["mft_record"], "mft_sequence": e["mft_sequence"],
               "created": e["created"], "modified": e["modified"], "accessed": e["accessed"],
               "created_unix_ms": unix_ms(e["created"]), "modified_unix_ms": unix_ms(e["modified"]),
               "accessed_unix_ms": unix_ms(e["accessed"]),
               "deleted": bool(e.get("deleted")), "content_type": e["content_type"], "tags": e["tags"]}
        issues = name_issues(base_of(e["path"]), siblings[parent_of(e["path"])])
        if issues:
            out["name_issues"] = issues
        if "dos_name" in e:
            out["dos_name"] = e["dos_name"]
        if "streams" in e:
            out["streams"] = e["streams"]
        out["file_system"] = fs.name
        if fs.container:
            out["embedded_in"] = fs.container
        recs = [e["mft_record"]] + od.get("extension_records", [])
        state_ok = not e.get("deleted") or od["record_state"] == "intact"
        after = {"record_intact": state_ok and not any(record_changed.get(n, True) for n in recs)}
        if state_ok:
            si = si_times(pre.record(e["mft_record"]))
            out["mft_changed"] = si["mft_changed"] if si else None
        else:
            out["mft_changed"] = None
        own = file_extents(fs, dict(e, ondisk=od)) if state_ok else None
        main = [m for o, n in (own or []) for m in fs.to_main(o, n)]
        e["_main"] = main
        if e["type"] == "file":
            if not state_ok:
                after["content_intact"] = False
                after["overlaps_new_allocation"] = None
            else:
                stream = pre.stream(e["mft_record"], "", pre.gather(e["mft_record"], od["extension_records"]))
                try:
                    data = fs.now.read_stream(stream)
                except (Fail, IndexError, struct.error):      # overwritten compressed data
                    data = b""
                after["content_intact"] = len(data) == e["size"] and sha256_hex(data) == e["sha256"]
                if not stream["resident"]:
                    after["clusters_changed"] = sum(count_changed(pre.fd, fs.post_fd, o, n, pre.cs)
                                                    for o, n in own)
                after["overlaps_new_allocation"] = any(c.allocated(o, n) for c in current for o, n in main)
                if e.get("deleted") and after["content_intact"] and not od.get("content_intact_before_accident"):
                    fail("%r: content that was damaged before the accident reads back intact after it" % e["path"])
        else:
            after["content_intact"] = None
            after["overlaps_new_allocation"] = None
        after["index_entry_intact"] = (e["mft_record"], e["mft_sequence"], base_of(e["path"])) in triples
        if e["type"] == "file" and state_ok:
            lo, hi = ctx["scan_range"]
            after["data_inside_scan_range"] = all(lo <= ctx["vol_off"] + o and ctx["vol_off"] + o + n <= hi
                                                  for o, n in main)
        else:
            after["data_inside_scan_range"] = None
        if "record_offset" in od:
            od["record_offset"] = ctx["vol_off"] + fs.main_point(od["record_offset"] - 0)
        out["ondisk"] = od
        out["after"] = after
        out_entries.append(out)
        if after["record_intact"]:
            ctx["owners"][od["record_offset"]] = {"file_system": fs.name, "path": e["path"],
                                                  "mft_record": e["mft_record"], "deleted": out["deleted"]}
    fs.out = out_entries
    fs.out_by_path = {x["path"]: x for x in out_entries}
    return out_entries


def finish_fs(fs, ctx):
    """Second pass: what needs every file system's first pass (names, ancestry, collisions, evidence)."""
    all_entries = [(o, x) for o in ctx["fs"] for x in o.index_entries]
    current = ctx["current"]
    post_format = RangeIndex([r for c in current for r in c.post_format])
    boots = RangeIndex([r for c in current for r in c.boot_sectors])
    newer = ctx["newer_ranges"] if fs.kind == "older" else None
    for out in fs.out:
        e = fs.by_path[out["path"]]
        after = out["after"]
        # ---- names of a destroyed folder that survive elsewhere
        if out["type"] == "dir" and not after["record_intact"] and out["mft_sequence"] is not None:
            key = (out["mft_record"], out["mft_sequence"])
            lo, hi = ctx["scan_range"]
            after["named_by"] = sorted(
                ({"name": x["name"], "namespace": x["namespace"], "source": x["source"],
                  "offset": ctx["vol_off"] + x["offset"], "parent_record": x["parent_record"],
                  "parent_sequence": x["parent_sequence"], "usable": x["usable"],
                  "inside_scan_range": lo <= ctx["vol_off"] + x["offset"] < hi}
                 for o, x in all_entries if o is fs and (x["record"], x["sequence"]) == key),
                key=lambda x: (x["source"], x["offset"], x["name"]))
            foreign = sorted(set((o.name, x["name"]) for o, x in all_entries
                                 if o is not fs and (x["record"], x["sequence"]) == key))
            if foreign:
                after["foreign_index_entries"] = [{"file_system": f, "name": n} for f, n in foreign]
        # ---- damage evidence and reuse by a newer generation
        if out["type"] == "file" and after["record_intact"]:
            main = e["_main"]
            if not after["content_intact"]:
                ev = []
                if after["overlaps_new_allocation"]:
                    ev.append("current-allocation")
                if post_format.any(main):
                    ev.append("post-format-record")
                if boots.any(main):
                    ev.append("current-boot-sector")
                if newer is not None and newer.any(main):
                    ev.append("newer-file")
                after["damage_evidence"] = ev or ["none"]
            if fs.kind == "older":
                after["clusters_reused_by_newer"] = ctx["newer_bitmap"] is not None and any(
                    ctx["newer_bitmap"].allocated(o, n) for o, n in main)
    # ---- ancestry: can the path be rebuilt, and what survives of it?
    lo, hi = ctx["scan_range"]

    def known(o, whole):
        """Does a scan learn this entry's name and parent: from its record, or from an index entry?"""
        a = o["after"]
        ro = o["ondisk"].get("record_offset")
        if a["record_intact"] and (whole or (ro is not None and lo <= ro < hi)):
            return True
        return any(x["usable"] and (whole or x["inside_scan_range"]) for x in a.get("named_by", []))

    def ancestry(out, whole):
        comps = out["path"].split("/")
        lost = None
        for i in range(1, len(comps)):
            if not known(fs.out_by_path["/".join(comps[:i])], whole):
                lost = i
        # a folder whose record is gone still gets its name (and parent) from a usable index entry
        return {"path_reconstructible": known(out, whole) and lost is None,
                "lost_ancestor": "/".join(comps[:lost]) if lost else None,
                "surviving_suffix": "/".join(comps[lost:]) if lost else out["path"]}

    partial = (lo, hi) != (0, ctx["image_size"])
    for out in fs.out:
        after = out["after"]
        after["ancestry"] = ancestry(out, False)
        if partial:
            whole = ancestry(out, True)
            if whole != after["ancestry"]:
                after["ancestry_whole_image"] = whole
        # ---- what the parent reference meets in the parent's slot now
        parent = parent_of(out["path"])
        if not after["record_intact"] or not parent:
            continue
        pe = fs.out_by_path[parent]
        if pe["after"]["record_intact"] or pe["mft_sequence"] is None:
            continue
        slot = fs.pre.record_offset(pe["mft_record"])
        if slot is None:
            continue
        rec = I.parse_file_record(os.pread(fs.post_fd, fs.pre.rs, slot), 0)
        if rec is None or not rec["valid"] or not rec["attributes"]:
            continue
        where = ctx["vol_off"] + fs.main_point(slot)
        owner = ctx["owners"].get(where, {"file_system": "unknown", "path": None})
        info = {"file_system": owner["file_system"], "path": owner["path"], "mft_record": pe["mft_record"],
                "mft_sequence": rec["sequence"], "in_use": rec["in_use"], "is_directory": rec["is_directory"],
                "record_offset": where}
        if resolves(rec, pe["mft_record"], pe["mft_sequence"], True):
            after["parent_reference_taken_by"] = info
        elif rec["in_use"]:
            after["parent_slot_contradiction"] = info


def describe_fs(fs, ctx, kind):
    g = dict(fs.old_volume)
    pre_vol = fs.pre
    si = si_times(pre_vol.record(3))
    g.update({"name": fs.name, "kind": kind,
              "format_time": si["created"] if si else None,
              "format_time_unix_ms": unix_ms(si["created"]) if si else None,
              "last_write": last_write(pre_vol),
              "last_write_unix_ms": unix_ms(last_write(pre_vol))})
    if fs.runs is None:
        g["offset"] = ctx["vol_off"]
    else:
        g["offset"] = None
        g["container"] = fs.container
        g["container_runs"] = [[lcn, n] for lcn, n in fs.runs]
        g["container_cluster_size"] = fs.run_cs
    g["length"] = os.fstat(pre_vol.fd).st_size
    return g


def read_through(fd, runs, cs, size, out_path):
    """Write the content of a file described by runs (holes as zeros) to out_path."""
    with open(out_path, "wb") as f:
        pos = 0
        for lcn, n in runs:
            if pos >= size:
                break
            take = min(n * cs, size - pos)
            if lcn is None:
                f.seek(pos + take)
            else:
                f.seek(pos)
                f.write(os.pread(fd, take, lcn * cs))
            pos += take
        f.truncate(size)


def cmd_finalize(args):
    state = load_state(args.state)
    if not state.get("analyzed"):
        fail("finalize must run after analyze")
    work = os.path.dirname(os.path.abspath(args.state))
    vol_off = args.volume_offset
    vol_len = os.path.getsize(args.volume)

    # the image must hold the volume byte for byte at its offset
    fd_vol = os.open(args.volume, os.O_RDONLY)
    fd_img = os.open(args.image, os.O_RDONLY)
    if not same_bytes(fd_vol, fd_img, 0, vol_len, vol_off):
        fail("%s does not contain the volume at byte %d" % (args.image, vol_off))
    image_size = os.fstat(fd_img).st_size
    h = hashlib.sha256()
    pos = 0
    while pos < image_size:
        b = os.pread(fd_img, 8 << 20, pos)
        h.update(b)
        pos += len(b)
    os.close(fd_img)
    os.close(fd_vol)
    partition_table = read_partition_table(args.image, state["old_volume"]["sector_size"]) if vol_off else None

    # ---- the file systems the accident made ----------------------------------
    current = [CurrentVolume(args.volume, 0, 0, "new")]
    for extra in args.new_volume:
        base = int(extra)
        current.append(CurrentVolume(args.volume, base, base, "new-%d" % len(current)))
    accident_time = current[0].format_time
    if accident_time % 10000000:
        fail("the new volume's creation time %d is not a whole second" % accident_time)
    last_old = state["accident"].get("last_old_write")
    if last_old is None:
        fail("the old volume's last write was not recorded before the accident")
    if not accident_time > last_old:
        fail("the accident's format (%d) is not later than the last write to the old volume (%d)"
             % (accident_time, last_old))
    for c in current[1:]:
        if not c.format_time > last_old:
            fail("%s was formatted before the last write to the old volume" % c.name)
    if args.scan_range:
        lo, n = [int(x) for x in args.scan_range.split(":")]
        scan_range = (lo, lo + n)
    else:
        scan_range = (vol_off, vol_off + vol_len) if vol_off else (0, image_size)

    # ---- the old file systems -----------------------------------------------
    main = FsView(state["fs_name"], "main", args.pre_volume, args.volume, state["expected"] + state["deleted"],
                  state["old_volume"])
    views = [main]
    for o in state.get("other_fs", []):
        if o["kind"] == "older":
            views.append(FsView(o["name"], "older", o["pre_volume"], args.volume, o["expected"] + o["deleted"],
                                o["old_volume"]))
        else:
            holder = [e for e in state["expected"] if e["path"] == o["container"]]
            if len(holder) != 1:
                fail("container %r is not a file of the main file system" % o["container"])
            st = main.pre.stream(holder[0]["mft_record"], "",
                                 main.pre.gather(holder[0]["mft_record"], holder[0]["ondisk"]["extension_records"]))
            runs = st["runs"]
            extracted = os.path.join(work, "%s.%s.after.img" % (state["scenario"], o["name"]))
            read_through(os.open(args.volume, os.O_RDONLY), runs, main.pre.cs, st["size"], extracted)
            views.append(FsView(o["name"], "embedded", o["pre_volume"], extracted, o["expected"] + o["deleted"],
                                o["old_volume"], runs=runs, run_cs=main.pre.cs, container=o["container"]))
    ctx = {"fs": views, "current": current, "accident_time": accident_time, "vol_off": vol_off,
           "scan_range": scan_range, "owners": {}, "newer_bitmap": None, "newer_ranges": None,
           "image_size": image_size}

    # new files written after the accident
    new_files = []
    for e in state.get("new_expected", []) + state.get("new_deleted", []):
        nv = current[0].vol
        r = nv.record(e["mft_record"])
        nf = {"path": e["path"], "type": e["type"], "size": e["size"], "sha256": e["sha256"],
              "mft_record": e["mft_record"], "deleted": bool(e.get("deleted")),
              "created": e["created"], "modified": e["modified"], "accessed": e["accessed"],
              "content_type": e["content_type"], "tags": e["tags"], "file_system": "new",
              "record_offset": vol_off + nv.record_offset(e["mft_record"])}
        if not e.get("deleted"):
            if not r or not r["in_use"] or base_of(e["path"]) not in [fn["name"] for fn in r["names"]]:
                fail("new file %r: record %d of the new MFT does not hold it" % (e["path"], e["mft_record"]))
            nf["mft_sequence"] = r["sequence"]
            if e["type"] == "file":
                data = nv.read_stream(nv.stream(e["mft_record"]))
                if sha256_hex(data) != e["sha256"]:
                    fail("new file %r: raw content does not match the driver's SHA-256" % e["path"])
            ctx["owners"][nf["record_offset"]] = {"file_system": "new", "path": e["path"],
                                                  "mft_record": e["mft_record"], "deleted": False}
        else:
            if not r or r["in_use"] or e["mft_record"] not in current[0].post_format_records:
                fail("deleted new file %r: record %d is not a post-format record of the new MFT"
                     % (e["path"], e["mft_record"]))
            nf["mft_sequence"] = (r["sequence"] - 1) & 0xFFFF
        new_files.append(nf)
    new_files.sort(key=lambda x: x["path"])

    entries = assess_fs(main, ctx)
    for v in views[1:]:
        assess_fs(v, ctx)
    if any(v.kind == "older" for v in views):
        # for an older generation: what the newer one (the lost file system) had allocated before the
        # accident, and the data of its files that intact records still describe (ALGORITHMS G2)
        ctx["newer_bitmap"] = Bitmap(main.pre)
        ctx["newer_ranges"] = RangeIndex(
            [m for x in main.out if x["after"]["record_intact"] and not x["deleted"]
             for m in main.by_path[x["path"]]["_main"]])
    for v in views:
        finish_fs(v, ctx)
    for v in views:
        for e in v.entries:
            e.pop("_main", None)
    entries.sort(key=lambda x: x["path"])
    other_entries = sorted([x for v in views[1:] for x in v.out], key=lambda x: (x["file_system"], x["path"]))
    state["destroyed_records"] = main.destroyed

    new_geo = current[0].describe(vol_off)
    if new_geo["serial"] == state["old_volume"]["serial"]:
        fail("old and new volume have the same serial number")
    new_geo["mkntfs_args"] = state["new_volume"]["mkntfs_args"]
    new_geo["length"] = vol_len if len(current) == 1 else current[0].size + new_geo["sector_size"]
    state["new_volume"] = new_geo
    accident = dict(state["accident"])
    accident["last_old_write_unix_ms"] = unix_ms(last_old)
    accident["format_time"] = accident_time
    accident["format_time_unix_ms"] = unix_ms(accident_time)
    file_systems = [describe_fs(v, ctx, v.kind) for v in views]
    history = []
    for hst in state.get("history", []):
        hst = dict(hst)
        # the re-format's time is the creation time of the file system it made
        made = [d for d in file_systems if d["kind"] == "main"][0]
        hst["format_time"] = made["format_time"]
        hst["format_time_unix_ms"] = made["format_time_unix_ms"]
        hst["last_old_write_unix_ms"] = unix_ms(hst["last_old_write"])
        if not hst["format_time"] > hst["last_old_write"]:
            fail("the re-format over %s is not later than its last write" % hst["formatted_over"])
        history.append(hst)

    ctx.update({"entries": entries, "other_entries": other_entries, "partition_table": partition_table,
                "volume_offset": vol_off, "volume_length": vol_len, "views": views, "new_files": new_files,
                "image_size": image_size, "image_path": args.image})
    for name in state["expect"]:
        if name in EXPECT_POST:
            msg = EXPECT_POST[name](state, ctx)
            log("verified: " + msg)
            state["verified"].append(msg)
    for v in views:
        if v.runs is None:
            msg = scan_check(args.volume, [x for x in v.out], v.now, v.name)
            log("verified: " + msg)
            state["verified"].append(msg)

    live = [e for e in entries if not e["deleted"]]
    files = [e for e in live if e["type"] == "file"]
    summary = {
        "files": len(files),
        "folders": len(live) - len(files),
        "deleted_entries": len(entries) - len(live),
        "bytes_in_files": sum(e["size"] for e in files),
        "old_mft_records": main.pre.nrecords,
        "old_mft_fragments": len(main.pre.mft_runs),
        "destroyed_records": len(main.destroyed),
        "live_entries_with_destroyed_record": sum(1 for e in live if not e["after"]["record_intact"]),
        "live_files_with_damaged_content": sum(1 for e in files if not e["after"]["content_intact"]),
        "live_files_fully_intact": sum(1 for e in files if e["after"]["content_intact"] and e["after"]["record_intact"]),
        "samples_with_destroyed_record_still_contiguous_and_intact": sum(
            1 for e in files if "sample" in e["tags"] and not e["after"]["record_intact"]
            and e["after"]["content_intact"] and e["ondisk"]["fragments"] == 1),
        "old_indx_blocks": main.indx_blocks,
        "old_indx_blocks_surviving": main.indx_blocks_surviving,
        "new_files": len(new_files),
        "live_entries_with_reconstructible_path": sum(1 for e in live if e["after"]["ancestry"]["path_reconstructible"]),
        "live_entries_below_a_lost_folder": sum(1 for e in live if e["after"]["record_intact"]
                                               and not e["after"]["ancestry"]["path_reconstructible"]),
        "destroyed_folders_named_by_index_entries": sum(1 for e in live if e["type"] == "dir"
                                                       and any(x["usable"] for x in e["after"].get("named_by", []))),
        "entries_whose_parent_reference_is_taken": sum(1 for e in entries if "parent_reference_taken_by" in e["after"]),
        "other_entries": len(other_entries),
    }
    old_geo = dict(state["old_volume"])
    old_geo["offset"] = vol_off
    old_geo["length"] = vol_len
    manifest = {
        "manifest_version": MANIFEST_VERSION,
        "scenario": state["scenario"],
        "description": state["description"],
        "seed": C.SEED,
        "generated_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "tools": tool_versions(),
        "image": {"file": os.path.basename(args.image), "size": image_size,
                  "sector_size": state["old_volume"]["sector_size"], "sha256": h.hexdigest(),
                  "pre_image": args.pre_image_name or None, "partition_table": partition_table},
        "old_volume": old_geo,
        "new_volume": new_geo,
        "new_volumes": [c.describe(vol_off) for c in current],
        "scan_range": {"offset": scan_range[0], "length": scan_range[1] - scan_range[0],
                       "what": args.scan_range_what},
        "accident": accident,
        "history": history,
        "file_systems": file_systems,
        "destroyed_records": main.destroyed,
        "summary": summary,
        "verified": state["verified"],
        "notes": state["notes"],
        "samples_skipped": state.get("samples_skipped", []),
        "new_files": new_files,
        "entries": entries,
        "other_entries": other_entries,
    }
    write_manifest(args.manifest, manifest)
    for v in views:
        v.pre.close()
        os.close(v.post_fd)
        if v.runs is not None:
            os.remove(os.path.join(work, "%s.%s.after.img" % (state["scenario"], v.name)))
    for c in current:
        c.vol.close()
    log("manifest: %s (%d files, %d folders, %d deleted entries, %d old MFT records destroyed%s)"
        % (os.path.basename(args.manifest), summary["files"], summary["folders"], summary["deleted_entries"],
           len(main.destroyed), ", %d entries of other file systems" % len(other_entries) if other_entries else ""))


# ==========================================================================
# 8. Small commands
# ==========================================================================

def cmd_samples(args):
    samples, skipped = C.make_samples(args.out)
    with open(os.path.join(args.out, "index.json"), "w", encoding="utf-8") as f:
        json.dump({"samples": samples, "skipped": skipped}, f, indent=1)
    log("%d samples, %d bytes: %s" % (len(samples), sum(s["size"] for s in samples),
                                       " ".join(sorted(set(s["content_type"] for s in samples)))))
    for s in skipped:
        log("skipped " + s)


def cmd_init(args):
    with open(os.path.join(args.samples, "index.json"), "r", encoding="utf-8") as f:
        skipped = json.load(f)["skipped"]
    state = {"scenario": args.scenario, "description": args.description,
             "expect": [x for x in args.expect.split(",") if x],
             "notes": [], "verified": [], "samples_skipped": skipped,
             "fs_name": args.fs_name, "boundary": args.boundary, "history": [],
             "accident": {"steps": [], "wiped_records": None}}
    save_state(args.state, state)


def cmd_format(args):
    """Record how a volume was formatted: the mkntfs arguments and what the boot sector says."""
    state = load_state(args.state)
    with open(args.volume, "rb") as f:
        boot = I.parse_boot_sector(f.read(512), 0)
    if not boot or not boot["valid"]:
        fail("%s: mkntfs left no valid boot sector" % args.volume)
    # the boot sector must say what mkntfs was told
    opt = dict(zip(args.mkntfs_args, args.mkntfs_args[1:]))
    for flag, key in (("-s", "bytes_per_sector"), ("-c", "cluster_size"), ("-p", "hidden_sectors"),
                      ("-H", "heads"), ("-S", "sectors_per_track")):
        if flag in opt and int(opt[flag]) != boot[key]:
            fail("%s: boot sector %s is %d, mkntfs was given %s %s"
                 % (args.volume, key, boot[key], flag, opt[flag]))
    if boot["total_sectors"] != os.path.getsize(args.volume) // boot["bytes_per_sector"] - 1:
        fail("%s: total_sectors %d is not one less than the size of the volume file in sectors"
             % (args.volume, boot["total_sectors"]))
    with open(args.volume, "rb") as f:
        f.seek((boot["total_sectors"]) * boot["bytes_per_sector"])
        backup = f.read(512)
    with open(args.volume, "rb") as f:
        if backup != f.read(512):
            fail("%s: the last sector is not a copy of the boot sector" % args.volume)
    info = {"mkntfs_args": args.mkntfs_args, "sector_size": boot["bytes_per_sector"],
            "cluster_size": boot["cluster_size"], "mft_record_size": boot["mft_record_size"],
            "index_block_size": boot["index_block_size"]}
    if args.which == "old":
        state["old_volume"] = info
    else:
        state["new_volume"] = info
        state["accident"]["steps"].append("quick-format")
    save_state(args.state, state)
    print("%d %d" % (boot["cluster_size"], boot["mft_record_size"]))


def cmd_note(args):
    state = load_state(args.state)
    state["notes"].append(args.text)
    if args.step:
        state["accident"]["steps"].append(args.step)
    save_state(args.state, state)


def cmd_report(args):
    """Print one block per manifest found in a directory."""
    for name in args.scenarios.split():
        path = os.path.join(args.dir, name + ".manifest.json")
        with open(path, "r", encoding="utf-8") as f:
            m = json.load(f)
        s = m["summary"]
        print("  %-18s %4d MiB  %5d files %4d folders %4d deleted | cluster %5d record %4d | "
              "lost: %3d old MFT records, %3d live entries, %3d files' content"
              % (m["image"]["file"], m["image"]["size"] >> 20, s["files"], s["folders"],
                 s["deleted_entries"], m["old_volume"]["cluster_size"], m["old_volume"]["mft_record_size"],
                 s["destroyed_records"], s["live_entries_with_destroyed_record"],
                 s["live_files_with_damaged_content"]))
        if args.verbose:
            for line in m["verified"]:
                print("      - " + line)


def cmd_embed(args):
    """Copy a volume file into a whole-disk image at a byte offset, leaving holes for zero blocks."""
    zero = bytes(1 << 20)
    with open(args.volume, "rb") as src, open(args.image, "r+b") as dst:
        pos = 0
        while True:
            block = src.read(1 << 20)
            if not block:
                break
            if block != zero[:len(block)]:
                dst.seek(args.offset + pos)
                dst.write(block)
            pos += len(block)
        dst.seek(0, os.SEEK_END)
        if dst.tell() < args.offset + pos:
            fail("%s is too small for a %d-byte volume at offset %d" % (args.image, pos, args.offset))


def cmd_parttable(args):
    json.dump(read_partition_table(args.image), sys.stdout, indent=1)
    sys.stdout.write("\n")


def main():
    ap = argparse.ArgumentParser(description="Worker behind make-fixtures.sh; not meant to be run by hand.")
    sub = ap.add_subparsers(dest="cmd")
    sub.required = True

    p = sub.add_parser("samples")
    p.add_argument("--out", required=True)
    p.set_defaults(fn=cmd_samples)

    p = sub.add_parser("init")
    p.add_argument("--state", required=True)
    p.add_argument("--scenario", required=True)
    p.add_argument("--description", required=True)
    p.add_argument("--expect", default="")
    p.add_argument("--samples", required=True)
    p.add_argument("--fs-name", default="main", help="name of the lost file system in the manifest")
    p.add_argument("--boundary", type=int, default=0,
                   help="highest old record number the accident is expected to destroy (0: none promised)")
    p.set_defaults(fn=cmd_init)

    p = sub.add_parser("format")
    p.add_argument("--state", required=True)
    p.add_argument("--which", choices=("old", "new"), required=True)
    p.add_argument("--volume", required=True)
    p.add_argument("mkntfs_args", nargs="*")
    p.set_defaults(fn=cmd_format)

    p = sub.add_parser("populate")
    p.add_argument("--state", required=True)
    p.add_argument("--root", required=True)
    p.add_argument("--samples", required=True)
    p.add_argument("--profile", choices=sorted(PROFILES), required=True)
    p.add_argument("--compression", action="store_true")
    p.add_argument("--phase", type=int, choices=(1, 2), required=True)
    p.add_argument("--prelude", type=int, default=0,
                   help="create the prelude first, pushing later entries past this record number")
    p.add_argument("--restored", action="store_true", help="gen2: creation times as restored from a backup")
    p.add_argument("--source", default="", help="host: the volume image stored as a file")
    p.set_defaults(fn=cmd_populate)

    p = sub.add_parser("capture")
    p.add_argument("--state", required=True)
    p.add_argument("--root", required=True)
    p.set_defaults(fn=cmd_capture)

    p = sub.add_parser("analyze")
    p.add_argument("--state", required=True)
    p.add_argument("--volume", required=True)
    p.set_defaults(fn=cmd_analyze)

    p = sub.add_parser("crosscheck")
    p.add_argument("--state", required=True)
    p.add_argument("--volume", required=True)
    p.add_argument("--sample", type=int, default=40)
    p.set_defaults(fn=cmd_crosscheck)

    p = sub.add_parser("wipe")
    p.add_argument("--state", required=True)
    p.add_argument("--pre-volume", required=True, help="the file system being formatted over, same coordinates")
    p.add_argument("--volume", required=True, help="the freshly formatted volume")
    p.add_argument("--volume-offset", type=int, default=0, help="where the volume will sit in the image")
    p.add_argument("--last", type=int, default=255)
    p.add_argument("--fill", choices=("zeros", "blank"), default="zeros")
    p.add_argument("--record", choices=("accident", "history"), default="accident")
    p.set_defaults(fn=cmd_wipe)

    p = sub.add_parser("lastwrite")
    p.add_argument("--state", required=True)
    p.add_argument("--volume", required=True)
    p.add_argument("--record", default="accident", help="'accident' or the name of the file system formatted over")
    p.set_defaults(fn=cmd_lastwrite)

    p = sub.add_parser("stash")
    p.add_argument("--state", required=True)
    p.add_argument("--name", required=True)
    p.add_argument("--kind", choices=("older", "embedded"), required=True)
    p.add_argument("--pre-volume", required=True)
    p.add_argument("--container", default="")
    p.set_defaults(fn=cmd_stash)

    p = sub.add_parser("newfiles")
    p.add_argument("--state", required=True)
    p.add_argument("--root", required=True)
    p.add_argument("--samples", required=True)
    p.set_defaults(fn=cmd_newfiles)

    p = sub.add_parser("collide")
    p.add_argument("--state", required=True)
    p.add_argument("--root", required=True)
    p.add_argument("--samples", required=True)
    p.set_defaults(fn=cmd_collide)

    p = sub.add_parser("capture-new")
    p.add_argument("--state", required=True)
    p.add_argument("--root", required=True)
    p.set_defaults(fn=cmd_capture_new)

    p = sub.add_parser("note")
    p.add_argument("--state", required=True)
    p.add_argument("--step", default="")
    p.add_argument("text")
    p.set_defaults(fn=cmd_note)

    p = sub.add_parser("report")
    p.add_argument("--dir", required=True)
    p.add_argument("--scenarios", required=True)
    p.add_argument("--verbose", action="store_true")
    p.set_defaults(fn=cmd_report)

    p = sub.add_parser("embed")
    p.add_argument("--volume", required=True)
    p.add_argument("--image", required=True)
    p.add_argument("--offset", type=int, required=True)
    p.set_defaults(fn=cmd_embed)

    p = sub.add_parser("parttable")
    p.add_argument("image")
    p.set_defaults(fn=cmd_parttable)

    p = sub.add_parser("finalize")
    p.add_argument("--state", required=True)
    p.add_argument("--pre-volume", required=True)
    p.add_argument("--volume", required=True)
    p.add_argument("--volume-offset", type=int, default=0)
    p.add_argument("--image", required=True)
    p.add_argument("--pre-image-name", default="")
    p.add_argument("--manifest", required=True)
    p.add_argument("--new-volume", action="append", default=[],
                   help="byte offset, inside the volume file, of a further volume the accident made")
    p.add_argument("--scan-range", default="", help="START:LENGTH in the image (default: the old partition)")
    p.add_argument("--scan-range-what", default="the old volume's partition")
    p.set_defaults(fn=cmd_finalize)

    args = ap.parse_args()
    try:
        args.fn(args)
    except Fail as e:
        sys.stderr.write("fixture_helper: FAILED: %s\n" % e)
        sys.exit(2)


if __name__ == "__main__":
    main()
