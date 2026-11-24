#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
inspect-ntfs.py - dump the NTFS structures found in a disk or volume image.

This tool shares no code with HDSavior.  It exists so that the authors of the
engine's tests can cross-check what their parser sees against an independent
reading of the same bytes.  It needs nothing but the Python 3 standard library
(3.8 or newer).

What it does
------------
The image is read sequentially and every 512-byte offset is tested for

  FILE  an MFT file record            (magic "FILE" at +0)
  INDX  a directory index block       (magic "INDX" at +0)
  BOOT  an NTFS boot sector           ("NTFS    " at +3 and 55 AA at +510)

Nothing is skipped: a hit inside the bytes of an earlier hit is reported too,
and a hit whose header makes no sense is reported with "valid": false and the
reason, so that the list is complete for every sector-aligned magic.  No
partition table, boot sector or $MFT runlist is needed or used; every item is
decoded from its own bytes only, which is exactly the situation HDSavior is in
after a quick format.

    inspect-ntfs.py IMAGE                    human readable listing
    inspect-ntfs.py --json IMAGE             one JSON document (schema below)
    inspect-ntfs.py --only file --record 5 --record 64 IMAGE
    inspect-ntfs.py --start 1048576 --length 67108864 IMAGE
    inspect-ntfs.py --summary IMAGE          counts only

JSON output (--json)
--------------------
{
  "tool": "inspect-ntfs", "format": 1,
  "image": "<path>", "image_size": <bytes>,
  "scan_start": <byte>, "scan_end": <byte>,          range actually scanned
  "counts": {"boot_sectors": n, "file_records": n, "index_blocks": n, "invalid": n},
  "boot_sectors": [BOOT, ...],                       each list in ascending offset order
  "file_records": [FILE, ...],
  "index_blocks": [INDX, ...]
}

Offsets called "offset" at the top level of an item are absolute byte offsets
in the image.  Offsets inside an item (attributes, index entries) are relative
to the start of that record or block.  All timestamps are raw NTFS FILETIME
integers: 100 ns ticks since 1601-01-01 00:00:00 UTC
(unix_ns = (filetime - 116444736000000000) * 100).

BOOT  {"kind": "BOOT", "offset", "valid", "errors": [...],
       "bytes_per_sector", "sectors_per_cluster_raw" (the u8 at 0x0D),
       "sectors_per_cluster", "cluster_size", "hidden_sectors",
       "sectors_per_track", "heads", "media_type",
       "total_sectors" (the volume is this many sectors; the partition has one
       more, which holds the backup boot sector), "volume_size" (bytes),
       "mft_lcn", "mftmirr_lcn",
       "clusters_per_mft_record_raw" (the s8 at 0x40), "mft_record_size",
       "clusters_per_index_block_raw" (the s8 at 0x44), "index_block_size",
       "serial" (16 hex digits, the le64 at 0x48 printed as a number)}

FILE  {"kind": "FILE", "offset", "valid", "errors": [...],
       "record_size" (bytes allocated, 0x1C), "bytes_in_use",
       "usa_offset", "usa_count", "usn", "fixup_ok",
       "fixup_mismatch_sectors": [index of every 512-byte block whose last two
                                  bytes did not equal the usn],
       "lsn", "sequence", "link_count", "attrs_offset",
       "flags" (raw u16), "in_use", "is_directory",
       "base_record", "base_sequence" (both 0 for a base record),
       "next_attr_id",
       "record_number" (u32 at 0x2C; null when the header is the short
                        pre-NTFS-3.1 form, i.e. usa_offset < 0x30),
       "mft_origin" (offset - record_number * record_size; null without number),
       "names": [NAME, ...]          every $FILE_NAME attribute, in on-disk order
       "data": [STREAM, ...]         every $DATA attribute, in on-disk order
       "attributes": [ATTR, ...]}    every attribute, in on-disk order

NAME  {"name", "namespace" (0 POSIX, 1 WIN32, 2 DOS, 3 WIN32+DOS),
       "namespace_name", "parent_record", "parent_sequence",
       "created", "modified", "mft_changed", "accessed",
       "allocated_size", "data_size", "file_attributes"}

STREAM {"name" ("" for the unnamed stream), "non_resident", "flags",
        "compressed", "encrypted", "sparse", "attr_id",
        resident:      "value_offset" (within the record), "value_length"
        non-resident:  "lowest_vcn", "highest_vcn", "compression_unit",
                       "allocated_size", "data_size", "initialized_size",
                       "compressed_size" (null unless the 72-byte header form),
                       "runs": [{"vcn", "lcn" (null = sparse hole), "length"}],
                       "runlist_ok"}
        The three sizes are only meaningful in the piece with lowest_vcn 0.

ATTR  {"type", "type_name", "offset", "length", "non_resident", "name",
       "flags", "attr_id", then either the resident fields ("value_offset"
       relative to the record, "value_length", "resident_flags") or the
       non-resident fields listed under STREAM and "mapping_pairs_offset",
       plus one decoded member depending on the type:
         0x10 "standard_information": {"created", "modified", "mft_changed",
              "accessed", "file_attributes", and when the value has the 72-byte
              NTFS 3.x form: "owner_id", "security_id", "quota_charged", "usn"}
         0x20 "attribute_list": [{"type", "type_name", "name", "lowest_vcn",
              "record", "sequence", "attr_id"}]   (resident lists only)
         0x30 "file_name": NAME
         0x60 "volume_name": "<label>"
         0x70 "volume_information": {"major", "minor", "flags"}
         0x90 "index_root": {"indexed_type", "collation", "index_block_size",
              "clusters_per_index_block_raw", "entries_offset", "index_length",
              "allocated_size", "node_flags", "entries": [ENTRY, ...]}
         0xB0 "bitmap_hex" (resident bitmaps only)}

INDX  {"kind": "INDX", "offset", "valid", "errors": [...],
       "block_size", "usa_offset", "usa_count", "usn", "fixup_ok",
       "fixup_mismatch_sectors", "lsn",
       "vcn" (index_block_vcn at 0x10; unit: clusters when the cluster size is
              <= the index block size, otherwise 512-byte sectors),
       "entries_offset", "index_length", "allocated_size" (all three relative
       to 0x18), "node_flags", "is_leaf",
       "entries": [ENTRY, ...]}

