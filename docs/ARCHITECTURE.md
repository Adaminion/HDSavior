# HDSavior — Architecture

HDSavior rescues files from hard disks whose NTFS volume was accidentally
(quick-)formatted as NTFS again. Windows only, plain HDDs, no encryption, no
NTFS compression.

This document, [ALGORITHMS.md](ALGORITHMS.md) and [PROTOCOL.md](PROTOCOL.md)
are the contract every component is built against. Verified reference facts
(with sources and experiments) are in [notes/](notes/).

---

## 1. Scope

In scope

- Source: a physical disk (`\\.\PhysicalDriveN`), one partition of it, or a raw
  image file of either.
- One sequential pass over the source that collects surviving MFT `FILE`
  records, directory index data, NTFS boot sectors and file-signature hits.
- Rebuilding the old folder tree, separately for every file system found.
- Recovering file contents through runlists, including files whose runlist is
  spread over several MFT records (`$ATTRIBUTE_LIST`), sparse runs and the
  initialized-size rule.
- Signature carving for files whose MFT record is gone (contiguous files).
- Sector-by-sector imaging of the source to a file on another disk.
- Saved scan sessions: no rescan needed to recover more files later.

Out of scope — detected, flagged `unsupported` and never written as if intact

- LZNT1- and WOF-compressed files, EFS-encrypted files, cloud placeholders and
  deduplication stubs (their content is not stored as plain clusters).
- File systems inside a disk-image file on the drive: shown, but the way to
  their files is to recover the image file and open it as a source.
- Alternate data streams (ignored).

Not recoverable by design — warned about

- Devices that discard data on format: SSDs and other TRIM-capable devices,
  **including many SMR hard disks**; and Vista+ *full* formats (zero-filled).
- BitLocker volumes, Storage Spaces, dynamic disks, ReFS/exFAT/FAT sources:
  the scan runs, but writes since the format cannot be detected on a
  non-NTFS current volume.
- HDSavior never repairs a disk in place. It only copies data out.

## 2. Safety invariants

Hard requirements. A reviewer must be able to verify each by reading the code.

- **S1 — The source is opened for reading only, by one function.**
  `hds_io::open_device(path, Query | Read)` is the only place a device is
  opened: `Query` = desired access 0 (enumeration, volume handles of the
  guard), `Read` = `GENERIC_READ` (the selected source). Never
  `GENERIC_WRITE`, never `FILE_FLAG_OVERLAPPED`. All control codes go through
  one wrapper with a closed list: `IOCTL_DISK_GET_LENGTH_INFO`,
  `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`, `IOCTL_STORAGE_QUERY_PROPERTY`,
  `IOCTL_STORAGE_GET_DEVICE_NUMBER`, `IOCTL_STORAGE_CHECK_VERIFY2`,
  `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`. No lock, dismount, offline or
  attribute change, ever. The `BlockSource` trait has no write method.
- **S2 — The rescued area is read-only territory.** (a) Every read made on
  behalf of a session stays inside its session range; a run that maps outside
  is `out_of_bounds`, yields zeros and can never be reported OK. (b) Nothing
  the product writes may land inside the *protected ranges*: recovery output,
  image files, session files, reports, temporary files, and the UI's own
  data. The protected ranges are the session range (for imaging: the imaged
  range) and, as soon as a catalog exists, the *lost-volume ranges* — the
  extent of every lost file system whose geometry is validated (ALGORITHMS
  F5), which after a re-partitioning reaches into other partitions of the
  disk. Every output location is resolved **by handle** to its volume and
  physical-disk extents (through VHD backing files, recursively) before
  anything is created. An overlap is refused; there is no override. The
  guard runs again at every job start, because the ranges grow when a
  catalog is built or a geometry changes.
- **S3 — Same physical disk needs explicit consent.** A location on the
  source disk outside the protected ranges is refused unless the caller
  passes `allow_same_disk`; before a catalog exists nobody knows how far the
  lost volume reached, and the message says so. A location whose disk cannot
  be determined is refused unless the caller passes `allow_unknown_disk`;
  the two consents are separate. Exception: session files on the Windows
  system volume need no consent (S2 still applies to them).
