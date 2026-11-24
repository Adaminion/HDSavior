# HDSavior — Architecture

HDSavior rescues files from hard disks whose NTFS volume was accidentally
(quick-)formatted as NTFS again. Windows only, plain HDDs, no encryption, no
NTFS compression.

This document is the contract every component is built against. The wire
protocol between the UI and the engine is specified separately in
[PROTOCOL.md](PROTOCOL.md).

---

## 1. Scope

In scope

- Source: a physical disk (`\\.\PhysicalDriveN`), one partition of it, or a raw
  image file of either.
- Scan the source for surviving MFT `FILE` records, directory `INDX` blocks, NTFS
  boot sectors and file-signature hits, in a single sequential pass.
- Rebuild the old folder tree from parent references.
- Recover file contents by following runlists (including files whose runlist is
  spread over several MFT records through `$ATTRIBUTE_LIST`), sparse runs and
  the initialized-size rule.
- Signature carving for files whose MFT record is gone (contiguous files only).
- Optional sector-by-sector imaging of the source to a file on another disk.
- Saved scan sessions (no need to rescan to recover more files later).

Out of scope (detected and reported, never silently mishandled)

- LZNT1-compressed and EFS-encrypted files: listed, flagged `unsupported`, not
  recovered.
- BitLocker, Storage Spaces, dynamic disks, ReFS/exFAT/FAT sources.
- SSDs (TRIM makes a formatted SSD unrecoverable) and drives that were *fully*
  formatted (Windows Vista+ zero-fills on a full format).
- Alternate data streams (ignored).
- Repairing the disk in place. HDSavior only ever copies data *out*.

## 2. Safety invariants (read-only discipline)

These are hard requirements. Reviewers must be able to verify each one by
reading the code.

- **S1 — The source is never opened for writing.** Exactly one function opens
  raw devices and one opens image files; both request read access only
  (`GENERIC_READ`, never `GENERIC_WRITE`). The `BlockSource` trait has no
  write method. No IOCTL/FSCTL that changes device state is ever issued
  (no lock, dismount, offline, set-attributes).
- **S2 — Nothing is written into the region being rescued.** Every path the
  engine writes to (recovery destination, image file, session file, report)
  is resolved to its volume and physical-disk extents first. If any extent
  overlaps the scanned byte range of the source disk, the operation is
  refused; there is no override.
- **S3 — Same physical disk needs explicit consent.** A destination on the
  same physical disk but outside the scanned range (another partition) is
  refused unless the caller passes `allow_same_disk: true`, which the UI only
  sets after an explicit warning dialog. If the destination's disk cannot be
  determined (and it is not a network path), it is treated the same way.
  Session files are exempt from S3 (but not from S2).
- **S4 — Image-file sources** are opened read-only; destinations are not
  restricted except that the destination may not be the image file itself.
- **S5 — No silent corruption.** A file is reported as recovered-OK only if
  every byte came from a successfully read sector at a location justified by
  its runlist. Anything else (unreadable sectors, missing fragments, overlap
  with newer data, signature mismatch) is reported per file.

## 3. Background: what a quick format leaves behind

Working assumptions (the scanner does not *depend* on them, they only explain
the design and drive heuristics):

- A quick format writes a new boot sector (plus the backup copy in the last
  sector of the partition), a new small `$MFT` (first ~256 records, 256 KiB),
  `$MFTMirr`, `$LogFile`, `$Bitmap`, `$AttrDef`, `$UpCase`, `$Secure` and a root
  directory index. Everything else on the volume is left untouched.
- With the same partition, size and cluster size, the new `$MFT` lands at the
  same LCN as the old one, so the *first few hundred records of the old MFT are
  overwritten* — including old record 0 (`$MFT` itself, which held the MFT's
  own runlist), record 5 (root directory) and usually the top-level folders
  that were created first. All later records survive.
- Because old record 0 is gone, the old MFT cannot be walked through its
  runlist. A fragmented old MFT can only be found completely by scanning the
  whole volume for `FILE` signatures.
- Runlists hold LCNs relative to the start of the *old* volume in units of the
  *old* cluster size. These normally equal the new volume's, but are detected
  independently (section 6.6).

## 4. Components

