# NTFS on-disk reference (verified facts for `hds-ntfs`, `hds-core`, `hds-testkit`)

Every line carries its evidence level:

- **[B]** verified against bytes. Raw images built here with `mkntfs` / `ntfs-3g` 2022.10.3
  (sector 512 and 4096, cluster 512 … 2 MiB), populated through the FUSE mount, then inspected
  with an independent Python reader. ntfs-3g is not Windows; known differences are in section 12.
- **[H]** verified against a header or source file: `layout.h` = `/usr/include/ntfs-3g/layout.h`
  (ntfs-3g-dev 2022.10.3, line numbers given), `ntfs3` = Linux `fs/ntfs3` (`ntfs.h`, `super.c`,
  `index.c`, `run.c`, `fsntfs.c`), `crate` = Rust crate `ntfs` 0.4.0, `winnt.h` = mingw-w64.
- **[D]** from documentation, not reproduced here: `MS` = Microsoft Win32 developer notes
  (`FILE_RECORD_SEGMENT_HEADER`, `MULTI_SECTOR_HEADER`, `ATTRIBUTE_RECORD_HEADER`,
  `ATTRIBUTE_LIST_ENTRY`), `libfsntfs` = libyal NTFS format documentation. URLs in section 15.

All integers are little-endian. Offsets are hexadecimal, sizes are bytes.

## 1. Boot sector

Only the first 512 bytes are used, also on 4096-byte-sector volumes.

| Off | Size | Field | Evidence |
|---|---|---|---|
| 0x00 | 3 | jump (`EB 52 90`) | [B] |
| 0x03 | 8 | OEM id `"NTFS    "` (`4E 54 46 53 20 20 20 20`) | [B] [H layout.h:72] |
| 0x0B | 2 | bytes per sector (512 and 4096 seen; valid: power of two, 512 … 4096) | [B] [H crate boot_sector.rs] |
| 0x0D | 1 | sectors per cluster, encoded (below) | [B] [H layout.h:51] |
| 0x15 | 1 | media descriptor (`F8`) | [B] |
| 0x1C | 4 | hidden sectors = partition start LBA at format time (0 if the formatter did not know) | [B] [H layout.h:60] |
| 0x28 | 8 | total sectors, signed; **partition sectors − 1** | [B] [H layout.h:78] |
| 0x30 | 8 | `$MFT` start LCN | [B] [H layout.h:83] |
| 0x38 | 8 | `$MFTMirr` start LCN | [B] [H layout.h:84] |
| 0x40 | 1 (+3 unused) | MFT record size, signed, encoded (below) | [B] [H layout.h:85] |
| 0x44 | 1 (+3 unused) | index block size, signed, encoded (below) | [B] [H layout.h:87] |
| 0x48 | 8 | volume serial number | [B] [H layout.h:89] |
| 0x50 | 4 | checksum, unused | [H layout.h:90] |
| 0x1FE | 2 | `55 AA` | [B] [H layout.h:92] |

Encodings and rules

- Sectors per cluster `v`: `v <= 0x80` → `v` sectors (power of two, not 0); `0xF4..=0xFF` → `2^(256 − v)`
  sectors; `0x81..=0xF3` is invalid. [H ntfs3 super.c `true_sectors_per_clst`] [D libfsntfs "244 to 255"].
  Seen: `01 02 04 08 10 80`, `F8` (128 KiB at 512), `F5` (1 MiB), `F4` (2 MiB at 512), `F7` (2 MiB at 4096),
  `20` (128 KiB at 4096: still a direct value). [B]
- Record / index size `v` (signed): `v > 0` → `v` clusters; `v < 0` → `2^(−v)` bytes. Seen: `02` (2 × 512),
  `01`, `F6` (−10 → 1024), `F4` (−12 → 4096); index: `08 04 02 01 F4`. [B]
  Bound the exponent before shifting: ntfs3 accepts at most 12 (4096) for both. [H ntfs3 super.c]
- Cluster size = bytes per sector × sectors per cluster, at most 2 MiB. [H crate, ntfs3] Windows `format /A`
  offers 512 … 64K, 128K, 256K, 512K, 1M, 2M. [D MS format]
- Backup boot sector: byte-identical copy in the sector directly after the volume, at
  `volume_start + total_sectors × bytes_per_sector` (= last sector of the partition). True for 512- and
  4096-byte sectors. There is no copy in the middle (that was NT 3.51 and older). [B] [H layout.h:38-43]
  So a boot sector found at `X` is either a primary (volume starts at `X`) or a backup (volume starts at
  `X − total_sectors × bps`); `hidden sectors` can disambiguate on whole-disk scans when it is non-zero.
- Windows only reads the primary boot sector and does not require `55 AA`. [H ntfs3 super.c comments]
- MFT record size: 1024 with 512-byte sectors, 4096 with 4096-byte sectors (mkntfs). [B]
  Windows: 1024 by default, larger with `format /L` ("Overrides the default size of file record"). [D MS format]
