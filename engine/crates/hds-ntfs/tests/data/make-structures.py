#!/usr/bin/env python3
"""Rebuild structures.bin and structures.json from the fixture images.

    make-structures.py FIXTURES_DIR

structures.bin is a handful of real NTFS structures (FILE records, INDX
blocks, boot sectors written by mkntfs and ntfs-3g), copied out of the
fixture images that scripts/linux/make-fixtures.sh builds and laid one behind
the other at 512-byte boundaries. One structure per feature, the first that
matches in ascending offset order; the list is printed.

structures.json is what scripts/linux/inspect-ntfs.py --json says about
structures.bin. tests/corpus.rs compares the parser with it on every test
run, without needing the fixtures or Python; tests/robustness.rs mutates the
structures.

Only needed when the fixtures change in a way that matters here. Needs
nothing but the Python 3 standard library.
"""
import json
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
INSPECT = os.path.normpath(os.path.join(HERE, "../../../../../scripts/linux/inspect-ntfs.py"))


def inspect(image):
    out = subprocess.run([sys.executable, INSPECT, "--json", image], check=True, stdout=subprocess.PIPE).stdout
    return json.loads(out)


def attrs(record, type_code):
    return [a for a in record["attributes"] if a["type"] == type_code]


def unnamed_data(record):
    return [d for d in record["data"] if d["name"] == ""]


def names_in(block):
    return [e for e in block["entries"] if e.get("file_name")]


def user(record):
    return record["in_use"] and record["record_number"] is not None and record["record_number"] > 30


FILES = {
    "basic": [
        ("$MFT: non-resident $DATA and $BITMAP",
         lambda r: r["record_number"] == 0 and r["in_use"] and any(n["name"] == "$MFT" for n in r["names"])),
        ("$Volume: $VOLUME_NAME, $VOLUME_INFORMATION",
         lambda r: r["in_use"] and attrs(r, 0x60)),
        ("root directory: $INDEX_ROOT, $INDEX_ALLOCATION and $BITMAP named $I30",
         lambda r: r["record_number"] == 5 and r["in_use"] and attrs(r, 0xA0)),
        ("$Secure: the view indexes $SDH and $SII",
         lambda r: r["in_use"] and any(a["name"] == "$SII" for a in r["attributes"])),
        ("never-used record",
         lambda r: not r["in_use"] and not r["attributes"]),
        ("file with resident $DATA",
         lambda r: user(r) and not r["is_directory"] and r["names"]
         and any(not d["non_resident"] and d["value_length"] > 100 for d in unnamed_data(r))),
        ("file with a fragmented non-resident $DATA",
         lambda r: user(r) and any(d["non_resident"] and len(d["runs"]) >= 3 for d in unnamed_data(r))),
        ("directory with a resident index of several entries",
         lambda r: user(r) and r["is_directory"] and not attrs(r, 0xA0)
         and any(len(a.get("index_root", {}).get("entries", [])) >= 4 for a in attrs(r, 0x90))),
        ("directory with index blocks",
         lambda r: user(r) and r["is_directory"] and attrs(r, 0xA0)),
        ("deleted file (not in use, attributes kept)",
         lambda r: not r["in_use"] and not r["is_directory"] and unnamed_data(r)),
        ("deleted directory (directory flag kept)",
         lambda r: not r["in_use"] and r["is_directory"] and r["attributes"]),
        ("file with a named $DATA stream",
         lambda r: user(r) and any(d["name"] for d in r["data"])),
        ("base record with a non-resident $ATTRIBUTE_LIST",
         lambda r: user(r) and attrs(r, 0x20)),
        ("extension record",
         lambda r: r["in_use"] and r["base_record"] != 0),
        ("compressed file",
         lambda r: r["in_use"] and any(d["non_resident"] and d["compressed"] for d in r["data"])),
        ("file with a long name and a DOS alias",
         lambda r: r["in_use"] and {n["namespace"] for n in r["names"]} == {1, 2}),
        ("file with a WIN32+DOS name",
         lambda r: user(r) and any(n["namespace"] == 3 for n in r["names"])),
        ("file whose name is not ASCII",
         lambda r: r["in_use"] and any(any(ord(c) > 0x2000 for c in n["name"]) for n in r["names"])),
    ],
    "fragmented": [
        ("extension record full of runs",
         lambda r: r["in_use"] and r["base_record"] != 0
         and any(len(d.get("runs") or []) > 150 for d in r["data"])),
        ("sparse file with holes in the middle",
         lambda r: r["in_use"] and any(d["non_resident"] and d["sparse"] and len(d["runs"]) >= 4 for d in r["data"])),
        ("file with an initialized size below its size",
         lambda r: user(r) and any(d["non_resident"] and d["lowest_vcn"] == 0
                                   and 0 < d["initialized_size"] < d["data_size"] for d in r["data"])),
    ],
    "4k-records": [
        ("4096-byte record: root directory",
         lambda r: r["record_size"] == 4096 and r["record_number"] == 5 and r["in_use"]),
        ("4096-byte record: large resident $DATA across several 512-byte blocks",
         lambda r: r["record_size"] == 4096 and r["in_use"]
         and any(not d["non_resident"] and d["value_length"] > 1500 for d in unnamed_data(r))),
    ],
}