- **S4 — Image-file sources** are opened read-only with share mode *read*, so
  the kernel refuses any writer, truncation or deletion while they are the
  source.
- **S5 — No silent corruption.** A file is reported OK only if every byte came
  from a successfully read sector at a location justified by its runlist, its
  record was unchanged since the scan, and nothing indicates that the
  location was overwritten. Everything else is reported per file.
- **S6 — Output never follows links and never replaces.** Output paths are
  built from the canonical final path of the destination root. Below it the
  engine never enters or writes through a reparse point, creates files only
  with create-new semantics, and renames only without replacing. The two
  flows that reopen a file of their own (appending to a session file,
  resuming an image) open it without following links, accept only a regular
  file that is not a reparse point, and never truncate on open. All
  file-system writes of the engine go through `hds_io::out`.
- **S7 — A vanished device is not a bad sector.** When the source stops
  answering, the job ends with `source_lost`; nothing is recorded as scanned
  or as bad, no image is extended, no zero-filled file is written.

The UI adds: a permanent notice to stop using the rescued disk, and
read-only-computed `source_warnings` (the range is a mounted volume, a page
file lives on it, the disk carries the system volume, TRIM is supported).

## 3. Background: what a quick format leaves behind

Measured and sourced in [notes/quick-format-behaviour.md](notes/quick-format-behaviour.md).

- Quick format plus the automatic first mount on Windows 7–11 rewrite: both
  boot sectors (the backup is the last sector of the partition); MFT records
  0–255 (16 by format, the rest by the driver; unused slots are blank `FILE`
  records or zeros depending on the Windows version); `$MFTMirr`; the whole
  `$LogFile`; `$Bitmap`; `$UpCase`, `$AttrDef`, `$Secure`; the root
  directory's first index block; and some 22–25 MiB of TxF log, `$Repair`
  streams and System Volume Information. Everything else stays — on devices
  that do not discard on format.
