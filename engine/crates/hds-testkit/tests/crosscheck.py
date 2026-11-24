#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
crosscheck.py - compare an hds-testkit image with its manifest, using the
independent decoder scripts/linux/inspect-ntfs.py.

    crosscheck.py INSPECTOR IMAGE MANIFEST.json

The builder (Rust, hds-testkit) and the decoder (Python, inspect-ntfs.py)
share no code.  This script decodes the image with the decoder and requires
that it sees exactly what the manifest says was written:

  1. the same set of NTFS boot sectors, with the same fields;
  2. the same set of FILE records (by offset); for every intact one the same
     header fields, $STANDARD_INFORMATION, names, $DATA and $INDEX_ALLOCATION
     extents with their runs, attribute lists, index roots, reparse tags;
  3. the same set of INDX blocks, with the same VCN and entries;
  4. for every file the manifest calls intact, the content rebuilt from the
     decoder's view of the records (resident value, or runs with holes and
     the initialized-size rule) has the manifest's SHA-256; for every file
     it calls not intact, it has not;
  5. for every current volume, the $Bitmap and the $MFT bitmap read through
     the decoder's runlists mark exactly the clusters and slots the manifest
     lists as live.

Exit status 0 when everything agrees; otherwise the differences are printed
(at most 60) and the status is 1.
"""

import hashlib
import importlib.util
import json
import os
import sys

MAX_REPORT = 60


def load_inspector(path):
    spec = importlib.util.spec_from_file_location("inspect_ntfs", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class Checker:
    def __init__(self, inspector, image_path, manifest):
        self.ins = inspector
        self.image_path = image_path
        self.fd = os.open(image_path, os.O_RDONLY)
        self.size = os.fstat(self.fd).st_size
        self.m = manifest
        self.problems = []
        self.checked = {"boot": 0, "file": 0, "indx": 0, "content": 0, "content_broken": 0,
                        "bitmap": 0, "fields": 0}

    def problem(self, text):
        self.problems.append(text)

    def eq(self, where, name, got, want):
        self.checked["fields"] += 1
        if got != want:
            self.problem("%s: %s is %r, manifest says %r" % (where, name, got, want))

    def read(self, offset, length):
        if offset < 0 or offset + length > self.size:
            return None
        return os.pread(self.fd, length, offset)

    # ---------------------------------------------------------------- names

    @staticmethod
    def units(s):
        """UTF-16 code units of a decoded name (lone surrogates survive)."""
        b = s.encode("utf-16-le", errors="surrogatepass")
        return [b[i] | (b[i + 1] << 8) for i in range(0, len(b), 2)]

    def check_name(self, where, got, want):
        self.eq(where, "name", self.units(got["name"]), want["name_utf16"])
        self.eq(where, "namespace", got["namespace"], want["namespace"])
        self.eq(where, "parent", [got["parent_record"], got["parent_sequence"]],
                [want["parent"]["number"], want["parent"]["seq"]])
        self.eq(where, "name times",
                [got["created"], got["modified"], got["mft_changed"], got["accessed"]],
                [self.signed(t) for t in want["times"]])
        self.eq(where, "name allocated size", got["allocated_size"], want["allocated_size"])
        self.eq(where, "name data size", got["data_size"], want["real_size"])
        self.eq(where, "name flags", got["file_attributes"], want["flags"])

    @staticmethod
    def signed(v):
        return v - (1 << 64) if v >= (1 << 63) else v

    # --------------------------------------------------------- boot sectors

    def check_boot(self, found):
        got = {b["offset"]: b for b in found if b["valid"]}
        want = {b["offset"]: b for b in self.m["boot_sectors"]}
        for off in sorted(set(got) - set(want)):
            self.problem("boot sector at %d is in the image but not in the manifest" % off)
        for off in sorted(set(want) - set(got)):
            self.problem("boot sector at %d is in the manifest but the decoder finds no valid one" % off)
        for off in sorted(set(got) & set(want)):
            g, w = got[off], want[off]
            where = "boot sector at %d" % off
            self.checked["boot"] += 1
            self.eq(where, "bytes per sector", g["bytes_per_sector"], w["bytes_per_sector"])
            self.eq(where, "cluster size", g["cluster_size"], w["cluster_size"])
            self.eq(where, "total sectors", g["total_sectors"], w["total_sectors"])
            self.eq(where, "mft lcn", g["mft_lcn"], w["mft_lcn"])
            self.eq(where, "mftmirr lcn", g["mftmirr_lcn"], w["mftmirr_lcn"])
            self.eq(where, "record size", g["mft_record_size"], w["record_size"])
            self.eq(where, "index block size", g["index_block_size"], w["index_block_size"])
            self.eq(where, "hidden sectors", g["hidden_sectors"], w["hidden_sectors"])
            self.eq(where, "serial", g["serial"], "%016X" % w["serial"])

    # --------------------------------------------------------- FILE records

    def check_stream(self, where, got, want):
        self.eq(where, "stream name", got["name"], want["name"])
        self.eq(where, "stream flags", got["flags"], want["flags"])
        self.eq(where, "stream attr id", got["attr_id"], want["attr_id"])
        self.eq(where, "stream offset", got["offset"], want["attr_offset"])
        if want["resident"] is not None:
            self.eq(where, "stream is resident", got["non_resident"], False)
            self.eq(where, "resident value", [got.get("value_offset"), got.get("value_length")],
                    list(want["resident"]))
        else:
            nr = want["non_resident"]
            self.eq(where, "stream is non-resident", got["non_resident"], True)
            self.eq(where, "lowest vcn", got.get("lowest_vcn"), nr["lowest_vcn"])
            self.eq(where, "highest vcn", got.get("highest_vcn"), nr["highest_vcn"])
            self.eq(where, "allocated size", got.get("allocated_size"), nr["allocated_size"])
            self.eq(where, "data size", got.get("data_size"), nr["real_size"])
            self.eq(where, "initialized size", got.get("initialized_size"), nr["initialized_size"])
            self.eq(where, "compressed size", got.get("compressed_size"), nr["compressed_size"])
            self.eq(where, "compression unit", got.get("compression_unit"), nr["compression_unit"])
            self.eq(where, "runs", [[r["lcn"], r["length"]] for r in got.get("runs", [])],
                    [[r["lcn"], r["len"]] for r in nr["runs"]])

    def check_index_entries(self, where, got_entries, want_entries):
        got = [e for e in got_entries if not e["is_last"]]
        self.eq(where, "number of index entries", len(got), len(want_entries))
        for i, (g, w) in enumerate(zip(got, want_entries)):
            ew = "%s entry %d" % (where, i)
            self.eq(ew, "child", [g["record"], g["sequence"]], [w["child"]["number"], w["child"]["seq"]])
            self.eq(ew, "sub-node", g["subnode_vcn"], w["subnode"])
            if g["file_name"] is None:
                self.problem("%s: key is not a file name" % ew)
            else:
                self.check_name(ew, g["file_name"], w["key"])

    def check_records(self, found):
        got = {r["offset"]: r for r in found}
        want = {r["offset"]: r for r in self.m["records"]}
        for off in sorted(set(got) - set(want)):
            self.problem("FILE record at %d (number %s) is in the image but not in the manifest"
                         % (off, got[off]["record_number"]))
        for off in sorted(set(want) - set(got)):
            self.problem("FILE record at %d (slot %d) is in the manifest but not in the image"
                         % (off, want[off]["slot"]))
        for off in sorted(set(got) & set(want)):
            g, w = got[off], want[off]
            if w["state"] != "intact":
                continue
            where = "FILE record at %d (slot %d, %s)" % (off, w["slot"], w["role"])
            self.checked["file"] += 1
            self.forgive_highest_vcn_zero(g, w)
            if not g["valid"]:
                self.problem("%s: the decoder calls it invalid: %s" % (where, g["errors"]))
                continue
            if w["torn_blocks"]:
                # Two bytes of every torn block are not what the record was
                # written with, so only the header is compared.
                self.eq(where, "torn blocks", g["fixup_mismatch_sectors"], w["torn_blocks"])
                self.eq(where, "record number", g["record_number"], w["number"])
                self.eq(where, "sequence", g["sequence"], w["sequence"])
                self.eq(where, "flags", g["flags"], w["flags"])
                self.eq(where, "base reference", [g["base_record"], g["base_sequence"]],
                        [w["base"]["number"], w["base"]["seq"]])
                continue
            self.eq(where, "record size", g["record_size"], w["size"])
            self.eq(where, "record number", g["record_number"], w["number"])
            self.eq(where, "update sequence offset", g["usa_offset"], 0x2A if w["number"] is None else 0x30)
            self.eq(where, "sequence", g["sequence"], w["sequence"])
            self.eq(where, "flags", g["flags"], w["flags"])
            self.eq(where, "link count", g["link_count"], w["link_count"])
            self.eq(where, "lsn", g["lsn"], w["lsn"])
            self.eq(where, "base reference", [g["base_record"], g["base_sequence"]],
                    [w["base"]["number"], w["base"]["seq"]])
            self.eq(where, "bytes in use", g["bytes_in_use"], w["bytes_in_use"])
            self.eq(where, "update sequence number", g["usn"], w["usn"])
            self.eq(where, "torn blocks", g["fixup_mismatch_sectors"], w["torn_blocks"])
            self.eq(where, "attribute types", [a["type"] for a in g["attributes"]], w["attr_types"])
            # $STANDARD_INFORMATION
            si = [a for a in g["attributes"] if a["type"] == 0x10]
            if w["standard_information"] is None:
                self.eq(where, "has $STANDARD_INFORMATION", bool(si), False)
            elif len(si) != 1 or "standard_information" not in si[0]:
                self.problem("%s: no decodable $STANDARD_INFORMATION" % where)
            else:
                times, attrs, length = w["standard_information"]
                s = si[0]["standard_information"]
                self.eq(where, "$SI times", [s["created"], s["modified"], s["mft_changed"], s["accessed"]],
                        [self.signed(t) for t in times])
                self.eq(where, "$SI file attributes", s["file_attributes"], attrs)
                self.eq(where, "$SI length", si[0]["value_length"], length)
            # names
            names = [a for a in g["attributes"] if a["type"] == 0x30]
            self.eq(where, "number of names", len(names), len(w["names"]))
            for a, wn in zip(names, w["names"]):
                if a.get("file_name") is None:
                    self.problem("%s: undecodable $FILE_NAME" % where)
                    continue
                self.check_name(where + " name", a["file_name"], wn)
                self.eq(where, "name attr id", a["attr_id"], wn["attr_id"])
            # $DATA and $INDEX_ALLOCATION extents
            for type_code, key in ((0x80, "data"), (0xA0, "index_allocation")):
                streams = [a for a in g["attributes"] if a["type"] == type_code]
                self.eq(where, "number of 0x%x attributes" % type_code, len(streams), len(w[key]))
                for a, ws in zip(streams, w[key]):
                    self.check_stream("%s 0x%x" % (where, type_code), a, ws)
            # attribute list
            lists = [a for a in g["attributes"] if a["type"] == 0x20]
            if w["attribute_list"] is None:
                self.eq(where, "has $ATTRIBUTE_LIST", bool(lists), False)
            elif len(lists) != 1:
                self.problem("%s: %d attribute lists" % (where, len(lists)))
            else:
                wl = w["attribute_list"]
                self.eq(where, "list is resident", not lists[0]["non_resident"], wl["resident"])
                if wl["resident"]:
                    entries = lists[0].get("attribute_list", [])
                else:
                    self.eq(where, "list runs", [[r["lcn"], r["length"]] for r in lists[0]["runs"]],
                            [[r["lcn"], r["len"]] for r in wl["runs"]])
                    entries = self.non_resident_list(w, lists[0])
                self.eq(where, "list entries",
                        [[e["type"], e["name"], e["lowest_vcn"], e["record"], e["sequence"], e["attr_id"]]
                         for e in entries],
                        [[e["type_code"], e["name"], e["lowest_vcn"], e["holder"]["number"],
                          e["holder"]["seq"], e["attr_id"]] for e in wl["entries"]])
            # $INDEX_ROOT:$I30
            roots = [a for a in g["attributes"] if a["type"] == 0x90 and a["name"] == "$I30"]
            if w["index_root"] is None:
                self.eq(where, "has $INDEX_ROOT:$I30", bool(roots), False)
            elif len(roots) != 1 or "index_root" not in roots[0]:
                self.problem("%s: no decodable $INDEX_ROOT:$I30" % where)
            else:
                self.check_index_entries(where + " index root", roots[0]["index_root"]["entries"],
                                         w["index_root"])
            # reparse tag: the decoder gives the value's place, the tag is its first four bytes
            reparse = [a for a in g["attributes"] if a["type"] == 0xC0]
            if w["reparse_tag"] is None:
                self.eq(where, "has $REPARSE_POINT", bool(reparse), False)
            elif len(reparse) != 1 or reparse[0]["non_resident"]:
                self.problem("%s: no resident $REPARSE_POINT" % where)
            else:
                raw = self.fixed_record(off, g)
                at = reparse[0]["value_offset"]
                self.eq(where, "reparse tag", int.from_bytes(raw[at:at + 4], "little"), w["reparse_tag"])

    @staticmethod
    def forgive_highest_vcn_zero(got, want):
        """The decoder insists that the highest VCN matches the runs.

        layout.h says the field "can be 0 for single extent attributes" and
        one scenario writes exactly that.  Where the manifest announces such
        an extent and it is the decoder's only complaint, the record counts
        as valid; everything else in it is still compared.
        """
        announced = any(d["non_resident"] and d["non_resident"]["highest_vcn"] == 0
                        and sum(r["len"] for r in d["non_resident"]["runs"]) > 1
                        for d in want["data"])
        if announced and got["errors"] and all("header says 1" in e for e in got["errors"]):
            got["valid"] = True

    def fixed_record(self, offset, rec):
        """The record's bytes with the fixups undone by the decoder's own routine."""
        buf = bytearray(self.read(offset, rec["record_size"]))
        self.ins.apply_fixups(buf, rec["usa_offset"], rec["usa_count"])
        return bytes(buf)

    def non_resident_list(self, want_record, attr):
        inst = self.m["instances"][want_record["instance"]]
        data = self.read_runs(inst, attr["runs"], attr["data_size"], attr["initialized_size"])
        if data is None:
            self.problem("attribute list of record at %d cannot be read" % want_record["offset"])
            return []
        return self.ins.parse_attribute_list_value(data)

    # ---------------------------------------------------------- INDX blocks

    def check_index_blocks(self, found):
        got = {b["offset"]: b for b in found}
        want = {b["offset"]: b for b in self.m["index_blocks"]}
        for off in sorted(set(got) - set(want)):
            self.problem("INDX block at %d is in the image but not in the manifest" % off)
        for off in sorted(set(want) - set(got)):
            self.problem("INDX block at %d is in the manifest but not in the image" % off)
        for off in sorted(set(got) & set(want)):
            g, w = got[off], want[off]
            if w["state"] != "intact":
                continue
            where = "INDX block at %d" % off
            self.checked["indx"] += 1
            if not g["valid"]:
                self.problem("%s: the decoder calls it invalid: %s" % (where, g["errors"]))
                continue
            self.eq(where, "block size", g["block_size"], w["size"])
            self.eq(where, "vcn", g["vcn"], w["vcn"])
            self.eq(where, "lsn", g["lsn"], w["lsn"])
            self.eq(where, "is leaf", g["is_leaf"], w["leaf"])
            self.eq(where, "fixups", g["fixup_ok"], True)
            self.check_index_entries(where, g["entries"], w["entries"])
            for e in g["entries"]:
                if not e["is_last"] and e["file_name"] is not None:
                    self.eq(where, "entry parent is the owner",
                            [e["file_name"]["parent_record"], e["file_name"]["parent_sequence"]],
                            [w["owner"]["number"], w["owner"]["seq"]])

    # -------------------------------------------------------------- content

    def read_volume(self, inst, offset, length):
        """Bytes of a volume, by offset from its cluster 0.

        A volume inside a file is read through that file's extents, which is
        how its clusters got onto the disk.
        """
        if inst["embedded_in"] is None:
            return self.read(inst["geometry"]["volume_offset"] + offset, length)
        host = self.m["entries"][inst["embedded_in"]]
        pos = inst["offset_in_host"] + offset
        out = bytearray()
        cluster = None
        for ext in host["extents"]:
            cluster = cluster or ext["bytes"] // ext["clusters"]
            start = ext["vcn"] * cluster
            if ext["offset"] is None or pos >= start + ext["bytes"] or pos + (length - len(out)) <= start:
                continue
            if pos < start:
                return None
            take = min(start + ext["bytes"] - pos, length - len(out))
            chunk = self.read(ext["offset"] + (pos - start), take)
            if chunk is None:
                return None
            out += chunk
            pos += take
            if len(out) == length:
                return bytes(out)
        return bytes(out) if len(out) == length else None

    def read_runs(self, inst, runs, data_size, initialized_size):
        """Content of a non-resident stream as a correct reader produces it."""
        geo = inst["geometry"]
        cs = geo["cluster_size"]
        out = bytearray()
        for run in runs:
            length = run["length"] * cs
            if run["lcn"] is None:
                out += bytes(length)
            else:
                chunk = self.read_volume(inst, run["lcn"] * cs, length)
                if chunk is None:
                    return None
                out += chunk
            if len(out) >= data_size:
                break
        if len(out) < data_size:
            out += bytes(data_size - len(out))
        out = out[:data_size]
        if initialized_size < data_size:
            out[initialized_size:] = bytes(data_size - initialized_size)
        return bytes(out)

    def rebuild_content(self, entry, by_offset):
        """Rebuild a file from the decoder's records alone; None if impossible."""
        inst = self.m["instances"][entry["instance"]]
        base = by_offset.get(entry["record_offset"])
        if base is None or not base["valid"]:
            return None
        group = [base]
        for _, offset in entry["extension_records"]:
            ext = by_offset.get(offset)
            if ext is None or not ext["valid"]:
                return None
            # the decoder must see it as an extension of this very file
            if (ext["base_record"], ext["base_sequence"]) != (entry["record"], entry["sequence"]):
                self.problem("record at %d is listed as an extension of record %d but refers to %d/%d"
                             % (offset, entry["record"], ext["base_record"], ext["base_sequence"]))
            group.append(ext)
        pieces = []
        for rec in group:
            for d in rec["data"]:
                if d["name"] == "":
                    pieces.append((rec, d))
        if not pieces:
            return None
        resident = [p for p in pieces if not p[1]["non_resident"]]
        if resident:
            rec, d = resident[0]
            raw = self.fixed_record(rec["offset"], rec)
            return raw[d["value_offset"]:d["value_offset"] + d["value_length"]]
        pieces.sort(key=lambda p: p[1]["lowest_vcn"])
        first = pieces[0][1]
        if first["lowest_vcn"] != 0:
            return None
        runs = []
        next_vcn = 0
        for _, d in pieces:
            if d["lowest_vcn"] != next_vcn:
                return None
            runs += d["runs"]
            next_vcn += sum(r["length"] for r in d["runs"])
        return self.read_runs(inst, runs, first["data_size"], first["initialized_size"])

    def check_content(self, found):
        by_offset = {r["offset"]: r for r in found}
        for e in self.m["entries"]:
            if e["kind"] != "file" or e["sha256"] is None or e["truth"]["content_intact"] is None:
                continue
            where = "file %r of instance %d (record %d)" % (e["path"], e["instance"], e["record"])
            if not e["truth"]["record_intact"] or e["record_offset"] is None:
                continue
            data = self.rebuild_content(e, by_offset)
            digest = hashlib.sha256(data).hexdigest() if data is not None else None
            if e["truth"]["content_intact"]:
                self.checked["content"] += 1
                if data is None:
                    self.problem("%s: cannot be rebuilt from the decoded records" % where)
                elif digest != e["sha256"]:
                    self.problem("%s: content rebuilt from the decoded records has SHA-256 %s, manifest says %s"
                                 " (%d bytes, manifest %d)" % (where, digest, e["sha256"], len(data), e["size"]))
            else:
                self.checked["content_broken"] += 1
                if digest == e["sha256"]:
                    self.problem("%s: the manifest says the content is damaged, but it reads back intact" % where)

    # -------------------------------------------------------------- bitmaps

    def check_bitmaps(self, found):
        by_offset = {r["offset"]: r for r in found}
        for inst in self.m["instances"]:
            if inst["role"] != "current":
                continue
            geo = inst["geometry"]
            cs = geo["cluster_size"]
            rs = geo["record_size"]
            first = geo["mft_extents"][0]

            def record(slot):
                for ext in geo["mft_extents"]:
                    if ext["first_record"] <= slot < ext["first_record"] + ext["records"]:
                        return by_offset.get(ext["offset"] + (slot - ext["first_record"]) * rs)
                return None

            where = "current volume %r" % inst["label"]
            listed = {r["slot"] for r in self.m["records"]
                      if r["instance"] == inst["id"] and r["state"] == "intact" and r["role"] == "base"}
            if not {0, 6} <= listed:
                continue      # the scenario destroyed them on purpose
            mft, bitmap = record(0), record(6)
            if mft is None or bitmap is None or not mft["valid"] or not bitmap["valid"]:
                self.problem("%s: records 0 and 6 are not both decodable" % where)
                continue
            self.eq(where, "record 0 is where the boot sector's MFT LCN says",
                    mft["offset"], geo["volume_offset"] + first["lcn"] * cs)
            # $Bitmap
            d = [x for x in bitmap["data"] if x["name"] == ""][0]
            bits = self.read_runs(inst, d["runs"], d["data_size"], d["initialized_size"])
            want = set()
            for start, length in inst["live_allocated"]:
                lcn = (start - geo["volume_offset"]) // cs
                want.update(range(lcn, lcn + length // cs))
            got = {c for c in range(geo["total_clusters"]) if bits[c // 8] >> (c % 8) & 1}
            self.checked["bitmap"] += 1
            if got != want:
                self.problem("%s: $Bitmap marks %d clusters, the manifest lists %d as live (difference %s ...)"
                             % (where, len(got), len(want), sorted(got ^ want)[:8]))
            # $MFT:$BITMAP
            attr = [a for a in mft["attributes"] if a["type"] == 0xB0][0]
            bits = self.read_runs(inst, attr["runs"], attr["data_size"], attr["initialized_size"])
            got = {s for s in range(geo["mft_records"]) if bits[s // 8] >> (s % 8) & 1}
            want = set()
            for start, count in inst["live_slots"]:
                want.update(range(start, start + count))
            self.checked["bitmap"] += 1
            if got != want:
                self.problem("%s: the MFT bitmap marks %d slots, the manifest lists %d (difference %s ...)"
                             % (where, len(got), len(want), sorted(got ^ want)[:8]))
            # every record the bitmap marks is in use, and it lies where the $MFT's own runs put it
            mft_data = [x for x in mft["data"] if x["name"] == ""][0]
            runs = list(mft_data["runs"])
            if mft["attributes"] and any(a["type"] == 0x20 for a in mft["attributes"]):
                ext = record(15)
                if ext is not None and ext["valid"]:
                    for x in ext["data"]:
                        if x["name"] == "" and x["non_resident"]:
                            runs += x["runs"]
            self.eq(where, "$MFT runs", [[r["lcn"], r["length"]] for r in runs],
                    [[e["lcn"], e["clusters"]] for e in geo["mft_extents"]])

    # ----------------------------------------------------------------- main

    def run(self):
        doc = {"boot_sectors": [], "file_records": [], "index_blocks": []}
        key = {"BOOT": "boot_sectors", "FILE": "file_records", "INDX": "index_blocks"}
        for item in self.ins.scan_image(self.image_path):
            doc[key[item["kind"]]].append(item)
        # round-trip through JSON so that the comparison sees what --json prints
        doc = json.loads(json.dumps(doc))
        self.check_boot(doc["boot_sectors"])
        self.check_records(doc["file_records"])
        self.check_index_blocks(doc["index_blocks"])
        self.check_content(doc["file_records"])
        self.check_bitmaps(doc["file_records"])
        return doc


def main(argv):
    if len(argv) != 4:
        sys.stderr.write(__doc__)
        return 2
    inspector = load_inspector(argv[1])
    with open(argv[3], "r", encoding="utf-8") as f:
        manifest = json.load(f)
    checker = Checker(inspector, argv[2], manifest)
    checker.run()
    c = checker.checked
    print("%s: %d boot sectors, %d FILE records, %d INDX blocks, %d fields, %d files rebuilt, "
          "%d damaged files confirmed, %d bitmaps"
          % (manifest["scenario"], c["boot"], c["file"], c["indx"], c["fields"], c["content"],
             c["content_broken"], c["bitmap"]))
    for p in checker.problems[:MAX_REPORT]:
        print("  MISMATCH " + p)
    if len(checker.problems) > MAX_REPORT:
        print("  ... and %d more" % (len(checker.problems) - MAX_REPORT))
    return 1 if checker.problems else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