ENTRY {"offset" (within the block or record), "length", "key_length",
       "flags", "is_last", "has_subnode", "subnode_vcn" (null without one),
       "record", "sequence" (the indexed file; null for the end entry),
       "file_name": NAME or null when the key is not a valid $FILE_NAME (index
       blocks of $Secure, $ObjId, $Quota and $Reparse carry other keys),
       "key_hex" (only for such foreign keys, first 64 bytes)}

Structure reference and verification
------------------------------------
Field offsets follow /usr/include/ntfs-3g/layout.h (package ntfs-3g-dev,
libntfs-3g 2022.10.3).  The comments below say how each field was checked
against real bytes written by mkntfs and ntfs-3g:

  [v]  compared with the output of "ntfsinfo -v" / "ntfsinfo -m".
       make-fixtures.sh repeats that comparison on every run for about a
       hundred records per fixture (every header and attribute field, all
       runlists, all names, index roots and INDX blocks).
  [x]  has no counterpart in ntfsinfo's output but is cross-checked by
       make-fixtures.sh against other data on every fixture: sequence numbers
       in references against the referenced record's own header, index
       entries against the driver's directory listing, timestamps to the
       100 ns tick against the values set through the driver, file content
       read through the decoded runlists against the SHA-256 obtained through
       the mounted driver (every live file of every fixture).
  [l]  decoded as layout.h describes; nothing to compare it with.