- If the old volume was itself formatted by Windows XP or later on the same
  partition, the new `$MFT` lands exactly on the old one, so **about the
  first 220 files and folders ever created lose their MFT record** —
  including old record 0 (the MFT's own runlist) and the root directory. All
  later records survive. Otherwise the old MFT is elsewhere and survives
  entirely.
- Because old record 0 is normally gone, the old MFT cannot be walked through
  its runlist; a fragmented MFT is only found completely by scanning the
  whole range for `FILE` signatures.
- Every earlier format on the same partition put its MFT in the same place:
  leftovers of older file systems sit directly behind the lost one's records.
- Runlists hold cluster numbers relative to the start of the *old* volume in
  units of the *old* cluster size; both are detected, not assumed.
- Windows keeps writing to the new, mounted volume. Those writes are visible
  in the new file system's own bitmap and MFT, which the engine reads to tell
  what has been overwritten.

## 4. Components

```
┌───────────────────────────────┐        JSON lines over stdin/stdout
│ Flutter Windows app (app/)    │ ─────────────────────────────────────┐
│ UI only: no disk access       │                                      │
└───────────────────────────────┘                                      ▼
                                        ┌──────────────────────────────────────┐
                                        │ hdsavior-engine.exe (Rust, engine/)  │
                                        │ CLI + `serve` mode                   │
                                        │  hds-core   scan, catalog, queries   │
                                        │  hds-recover extraction, imaging     │
                                        │  hds-ntfs   on-disk structure parser │
                                        │  hds-io     block sources, disks,    │
                                        │             destination guard, output│
                                        │  hds-carve  signatures + validators  │
                                        └──────────────────────────────────────┘
```

- The Flutter app is declared `requireAdministrator` in its Windows manifest
  (one UAC prompt at start; raw disk reads require it). It starts
  `hdsavior-engine.exe serve` as a child process, which inherits the elevated
  token, and talks to it over the child's stdin/stdout. A crash of the engine
  cannot take the UI down. The UI cancels jobs with `job_cancel`; killing the
  child is a last resort and can leave one `~hds-*.part` file behind.
- The engine is also a complete command-line tool, so it can be tested
  headlessly against image files on any OS.
- The engine is pure Rust (no C dependencies) to keep Windows builds simple.

Repository layout

```
docs/                    ARCHITECTURE.md, ALGORITHMS.md, PROTOCOL.md, notes/
engine/                  Cargo workspace
  crates/hds-ntfs/       pure parsing, no I/O, never panics on any input
  crates/hds-io/         BlockSource, image files, Windows raw devices, disk enumeration,
                         MBR/GPT parsing, destination guard, output primitives
  crates/hds-carve/      signature sniffing + per-format length/validity walkers
  crates/hds-recover/    extraction engine: content reader, recovery writer + report, imaging
  crates/hds-core/       scanner, session, current volumes, instances, catalog, geometry,
                         health, queries, carving
  crates/hds-engine/     binary `hdsavior-engine`: CLI + JSON-lines server
  crates/hds-testkit/    dev-only: synthetic NTFS image builder for tests
app/                     Flutter app (Windows desktop only)
scripts/                 build.ps1, windows/ (CI round trip), linux/ (fixtures, e2e)
.github/workflows/       CI
```

## 5. Engine crates

### 5.1 `hds-ntfs` (pure parser)

Input is always a byte slice; nothing here performs I/O. Every function is
total: arbitrary or hostile bytes produce `Err`/`None`, never a panic,
arithmetic overflow, out-of-bounds access or unbounded loop.
[notes/ntfs-on-disk.md](notes/ntfs-on-disk.md) is the field-level reference.

- `parse_boot_sector` — OEM id `"NTFS    "` at 0x03, 0x55AA trailer. Bytes per
  sector: a power of two in 512..=4096. Sectors per cluster (u8 at 0x0D):
  `1..=0x80` must be a power of two; `0xF4..=0xFF` means `2^(256 − v)`;
  anything else is invalid; cluster size at most 2 MiB. MFT-record and
  index-block size (i8 at 0x40 / 0x44): positive = clusters, negative =
  `2^(−v)` bytes with `9 ≤ −v ≤ 16`. Also: total sectors, hidden sectors, MFT
  and MFTMirr LCN, serial.
- `probe_file_record` — cheap check on the first sector: `FILE` magic, update
  sequence offset/count sane, allocated size a power of two in
  1024..=4096 (upper bound configurable to 65536), `count == size/512 + 1`.
- `apply_fixups` — the stride is always 512 bytes; a mismatch is reported
  (`torn`) and parsing continues best-effort.
- `parse_file_record` — header (LSN, sequence, link count, flags `0x1` in use
  and `0x2` directory, used/allocated size, base reference, record number at
  0x2C exactly when the update sequence offset is ≥ 0x30) and attributes,
  each located only through its own offset fields (a non-resident header is
  0x40 or 0x48 bytes): `$STANDARD_INFORMATION` (48 or 72 bytes),
  every `$FILE_NAME`, every `$DATA` (named and unnamed; resident as offset and
  length inside the record, non-resident with start VCN, last VCN, runs,
  flags, sizes), `$ATTRIBUTE_LIST` (resident entries decoded, or
  non-resident runs), `$INDEX_ROOT:$I30` (entries decoded with the index-entry
  walker), `$INDEX_ALLOCATION:$I30` (start VCN, runs), `$BITMAP`,
  `$VOLUME_NAME`, `$REPARSE_POINT` (the 32-bit tag).
- `decode_runlist` — header byte: low nibble = size of the length field, high
  nibble = size of the offset field. Length: signed, must be > 0. Offset:
  signed, added to the LCN of the last non-sparse run (0 before the first);
  offset size 0 = sparse run, which leaves that LCN unchanged; an offset of
  value 0 with size > 0 is a real run. The list ends at the first `0x00`
  header byte. Rejects field sizes over 8, negative LCNs and overflow.
- `parse_index_block` — `INDX` magic, fixups, block VCN at 0x10, node header at
  0x18 (offsets relative to it); entries: file reference, entry length, key
  length, flags (`0x1` sub-node, `0x2` last). Only entries whose key validates
  as a `$FILE_NAME` are returned.
- `FileRef`, FILETIME conversion with checked arithmetic, FNV-1a 64.

### 5.2 `hds-io`

[notes/windows-raw-io.md](notes/windows-raw-io.md) holds the verified recipes
(compiled snippets, feature flags, error codes).

```rust
pub trait BlockSource: Send + Sync {
    fn size(&self) -> u64;                  // bytes
    fn sector_size(&self) -> u32;           // logical: 512 or 4096
    fn physical_sector_size(&self) -> u32;
    /// Positional read; offset and buf.len() must be multiples of sector_size();
    /// never called at or beyond size().
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()>;
    fn info(&self) -> SourceInfo;
    fn is_alive(&self) -> bool;             // device still answers (S7)
}
```

- `open_device` (S1), `RawDisk` (Windows; accepts only `\\.\PhysicalDrive<N>`,
  verified with `IOCTL_STORAGE_GET_DEVICE_NUMBER`: device type disk,
  partition 0), `ImageFile` (S4; sector size 512 unless a GPT header shows
  4096 or the caller says so).
- `RangedSource` — wraps a source together with a session range and refuses
  every read outside it (S2a is enforced structurally: `hds-core` and
  `hds-recover` only ever hold a `RangedSource`). Offsets stay absolute.
- `RangeSet` — sorted set of byte ranges (done set, bad ranges, live-allocated
  ranges, overwritten set); serializable.
- `AlignedBuf` — heap buffer aligned to 4096 bytes; all device reads use it.
- `read_resilient(src, offset, buf, cancel) -> ReadOutcome` — classifies OS
  errors: *media* (zero-fill and report as bad ranges, after showing the
  device is alive), *device lost* (`SourceLost`), *caller* (internal error),
  *resource* (retry smaller), *cancelled*. A failed chunk is retried in
  64 KiB blocks, then per physical sector; after 8 consecutive bad sectors
  the rest of the block is skipped as bad. Cancellation is polled between
  retries.
- `list_disks()` (Windows; empty elsewhere) — query handles only; size from
  `GEOMETRY_EX`; gaps in numbering tolerated; per disk: model, serial, bus,
  removable, logical/physical sector size, `media` (seek penalty), `trim`,
  flags (`virtual`, `spaces`, `dynamic`, `no_media`), `is_system`, partitions
  with drive letters, label and file system of the mounted volume.
- `parse_partition_table(&dyn BlockSource)` — pure: a file-system boot sector
  in sector 0 is tested **before** the MBR signature (table `none`, one
  pseudo-partition covering the source); MBR primaries; EBR chain; GPT
  (primary header with CRC, backup header as fallback) honouring the sector
  size. Each partition gets `fs` sniffed from its first sector (`ntfs`, `fat`,
  `exfat`, `refs`, `bitlocker`, `other`).
- `destination_check(path, purpose, source, protected, need_bytes)` —
  S2/S3. `protected` is a list of absolute byte ranges of the source (the
  caller passes the session range first, then the lost-volume ranges). The
  path must be an absolute drive or UNC path. Resolution is by handle: open
  the deepest existing ancestor with access 0, take its final path with a
  volume GUID, open the volume, read all disk extents; for virtual disks
  resolve the host volumes recursively. Creates nothing. Verdicts: `ok`,
  `overlaps_source` (with the index of the first protected range that was
  hit), `same_disk`, `unknown_disk` (extents unavailable, Storage Spaces, a
  UNC path to this machine), `not_found`, `not_writable`, `invalid_path`;
  plus free space, file-system name, its maximum file size, and the
  destination's extents on the source disk, so that `hds-core` can say how
  many catalog files lie there.
- `hds_io::out` (S6) — the only code that writes files: canonical root,
  one-level directory creation that refuses reparse points, create-new part
  files, rename without replace, timestamp setting, leftover cleanup.

### 5.3 `hds-carve` (built)

```rust
pub enum FileKind { Jpeg, Png, Gif, Bmp, Tiff, Pdf, Zip, Mp4, Heic, Cr3, Avi, Wav, Webp, Mkv, Mp3, SevenZip, Rar }
pub const SNIFF_LEN: usize = 64;
pub fn sniff(head: &[u8]) -> Option<FileKind>;
pub trait ReadAt { fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize>; }
pub struct Measured { pub len: u64, pub confidence: Confidence, pub ext: &'static str, pub end_verified: bool }
pub fn try_measure(kind, r: &dyn ReadAt, max_len: u64, limits: &Limits) -> Result<Measured, MeasureError>;
pub fn matches_extension(head: &[u8], ext: &str) -> Option<bool>;
```

`sniff` is strict (≥ 32 fixed bits plus header validation; about one false
candidate per 2 TiB of random data; ~1.5 ns per sector). `try_measure` walks
the format's own structure to the end of a contiguous file. `end_verified`
tells whether bytes at the computed end were structurally confirmed (JPEG
EOI, PNG IEND with CRCs, a box or chunk chain landing exactly, ZIP end
record, archive end headers) as opposed to a length taken from a header
alone. An end concluded because no more data could be read is never
verified, and JPEG and GIF ends are verified by decoding the scan or LZW data
to exactly the frame's size. `matches_extension` is the lenient check used for S5. The crate's
documentation lists every format with its confidence and known gaps.

### 5.4 `hds-recover`

The extraction engine. It knows nothing about catalogs: it executes *plans*.

- `ContentReader` — reads the content of one file described by a
  `FileSource`: `Resident { record_offset, record_size, attr_offset, attr_len,
  hash }` or `Extents { runs: absolute byte ranges or holes, real_size,
  initialized_size }`, through a `RangedSource`, with the rules of
  ALGORITHMS H7 (zeros for holes, for out-of-range runs and beyond the
  initialized size; hash check for resident data). Used by recovery,
  `read_file`, geometry validation and carving.
- `recover(plan, dest, options, observer)` — ALGORITHMS H3–H12: a plan is a
  list of items (`FileSource`, relative output path as name components,
  times, extension, pre-read flags, node id) plus the folders to create;
  writes through `hds_io::out`, verifies signatures, appends the report,
  reports progress, honours cancel.
- `image(source, range, dest, options, observer)` — ALGORITHMS J.

### 5.5 `hds-core`

Modules: `scan`, `session`, `live`, `instances`, `catalog`, `geometry`,
`health`, `query` (listing, search, selection, plan building), `carving`.
Rules: [ALGORITHMS.md](ALGORITHMS.md). Scale targets: ten million records on a
16 TB disk — no per-record heap allocation in the scan state, one name arena
for search, sorted listings cached per (node or search arguments, sort,
filters) so further pages cost O(limit).

### 5.6 `hds-engine`

Binary `hdsavior-engine`. `serve` runs the protocol server. Every other
subcommand is a thin wrapper around the same protocol commands and prints
their results and events as JSON, one object per line, on stdout; logs go to
stderr. Exit code 0 = ok, 1 = the job failed, 2 = usage error.

```
hdsavior-engine serve
hdsavior-engine disks
hdsavior-engine scan    <source> --session <file> [--kind disk|image] [--partition K | --start N --end N]
                        [--no-carve-hits] [--resume | --force]
hdsavior-engine info    --session <file> [--source <path>]
hdsavior-engine ls      --session <file> [--node ID | --path P] [--deleted] [--limit N]
hdsavior-engine find    --session <file> <query> [--ext a,b] [--deleted] [--limit N]
hdsavior-engine recover --session <file> --dest <dir> [--source <path>] (--all | --include IDs) [--exclude IDs]
                        [--include-deleted] [--skip-damaged] [--allow-same-disk] [--allow-unknown-disk]
                        [--on-exists rename|skip|overwrite]
hdsavior-engine carve   --session <file> [--source <path>] [--kinds a,b]
                        [--allow-same-disk] [--allow-unknown-disk]
hdsavior-engine image   <source> --dest <file> [--partition K | --start N --end N] [--resume]
                        [--allow-same-disk] [--allow-unknown-disk]
```

- `scan` refuses to replace an existing session file unless `--force` is
  given; `--resume` continues it. A resumed session too young to be
  recognised by its records (fewer than 8, ALGORITHMS B4) is attached to
  the source named on the command line with `force`; any other mismatch
  fails.
- `recover` and `carve` save the session back to `--session`
  (`session_save`, with the consent options given) after the job: a loaded
  file is not always continued in place, and what the job added (carved
  files, what the reads found) must not be lost.
- `info` fails (exit code 1) when `--source` is given and is not the
  session's source.

`docs/protocol-examples.jsonl` is a transcript recorded from the real engine
on the `basic` fixture; the engine's tests regenerate and compare it, and the
Dart fake engine replays it, so both sides stay in step.

## 6. UI (Flutter, Windows desktop)

1. **Source** — disks and partitions as cards (model, size, letter, label,
   file system, warnings such as TRIM or system disk); "Scan" per partition
   or whole disk; "Open disk image…", "Open saved session…", "Create disk
   image…". Permanent notice: the disk being rescued is never written to;
   stop using it.
2. **Scan** — progress, speed, ETA, counters; phase (`mft_probe` → `sweep` →
   `analyzing`); "Stop and show results" (the phase change to `sweep` is the
   cue that most of the tree is already found). A stopped scan can be resumed.
3. **Results** — the main file system's tree plus one entry per other file
   system found; lazy folder tree with tri-state checkboxes; virtualized file
   table (name, size, modified, status); search and filters (deleted,
   damaged, type); a geometry chip with its evidence and a manual override;
   "Find more files by signature" (carving); details and preview pane;
   selection summary and "Recover…". When files lie outside the scanned
   range the page offers "Scan the whole disk". Per file system the user can
   overrule the engine's verdict ("this is the one I want", "treat as a
   separate volume").
4. **Recover** — destination (picker or typed path, including UNC: mapped
   drive letters are invisible to an elevated program), validated by the
   engine with the reason for any refusal; options; progress; summary with
   the report path.
5. **Image** — range, destination, progress.

The UI holds no file data: it pages through the engine. It never writes into
the protected ranges either (S2): it asks `check_destination` with purpose
`app_data` for its own directories when a source is opened and again
whenever a catalog arrives or a geometry changes.

## 7. Testing

1. Unit tests in every crate; randomized robustness tests for all parsers
   (arbitrary and mutated input must never panic or hang).
2. `hds-testkit`: a synthetic NTFS image builder written from the on-disk
   notes, sharing no code with `hds-ntfs`, for what real drivers do not
   produce on demand: resident attribute lists, VCN 0 in an extension record,
   deleted files with extension records, a free extension next to a live
   base, 4 KiB records, pre-3.1 headers mixed with numbered ones, sparse
   runs, initialized < real size, torn fixups, duplicate record numbers,
   parent cycles, reparse tags, DOS aliases, every cluster size including
   64 KiB with one large directory, a million-record volume for scale.
3. Real-driver fixtures on Linux (`scripts/linux/make-fixtures.sh`: `mkntfs` +
   `ntfs-3g`, ground-truth manifests). Because `mkntfs` rewrites far fewer
   records than Windows, fixtures overwrite old MFT slots up to 255 after the
   re-format (zeros or blank records) unless they exist to test something
   else. Scenarios include: basic, fragmented (attribute list), wiped
   (carving and lost folders), cluster-change, partitions at offsets (MBR,
   MBR at sector 63, GPT), post-write, small and big clusters, 4 KiB
   records, two generations on one partition, an NTFS image file stored on
   the volume, and an old volume larger than the scanned partition (scanned
   twice: as a partition — the out-of-range files are counted, nothing
   outside the range is read, the other partition is refused as a
   destination — and as a whole disk — the files come back bit-identical).
   Instance separation has its own set, each with the expected outcome:
   one file system with a "delete the oldest folder, copy a new one" history
   and one with the oldest files renamed last (one instance, nothing older);
   the same histories inside the newer of two generations; a newer
   generation of about 20 records; an older tail that is larger than the
   newer generation (the newer one is `main`); files created and deleted
   after the accident; an image file with more records than its host; a
   small image file (under 256 records) on a volume with a fragmented
   `$MFT`; two volumes with folders at equal record numbers above 255; a
   stale file of an older partition layout that covers the formatted
   volume's `$MFT`, scanned as a whole disk; a re-partitioned disk whose old
   volume is intact at another offset (its tree is shown, not hidden as
   current); copies of MFT blocks inside a file (strays, not a second
   tree); a volume with no index blocks and no known file types. The
   re-format always starts in a later whole second than the last write to
   the old volume.
   Properties: every file whose record and content survived is recovered
   bit-identical; at its original path when all its ancestors survived,
   otherwise under a lost or index-named folder; nothing reported `ok`
   differs from the manifest; nothing outside the session range is read.
4. Windows CI (GitHub Actions, every push): build and unit-test on
   `windows-latest`; then the round trip on a **real Windows-formatted
   virtual disk** of at least 16 GiB through `\\.\PhysicalDriveN` (populate,
   `format /Q`, scan, recover, compare), asserting the facts that could not
   be verified without Windows (MFT placement, records 0–255 gone, record 256
   intact, every slot below the new `$MFT`'s initialized size rewritten,
   resident attribute lists produced by real NTFS, destination guard on a
   volume of that disk, junction, symlink, dangling file symlink and mounted
   folder inside the destination, timestamp edge values, the MFT-change time
   after a move and a `robocopy` between volumes (measured: copies keep the
   source's MFT-change time, which is why ALGORITHMS uses the *record time*
   with the `$FILE_NAME` times), the slot a new file gets
   after low slots were freed); then `flutter build windows`, bundling and
   artifact upload.
5. Flutter: widget tests against a fake engine that replays the recorded
   protocol transcript; an integration test driving the real engine on a
   fixture image (Linux).

## 8. Known limitations

- Files whose MFT record was overwritten lose their content location. They
  can come back through carving if they were stored unfragmented and are of a
  supported type; files small enough to live inside their MFT record are
  gone for good.
- Folders whose record number was below 256 lose their name unless an index
  entry for them survives (later root index blocks, the index root of a
  surviving parent folder, stale copies). Their contents appear under
  `[Lost folders]/Folder #N`.
- Carving cannot notice fragmentation in formats whose length comes from a
  header alone; such files are rated `uncertain`.
- Leftovers of a file system from *before* the previous format are shown in
  their own tree and rated at most `uncertain`: many of their clusters have
  been reused. They are separated only where the records prove a second file
  system; leftovers nothing contradicts stay in the main tree (the modified
  date tells them apart).
- If the lost file system held fewer files than a new MFT has slots (about
  220 on current Windows), none of its records survive; whatever the tree
  then shows belongs to an older file system, and only carving reaches the
  lost files.
- Files written and deleted again after the accidental format leave no trace
  once their record has been reused; what they overwrote can look intact.
  Files of a type the engine can check are caught by the signature check;
  others are reported `ok_unverified`.
- Volumes formatted by Windows NT4/2000 store no record number in the
  record; such files are arranged in a tree only where their position among
  numbered records reveals it.
- Nothing can be recovered from a device that discarded its data on format
  (TRIM), or after a full format.