- Index block size was 4096 in every image, for every cluster and sector size. [B]

## 2. Multi-sector header and fixups (`FILE`, `INDX`)

| Off | Size | Field | Evidence |
|---|---|---|---|
| 0x00 | 4 | magic `FILE` / `INDX` (others: `BAAD`, `RCRD`, `RSTR`, `CHKD`, `HOLE`) | [B] [H layout.h:103-128] |
| 0x04 | 2 | offset of the update sequence array (USA) | [B] [H layout.h:194] |
| 0x06 | 2 | USA entry count = `1 + size / 512` | [B] [H layout.h:196] |

- **The stride is always 512 bytes, never the sector size.** [D MS MULTI_SECTOR_HEADER: space to reserve is
  "(size of structure / 512 + 1) * sizeof(USHORT)"] [H layout.h:170 `NTFS_BLOCK_SIZE 512`; ntfs3 `SECTOR_SIZE`
  in fsntfs.c; crate record.rs] [B: 4096-byte-sector volume with 4096-byte FILE records: count 9, the update
  sequence number sits at 510, 1022, …, 4094; INDX 4096: count 9]
- `USA[0]` = update sequence number (USN). `USA[i]`, `i >= 1` = the original last two bytes of 512-byte
  block `i`. On disk the last two bytes of every block equal the USN. [B]
- The array must end inside the first block: `usa_ofs + 2 × count <= 510`. [H layout.h:185-189] [D MS]
- Seen: FILE 1024 → ofs 0x30, count 3; FILE 4096 → ofs 0x30, count 9 (first attribute at 0x48);
  INDX 4096 → ofs 0x28, count 9 (first entry at 0x40). [B]

## 3. FILE record header

| Off | Size | Field | Evidence |
|---|---|---|---|
| 0x00 | 8 | multi-sector header (section 2) | [B] |
| 0x08 | 8 | `$LogFile` sequence number (always 0 on ntfs-3g volumes) | [B] [H layout.h:352] |
| 0x10 | 2 | sequence number | [B] [H layout.h:354] |
| 0x12 | 2 | link count = number of `$FILE_NAME` attributes, DOS alias included; 0 in extension records | [B] [H layout.h:359] |
| 0x14 | 2 | offset of the first attribute | [B] [H layout.h:369] |
| 0x16 | 2 | flags | [B] [H layout.h:263-270] |
| 0x18 | 4 | bytes in use (multiple of 8; = offset of the end marker + 8) | [B] [H layout.h:375] |
| 0x1C | 4 | bytes allocated = record size | [B] [H layout.h:377] |
| 0x20 | 8 | base record reference; 0 in a base record | [B] [H layout.h:380] [D MS] |
| 0x28 | 2 | next attribute instance | [B] [H layout.h:393] |
| 0x2A | 2 | NTFS 3.1+: reserved / high 16 bits of the record number | [H layout.h:401; ntfs3 ntfs.h "High part of MFT record?"] |
| 0x2C | 4 | NTFS 3.1+: record number, low 32 bits | [B] [H layout.h:402] |
| 0x30 (3.1+) / 0x2A (≤ 3.0) | 2 × count | update sequence array | [B for 0x30] [H layout.h:345-413 vs 420-485] |

Flags: `0x0001` in use; `0x0002` directory (has a `$I30` index); `0x0004` set on `$Extend` children;
`0x0008` has a non-`$I30` ("view") index. Seen: files `0x0001`, directories `0x0003`, `$Secure` `0x0009`,
`$Quota`/`$ObjId`/`$Reparse` `0x000D`, deleted file `0x0000`, deleted directory `0x0002`. [B] [H ntfs3 ntfs.h `RECORD_FLAG_*`]

Record number

- Present exactly when the USA offset is 0x30 or more (`FILE0`); absent in the 42-byte header of
  NTFS ≤ 3.0 (`FILE*`, USA at 0x2A; NT4 / Windows 2000). [H layout.h:345-485; ntfs-3g mft.c
  `ntfs_mft_record_layout`] [D libfsntfs: "Version 3.1: MFT entry index"] Not producible with mkntfs (always 3.1).
- Equal to the slot number in all 51 661 records of the test volume, except mkntfs's never-used records
  16–23, whose number field is 0. [B] Treat the field as a claim to be checked, not as truth.

File reference (8 bytes): low 48 bits record number, high 16 bits sequence number. [B] [H layout.h:322-330]

Sequence number

- A record that was never used has 1 on mkntfs / ntfs-3g volumes; system files 1–15 have sequence = record
  number; `$MFT` (0) has 1. [B] [H layout.h:205-208; ntfs-3g mft.c] Microsoft documents 0 for a segment that is
  "not used". [D MS]