BLOCKS = [
    ("basic", "INDX leaf block of a directory",
     lambda b: len(names_in(b)) >= 20 and b["is_leaf"]),
    ("basic", "INDX block whose entries point at sub-nodes",
     lambda b: any(e["has_subnode"] for e in b["entries"]) and names_in(b)),
    ("big-clusters", "INDX block with a VCN counted in 512-byte units (64 KiB clusters)",
     lambda b: b["vcn"] > 0 and names_in(b)),
    ("4k-records", "INDX block from a volume with 4096-byte sectors",
     lambda b: names_in(b)),
]

BOOTS = [
    ("basic", "boot sector: 4 KiB clusters"),
    ("big-clusters", "boot sector: 64 KiB clusters"),
    ("4k-records", "boot sector: 4096-byte sectors"),
]


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    fixtures = sys.argv[1]
    docs = {}

    def doc(name):
        if name not in docs:
            docs[name] = inspect(os.path.join(fixtures, name + ".img"))
        return docs[name]

    picks = []  # (image, offset, length, why)

    def pick(image, items, wanted, why, size_key):
        for item in items:
            if item["valid"] and wanted(item) and (image, item["offset"]) not in [(p[0], p[1]) for p in picks]:
                picks.append((image, item["offset"], item[size_key] if size_key else 512, why))
                return
        sys.exit("nothing matches %r in %s.img" % (why, image))

    for image, wanted in FILES.items():
        for why, predicate in wanted:
            pick(image, doc(image)["file_records"], predicate, why, "record_size")
    for image, why, predicate in BLOCKS:
        pick(image, doc(image)["index_blocks"], predicate, why, "block_size")
    for image, why in BOOTS:
        pick(image, doc(image)["boot_sectors"], lambda b: True, why, None)

    blob = bytearray()
    for image, offset, length, why in picks:
        with open(os.path.join(fixtures, image + ".img"), "rb") as f:
            f.seek(offset)
            data = f.read(length)
        assert len(data) == length and length % 512 == 0
        print("%6d  %5d bytes  %s.img @ %d: %s" % (len(blob), length, image, offset, why))
        blob += data
    out_bin = os.path.join(HERE, "structures.bin")
    with open(out_bin, "wb") as f:
        f.write(blob)

    result = inspect(out_bin)
    result["image"] = "structures.bin"
    assert result["counts"]["invalid"] == 0
    with open(os.path.join(HERE, "structures.json"), "w", encoding="ascii", newline="\n") as f:
        json.dump(result, f, ensure_ascii=True, separators=(",", ":"))
        f.write("\n")
    print("%d structures, %d bytes" % (len(picks), len(blob)))


if __name__ == "__main__":
    main()