```
┌───────────────────────────────┐        JSON lines over stdin/stdout
│ Flutter Windows app (app/)    │ ─────────────────────────────────────┐
│ UI only: no disk access       │                                      │
└───────────────────────────────┘                                      ▼
                                        ┌──────────────────────────────────────┐
                                        │ hdsavior-engine.exe (Rust, engine/)  │
                                        │ CLI + `serve` mode                   │
                                        │  hds-core   scan/catalog/recover     │
                                        │  hds-ntfs   on-disk structure parser │
                                        │  hds-io     block sources, disks     │
                                        │  hds-carve  signatures + validators  │
                                        └──────────────────────────────────────┘
```

- The Flutter app is declared `requireAdministrator` in its Windows manifest
  (one UAC prompt at start). It launches `hdsavior-engine.exe serve` as a child
  process (which inherits the elevated token) and talks to it over the child's
  stdin/stdout. A crash of the engine cannot take the UI down, and a cancelled
  job can always be stopped by killing the child.
- The engine is also a complete command-line tool (`scan`, `ls`, `recover`,
  `image`, …) so it can be tested headlessly against image files on any OS.
- The engine is pure Rust (no C dependencies) to keep Windows builds simple.

Repository layout

```
docs/                    ARCHITECTURE.md, PROTOCOL.md
engine/                  Cargo workspace
  crates/hds-ntfs/       pure parsing, no I/O, no_std-style discipline (no panics on any input)
  crates/hds-io/         BlockSource, image files, Windows raw devices, disk enumeration,
                         MBR/GPT parsing, destination guard, aligned buffers, resilient reads
  crates/hds-carve/      signature sniffing + per-format length/validity walkers
  crates/hds-core/       scanner, session, catalog, geometry, health, recovery, imaging, carving
  crates/hds-engine/     binary `hdsavior-engine`: CLI + JSON-lines server
  crates/hds-testkit/    dev-only: synthetic NTFS image builder for tests
app/                     Flutter app (Windows desktop only)
scripts/                 build.ps1, e2e-windows.ps1, linux fixture + e2e scripts
.github/workflows/       CI
```

## 5. Engine crates

### 5.1 `hds-ntfs` (pure parser)

Input is always a byte slice; nothing here performs I/O. Every function is
total: arbitrary/hostile bytes produce `Err`/`None`, never a panic, overflow,
out-of-bounds access or unbounded loop.

- `parse_boot_sector(&[u8]) -> Option<BootSector>` — validates OEM id
  `"NTFS    "` at 0x03 and the 0x55AA trailer; yields bytes/sector, cluster
  size, total sectors, MFT LCN, MFTMirr LCN, MFT record size, index block
  size, serial. `sectors_per_cluster` (u8 at 0x0D) values above 0x80 mean
  `2^(256 - v)` sectors. `clusters_per_mft_record` (i8 at 0x40) and
  `clusters_per_index_block` (i8 at 0x44): positive = clusters, negative =
  `2^(-v)` bytes.
- `probe_file_record(&[u8]) -> Option<usize>` — cheap check on the first
  sector: `FILE` magic, sane update-sequence offset/count, allocated size a
  power of two in `1024..=4096` (configurable upper bound 65536), `count ==
  alloc/512 + 1`. Returns the record size.
- `apply_fixups(&mut [u8]) -> FixupStatus` — update sequence array at the
  offset in the header; the stride is always 512 bytes regardless of the
  device sector size; the last two bytes of each 512-byte block must equal the
  update sequence number and are replaced with the saved values. A mismatch
  is reported (`torn`), parsing continues on a best-effort basis.
- `parse_file_record(&[u8]) -> Result<FileRecord>` — header (LSN, sequence
  number, link count, flags `0x1 in-use`, `0x2 directory`, used/allocated
  size, base record reference, record number at 0x2C when the update sequence
  offset is ≥ 0x30) and the attributes:
  - `$STANDARD_INFORMATION` (0x10): four FILETIMEs, file attribute flags.
  - `$FILE_NAME` (0x30), all instances: parent reference (48-bit record
    number + 16-bit sequence), times, sizes, flags, namespace, UTF-16 name.
  - `$DATA` (0x80), all instances, named and unnamed: resident (offset and
    length *within the record*; the content is not copied) or non-resident
    (start VCN, last VCN, decoded runlist, allocated/real/initialized size,
    attribute flags `0x0001 compressed`, `0x4000 encrypted`, `0x8000 sparse`,
    compression unit).
  - `$ATTRIBUTE_LIST` (0x20): presence, resident entries decoded (type,
    start VCN, holder reference, attribute id, name) or non-resident runs.
  - `$INDEX_ALLOCATION` (0xA0) named `$I30`: decoded runlist (for geometry
    detection).
  - `$BITMAP` (0xB0) and `$VOLUME_NAME` (0x60): exposed raw (resident bytes
    or runs); used for the live file system only.