- It is incremented when the record is **freed** (file deleted), not when the slot is reused; `0xFFFF`
  wraps to 1; 0 stays 0. [D MS: "incremented each time that a file record segment is freed"]
  [H layout.h:354-358; ntfs-3g mft.c `ntfs_mft_record_free`] [B: delete 1 → 2; the next file created in that slot is in use with 2]
- Therefore a reference `(N, s)` is satisfied by record `N` **in use with sequence `s`**, or by record `N`
  **not in use with sequence `s + 1`** (the referenced object was deleted). Record `N` in use with `s + 1`
  is a different, newer object. [B]
- A reference whose sequence is 0 means "do not check". [H layout.h:286-287]
- Fresh volumes are full of sequence-1 records, so `(N, 1)` in an old record very often equals the
  `(number, sequence)` of an unrelated record written later into slot `N`. [B: old child with parent `(64, 1)`
  next to a new directory in slot 64 with sequence 1 after re-format]

Record states seen [B]

- Deleted (ntfs-3g): in-use bit cleared, directory bit kept, sequence + 1, link count 0, attributes left in
  place (`$FILE_NAME` with the parent reference, `$DATA` with runlist and sizes). A deleted directory's
  `$INDEX_ROOT` is empty.
- Never used (mkntfs, records 27–63): `FILE`, flags 0, sequence 1, first attribute is the end marker.
  Records 16–23: flags 0, sequence = record number, number field 0, a lone `$STANDARD_INFORMATION`.
- Slots past the MFT's initialized size are zero-filled or still hold older records (no guarantee).
- Extension record: flags `0x0001` only (never the directory bit), link count 0, base reference =
  (base number, base sequence at link time), its own sequence and record number, no
  `$STANDARD_INFORMATION`; any attribute type can be the first attribute.
- Deleted file with extension records: base and every extension have their **own** sequence incremented,
  but the extensions' base reference still carries the base's **old** sequence (base 3085: seq 3, free;
  extensions: base ref `(3085, 2)`).
- Extension released while the file lives (truncate): extension free, sequence + 1, base reference
  unchanged and equal to the still-in-use base's current sequence; ntfs-3g empties such a record
  (end marker first) but old attribute bytes remain behind the marker.

## 4. Attribute header

Common part

| Off | Size | Field | Evidence |
|---|---|---|---|
| 0x00 | 4 | type (list below); `0xFFFFFFFF` = end marker | [B] [H layout.h:498-518] |
| 0x04 | 4 | attribute length (multiple of 8) | [B] [H layout.h:724] |
| 0x08 | 1 | 0 resident, 1 non-resident | [B] [H layout.h:727] |
| 0x09 | 1 | name length in UTF-16 units (0 = unnamed) | [B] [H layout.h:729] |
| 0x0A | 2 | name offset | [B] [H layout.h:731] |
| 0x0C | 2 | flags: `0x00FF` compression mask (`0x0001` = LZNT1), `0x4000` encrypted, `0x8000` sparse | [B] [H layout.h:631-638] [D MS] |
| 0x0E | 2 | instance (attribute id, unique inside its record) | [B] [H layout.h:741] |

Resident part

| Off | Size | Field | Evidence |
|---|---|---|---|
| 0x10 | 4 | value length | [B] [H layout.h:748] |
| 0x14 | 2 | value offset (0x18 unnamed; 0x20 for `$I30`; 0x38 for a 15-character name) | [B] [H layout.h:749] |
| 0x16 | 1 | indexed flag | [B] [H layout.h:756] |

Non-resident part

| Off | Size | Field | Evidence |
|---|---|---|---|
| 0x10 | 8 | lowest VCN of this extent (signed) | [B] [H layout.h:765] |
| 0x18 | 8 | highest VCN of this extent (signed; −1 when empty; may be 0 on old single-extent attributes) | [B] [H layout.h:770-775] |
| 0x20 | 2 | mapping pairs (runlist) offset | [B] [H layout.h:776] |
| 0x22 | 1 | compression unit, log2 clusters (0, or 4 = 16 clusters) | [B] [H layout.h:782] |
| 0x28 | 8 | allocated size | [B] [H layout.h:791] |
| 0x30 | 8 | data (real) size | [B] [H layout.h:799] |
| 0x38 | 8 | initialized size ("valid data length") | [B] [H layout.h:802] |
| 0x40 | 8 | total allocated ("compressed") size: only in compressed or sparse attributes | [B] [H layout.h:810; ntfs3 `SIZEOF_NONRESIDENT_EX 0x48`] |

Rules

- The non-resident header is 0x40 or 0x48 bytes. Never assume either: take the name from `name offset` and
  the runlist from `mapping pairs offset`. Seen: plain `0x40/0x40`; named `$I30` name 0x40, runs 0x48;
  sparse or compressed: name offset 0x48, runs 0x48; named stream `bigstream`: name 0x40, runs 0x58. [B]
- End marker: type `0xFFFFFFFF`. The 4 bytes after it are not a length. [B] [H layout.h:341-343]
- Allocated / real / initialized sizes are valid only in the extent with lowest VCN 0; they are 0 in all
  other extents. [B] [D MS: "not valid if the LowestVcn member is nonzero"] [H layout.h:789]