"""

import argparse
import json
import os
import struct
import sys

SECTOR = 512            # NTFS_BLOCK_SIZE: scan granularity and fixup stride
FORMAT = 1
EPOCH_DIFF = 116444736000000000   # FILETIME ticks between 1601-01-01 and 1970-01-01

ATTR_NAMES = {
    0x10: "$STANDARD_INFORMATION", 0x20: "$ATTRIBUTE_LIST", 0x30: "$FILE_NAME",
    0x40: "$OBJECT_ID", 0x50: "$SECURITY_DESCRIPTOR", 0x60: "$VOLUME_NAME",
    0x70: "$VOLUME_INFORMATION", 0x80: "$DATA", 0x90: "$INDEX_ROOT",
    0xA0: "$INDEX_ALLOCATION", 0xB0: "$BITMAP", 0xC0: "$REPARSE_POINT",
    0xD0: "$EA_INFORMATION", 0xE0: "$EA", 0xF0: "$PROPERTY_SET",
    0x100: "$LOGGED_UTILITY_STREAM",
}
NAMESPACE_NAMES = {0: "POSIX", 1: "WIN32", 2: "DOS", 3: "WIN32+DOS"}

MFT_RECORD_IN_USE = 0x0001
MFT_RECORD_IS_DIRECTORY = 0x0002
ATTR_IS_COMPRESSED = 0x0001      # ATTR_COMPRESSION_MASK is 0x00ff
ATTR_IS_ENCRYPTED = 0x4000
ATTR_IS_SPARSE = 0x8000
INDEX_ENTRY_NODE = 0x0001
INDEX_ENTRY_END = 0x0002


# --------------------------------------------------------------------------
# little helpers
# --------------------------------------------------------------------------

def u16(b, o):
    return struct.unpack_from("<H", b, o)[0]


def u32(b, o):
    return struct.unpack_from("<I", b, o)[0]


def u64(b, o):
    return struct.unpack_from("<Q", b, o)[0]


def s64(b, o):
    return struct.unpack_from("<q", b, o)[0]


def s8(b, o):
    return struct.unpack_from("<b", b, o)[0]


def split_mref(v):
    """MFT reference -> (48-bit record number, 16-bit sequence number)."""
    return v & 0x0000FFFFFFFFFFFF, (v >> 48) & 0xFFFF


def is_pow2(n):
    return n > 0 and (n & (n - 1)) == 0


def utf16(b):
    """Decode UTF-16LE; unpaired surrogates survive (NTFS does not forbid them)."""
    return b.decode("utf-16-le", errors="surrogatepass")


def filetime_iso(ft):
    """FILETIME -> 'YYYY-MM-DDTHH:MM:SS.fffffffZ' (None if out of range)."""
    import datetime
    if ft is None or ft < 0:
        return None
    try:
        dt = datetime.datetime(1601, 1, 1) + datetime.timedelta(microseconds=ft // 10)
    except OverflowError:
        return None
    return dt.strftime("%Y-%m-%dT%H:%M:%S") + ".%07dZ" % (ft % 10000000)


# --------------------------------------------------------------------------
# boot sector  (layout.h: NTFS_BOOT_SECTOR, BIOS_PARAMETER_BLOCK)
# --------------------------------------------------------------------------

def looks_like_boot_sector(b):
    return len(b) >= 512 and b[3:11] == b"NTFS    " and b[510:512] == b"\x55\xaa"


def parse_boot_sector(b, offset=None):
    """Decode a 512-byte NTFS boot sector; None if the signature is absent."""
    if not looks_like_boot_sector(b):
        return None
    errors = []
    bps = u16(b, 0x0B)                    # [v] ntfsinfo -m "Sector Size"
    spc_raw = b[0x0D]                     # [v] via "Cluster Size"
    # Values up to 0x80 are a sector count.  Larger values are a negative
    # shift: 0xF4 (-12) means 2^12 sectors.  Verified on mkntfs volumes with
    # 128 KiB .. 2 MiB clusters (raw 0xF8 .. 0xF4); 64 KiB is still 0x80.
    if spc_raw > 0x80:
        spc = 1 << (256 - spc_raw)
    else:
        spc = spc_raw
    if not (is_pow2(bps) and 256 <= bps <= 4096):
        errors.append("bytes_per_sector %d is not a power of two in 256..4096" % bps)
    # libntfs-3g accepts 1, 2, 4 .. 128 and the shift codes 0xF0 .. 0xFD.
    if not (is_pow2(spc) and (spc_raw <= 0x80 or 0xF0 <= spc_raw <= 0xFD)):
        errors.append("sectors_per_cluster raw value 0x%02x is not valid" % spc_raw)
    cluster = bps * spc
    total = s64(b, 0x28)                  # [v] "Volume Size in Clusters"; [x] partition sectors - 1
    mft_lcn = s64(b, 0x30)                # [v] "LCN of Data Attribute for FILE_MFT"
    mirr_lcn = s64(b, 0x38)               # [v] "LCN of Data Attribute for File_MFTMirr"
    cpm = s8(b, 0x40)                     # [v] via "MFT Record Size"
    cpi = s8(b, 0x44)                     # [v] via "Index Block Size"
    # positive: number of clusters; negative: 2^(-value) bytes
    rec_size = cpm * cluster if cpm > 0 else (1 << -cpm if -cpm < 31 else 0)
    idx_size = cpi * cluster if cpi > 0 else (1 << -cpi if -cpi < 31 else 0)
    if cpm == 0 or not is_pow2(rec_size):
        errors.append("clusters_per_mft_record %d gives no usable record size" % cpm)
    if cpi == 0 or not is_pow2(idx_size):
        errors.append("clusters_per_index_block %d gives no usable block size" % cpi)
    if total <= 0:
        errors.append("number_of_sectors %d" % total)
    elif cluster and not errors:
        clusters = (total * bps) // cluster
        if not (0 <= mft_lcn < clusters):
            errors.append("mft_lcn %d outside the volume (%d clusters)" % (mft_lcn, clusters))
        if not (0 <= mirr_lcn < clusters):
            errors.append("mftmirr_lcn %d outside the volume (%d clusters)" % (mirr_lcn, clusters))
    return {
        "kind": "BOOT",
        "offset": offset,
        "valid": not errors,
        "errors": errors,
        "bytes_per_sector": bps,
        "sectors_per_cluster_raw": spc_raw,
        "sectors_per_cluster": spc,
        "cluster_size": cluster,
        "sectors_per_track": u16(b, 0x18),     # [x] equals mkntfs -S
        "heads": u16(b, 0x1A),                 # [x] equals mkntfs -H
        "hidden_sectors": u32(b, 0x1C),        # [x] equals mkntfs -p (the partition's start sector)
        "media_type": b[0x15],                 # [l] 0xf8
        "total_sectors": total,
        "volume_size": total * bps if total > 0 else 0,
        "mft_lcn": mft_lcn,
        "mftmirr_lcn": mirr_lcn,
        "clusters_per_mft_record_raw": cpm,
        "mft_record_size": rec_size,
        "clusters_per_index_block_raw": cpi,
        "index_block_size": idx_size,
        "serial": "%016X" % u64(b, 0x48),      # [v] the UUID blkid reports
    }


# --------------------------------------------------------------------------
# multi-sector transfer protection ("fixups")  (layout.h: NTFS_RECORD)
# --------------------------------------------------------------------------

def apply_fixups(buf, usa_ofs, usa_count):
    """Undo the update sequence array in place.

    buf is a bytearray holding the whole record.  The array has usa_count
    le16 entries at usa_ofs: the update sequence number, then one saved value
    per 512-byte block.  The last two bytes of every 512-byte block must equal
    the number and are replaced by the saved value.  The stride is 512 bytes
    whatever the sector size of the volume is ([v]: a 4096-byte record on a
    4096-byte-sector volume has usa_count 9).

    Returns (usn, [indexes of blocks whose tail did not match]).
    """
    usn = bytes(buf[usa_ofs:usa_ofs + 2])
    bad = []
    for i in range(1, usa_count):
        end = i * SECTOR
        if end > len(buf):
            bad.append(i - 1)
            continue
        if bytes(buf[end - 2:end]) != usn:
            bad.append(i - 1)
        else:
            buf[end - 2:end] = buf[usa_ofs + 2 * i:usa_ofs + 2 * i + 2]
    return struct.unpack("<H", usn)[0], bad


# --------------------------------------------------------------------------
# runlists ("mapping pairs")
# --------------------------------------------------------------------------

def decode_runlist(b, start, end, lowest_vcn=0):
    """Decode the mapping pairs in b[start:end].

    Each run starts with a header byte: low nibble = byte count of the length
    field, high nibble = byte count of the offset field.  The length is
    unsigned.  The offset is signed and relative to the LCN of the previous
    run that had one; an offset field of size 0 means a sparse hole (no LCN).
    A header byte of 0 ends the list.               [v] against ntfsinfo -v

    Returns (runs, ok, error) with runs = [{"vcn", "lcn" or None, "length"}].
    """
    runs = []
    pos = start
    vcn = lowest_vcn
    lcn = 0
    while True:
        if pos >= end:
            return runs, False, "runlist is not terminated inside the attribute"
        head = b[pos]
        pos += 1
        if head == 0:
            return runs, True, None
        len_size = head & 0x0F
        off_size = head >> 4
        if len_size == 0 or len_size > 8 or off_size > 8:
            return runs, False, "bad run header 0x%02x" % head
        if pos + len_size + off_size > end:
            return runs, False, "run crosses the end of the attribute"
        length = int.from_bytes(b[pos:pos + len_size], "little", signed=False)
        pos += len_size
        if length == 0:
            return runs, False, "zero-length run"
        if off_size == 0:
            runs.append({"vcn": vcn, "lcn": None, "length": length})
        else:
            delta = int.from_bytes(b[pos:pos + off_size], "little", signed=True)
            pos += off_size
            lcn += delta
            if lcn < 0:
                return runs, False, "negative LCN"
            runs.append({"vcn": vcn, "lcn": lcn, "length": length})
        vcn += length


# --------------------------------------------------------------------------
# attribute values
# --------------------------------------------------------------------------

def parse_file_name_value(v):
    """Decode a $FILE_NAME value (layout.h: FILE_NAME_ATTR); None if malformed."""
    if len(v) < 0x42:
        return None
    name_len = v[0x40]
    namespace = v[0x41]
    if namespace > 3 or name_len == 0 or 0x42 + 2 * name_len > len(v):
        return None
    parent, parent_seq = split_mref(u64(v, 0))
    return {
        "name": utf16(bytes(v[0x42:0x42 + 2 * name_len])),   # [v] "Filename"
        "namespace": namespace,                              # [v] "Namespace"
        "namespace_name": NAMESPACE_NAMES[namespace],
        "parent_record": parent,                             # [v] "Parent directory"
        "parent_sequence": parent_seq,                       # [x] equals the parent record's sequence
        "created": s64(v, 0x08),                             # [v] the four times, to the second
        "modified": s64(v, 0x10),
        "mft_changed": s64(v, 0x18),
        "accessed": s64(v, 0x20),
        "allocated_size": s64(v, 0x28),                      # [v] "Allocated Size"
        "data_size": s64(v, 0x30),                           # [v] "Data Size"
        "file_attributes": u32(v, 0x38),                     # [v] "File attributes"
    }


def parse_index_entries(b, start, end):
    """Walk index entries in b[start:end] (layout.h: INDEX_ENTRY).

    Returns (entries, error).  The walk stops at the entry flagged
    INDEX_ENTRY_END, which carries no key (but may carry a sub-node VCN).
    """
    entries = []
    pos = start
    while True:
        if pos + 16 > end:
            return entries, "index entries run past the used size without an end entry"
        length = u16(b, pos + 8)          # [v] "Entry length"
        key_len = u16(b, pos + 10)        # [v] "Key length"
        flags = u16(b, pos + 12)          # [v] "Index entry flags"
        if length < 16 or length % 8 or pos + length > end:
            return entries, "index entry at +%d has bad length %d" % (pos, length)
        e = {
            "offset": pos,
            "length": length,
            "key_length": key_len,
            "flags": flags,
            "is_last": bool(flags & INDEX_ENTRY_END),
            "has_subnode": bool(flags & INDEX_ENTRY_NODE),
            "subnode_vcn": None,
            "record": None,
            "sequence": None,
            "file_name": None,
        }
        if flags & INDEX_ENTRY_NODE:
            if length < 24:
                return entries, "index entry at +%d too short for a sub-node VCN" % pos
            e["subnode_vcn"] = s64(b, pos + length - 8)      # [v] "Subnode VCN"
        if not flags & INDEX_ENTRY_END:
            if 16 + key_len > length:
                return entries, "index entry at +%d: key does not fit" % pos
            rec, seq = split_mref(u64(b, pos))               # [v] "FILE record number"; [x] sequence
            key = bytes(b[pos + 16:pos + 16 + key_len])
            fn = parse_file_name_value(key)
            # A real $FILE_NAME key is exactly as long as its name needs.
            if fn is not None and key_len == 0x42 + 2 * key[0x40]:
                e["record"], e["sequence"], e["file_name"] = rec, seq, fn
            else:
                e["key_hex"] = key[:64].hex()
        entries.append(e)
        if flags & INDEX_ENTRY_END:
            return entries, None
        pos += length


def parse_attribute_list_value(v):
    """Decode a resident $ATTRIBUTE_LIST value (layout.h: ATTR_LIST_ENTRY)."""
    out = []
    pos = 0
    while pos + 26 <= len(v):
        atype = u32(v, pos)
        length = u16(v, pos + 4)
        name_len = v[pos + 6]
        name_ofs = v[pos + 7]
        if length < 26 or pos + length > len(v):
            break
        rec, seq = split_mref(u64(v, pos + 16))
        out.append({
            "type": atype,
            "type_name": ATTR_NAMES.get(atype, "0x%x" % atype),
            "name": utf16(bytes(v[pos + name_ofs:pos + name_ofs + 2 * name_len])),
            "lowest_vcn": s64(v, pos + 8),       # [v] "Starting VCN"
            "record": rec,                       # [v] "MFT reference"
            "sequence": seq,                     # [x] equals the holder record's sequence
            "attr_id": u16(v, pos + 24),         # [v] "Instance"
        })
        pos += length
    return out


def parse_attributes(rec, attrs_offset, bytes_in_use):
    """Walk the attribute records of an MFT record (layout.h: ATTR_RECORD)."""
    attrs = []
    errors = []
    pos = attrs_offset
    limit = min(bytes_in_use, len(rec))
    while True:
        if pos + 4 > limit:
            errors.append("attribute list runs past bytes_in_use without an end marker")
            break
        atype = u32(rec, pos)
        if atype == 0xFFFFFFFF:           # AT_END
            break
        if pos + 16 > limit:
            errors.append("attribute header at +%d is cut off" % pos)
            break
        length = u32(rec, pos + 4)        # [v] "Attribute length"
        if length < 16 or length % 8 or pos + length > limit:
            errors.append("attribute 0x%x at +%d has bad length %d" % (atype, pos, length))
            break
        non_res = rec[pos + 8]            # [v] "Resident"
        name_len = rec[pos + 9]           # [v] "Name length"
        name_ofs = u16(rec, pos + 10)     # [v] "Name offset"
        flags = u16(rec, pos + 12)        # [v] "Attribute flags"
        a = {
            "type": atype,
            "type_name": ATTR_NAMES.get(atype, "0x%x" % atype),
            "offset": pos,
            "length": length,
            "non_resident": bool(non_res),
            "name": "",
            "flags": flags,
            "attr_id": u16(rec, pos + 14),    # [v] "Attribute instance"
        }
        if name_len:
            if name_ofs + 2 * name_len > length:
                errors.append("attribute 0x%x at +%d: name does not fit" % (atype, pos))
                break
            a["name"] = utf16(bytes(rec[pos + name_ofs:pos + name_ofs + 2 * name_len]))
        if non_res == 0:
            if length < 24:
                errors.append("resident attribute 0x%x at +%d is too short" % (atype, pos))
                break
            vlen = u32(rec, pos + 16)     # [v] "Data size"
            vofs = u16(rec, pos + 20)     # [v] "Data offset"
            a["value_offset"] = pos + vofs    # relative to the record
            a["value_length"] = vlen
            a["resident_flags"] = rec[pos + 22]   # [v] "Resident flags"
            if vofs + vlen > length:
                errors.append("resident attribute 0x%x at +%d: value does not fit" % (atype, pos))
                attrs.append(a)
                pos += length
                continue
            value = bytes(rec[pos + vofs:pos + vofs + vlen])
            decode_resident_value(a, atype, value, pos + vofs, rec, errors)
        elif non_res == 1:
            if length < 64:
                errors.append("non-resident attribute 0x%x at +%d is too short" % (atype, pos))
                break
            lowest = s64(rec, pos + 16)   # [v] "Lowest VCN"
            highest = s64(rec, pos + 24)  # [v] "Highest VCN"
            mp_ofs = u16(rec, pos + 32)   # [v] "Mapping pairs offset"
            a["lowest_vcn"] = lowest
            a["highest_vcn"] = highest
            a["mapping_pairs_offset"] = mp_ofs
            a["compression_unit"] = rec[pos + 34]         # [v] "Compression unit"
            a["allocated_size"] = s64(rec, pos + 40)      # [v] "Allocated size"
            a["data_size"] = s64(rec, pos + 48)           # [v] "Data size"
            a["initialized_size"] = s64(rec, pos + 56)    # [v] "Initialized size"
            # The 72-byte header form (compressed or sparse attributes) is
            # recognised by where the name or the mapping pairs begin.  [v]
            first = min(mp_ofs, name_ofs) if name_len else mp_ofs
            a["compressed_size"] = s64(rec, pos + 64) if first >= 72 and length >= 72 else None
            if mp_ofs < 64 or mp_ofs > length:
                errors.append("attribute 0x%x at +%d: bad mapping pairs offset" % (atype, pos))
                a["runs"], a["runlist_ok"] = [], False
            else:
                runs, ok, err = decode_runlist(rec, pos + mp_ofs, pos + length, lowest)
                a["runs"] = runs
                a["runlist_ok"] = ok
                if err:
                    errors.append("attribute 0x%x at +%d: %s" % (atype, pos, err))
                elif runs and highest >= lowest:
                    covered = runs[-1]["vcn"] + runs[-1]["length"] - lowest
                    if covered != highest - lowest + 1:
                        a["runlist_ok"] = False
                        errors.append("attribute 0x%x at +%d: runs cover %d clusters, header says %d"
                                      % (atype, pos, covered, highest - lowest + 1))
        else:
            errors.append("attribute 0x%x at +%d: non_resident flag is %d" % (atype, pos, non_res))
            break
        attrs.append(a)
        pos += length
    return attrs, errors


def decode_resident_value(a, atype, value, value_pos, rec, errors):
    """Add the type-specific decoded member to attribute dict a."""
    if atype == 0x10:                                     # $STANDARD_INFORMATION
        if len(value) >= 36:
            si = {
                "created": s64(value, 0),                 # [v] "File Creation Time"   [x] to the tick
                "modified": s64(value, 8),                # [v] "File Altered Time"    [x] to the tick
                "mft_changed": s64(value, 16),            # [v] "MFT Changed Time"
                "accessed": s64(value, 24),               # [v] "Last Accessed Time"   [x] to the tick
                "file_attributes": u32(value, 32),        # [v] "File attributes"
            }
            # ntfs-3g writes the 48-byte NTFS 1.2 form for user files; Windows
            # and mkntfs' system files use the 72-byte form.            [v]
            if len(value) >= 72:
                si["owner_id"] = u32(value, 48)
                si["security_id"] = u32(value, 52)
                si["quota_charged"] = u64(value, 56)
                si["usn"] = u64(value, 64)
            a["standard_information"] = si
        else:
            errors.append("$STANDARD_INFORMATION value is only %d bytes" % len(value))
    elif atype == 0x20:                                   # $ATTRIBUTE_LIST
        a["attribute_list"] = parse_attribute_list_value(value)
    elif atype == 0x30:                                   # $FILE_NAME
        fn = parse_file_name_value(value)
        if fn is None:
            errors.append("$FILE_NAME at +%d is malformed" % a["offset"])
        a["file_name"] = fn
    elif atype == 0x60:                                   # $VOLUME_NAME
        a["volume_name"] = utf16(value[:len(value) & ~1])  # [v] "Volume Name"
    elif atype == 0x70:                                   # $VOLUME_INFORMATION
        if len(value) >= 12:
            a["volume_information"] = {"major": value[8], "minor": value[9],   # [v] "Volume Version"
                                       "flags": u16(value, 10)}
    elif atype == 0x90:                                   # $INDEX_ROOT
        if len(value) >= 32:
            ih = 16                                       # INDEX_HEADER inside INDEX_ROOT
            entries_ofs = u32(value, ih)                  # [v] "Entries Offset"
            index_len = u32(value, ih + 4)                # [v] "Index Size"
            root = {
                "indexed_type": u32(value, 0),            # [v] 0x30 for directories
                "collation": u32(value, 4),               # [v] "Collation Rule"
                "index_block_size": u32(value, 8),        # [v] "Index Block Size"
                "clusters_per_index_block_raw": s8(value, 12),   # [v] "Clusters Per Block"
                "entries_offset": entries_ofs,
                "index_length": index_len,
                "allocated_size": u32(value, ih + 8),     # [v] "Allocated Size"
                "node_flags": value[ih + 12],             # [v] "Index header flags"
                "entries": [],
            }
            if ih + index_len <= len(value) and entries_ofs >= 16:
                entries, err = parse_index_entries(value, ih + entries_ofs, ih + index_len)
                for e in entries:                         # make offsets record-relative
                    e["offset"] += value_pos
                root["entries"] = entries
                if err:
                    errors.append("$INDEX_ROOT: " + err)
            else:
                errors.append("$INDEX_ROOT: header sizes do not fit the value")
            a["index_root"] = root
    elif atype == 0xB0:                                   # $BITMAP
        a["bitmap_hex"] = value.hex()


# --------------------------------------------------------------------------
# FILE records  (layout.h: MFT_RECORD)
# --------------------------------------------------------------------------

def file_record_size(first_sector):
    """Record size announced by the header of a FILE record, or None.

    Only the first 512 bytes are needed.  Accepts any power of two from
    1024 to 65536 with a matching update sequence count.
    """
    if len(first_sector) < 48 or first_sector[0:4] != b"FILE":
        return None
    size = u32(first_sector, 0x1C)
    if not is_pow2(size) or not 1024 <= size <= 65536:
        return None
    return size


def parse_file_record(raw, offset=None):
    """Decode one MFT record from its raw (still fixed-up) bytes.

    raw must hold at least the whole record.  Returns None when raw does not
    start with "FILE"; otherwise a dict (see the module docstring) whose
    "valid" member tells whether the header made sense.
    """
    if len(raw) < 48 or raw[0:4] != b"FILE":
        return None
    usa_ofs = u16(raw, 4)                 # [v] "Upd. Seq. Array Off."
    usa_count = u16(raw, 6)               # [v] "Upd. Seq. Array Count"
    size = u32(raw, 0x1C)                 # [v] "Bytes Allocated"
    rec = {
        "kind": "FILE",
        "offset": offset,
        "valid": False,
        "errors": [],
        "record_size": size,
        "bytes_in_use": u32(raw, 0x18),   # [v] "Bytes Used"
        "usa_offset": usa_ofs,
        "usa_count": usa_count,
        "usn": None,
        "fixup_ok": False,
        "fixup_mismatch_sectors": [],
        "lsn": u64(raw, 8),               # [v] "LogFile Seq. Number" (0 on ntfs-3g volumes)
        "sequence": u16(raw, 0x10),       # [v] "MFT Record Seq. Numb."
        "link_count": u16(raw, 0x12),     # [v] "Number of Hard Links"
        "attrs_offset": u16(raw, 0x14),   # [v] "Attribute Offset"
        "flags": u16(raw, 0x16),          # [v] "MFT Record Flags"
        "in_use": bool(u16(raw, 0x16) & MFT_RECORD_IN_USE),
        "is_directory": bool(u16(raw, 0x16) & MFT_RECORD_IS_DIRECTORY),
        "base_record": None,
        "base_sequence": None,
        "next_attr_id": u16(raw, 0x28),   # [v] "Next Attribute Instance"
        "record_number": None,
        "mft_origin": None,
        "names": [],
        "data": [],
        "attributes": [],
    }
    base, base_seq = split_mref(u64(raw, 0x20))   # [x] base record / sequence of extension records
    rec["base_record"], rec["base_sequence"] = base, base_seq
    errors = rec["errors"]
    if not is_pow2(size) or not 1024 <= size <= 65536:
        errors.append("bytes_allocated %d is not a power of two in 1024..65536" % size)
        return rec
    if len(raw) < size:
        errors.append("only %d of %d bytes available" % (len(raw), size))
        return rec
    if usa_count != size // SECTOR + 1:
        errors.append("usa_count %d does not match record size %d" % (usa_count, size))
        return rec
    if usa_ofs < 42 or usa_ofs + 2 * usa_count > SECTOR - 2:
        errors.append("usa_offset %d out of range" % usa_ofs)
        return rec
    buf = bytearray(raw[:size])
    usn, bad = apply_fixups(buf, usa_ofs, usa_count)
    rec["usn"] = usn                      # [v] "Upd. Seq. Number"
    rec["fixup_mismatch_sectors"] = bad
    rec["fixup_ok"] = not bad
    if bad:
        errors.append("fixup mismatch in 512-byte block(s) %s" % bad)
    # NTFS 3.1+ stores the record's own number at 0x2C; the update sequence
    # array then starts at 0x30.  The older 42-byte header has no number.
    if usa_ofs >= 0x30:
        rec["record_number"] = u32(buf, 0x2C)     # [v] against "Dumping Inode N"
        if offset is not None:
            rec["mft_origin"] = offset - rec["record_number"] * size
    attrs_ofs = rec["attrs_offset"]
    in_use = rec["bytes_in_use"]
    if attrs_ofs < usa_ofs + 2 * usa_count or attrs_ofs % 8 or attrs_ofs >= size:
        errors.append("attrs_offset %d out of range" % attrs_ofs)
        return rec
    if in_use > size or in_use < attrs_ofs + 4:
        errors.append("bytes_in_use %d out of range" % in_use)
        return rec
    attrs, aerrors = parse_attributes(buf, attrs_ofs, in_use)
    rec["attributes"] = attrs
    errors.extend(aerrors)
    for a in attrs:
        if a["type"] == 0x30 and a.get("file_name"):
            rec["names"].append(a["file_name"])
        elif a["type"] == 0x80:
            d = {
                "name": a["name"],
                "non_resident": a["non_resident"],
                "flags": a["flags"],
                "compressed": bool(a["flags"] & 0x00FF),
                "encrypted": bool(a["flags"] & ATTR_IS_ENCRYPTED),
                "sparse": bool(a["flags"] & ATTR_IS_SPARSE),
                "attr_id": a["attr_id"],
            }
            if a["non_resident"]:
                for k in ("lowest_vcn", "highest_vcn", "compression_unit", "allocated_size",
                          "data_size", "initialized_size", "compressed_size", "runs", "runlist_ok"):
                    d[k] = a.get(k)
            else:
                d["value_offset"] = a.get("value_offset")
                d["value_length"] = a.get("value_length")
            rec["data"].append(d)
    rec["valid"] = not errors or errors == ["fixup mismatch in 512-byte block(s) %s" % bad]
    return rec


# --------------------------------------------------------------------------
# INDX blocks  (layout.h: INDEX_BLOCK, INDEX_HEADER)
# --------------------------------------------------------------------------

def index_block_size(first_sector):
    """Block size announced by the header of an INDX block, or None."""
    if len(first_sector) < 40 or first_sector[0:4] != b"INDX":
        return None
    usa_count = u16(first_sector, 6)
    size = u32(first_sector, 0x20) + 0x18         # allocated size is relative to 0x18
    if not is_pow2(size) or not 512 <= size <= 65536:
        return None
    if usa_count != size // SECTOR + 1:
        return None
    return size


def parse_index_block(raw, offset=None):
    """Decode one INDX block from its raw (still fixed-up) bytes."""
    if len(raw) < 40 or raw[0:4] != b"INDX":
        return None
    usa_ofs = u16(raw, 4)
    usa_count = u16(raw, 6)
    entries_ofs = u32(raw, 0x18)          # [v] "Entries Offset"   (relative to 0x18)
    index_len = u32(raw, 0x1C)            # [v] "Index Size"       (relative to 0x18)
    alloc = u32(raw, 0x20)                # [v] "Allocated Size"   (relative to 0x18)
    size = alloc + 0x18
    blk = {
        "kind": "INDX",
        "offset": offset,
        "valid": False,
        "errors": [],
        "block_size": size,
        "usa_offset": usa_ofs,
        "usa_count": usa_count,
        "usn": None,
        "fixup_ok": False,
        "fixup_mismatch_sectors": [],
        "lsn": u64(raw, 8),
        "vcn": s64(raw, 0x10),            # [v] "Node VCN"; [x] against the block's position
        "entries_offset": entries_ofs,
        "index_length": index_len,
        "allocated_size": alloc,
        "node_flags": raw[0x24],          # [v] "Index header flags" (1 = has sub-nodes)
        "is_leaf": not raw[0x24] & 1,
        "entries": [],
    }
    errors = blk["errors"]
    if not is_pow2(size) or not 512 <= size <= 65536:
        errors.append("allocated_size %d gives no usable block size" % alloc)
        return blk
    if len(raw) < size:
        errors.append("only %d of %d bytes available" % (len(raw), size))
        return blk
    if usa_count != size // SECTOR + 1:
        errors.append("usa_count %d does not match block size %d" % (usa_count, size))
        return blk
    if usa_ofs < 40 or usa_ofs + 2 * usa_count > SECTOR - 2:
        errors.append("usa_offset %d out of range" % usa_ofs)
        return blk
    buf = bytearray(raw[:size])
    usn, bad = apply_fixups(buf, usa_ofs, usa_count)
    blk["usn"] = usn
    blk["fixup_mismatch_sectors"] = bad
    blk["fixup_ok"] = not bad
    if bad:
        errors.append("fixup mismatch in 512-byte block(s) %s" % bad)
    if entries_ofs < 16 or entries_ofs % 8 or index_len > alloc or entries_ofs + 16 > index_len:
        errors.append("index header sizes are inconsistent")
        return blk
    entries, err = parse_index_entries(buf, 0x18 + entries_ofs, 0x18 + index_len)
    blk["entries"] = entries
    if err:
        errors.append(err)
    blk["valid"] = not errors or errors == ["fixup mismatch in 512-byte block(s) %s" % bad]
    return blk


# --------------------------------------------------------------------------
# scanning
# --------------------------------------------------------------------------

def scan_image(path, start=0, length=None, kinds=("BOOT", "FILE", "INDX"), chunk_size=8 << 20):
    """Yield every BOOT / FILE / INDX item of the image in ascending offset order.

    Every offset that is a multiple of 512 (counted from byte 0 of the image)
    inside [start, start + length) is tested.
    """
    fd = os.open(path, os.O_RDONLY)
    try:
        size = os.fstat(fd).st_size
        if size == 0:
            # block devices report 0 through fstat
            size = os.lseek(fd, 0, os.SEEK_END)
        start = (start + SECTOR - 1) // SECTOR * SECTOR
        end = size if length is None else min(size, start + length)
        end = end // SECTOR * SECTOR
        want_boot = "BOOT" in kinds
        want_file = "FILE" in kinds
        want_indx = "INDX" in kinds
        pos = start
        while pos < end:
            n = min(chunk_size, end - pos)
            chunk = os.pread(fd, n, pos)
            if not chunk:
                break
            hits = []
            if want_file:
                i = chunk.find(b"FILE")
                while i != -1:
                    if i % SECTOR == 0:
                        hits.append((i, "FILE"))
                    i = chunk.find(b"FILE", i + 1)
            if want_indx:
                i = chunk.find(b"INDX")
                while i != -1:
                    if i % SECTOR == 0:
                        hits.append((i, "INDX"))
                    i = chunk.find(b"INDX", i + 1)
            if want_boot:
                i = chunk.find(b"NTFS    ")
                while i != -1:
                    if i % SECTOR == 3:
                        hits.append((i - 3, "BOOT"))
                    i = chunk.find(b"NTFS    ", i + 1)
            hits.sort()
            for i, kind in hits:
                off = pos + i
                head = chunk[i:i + SECTOR]
                if len(head) < SECTOR:
                    continue
                if kind == "BOOT":
                    item = parse_boot_sector(head, off)
                    if item is not None:
                        yield item
                    continue
                if kind == "FILE":
                    size_hint = file_record_size(head)
                else:
                    size_hint = index_block_size(head)
                need = size_hint or SECTOR
                raw = chunk[i:i + need]
                if len(raw) < need:                    # crosses the chunk end
                    raw = os.pread(fd, need, off)
                item = parse_file_record(raw, off) if kind == "FILE" else parse_index_block(raw, off)
                if item is not None:
                    yield item
            pos += len(chunk)
    finally:
        os.close(fd)


# --------------------------------------------------------------------------
# text output
# --------------------------------------------------------------------------

def fmt_runs(runs, limit):
    parts = []
    for r in runs[:limit] if limit else runs:
        parts.append(("%d+%d" % (r["lcn"], r["length"])) if r["lcn"] is not None
                     else ("hole+%d" % r["length"]))
    more = len(runs) - len(parts)
    return " ".join(parts) + (" ... (%d more)" % more if more > 0 else "")


def fmt_name(fn):
    return "%s [%s] parent=%d/%d" % (json.dumps(fn["name"], ensure_ascii=False),
                                     fn["namespace_name"], fn["parent_record"], fn["parent_sequence"])


def print_boot(b, out):
    out.write("0x%012x BOOT  sector=%d cluster=%d (raw 0x%02x) total_sectors=%d mft_lcn=%d "
              "mftmirr_lcn=%d mft_record=%d (raw %d) index_block=%d (raw %d) hidden=%d serial=%s%s\n"
              % (b["offset"], b["bytes_per_sector"], b["cluster_size"], b["sectors_per_cluster_raw"],
                 b["total_sectors"], b["mft_lcn"], b["mftmirr_lcn"], b["mft_record_size"],
                 b["clusters_per_mft_record_raw"], b["index_block_size"],
                 b["clusters_per_index_block_raw"], b["hidden_sectors"], b["serial"],
                 "" if b["valid"] else "  INVALID: " + "; ".join(b["errors"])))


def print_file(r, out, verbose, run_limit):
    flags = []
    if r["in_use"]:
        flags.append("IN_USE")
    if r["is_directory"]:
        flags.append("DIR")
    if r["flags"] & ~3:
        flags.append("0x%x" % (r["flags"] & ~3))
    num = "#%d" % r["record_number"] if r["record_number"] is not None else "#?"
    base = "base=%d/%d" % (r["base_record"], r["base_sequence"]) if (r["base_record"] or r["base_sequence"]) else "base=-"
    out.write("0x%012x FILE  %s seq=%d flags=%s links=%d %s used=%d/%d lsn=%d usn=%s fixup=%s%s\n"
              % (r["offset"], num, r["sequence"], "|".join(flags) or "0", r["link_count"], base,
                 r["bytes_in_use"], r["record_size"], r["lsn"], r["usn"],
                 "ok" if r["fixup_ok"] else "BAD",
                 "" if r["valid"] else "  INVALID"))
    for e in r["errors"]:
        out.write("      ! %s\n" % e)
    for a in r["attributes"]:
        t = a["type"]
        label = a["type_name"] + (":" + a["name"] if a["name"] else "")
        if t == 0x30 and a.get("file_name"):
            fn = a["file_name"]
            out.write("      %-22s %s size=%d alloc=%d attrs=0x%x\n"
                      % (label, fmt_name(fn), fn["data_size"], fn["allocated_size"], fn["file_attributes"]))
            if verbose:
                out.write("      %-22s created=%s modified=%s\n"
                          % ("", filetime_iso(fn["created"]), filetime_iso(fn["modified"])))
        elif t == 0x10 and a.get("standard_information"):
            si = a["standard_information"]
            out.write("      %-22s created=%s modified=%s attrs=0x%x (%d bytes)\n"
                      % (label, filetime_iso(si["created"]), filetime_iso(si["modified"]),
                         si["file_attributes"], a["value_length"]))
            if verbose:
                out.write("      %-22s mft_changed=%s accessed=%s\n"
                          % ("", filetime_iso(si["mft_changed"]), filetime_iso(si["accessed"])))
        elif a["non_resident"]:
            extra = ""
            if a["flags"]:
                extra += " flags=0x%04x" % a["flags"]
            if a.get("compressed_size") is not None:
                extra += " compressed_size=%d cu=%d" % (a["compressed_size"], a["compression_unit"])
            out.write("      %-22s non-resident vcn %d..%d size=%d alloc=%d init=%d%s runs(%d): %s\n"
                      % (label, a["lowest_vcn"], a["highest_vcn"], a["data_size"], a["allocated_size"],
                         a["initialized_size"], extra, len(a["runs"]), fmt_runs(a["runs"], run_limit)))
        else:
            extra = ""
            if t == 0x20:
                extra = " entries=%d" % len(a.get("attribute_list", []))
            elif t == 0x60:
                extra = " label=%s" % json.dumps(a.get("volume_name", ""), ensure_ascii=False)
            elif t == 0x70 and a.get("volume_information"):
                extra = " version=%d.%d" % (a["volume_information"]["major"], a["volume_information"]["minor"])
            elif t == 0x90 and a.get("index_root"):
                ir = a["index_root"]
                extra = " index_block=%d node_flags=%d entries=%d" % (
                    ir["index_block_size"], ir["node_flags"],
                    len([e for e in ir["entries"] if not e["is_last"]]))
            out.write("      %-22s resident +%d len=%d%s\n" % (label, a["value_offset"], a["value_length"], extra))
            if t == 0x20 and verbose:
                for e in a.get("attribute_list", []):
                    out.write("          %-22s vcn=%d in #%d/%d id=%d\n"
                              % (e["type_name"] + (":" + e["name"] if e["name"] else ""),
                                 e["lowest_vcn"], e["record"], e["sequence"], e["attr_id"]))
            if t == 0x90 and a.get("index_root"):
                print_entries(a["index_root"]["entries"], out, "          ")


def print_entries(entries, out, indent):
    for e in entries:
        sub = " subnode_vcn=%d" % e["subnode_vcn"] if e["has_subnode"] else ""
        if e["is_last"]:
            out.write("%s<end>%s\n" % (indent, sub))
        elif e["file_name"]:
            fn = e["file_name"]
            out.write("%s#%d/%d %s size=%d attrs=0x%x%s\n"
                      % (indent, e["record"], e["sequence"], fmt_name(fn), fn["data_size"],
                         fn["file_attributes"], sub))
        else:
            out.write("%s<key %d bytes: %s>%s\n" % (indent, e["key_length"], e.get("key_hex", ""), sub))


def print_indx(b, out, with_entries):
    n = len([e for e in b["entries"] if not e["is_last"]])
    out.write("0x%012x INDX  vcn=%d size=%d used=%d/%d %s entries=%d lsn=%d usn=%s fixup=%s%s\n"
              % (b["offset"], b["vcn"], b["block_size"], b["index_length"], b["allocated_size"],
                 "leaf" if b["is_leaf"] else "node", n, b["lsn"], b["usn"],
                 "ok" if b["fixup_ok"] else "BAD", "" if b["valid"] else "  INVALID"))
    for e in b["errors"]:
        out.write("      ! %s\n" % e)
    if with_entries:
        print_entries(b["entries"], out, "      ")


# --------------------------------------------------------------------------
# command line
# --------------------------------------------------------------------------

def main(argv=None):
    ap = argparse.ArgumentParser(
        description="Dump NTFS boot sectors, FILE records and INDX blocks found at any "
                    "512-byte offset of an image, independently of HDSavior.",
        epilog="See the comment at the top of this file for the JSON schema.")
    ap.add_argument("image")
    ap.add_argument("--json", action="store_true", help="emit one JSON document instead of text")
    ap.add_argument("--start", type=int, default=0, metavar="BYTES", help="first byte to scan (default 0)")
    ap.add_argument("--length", type=int, default=None, metavar="BYTES", help="number of bytes to scan")
    ap.add_argument("--only", default="boot,file,indx", metavar="KINDS",
                    help="comma separated subset of boot,file,indx (default: all)")
    ap.add_argument("--record", type=int, action="append", metavar="N",
                    help="only FILE records with this record number (repeatable)")
    ap.add_argument("--summary", action="store_true", help="print counts only")
    ap.add_argument("--no-entries", action="store_true", help="text mode: do not list index entries")
    ap.add_argument("-v", "--verbose", action="store_true",
                    help="text mode: more timestamps, attribute list entries, all runs")
    args = ap.parse_args(argv)

    kinds = tuple(k.strip().upper() for k in args.only.split(",") if k.strip())
    for k in kinds:
        if k not in ("BOOT", "FILE", "INDX"):
            ap.error("unknown kind %r in --only" % k)
    wanted = set(args.record) if args.record else None

    image_size = os.path.getsize(args.image)
    start = (args.start + SECTOR - 1) // SECTOR * SECTOR
    end = image_size if args.length is None else min(image_size, start + args.length)
    counts = {"boot_sectors": 0, "file_records": 0, "index_blocks": 0, "invalid": 0}
    doc = {"tool": "inspect-ntfs", "format": FORMAT, "image": args.image, "image_size": image_size,
           "scan_start": start, "scan_end": end // SECTOR * SECTOR, "counts": counts,
           "boot_sectors": [], "file_records": [], "index_blocks": []}
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8", errors="backslashreplace")
    out = sys.stdout
    text = not args.json and not args.summary
    if text:
        out.write("# %s: %d bytes, scanning %d..%d at every %d-byte offset\n"
                  % (args.image, image_size, start, doc["scan_end"], SECTOR))
    key = {"BOOT": "boot_sectors", "FILE": "file_records", "INDX": "index_blocks"}
    for item in scan_image(args.image, start, args.length, kinds):
        if item["kind"] == "FILE" and wanted is not None and item["record_number"] not in wanted:
            continue
        counts[key[item["kind"]]] += 1
        if not item["valid"]:
            counts["invalid"] += 1
        if args.json:
            doc[key[item["kind"]]].append(item)
        elif text:
            if item["kind"] == "BOOT":
                print_boot(item, out)
            elif item["kind"] == "FILE":
                print_file(item, out, args.verbose, 0 if args.verbose else 8)
            else:
                print_indx(item, out, not args.no_entries)
    if args.json:
        json.dump(doc, out, ensure_ascii=True, separators=(",", ":"))
        out.write("\n")
    else:
        out.write("# boot sectors: %d, FILE records: %d, INDX blocks: %d, invalid: %d\n"
                  % (counts["boot_sectors"], counts["file_records"], counts["index_blocks"],
                     counts["invalid"]))
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except BrokenPipeError:
        sys.exit(0)