- `decode_runlist(&[u8]) -> (Vec<Run>, complete: bool)` — header byte: low
  nibble = size of the length field, high nibble = size of the offset field;
  length unsigned, offset signed and relative to the previous run's LCN;
  offset size 0 = sparse run; `0x00` terminates. Rejects zero-length runs,
  field sizes over 8, negative absolute LCNs and LCN/length overflow.
- `parse_index_block(&[u8]) -> Result<IndexBlock>` — `INDX` magic, fixups,
  block VCN (0x10), node header at 0x18 (entries offset, used size, allocated
  size — all relative to 0x18), then entries: file reference (8), entry
  length (2), key length (2), flags (`0x1` has sub-node, `0x2` last). Only
  entries whose key validates as a `$FILE_NAME` are returned (index blocks of
  `$Secure`, `$ObjId`, … carry other keys and are discarded).
- `FileRef { record: u64 /* 48 bit */, seq: u16 }`, FILETIME helpers.

### 5.2 `hds-io`

```rust
pub trait BlockSource: Send + Sync {
    fn size(&self) -> u64;                  // bytes
    fn sector_size(&self) -> u32;           // logical: 512 or 4096
    /// Positional read; offset and buf.len() must be multiples of sector_size().
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()>;
    fn info(&self) -> SourceInfo;           // kind, path, model, serial, disk number
}
```

- `ImageFile` — any regular file, opened read-only.
- `RawDisk` (Windows only) — `\\.\PhysicalDriveN`, opened with read access and
  full sharing; size from `IOCTL_DISK_GET_LENGTH_INFO`, sector sizes from
  `IOCTL_STORAGE_QUERY_PROPERTY` (access alignment) with
  `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX` as fallback. Positional reads use an
  `OVERLAPPED` offset (`FileExt::seek_read`).
- `AlignedBuf` — heap buffer aligned to 4096 bytes; all device reads go
  through it.
- `read_resilient(src, offset, buf) -> BadRanges` — tries the whole buffer;
  on failure retries in 64 KiB blocks, then per sector; unreadable sectors are
  zero-filled and returned as bad ranges. After 8 consecutive bad sectors the
  remainder of the 64 KiB block is skipped as bad.
- `read_unaligned(src, offset, len)` — helper for arbitrary ranges.
- `list_disks()` (Windows) — physical drives 0..63 with size, model, serial,
  bus type, removable flag, sector sizes, whether the disk carries the system
  volume, and its partitions; each partition annotated with drive letter(s),
  label and file system of the *currently mounted* volume when there is one
  (`IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`, `GetVolumeInformationW`). Returns an
  empty list on other platforms.
- `parse_partition_table(&dyn BlockSource) -> PartitionTable` — pure: MBR
  primaries, EBR chain for logical partitions, GPT (primary header, CRC
  checked, falls back to the backup header), honouring the logical sector
  size. Works on disks and whole-disk images alike. Each partition also gets
  an `fs_hint` from its first sector (NTFS boot sector → cluster size etc.).
- `destination_check(path, source: &SourceInfo, scan_range) -> DestinationVerdict`
  — implements S2/S3: `Ok`, `OverlapsSource` (hard refusal), `SameDisk`
  (needs consent), `Unknown` (needs consent), plus free space.

### 5.3 `hds-carve`

- `sniff(&[u8]) -> Option<FileKind>` — magic check at the start of a buffer
  (the scanner calls this at every sector-aligned offset; it must be
  branch-cheap: first-byte dispatch, no allocation).
- `measure(kind, reader, max_len) -> Measured { Exact(len) | Probable(len) | Invalid }`
  — walks the format's own structure to find the true end of a *contiguous*
  file: JPEG (segment walk, then entropy-coded scan to EOI), PNG (chunk walk
  with CRC), GIF (block walk), BMP (header size), TIFF and TIFF-based RAW
  (IFD walk, furthest strip/tile/sub-IFD extent), PDF (`%%EOF` handling incl.
  incremental updates), ZIP family incl. OOXML/ODF/EPUB (local headers →
  central directory → EOCD; content sniffing chooses the extension), ISO-BMFF
  family mp4/mov/m4v/m4a/3gp/heic/cr3 (top-level box walk incl. 64-bit sizes;
  must contain `moov` or `meta`), RIFF family avi/wav/webp (header size, RF64
  aware), Matroska/WebM (EBML header + segment size), MP3 (ID3v2 + frame
  walk), 7z (start header + next-header offset/size, CRC), RAR 4/5 (block
  walk).