- `initialized <= real`. Bytes in `[initialized, real)` read as zero even if clusters are allocated.
  [B: `extended.bin`: 3 clusters allocated, initialized 10 000, real 500 000; reconstruction with that rule
  equals what the driver returns] [D libfsntfs]
- Empty non-resident attribute: lowest 0, highest −1, no runs. [H layout.h:773; ntfs3 run.c "evcn + 1 == svcn"]
  ntfs-3g converts a file truncated to 0 back to resident (length 0). [B]
- Decide contiguity from the sum of run lengths, not from `highest VCN`. On consistent records
  `sum(lengths) == highest − lowest + 1`. [B] [H ntfs3 run.c; layout.h:774 "Can be 0 for single extent"]
- "Compressed" means: non-resident **and** `flags & 0x00FF != 0`. A resident attribute can carry `0x0001`
  (file created in a compressed folder) and its bytes are stored raw. [B: 4-byte file `tiny`, flags 0x0001]
  The compression unit field is not a compression indicator: ntfs-3g writes 4 for sparse, uncompressed
  attributes. [B] libfsntfs has seen compressed data with unit 0 on XP. [D]
- Sparse, uncompressed (ntfs-3g): flag `0x8000`, unit 4, 0x48 header; allocated size = all VCNs × cluster
  size; the field at 0x40 = bytes really allocated; `$STANDARD_INFORMATION` flag `0x200`. [B]
- Compressed (ntfs-3g): flag `0x0001`, unit 4, allocated size a multiple of 16 clusters, runs alternate
  data and sparse inside each 16-cluster unit. [B] [H layout.h:640-706]

Attribute types: `0x10` `$STANDARD_INFORMATION`, `0x20` `$ATTRIBUTE_LIST`, `0x30` `$FILE_NAME`, `0x40`
`$OBJECT_ID`, `0x50` `$SECURITY_DESCRIPTOR`, `0x60` `$VOLUME_NAME`, `0x70` `$VOLUME_INFORMATION`, `0x80`
`$DATA`, `0x90` `$INDEX_ROOT`, `0xA0` `$INDEX_ALLOCATION`, `0xB0` `$BITMAP`, `0xC0` `$REPARSE_POINT`, `0xD0`
`$EA_INFORMATION`, `0xE0` `$EA`, `0x100` `$LOGGED_UTILITY_STREAM`. [H layout.h:498-518] Attributes inside a
record are sorted by type, then name. [B]

## 5. Runlist (mapping pairs)

- Header byte: low nibble = byte count of the run length, high nibble = byte count of the LCN delta.
  A `00` byte ends the list. [B] [D MS: "count byte = v + (l * 16)"]
- Length: little-endian. Microsoft defines it as a **signed** quantity, so encoders add a zero byte when
  the top bit would be set: 128 clusters is `80 00`, not `80`. [B: `22 80 00 68 40`] [D MS] A length of 0 or
  a negative length is corruption. [H ntfs-3g runlist.c "Assume a negative length to indicate data
  corruption"; ntfs3 run.c `if (!len) return -EINVAL`]
- Delta: signed, sign-extended, added to the LCN of the **previous non-sparse run** (0 before the first).
  [B: `31 09 DF 3C FF` = −49 953; `31 01 47 81 00` uses 3 bytes for +33 095; in `sparse.bin`
  `31 01 47 81 00`, `02 FF 00`, `21 01 00 01` gives LCN 33 095, hole of 255, LCN 33 351]
- Delta size 0 = sparse run (hole), reads as zeros. [B: `02 FF 00`, `01 0F`] [H layout.h:656-663; ntfs3 run.c]
- A delta that is present but 0 is a real run. Only `$Boot` has it (`11 02 00` = 2 clusters at LCN 0). [B]
  Implementations disagree here: the `ntfs` crate treats delta 0 as sparse, ntfs3 rejects it, Microsoft's
  text says "CurrentLcn of 0 … unallocated". [H crate non_resident.rs; ntfs3 run.c] [D MS] No user file can
  own LCN 0.
- Bytes after the terminator are padding and are not always zero (`… 00 FF 00 00 00 00`). [B] [D libfsntfs]
- Volumes written by NT4 may mark holes with LCN −1. [H ntfs3 run.c "special combination"; ntfs-3g runlist.c]

## 6. `$STANDARD_INFORMATION` (0x10)

Resident, base record only. 48 bytes (NTFS 1.2 layout; also what ntfs-3g writes for every file it creates)
or 72 bytes (what Windows writes on NTFS 3.x). Accept both. [B] [H layout.h:910-990]

| Off | Size | Field |
|---|---|---|
| 0x00 | 8 | creation time |
| 0x08 | 8 | last data modification time |
| 0x10 | 8 | last MFT record change time |
| 0x18 | 8 | last access time |
| 0x20 | 4 | file attribute flags |
| 0x24 | 4 | maximum versions (72-byte form from here on) |
| 0x28 | 4 | version number |
| 0x2C | 4 | class id |
| 0x30 | 4 | owner id |
| 0x34 | 4 | security id |
| 0x38 | 8 | quota charged |
| 0x40 | 8 | update sequence number (`$UsnJrnl`) |

- Times are FILETIME: 100 ns units since 1601-01-01 UTC. `unix_ms = (t − 116444736000000000) / 10000`. [B]
- Flags: `0x1` read-only, `0x2` hidden, `0x4` system, `0x20` archive, `0x100` temporary, `0x200` sparse,
  `0x400` reparse point, `0x800` compressed, `0x1000` offline, `0x2000` not indexed, `0x4000` encrypted,
  `0x00040000` recall on open, `0x00080000` pinned, `0x00100000` unpinned, `0x00400000` recall on data
  access. [H layout.h:828-891; winnt.h:5961-5974] The directory bit `0x10` is not used: directories show
  `0x20` / `0x26` here. [B]

## 7. `$FILE_NAME` (0x30)

Resident. Also the key of every `$I30` index entry. [B] [H layout.h:1103-1142]

| Off | Size | Field |
|---|---|---|
| 0x00 | 8 | parent directory reference |
| 0x08 | 8 | creation time |
| 0x10 | 8 | last data modification time |
| 0x18 | 8 | last MFT record change time |
| 0x20 | 8 | last access time |
| 0x28 | 8 | allocated size |
| 0x30 | 8 | real size |
| 0x38 | 4 | flags: as section 6 plus `0x10000000` directory, `0x20000000` view index |
| 0x3C | 4 | EA size, or the reparse tag when the file is a reparse point |
| 0x40 | 1 | name length in UTF-16 units (1 … 255) |
| 0x41 | 1 | namespace: 0 POSIX, 1 Win32, 2 DOS, 3 Win32 + DOS |
| 0x42 | 2 × length | name, UTF-16LE; unpaired surrogates are legal |

- Directory test in a name or index entry: `flags & 0x10000000`. [B] [H layout.h:877]
- Sizes and times in the MFT copy are stale: written at create / rename only. [B: `real = 0` beside a
  `$DATA` of 5000 bytes] [H layout.h:1094-1097] [D libfsntfs] The copy inside the parent's index is kept
  current (ntfs-3g) and for sparse or compressed files its allocated size is the really allocated bytes. [B]
- One attribute per hard link. A long name plus its 8.3 alias are two attributes (namespace 1 and 2,
  same parent) and count as 2 links. Three hard links: 3 attributes, 2 different parents. [B]
- Root directory: record 5, name `.`, parent `(5, 5)`, namespace 3. [B]
- ntfs-3g writes namespace 0 for everything it creates (1 + 2 only after setting a DOS name);
  mkntfs system files use 3. [B] Windows writes 1, 2 and 3.
- A `$FILE_NAME` can live in an extension record. [B: two cases, a file and a directory]

## 8. `$ATTRIBUTE_LIST` (0x20)

| Off | Size | Field | Evidence |
|---|---|---|---|
| 0x00 | 4 | attribute type | [B] [H layout.h:1023] |
| 0x04 | 2 | entry length (0x20 unnamed, 0x28 for `$I30`) | [B] [H layout.h:1024] |
| 0x06 | 1 | name length (UTF-16 units) | [B] |
| 0x07 | 1 | name offset (always 0x1A) | [B] [H layout.h:1027] |
| 0x08 | 8 | lowest VCN of the extent (0 unless a non-resident attribute is split) | [B] [D MS] |
| 0x10 | 8 | reference to the record that holds the extent | [B] [H layout.h:1042] |
| 0x18 | 2 | instance of the attribute inside that record | [B] [H layout.h:1045] |
| 0x1A | … | name, then padding to 8 | [B] |

- One entry per attribute extent of the file, for **every** attribute type except the list itself,
  sorted by type, name, lowest VCN. [B] [D MS ATTRIBUTE_LIST_ENTRY remarks]
- Resident or non-resident; the non-resident form's runlist is in the base record; maximum 256 KiB.
  [H layout.h:1006-1009] ntfs-3g made every list non-resident, even 128 bytes long. [B] Windows keeps it
  resident while it fits: Microsoft's "Four Stages of NTFS File Growth" describes stage 3 (mapping pairs
  moved into child records, one list entry each) and stage 4 (the list itself becomes non-resident). [D]