- `kinds_for_extension(ext) -> &[FileKind]` — used to cross-check recovered
  MFT files against their extension (S5).

### 5.4 `hds-core`

Modules: `scan`, `session`, `livefs`, `instances`, `geometry`, `catalog`,
`health`, `recover`, `image`, `carving`. Algorithms are in section 6.

### 5.5 `hds-engine`

Binary `hdsavior-engine`. Subcommands: `serve` (protocol server), `disks`,
`scan`, `info`, `ls`, `find`, `recover`, `image`, `carve`. One long-running
job (scan / carve / recover / image) at a time.

## 6. Algorithms

### 6.1 Scan (single pass)

Input: source, absolute byte range `[start, end)` (a partition, or the whole
disk), options. The range is read sequentially in 8 MiB aligned chunks with
`read_resilient`. At **every 512-byte offset** of each chunk (partitions that
start at sector 63 are not 1 KiB aligned):

1. `FILE` → `probe_file_record`; if the record crosses the chunk end it is
   re-read on its own. Fixups are applied, the record is parsed and stored as
   a compact `RawRecord` (no file content — resident data is re-read from the
   record's disk offset at recovery time).
2. `INDX` → index block (size from its header), parsed; its `$FILE_NAME`
   entries stored compactly (child ref, parent ref, name, flags, size, mtime)
   together with the block's disk offset and VCN.
3. NTFS boot sector (`"NTFS    "` at +3 and 0x55AA) → stored with its offset
   (finds old backup boot sectors after re-partitioning).
4. `hds_carve::sniff` → `(offset, kind)` carve hit.

Scan order: if a likely MFT location is known (the live boot sector's MFT
offset, else `start + 3 GiB`), that region is scanned first — forward until
64 MiB pass without a `FILE` hit — so that most of the tree is available
within seconds to minutes; then the rest of the range in ascending order,
skipping what is done. Scanned ranges are tracked in a range set so a scan can
be stopped early ("stop and show results") and resumed later.

Progress events are throttled (~4/s). Cancellation is checked per chunk.
Bad ranges are accumulated and reported.

### 6.2 Session

`ScanState` = source identity (kind, path, size, model/serial, range), done
range set, raw records, index entries, boot hits, carve hits, bad ranges,
counters. Serialized (serde + a pure-Rust binary codec, versioned header
`HDSC`) to a session file — at scan end, on stop, and periodically during
long scans (atomic temp-file + rename). Loading a session and reopening the
source (identity is checked) restores everything without rescanning.

### 6.3 Live file system (`livefs`)

If the scan range starts with a valid NTFS boot sector, the *current* file
system is read through its own structures: record 0 → `$DATA` runs and
`$BITMAP` of the MFT → the set of disk offsets holding records that are in
use *in the live file system*; record 6 (`$Bitmap`) → the set of clusters the
live file system has allocated (stored as a range set of absolute byte
ranges). Uses:

- Scanned records sitting in a live in-use MFT slot are tagged `live` and
  kept out of the catalog (they are not lost).
- Any old file whose extents intersect live-allocated clusters has been at
  least partly overwritten since the format → `overwritten`.

### 6.4 MFT instances

A scan can find records of more than one MFT: stale copies, an NTFS volume
inside a disk-image/VM file stored on the drive, leftovers of an even older
format. Records are grouped into *fragments* by
`origin = disk_offset − record_number × record_size` (records without a
self-number — NTFS < 3.1 — form their own fragment each and are never
linked by number). Fragments are assigned to *instances* greedily, largest
fragment first: a fragment joins the first instance whose record-number
ranges it does not overlap. Live records take part in the grouping.
Instance 0 (most records) is the main tree; other instances appear as
separate top-level virtual folders, hidden by default. Parent/child and
base/extension links are only made inside one instance.

### 6.5 Catalog build

Per instance:

1. **Base/extension merge.** A record with a non-zero base reference is an
   extension record; its attributes belong to the base record with that
   record number (sequence must match; if the base record is missing the
   group is kept as a nameless orphan). The unnamed `$DATA` fragments of the
   base and all its extensions are sorted by start VCN and concatenated.
   Sizes come from the fragment with start VCN 0. A gap, an overlap, or fewer
   clusters than the real size needs → `missing_fragments`. `$ATTRIBUTE_LIST`
   itself is not needed for this (every surviving extension record names its
   base), but when a resident list is present it is used to cross-check that
   no listed `$DATA` fragment is absent.
2. **Name.** Prefer the Win32 (1) / Win32+DOS (3) namespace, then POSIX (0),
   then DOS (2). Further hard links are ignored.
3. **Parent link.** Parent reference `(P, s)` → directory record `P` in the
   same instance with sequence `s` (or `s + 1` if that record is not in use —
   deleting bumps the sequence). Record 5 is the root: children of 5 attach
   to the tree root regardless of whether record 5 survived.
4. **Lost directories.** A parent with no surviving record becomes a
   synthetic folder. If a scanned index entry names that record number (and
   its sequence matches), the folder gets its real name and is attached to
   *its* parent by the same rules; otherwise it is called `Folder #P` and
   lives under the virtual folder `[Lost folders]`. Files with no usable
   parent reference go to `[Orphaned files]`.
5. **Cycles** in parent chains are broken by re-attaching to `[Lost folders]`.
6. System metafiles (record numbers < 24, and everything under `$Extend`)
   are omitted.
7. Per-directory aggregates (file count, byte total) are computed bottom-up.

Node kinds: `dir`, `file`, `lost_dir`, `virtual`, `carved`. Every node has a
stable numeric id for the lifetime of a loaded session.

### 6.6 Geometry (cluster size + volume offset)

A candidate is a pair `(cluster_size, volume_offset)`; `volume_offset` is the
absolute byte offset of old LCN 0.

- **Index votes (primary evidence).** An `INDX` block found at disk offset
  `D` with block VCN `v`, whose entries name directory `P` as parent, belongs
  to directory `P`'s `$INDEX_ALLOCATION`. With that directory's runlist, for
  each cluster size `cs` in 512…2 MiB: the block's byte position inside the
  attribute is `v × cs` when `cs ≤ index_block_size`, else `v × 512`; map it
  through the runlist to a volume-relative byte `B`; vote for
  `(cs, D − B)`. Only the true pair collects votes from many blocks.
- **Boot sectors.** The live boot sector at the range start, and any other
  NTFS boot sector found by the scan (a backup boot sector at offset `X`
  with `total_sectors = n` implies a volume starting at `X − n × bps`).
- **Default.** `(4096, range.start)`.

Candidates are ranked by index votes, then validated by reading the first
sector of up to 200 in-use, non-resident files with well-known extensions
and counting signature matches. The best candidate is selected automatically;
all candidates with their evidence are reported, and the user can override
(no rescan needed — the catalog and health flags are simply rebuilt).

### 6.7 Health

Flags per file: `deleted`, `torn_record` (fixup mismatch), `missing_fragments`,
`out_of_bounds` (run beyond the volume/source), `overwritten` (intersects
live-allocated clusters, or lost an overlap conflict), `overlap` (shares
clusters with another in-use file and won/unclear), `compressed`,
`encrypted`, `sparse`, `no_data`, `bad_sectors`, `signature_mismatch`,
`signature_ok`.

Overlap conflicts (sort all extents, sweep): in-use beats deleted; between two
in-use files the one created later (by `$STANDARD_INFORMATION` creation time)
is presumed valid and the other is flagged `overwritten`; ties → both
`overlap`.

Rating shown to the user: `good`, `uncertain`, `damaged`, `unsupported`.

### 6.8 Recovery

1. Expand the selection (`include`/`exclude` node ids; folders are recursive;
   the nearest marked ancestor wins) into a file list.
2. Destination check (S2/S3), free-space check.
3. Output paths: original tree below the destination; each component
   sanitized for Windows (`<>:"/\|?*` and control characters → `_`, trailing
   dots/spaces trimmed, reserved device names prefixed with `_`, max 255
   UTF-16 units), case-insensitive collisions resolved with ` (2)`, ` (3)`…;
   long paths via the `\\?\` prefix.
4. Files are processed in ascending order of their first cluster (one sweep
   of the disk head instead of random seeks).
5. Content: resident → re-read the MFT record at its disk offset, re-apply
   fixups, copy the attribute content. Non-resident → for each run read
   `volume_offset + lcn × cluster_size` in large aligned chunks with
   `read_resilient`; sparse runs produce zeros; bytes at or beyond the
   initialized size are written as zeros; the output is truncated to the real
   size.
6. Creation/modification/access times are restored from
   `$STANDARD_INFORMATION`; directory times are set after their contents.
7. Each file's first bytes are checked against its extension
   (`signature_ok` / `signature_mismatch`).
8. A report (`HDSavior-report.csv` in the destination root) lists every file:
   source record, path, size, status (`ok`, `ok_unverified`, `warning:<flags>`,
   `skipped:<reason>`, `failed:<error>`). Files that are `unsupported` are
   skipped. Cancelling removes the partially written file.

### 6.9 Carving

Optional job after a scan. Carve hits that are (a) cluster-aligned under the
chosen geometry and (b) not inside any extent claimed by a catalog file are
measured with `hds_carve::measure`, bounded by the next claimed extent. Valid
results become `carved` nodes under `[Carved files]/<type>/` named
`<type>_<offset>.<ext>`; `Exact` → `good`, `Probable` → `uncertain`. Carved
files are recovered like any other file with a single run.

### 6.10 Imaging

Sequential copy of a byte range of the source to a file, with `read_resilient`
(bad sectors zero-filled and listed in `<image>.bad.txt`), destination guard,
free-space check, progress and cancel; resumable from the existing image
length.

## 7. UI (Flutter, Windows desktop)

1. **Source** — disks and partitions as cards (model, size, letter, label,
   file system), "Scan" per partition or whole disk; "Open disk image…",
   "Open saved session…", "Create disk image…". Permanent notice: the disk
   being rescued is never written to; stop using it.
2. **Scan** — progress, speed, ETA, counters (records, files, folders, index
   blocks, signatures, bad sectors); "Stop and show results".
3. **Results** — lazy folder tree with tri-state checkboxes; virtualized file
   table (name, size, modified, status); search and filters (show deleted,
   hide damaged, type); geometry chip with evidence and manual override;
   "Find more files by signature" (carving); details + preview pane (images,
   text/hex); selection summary and "Recover…".
4. **Recover** — destination picker validated by the engine, options,
   progress, result summary, open folder / open report.
5. **Image** — range, destination, progress.

The UI holds no file data: it pages through the engine (`list_children`,
`search`) so that a million-file catalog stays responsive.

## 8. Testing

1. Unit tests in every crate; randomized robustness tests for all parsers
   (arbitrary and mutated input must never panic or hang).
2. `hds-testkit`: an independent synthetic NTFS image builder (written from
   the on-disk specification, not sharing code with `hds-ntfs`) for cases real
   drivers rarely produce on demand: extension records + attribute lists,
   4 KiB records, pre-3.1 headers, sparse runs, initialized < real size, torn
   fixups, duplicate record numbers, embedded second MFT, parent cycles,
   volume at a non-zero offset, every cluster size.
3. Real-driver fixtures on Linux: `mkntfs` + `ntfs-3g` mount → populate
   (nested folders, Unicode and long names, resident/small/large files, a
   heavily fragmented large file, deleted files) → record a SHA-256 manifest →
   quick re-format → optionally write new files → scan → recover → compare.
   Property checked: every file whose record survived is recovered
   bit-identical at its original path, and nothing reported `ok` differs from
   the manifest.
4. Windows CI (GitHub Actions, `windows-latest`, every push): build, unit
   tests, then the same round trip against a **real Windows-formatted VHD**
   through `\\.\PhysicalDriveN`, then `flutter build windows`, bundling and
   artifact upload.
5. Flutter: widget tests against a fake engine; an integration test driving
   the real engine on a fixture image (Linux).

## 9. Known limitations

- Files whose MFT record was overwritten lose their name and path; they can
  only come back through carving, and only if unfragmented.
- Top-level folder names are often unrecoverable (their records are among the
  first overwritten and the old root index is overwritten by the new one);
  their contents appear under `[Lost folders]/Folder #N`.
- Volumes formatted by Windows NT4/2000 (NTFS < 3.1) store no record number
  in the record; such files are recovered but not arranged in a tree.