- Layouts seen [B]:
  - File, 10 270 runs: base { `$STANDARD_INFORMATION`, list, `$SECURITY_DESCRIPTOR`, `$DATA` VCN 0–467 with
    the sizes }, one extension with only `$FILE_NAME`, 47 extensions with one `$DATA` extent each
    (216 runs per 1 KiB record, sizes 0).
  - Directory, 48 556 entries: base { `$STANDARD_INFORMATION`, list, `$SECURITY_DESCRIPTOR`,
    `$INDEX_ALLOCATION` VCN 0–133 }, extensions holding `$FILE_NAME`, `$INDEX_ROOT`, `$BITMAP` and ten further
    `$INDEX_ALLOCATION` extents (VCN 134, 363, 595, …).
  - `$MFT` itself: record 0 { `$DATA` VCN 0–3722, `$BITMAP`, list }, record 15 (base `(0, 1)`) `$DATA`
    VCN 3723–4626.
- Do not assume the extent with VCN 0, the `$FILE_NAME` or the `$INDEX_ROOT` is in the base record.

## 9. Index structures (`$INDEX_ROOT` 0x90, `$INDEX_ALLOCATION` 0xA0, `INDX`)

`$INDEX_ROOT` value (resident, named `$I30` in directories) [B] [H layout.h:2124-2142]

| Off | Size | Field |
|---|---|---|
| 0x00 | 4 | indexed attribute type (`0x30`) |
| 0x04 | 4 | collation rule (1 = file name) |
| 0x08 | 4 | index block size in bytes |
| 0x0C | 1 | blocks per index block: clusters when cluster ≤ block, else 512-byte units (8) |
| 0x10 | 16 | node header |

Node header (in `$INDEX_ROOT` at 0x10, in an `INDX` block at 0x18) [B] [H layout.h:2084-2103]

| Off | Size | Field |
|---|---|---|
| 0x00 | 4 | offset of the first entry, **relative to this header** |
| 0x04 | 4 | used size, relative to this header |
| 0x08 | 4 | allocated size, relative to this header |
| 0x0C | 1 | flags: `0x01` = has sub-nodes |

`INDX` block [B] [H layout.h:2153-2173]

| Off | Size | Field |
|---|---|---|
| 0x00 | 8 | multi-sector header (USA at 0x28, count 9 for 4096) |
| 0x08 | 8 | `$LogFile` sequence number |
| 0x10 | 8 | VCN of this block inside `$INDEX_ALLOCATION` |
| 0x18 | 16 | node header (entries at 0x18 + 0x28 = 0x40; allocated size 0xFE8) |

Block size = `0x18 + allocated size` and must equal `(USA count − 1) × 512`. [B]

Index entry [B] [H layout.h:2301-2382]

| Off | Size | Field |
|---|---|---|
| 0x00 | 8 | file reference of the child (its **base** record) |
| 0x08 | 2 | entry length (multiple of 8) |
| 0x0A | 2 | key length |
| 0x0C | 2 | flags: `0x1` has sub-node (VCN in the last 8 bytes of the entry), `0x2` last entry (no key) |
| 0x10 | key length | `$FILE_NAME` (section 7) |

- VCN unit of a block: `VCN × cluster size` when cluster ≤ index block size, else `VCN × 512`, also on
  4096-byte-sector volumes. Blocks are numbered 0, 8, 16 … for 512-byte and for ≥ 8 KiB clusters, 0, 4, 8 …
  for 1 KiB, 0, 2, 4 … for 2 KiB, 0, 1, 2 … for 4 KiB. [B: nine sector/cluster combinations]
  [H ntfs3 index.c "Index record is smaller than a cluster, use 512 blocks"]
- The last entry has flags `0x2`, key length 0, length 0x10 (0x18 with a sub-node). [B]
- A file with a long name and an 8.3 alias has two entries with the same reference. [B]
- A large directory's `$INDEX_ROOT` still holds separator entries that are in no `INDX` block; a small
  directory has all its entries there and no `INDX` block at all. [B]
- `$INDEX_ALLOCATION` can be split over extension records like `$DATA`; each extent has its own lowest VCN. [B]
- Other indexes (`$SII`, `$SDH`, `$O`, `$Q`, `$R`) use the same block format with short non-name keys. [H layout.h:2351-2369]

## 10. Record numbers and system files

| Record | Content | Evidence |
|---|---|---|
| 0 | `$MFT` (`$DATA` = the table, `$BITMAP` = records in use) | [B] [H layout.h:211] |
| 1 | `$MFTMirr`: copy of the first 4 records (more with large clusters) | [B] [H layout.h:214-217] |
| 2–4 | `$LogFile`, `$Volume`, `$AttrDef` | [B] |
| 5 | root directory | [B] |
| 6–10 | `$Bitmap`, `$Boot`, `$BadClus`, `$Secure`, `$UpCase` | [B] |
| 11 | `$Extend` (directory) | [B] |
| 12–15 | reserved, marked in use, empty; 15 receives the first extra `$DATA` extent of `$MFT` | [B] [H layout.h:241-244] |
| 16–23 | not in use after format; kept for extension records (and chkdsk's log); "Windows will only use records 16 to 24 for normal files if the volume is completely out of space"; seen zero-filled on XP | [B: free] [H ntfs-3g mft.c comment above `ntfs_mft_record_alloc`] [D libfsntfs] |
| 24, 25, 26 | `$Extend\$Quota`, `$ObjId`, `$Reparse` (parent `(11, 11)`) | [B] [D libfsntfs] |
| 27 … | Windows Vista+: `$Extend\$RmMetadata`, `$Repair`, `$TxfLog`, `$Txf`, `$Tops`, `$TxfLog.blf`; Windows 10: `$Extend\$Deleted` at 29; `$Extend\$UsnJrnl` has no fixed number | [D libfsntfs] |

- First record available to users: 24 on Windows (ntfs3 `MFT_REC_USER = 24`, `MFT_REC_FREE = 16`); ntfs-3g
  allocates from 64 (`RESERVED_MFT_RECORDS`). [H ntfs3 ntfs.h; ntfs-3g mft.c] [B: first file = record 64]
- `$MFTMirr` records are byte-identical copies with the same record numbers at another disk offset. [B]

## 11. Reparse points and files whose unnamed `$DATA` holds no data

`$REPARSE_POINT` (0xC0) value: 0x00 tag (4), 0x04 data length (2), 0x06 reserved (2), 0x08 data.
The tag is also copied to `$FILE_NAME` + 0x3C. [H layout.h:2465-2470, 1133]

| Tag | Meaning | Evidence |
|---|---|---|
| `0xA0000003` | mount point / junction | [H layout.h:2439; winnt.h:6132] |
| `0xA000000C` | symbolic link | [H winnt.h:6141] |
| `0x80000013` | Data Deduplication stub | [H winnt.h:6144] |
| `0x80000017` | WOF ("compact /exe", CompactOS) | [H winnt.h:6147] |
| `0x9000001A`, `0x9000101A` … `0x9000F01A` | cloud files placeholder (OneDrive); match with `tag & 0xFFFF0FFF == 0x9000001A` | [H winnt.h:6151-6167] |

- WOF-compressed file: reparse tag `0x80000017`, the unnamed `$DATA` is sparse with no data, the content
  is in a named stream `WofCompressedData`. [D libfsntfs]
- Placeholders are reparse points marked sparse; a reader below the owning filter "might get a block of
  zeroes … rather than the actual data". [D MS driver docs "Placeholders"]
- Following the runlist of such files yields zeros of the full size. They need their own flag.
- ntfs-3g symlinks are not reparse points by default: system-flagged files with an `IntxLNK` payload. [B]

## 12. mkntfs / ntfs-3g versus Windows (what Linux fixtures do not show)

| Topic | mkntfs / ntfs-3g | Windows | Evidence |
|---|---|---|---|
| Quick re-format | changed 14 clusters of the 256 MiB test volume: 0, 2, 4–10 (a 27-record `$MFT`), 8197 (root `INDX` block, same LCN as the old root's first block), 8199–8200, 32767 (`$MFTMirr`), 65535 (backup boot sector); old records 28 and up survive | writes an empty `$MFT` of 256 KiB at a fixed address, destroying about 229 user records | [B] / [D R-TT article, search summary] |
| First user record | 64 | 24 and up | [B] [H] |
| Consequence | no user record is lost by an mkntfs re-format | top-level folders and early files lose their records | [B] |
| `$MFT` location | byte 16 384 of the volume for clusters ≤ 8 KiB (LCN 2 for larger clusters); `$MFTMirr` mid-volume | a different fixed address | [B] / [D R-TT] |
| Name namespace | 0 (POSIX) | 1, 2, 3 | [B] / [D] |
| `$STANDARD_INFORMATION` | 48 bytes plus a `$SECURITY_DESCRIPTOR` attribute per file | 72 bytes | [B] / [D libfsntfs] |
| `$LogFile` sequence number | 0 | set | [B] |
| Attribute list | always non-resident | resident first | [B] / [D] |
| Split `$DATA` | first extent stays in the base record | mapping pairs moved to child records | [B] / [D] |
| Delete | keeps the last `$FILE_NAME`, except when it sat in an extension record: then the name is removed | keeps it | [B] [H ntfs-3g dir.c comment] |
| Never-used records | 16–23 carry record number 0 | not known here | [B] |
| Symbolic links | `IntxLNK` files | reparse points | [B] |

Anything in the right-hand column must be covered by `hds-testkit` or the Windows CI run.

## 13. Geometry votes: aliasing between cluster sizes [B]

Position of block `v` inside the attribute (index block size 4096): `p(v, c) = v × c` if `c <= 4096`, else `v × 512`.
For a directory whose index allocation is one run at LCN `L`, the block is at
`D = offset + L × c + p(v, c)`. A hypothesis `c'` votes for `D − L × c' − p(v, c')`.

- True `c` in {1024, 2048, 4096}: `p` differs for every other `c'`, so wrong candidates scatter
  (4 KiB volume: 2705 votes for the truth, at most 2 for anything else).
- True `c` = 512 or ≥ 8192: `p(v, c) = p(v, c')` for every `c'` > 4096 (and for 512), so **all blocks of
  that run vote for the same wrong pair** `(c', offset + L × (c − c'))`. Measured: truth 116 votes,
  three wrong pairs 115 votes each; the single vote of difference came from the root directory's block,
  which a re-format overwrites.
- Wrong pairs differ per run (they depend on `L`), the true pair does not. Count distinct runs or
  directories, not blocks; many wrong pairs also have a negative volume offset.
- Using only the base record's `$INDEX_ALLOCATION` extent loses votes: 407 of 2705 on the test volume.
- `origin = disk_offset − record_number × record_size` is negative for MFT fragments that sit early on
  the disk and hold high record numbers (seen: −1 978 368). Use signed arithmetic.

## 14. Pitfalls checklist

1. Fixup stride is 512 bytes regardless of sector size and record size. [B] [D]
2. Non-resident header is 0x40 or 0x48 bytes; use the stored offsets. [B]
3. Sizes live only in the extent with lowest VCN 0, which need not be in the base record. [B] [D]
4. Base/extension and child/parent references go stale by exactly one sequence step when the target is
   deleted; match `s` when the target is in use, `s + 1` when it is not. [B]
5. A free extension whose base is in use with the referenced sequence is a leftover, not part of the file. [B]
6. Records written after the format reuse old `(number, sequence)` pairs; a match is not identity. [B]
7. `$FILE_NAME`, `$INDEX_ROOT`, `$BITMAP`, any `$DATA` or `$INDEX_ALLOCATION` extent can sit in an extension
   record; extension records never carry the directory flag. [B]
8. `$FILE_NAME` sizes in the MFT record are stale; use `$DATA`. [B]
9. Directory = record flag `0x0002`, or `0x10000000` in a name or index entry; never `0x10`. [B]
10. A long name and its DOS alias are two attributes and two index entries for one file. [B]
11. Hard links: several names with the same namespace and different parents. [B]
12. Resident data is never compressed, whatever the flag says; the compression unit is set on sparse files. [B]
13. Runlist deltas are relative to the last non-sparse run; lengths are signed; padding after the
    terminator is not zero. [B]
14. Never-used records (`FILE`, not in use, no attributes) and records with a wrong record number exist. [B]
15. `$MFTMirr` duplicates records 0–3 at another offset. [B]
16. `$MFT` can have its own attribute list; its further `$DATA` extents are in record 15 [B] and 16–23 [H ntfs-3g mft.c].
17. Index block VCNs count 512-byte units when the cluster is larger than the block. [B]
18. A re-format done with mkntfs does not destroy user records; simulate Windows by wiping the old MFT's
    first 256 KiB. [B]
19. Names are UTF-16 code units, not guaranteed valid UTF-16. [D libfsntfs]
20. Files with reparse tags WOF / cloud / dedup have an unnamed `$DATA` that reads as zeros. [D]

## 15. Sources

- `/usr/include/ntfs-3g/layout.h` (package ntfs-3g-dev 1:2022.10.3).
- ntfs-3g sources: <https://github.com/tuxera/ntfs-3g> (`libntfs-3g/mft.c`, `runlist.c`, `dir.c`).
- Linux ntfs3: <https://github.com/torvalds/linux/tree/master/fs/ntfs3> (`ntfs.h`, `super.c`, `index.c`, `run.c`, `fsntfs.c`).
- Rust crate `ntfs` 0.4.0: <https://crates.io/crates/ntfs>.
- Microsoft developer notes (read from the `MicrosoftDocs/win32` mirror of learn.microsoft.com):
  <https://learn.microsoft.com/en-us/windows/win32/devnotes/file-record-segment-header>,
  <https://learn.microsoft.com/en-us/windows/win32/devnotes/multi-sector-header>,
  <https://learn.microsoft.com/en-us/windows/win32/devnotes/attribute-record-header>,
  <https://learn.microsoft.com/en-us/windows/win32/devnotes/attribute-list-entry>.
- Microsoft `format` command: <https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/format>.
- Microsoft driver docs, "Placeholders": <https://learn.microsoft.com/en-us/windows-hardware/drivers/ifs/placeholders>.
- Microsoft, "The Four Stages of NTFS File Growth" (search summary only):
  <https://learn.microsoft.com/en-us/archive/blogs/askcore/the-four-stages-of-ntfs-file-growth>.
- libyal libfsntfs format documentation:
  <https://github.com/libyal/libfsntfs/blob/main/documentation/New%20Technologies%20File%20System%20(NTFS).asciidoc>.
- R-TT, "NTFS Recovery after Quick Format" (search summary only):
  <https://www.r-tt.com/Articles/NTFS_Recovery_after_Quick_Format/>.
- mingw-w64 `winnt.h` (`/usr/share/mingw-w64/include/winnt.h`) for reparse tags and file attribute constants.
