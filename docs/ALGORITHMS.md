# HDSavior — Algorithms

Normative rules for the engine (`hds-core`). The overview, safety invariants
and crate boundaries are in [ARCHITECTURE.md](ARCHITECTURE.md); the wire
protocol is in [PROTOCOL.md](PROTOCOL.md); verified on-disk facts are in
[notes/ntfs-on-disk.md](notes/ntfs-on-disk.md),
[notes/quick-format-behaviour.md](notes/quick-format-behaviour.md) and
[notes/windows-raw-io.md](notes/windows-raw-io.md).

Vocabulary

- *Session range* — the absolute byte range `[start, end)` of the source that
  the user asked to scan. It never changes for the life of a session.
- *Reference* `(N, s)` — a 48-bit record number and 16-bit sequence number.
  `next(s)` is `s + 1`, with `next(0xFFFF) = 1`.
- **Reference rule.** A reference `(N, s)` *resolves* to record `R` with number
  `N` when `R` is in use and `R.seq == s`, or `R` is not in use and
  `R.seq == next(s)` (freeing a record bumps its sequence; reuse does not).
- *Record time* — the latest of a record's `$STANDARD_INFORMATION` MFT-change
  time (its third timestamp) and the creation, modification and MFT-change
  times of each of its `$FILE_NAME` attributes; last-access times never count,
  and a record with neither has none. It stands for "when this record was
  last written on its volume". The MFT-change time alone does not: Windows' copy
  tools (robocopy, Copy-Item and Explorer's copy, Move-Item across volumes) copy
  the source's MFT-change time to the tick, so a volume filled from a backup holds
  records whose MFT-change times are years old. `$FILE_NAME` times are written
  when the name is created on the volume and cannot be set through the file API;
  for files that were not copied they are never later than the MFT-change time,
  so the record time equals it (measured on Windows 11 24H2's kernel:
  docs/notes/quick-format-behaviour.md, 7a).
- *Entry time* — of an `IndexEntry`: the latest of the creation, modification
  and MFT-change times of its `$FILE_NAME` copy. The last-access time is left
  out: Windows keeps an entry's times in step with the child's
  `$STANDARD_INFORMATION`, last access included, so it says when a folder was
  listed or a file read. For the same reason the entry of a file copied with its
  source's times may carry those old times instead of the copy time; C4 then
  excludes it by its source (a live-allocated block or a current-volume record),
  not by its time.

---

## A. Scan

**A1. Reading.** The session range is read in 8 MiB sector-aligned chunks
through `hds_io::read_resilient`. No byte outside the session range is ever
read, by the scan or by anything that later works on its results (S2).

**A2. Order.** Probe offsets are scanned first, then the rest of the range in
ascending order, skipping what is done. Probe offsets, duplicates removed:
the MFT offset from every valid NTFS boot sector at the range start or at a
partition start inside the range; and for each such partition start `P` (the
range start if there is no partition table) `P + d` with `d = min(3 GiB, size / 3)`
rounded down to 4 KiB, and `P + d` rounded down to whole GiB when `d ≥ 1 GiB`
(Windows' own quick format, measured: 256 MB and 1 GB volumes put the `$MFT`
at size / 3, a 4 GB volume at 1 GiB, 16 GB and 64 GB volumes at 3 GiB, with 4K
and 64K clusters alike). A probe reads forward until 64 MiB pass without a `FILE` hit.
Phase names: `mft_probe`, then `sweep`. The early phase exists so that
stopping early is worthwhile; no catalog exists while a scan runs.

**A3. Per 512-byte offset** of every chunk (partitions that start at sector 63
are not 1 KiB aligned):

1. `FILE` → `probe_file_record`. If the record crosses the chunk end it is read
   on its own. Compute `hash` = FNV-1a 64 of the record bytes **as read**
   (before fixups). Apply fixups, parse. Counted and dropped: a record that is
   not in use and has no attributes (never used), and a record in which
   **every** 512-byte block fails the fixup check (Windows logs whole record
   images into `$LogFile`; such copies describe the file as it was when it
   was created and must never compete with the real record). Otherwise store
   a `RawRecord` (A4).
2. `INDX` → parse the index block (size from its header). Store one
   `IndexBlock` tuple (disk offset, block VCN, block size, LSN, owner = the
   parent reference shared by most entries) and, for entries **with the
   directory flag only**, an `IndexEntry` (A5).
3. NTFS boot sector (`"NTFS    "` at +3, 0x55AA at +510, parses) → `BootHit`
   (offset and parsed fields).
4. If carve hits are collected: `hds_carve::sniff` on the bytes at that offset
   → `CarveHit`, packed as `(offset / 512) << 8 | kind`. At most 64 million
   hits are kept; when the cap is reached further hits are dropped and the
   warning `carve_hits_capped` is raised.

**A4. RawRecord.** No per-record heap allocation: names and runs live in
session-wide arenas (target ≤ 200 bytes per record on average). Fields:
disk offset; record size; record number (absent for pre-3.1 headers); sequence;
header flags; link count; LSN; base reference; `hash`; fixup status;
`$STANDARD_INFORMATION` (four times, file-attribute flags) when present; every
`$FILE_NAME` (parent reference, namespace, flags, attribute id, name as UTF-16
units), and the latest creation, modification or MFT-change time over its
`$FILE_NAME`s (for the record time); the unnamed `$DATA` extents present in this record — resident
(attribute offset and length **inside the record**; content is not stored) or
non-resident (start VCN, header last VCN, runs, attribute flags, and
allocated / real / initialized size, which are meaningful only when the start
VCN is 0); whether a `$DATA` stream named `WofCompressedData` exists; the
`$INDEX_ALLOCATION:$I30` extents (start VCN, runs, allocated size);
`$ATTRIBUTE_LIST` presence and, when resident, its entries for types 0x80 and
0xA0; the reparse tag when a resident `$REPARSE_POINT` exists (a non-resident
one counts as tag unknown). Alternate data streams
are otherwise ignored. Directory entries found in a `$INDEX_ROOT:$I30` are
emitted as `IndexEntry` values with the record's offset and LSN as source.

**A5. IndexEntry** (directory entries only): child reference, parent reference,
namespace, name, the entry time (from its `$FILE_NAME` copy),
source disk offset (block or record), source LSN.

**A6. Read errors** are classified by `hds-io` (media / device lost / caller /
resource / cancelled). Media errors become bad ranges (zero-filled, counted,
never "done-and-clean"). *Device lost* ends the job with error `source_lost`:
the failing chunk is recorded neither as done nor as bad, the session is
saved, and the scan can be resumed after `session_attach_source`. Caller-class
errors are internal errors, never bad sectors.

**A7. Dead source.** If the first 1 GiB read contains no non-zero byte, the
warning `source_reads_zero` is raised (TRIM-capable device or full format).

**A8. Progress and cancellation.** Progress events at most 4 per second.
Cancellation is polled per chunk and between retries of a failing read.
`bytes_done` is the size of the done set, so a resumed scan starts where it
stopped. Counters `files` / `dirs` are in-use base records without / with the
directory flag.

## B. Session file

**B1. Contents.** Source identity (kind, path, size, model, serial), session
range, scan options, done range set, records, index entries, index blocks,
boot hits, carve hits, bad ranges, counters, the latest current-volume
snapshot (C), the first-sector heads read for geometry checks (F3), the
user's overrides (geometry per instance, F7; main instance and embedded
label, D8), carved results (I), warnings.

**B2. Format.** `"HDSC"`, `u32` format version, then append-only blocks:
`u32 type | u32 reserved | u64 payload length | u32 CRC-32 of payload |
payload`. Block types: `RECORDS`, `INDEX_ENTRIES`, `INDEX_BLOCKS`, `BOOT_HITS`,
`CARVE_HITS`, `BAD_RANGES`, `LIVE`, `HEADS`, `OVERRIDES`, `CARVED`,
`CHECKPOINT` (done set, counters, options, identity, range, warnings). The state of a file is
everything up to its last valid `CHECKPOINT`; later bytes are ignored and
truncated on the next append. Payloads use one pure-Rust binary serde codec,
fixed per format version. A file with a newer format version is refused
(`unsupported`). Older versions are read; such a file is rewritten in the
current format before anything is appended to it, and its session carries
the warning `old_session_format`. The rewrite never replaces or deletes the
old file (S6) and is crash safe: (1) a compacted copy is written to a new
temporary file `~hds-session-<n>.tmp` next to it and synced; (2) the old
file is renamed, without replacing, to `<name>.v1.bak` (`<name>.v1-<n>.bak`,
n = 2 to 100, when that name is taken); (3) the copy is renamed, without
replacing, to `<name>`. At every moment a complete session file exists: the
old one under `<name>` or the backup name, and after (1) also the copy. A
crash between (2) and (3) leaves `<name>` missing, the old file under the
backup name and the complete copy under its temporary name; either can be
loaded. If (1) or (2) fails, the copy is removed and the old file stays as
it was. If (3) fails, the old file is renamed back and the copy removed; if
that also fails, the session continues in the copy under its temporary name
and the error names both files. The session file path stays `<name>`
(`scan_done.session_path` does not change); the message of
`old_session_format` names the backup file, which the engine never deletes.
A failed rewrite is reported (`io_error` or `no_space`); the engine does not
fall back to replacing the file with a fresh copy. In
version-1 files the record time falls back to the `$SI` MFT-change time and the
entry time to the entry's MFT-change time.

**B3. Saving.** During a scan, every 5 minutes and at the end (or stop, or
`source_lost`, or shutdown): append the new data blocks and a `CHECKPOINT`,
then sync. `session_save` to another path writes a compacted copy. Temporary
files live next to the session file; the engine never uses `%TEMP%`. Session
paths are subject to the destination guard (S2/S3). The engine never
overwrites a session file that belongs to another source or range. An
existing session file is reopened without following links, only when it is a
regular file that is not a reparse point, and never with a truncating open
(S6). When a catalog is built and the session file's own location turns out
to lie inside a lost-volume range (S2), the engine stops writing to that file
and raises the warning `session_file_at_risk`; further changes need
`session_save` to another place. A checkpoint that fails raises
`session_file_not_saved` until a later write succeeds; a loaded session whose
file had ignored bytes or no scanned data carries `session_incomplete`.

**B4. Identity.** A source matches a session when its size covers the session
range and at least 90 % of up to 64 stored records (spread over the range, not
current-volume records) re-read from disk have the stored `hash`. Model and
serial differences are only warnings (USB bridges change them). `force` on
`session_attach_source` skips nothing but the 90 % threshold being met by
fewer than 8 samples; a source whose samples mismatch is always refused.

## C. Current volumes

A *current volume* is an NTFS file system that exists inside the session range
now (normally: the result of the accidental format).

**C1. Discovery.** A current volume is one the operating system can reach
now. Candidates: a valid NTFS boot sector at the range start and at each
partition start inside the range (partition table as read from the source; a
partition session reads nothing outside its range, so only its start is a
candidate there). A
candidate is a current volume when the record at `offset + mft_lcn ×
cluster_size` parses as an in-use `FILE` record 0 named `$MFT`. A boot sector
found anywhere else never defines a current volume, however intact the file
system behind it looks: it belongs to a lost or an embedded file system, and
its records are ordinary input for D. It remains geometry evidence (F2).

**C2. Snapshot** (read through the volume's own structures, all reads inside
the session range): the `$MFT`'s `$DATA` extents and `$BITMAP`, following its
`$ATTRIBUTE_LIST` into extension records when present; the initialized size of
`$MFT:$DATA`; `format_time` = creation time of record 3 (`$Volume`), falling
back to record 0; the cluster allocation bitmap from record 6 (`$Bitmap`) as a
set of absolute byte ranges (*live-allocated ranges*). If any part cannot be
read the snapshot is marked incomplete (warning `live_volume_incomplete`).
The snapshot is taken whenever a catalog is built with a source attached and
again at `recover_start` and `carve_start`; the latest one is stored in the
session. If the range holds a file system that is not NTFS, or no current
volume at all can be read where a boot sector says there is one, the warning
`writes_not_detectable` is raised.

**C3. Current-volume records.** A scanned record whose disk offset lies inside
a current `$MFT`'s extents below the initialized size is classified:

- slot in use per the live `$BITMAP` → `live`: hidden, counted;
- slot free, the record has a `$STANDARD_INFORMATION`: record time ≥
  `format_time` → `post_format`, a file deleted on the current volume;
  an older time → an old record that survived in an unused slot;
- slot free, no `$STANDARD_INFORMATION` (an extension record): header flag
  *in use* still set → an old record (the current volume clears that flag
  when it frees a record); flag clear → old only if its base reference
  resolves to an old record of the same `$MFT`, otherwise `post_format`.

Records inside a current volume's `$MFTMirr` extents are `live`.

`post_format` records are not shown and are counted; their non-resident
extents, mapped with the **current** volume's geometry, join the overwritten
set (C5). Old records are treated like any other old record. `live` and
`post_format` records never take part in fragment or instance grouping and
are never the target of a parent or base reference: a reference that only
such a record would satisfy leads to a lost folder or an orphan group.

**C4. Current index data.** An `IndexBlock` inside live-allocated ranges, and
any `IndexEntry` whose entry time is ≥ `format_time` or whose source is
such a block or a current-volume record, belongs to the current volume and is
excluded from lost-folder naming (E6) and geometry votes (F).

**C5. Overwritten set** = live-allocated ranges ∪ extents of `post_format`
records. It is the evidence of what has been written since the format — not
proof of the opposite: once the slot of a deleted record has been reused,
nothing shows what that file overwrote. The signature check of S5 remains the
last line.

## D. MFT instances

A scan can meet records of several file systems: the one that was lost, an
older one formatted before it on the same partition, an NTFS volume inside a
disk-image or VM file stored on the drive, stale copies of MFT blocks, other
partitions in a whole-disk scan. Mixing them produces wrong trees.

Input: all stored records except `live` and `post_format` ones (C3).

**D1. Fragments.** Numbered records are grouped by `(record size, origin)`
with `origin = disk offset − number × record size` (signed 64-bit). A record
without a number (pre-3.1 header) that lies on the grid of a fragment and
within 4096 records of its number span takes the positional number and joins
it; other unnumbered records go to the strays (D6). Sets of record numbers are
always compared exactly, never as ranges: one origin can hold two stretches
whose gap lies at another origin.

**D2. Generation cuts.** Every format puts the MFT in the same place, so the
tail of an older file system's MFT survives directly behind the newer one's:
same origin, higher record numbers, older record times. Time alone does not
prove this — deleting an old folder and copying new files in, or renaming the
oldest files, leaves the same pattern inside one file system — so a cut needs
a time boundary **and** a contradiction. Within one fragment:

- A *timed* record has a plausible record time: not 0, not before 1980,
  not more than a day after the scan.
- A *changepoint* is a record number `p`, with at least one timed record on
  each side, such that every timed record numbered ≥ `p` is strictly older
  than every timed record numbered < `p`.
- A *contradiction* is a pair of in-use records `lo < hi` with intact fixups
  that cannot belong to one file system: (A) both map the same cluster number
  in a run of an unnamed `$DATA` or `$INDEX_ALLOCATION:$I30` extent; or (B)
  one of them names the other's slot in a parent or base reference and the
  record there does not fit — it is in use with another sequence number, or,
  for a parent reference, it is not a directory. A newer MFT overwrites a
  prefix of an older one, so a boundary between two file systems lies in
  `(lo, hi]` and `hi` is the stale record. Per `hi`, only its
  highest-numbered partner `lo` is kept.
- *Choosing cuts.* Take the contradictions by ascending `hi`. If no cut chosen
  so far lies in `(lo, hi]`, choose the highest changepoint in that interval;
  if there is none, the record number `p` in `(lo, hi]` that leaves the fewest
  timed records of the fragment out of order for the best time `T` (numbered
  below `p` and not newer than `T`, or numbered `p` or above and newer than
  `T`), the highest such `p` on ties, provided at most a quarter of the
  fragment's timed records are out of order (one session under a wrong clock
  must not hide a generation boundary); otherwise the contradiction is
  *unexplained*.
- A chosen cut is *kept* when it lies in the intervals of contradictions with
  at least 2 different `hi` records; otherwise it is dropped and those
  contradictions are unexplained.
- Every unexplained contradiction marks its `hi` record `older_generation`
  (never a parent or base target, loser of overlap conflicts, rated at most
  `uncertain`) and raises the catalog warning `mixed_generations`.

The pieces between kept cuts are *segments*, numbered from 0 at the lowest
record numbers (the newest generation of that fragment). A fragment without a
kept cut is one segment.

**D3. Boundary times.** For a kept cut `c`: `B_c` = the newest record time
among the timed records of the segment directly above it (the older side),
`A_c` = the oldest among those of the segment directly below that are newer
than `B_c` (for a cut at a changepoint that is all of them, and `B_c < A_c`:
the format happened in between). A segment `X` of another fragment is
*before* `c` when its newest time ≤ `B_c`; *after* `c` when its oldest time
> `B_c` and its newest time ≥ `A_c`; otherwise *undecided* (also when it has
no timed record).

**D4. Joining.** Start with one instance per segment. Two instances may merge
when at least one kind of evidence holds and nothing forbids it.

Evidence:

- (a) *References.* Count the parent references to directories and the base
  references from records of one side to record numbers present on the other,
  in both directions, leaving out targets 5 and 11 and every reference that
  more than one scanned record could resolve (identical copies count once).
  At least one resolves by the reference rule, and at least 90 % of the
  counted ones do.
- (b) *Content.* One side has a validated geometry (F6) under which at least
  80 % of at least 3 checkable files of the other side (F3) show the
  signature their extension promises.
- (c) *MFT map.* One side holds a record 0 named `$MFT` whose extents place at
  least 16 records of the other side exactly where they were found, with one
  volume offset, and that offset is the one at which this record 0 itself lies:
  as the first record of its `$MFT`, or as the first record of the `$MFTMirr`
  that the same side's record 1 describes; and the initialized size of this
  `$MFT:$DATA` covers every record number of the other side. (The first `$MFT`
  extent of every volume formatted by the same tool starts at the same cluster,
  so a record 0 of any other volume places this volume's records too; one that
  held fewer records cannot describe them.)
- (d) *Same geometry.* Both sides have a validated geometry of their own and
  the two are equal.

Forbidden when any of these holds:

- their sets of record numbers intersect;
- a kept cut separates them: one holds the segment below `c` and the other the
  segment above it, or one holds the segment below `c` and the other is not
  *after* `c`, or one holds the segment above `c` and the other is *after* `c`
  (D3);
- one holds old records at the origin of a current volume's `$MFT` and the
  other holds a record number below the lowest of those — the format
  overwrote these slots of that file system, so whatever shows them elsewhere
  is another file system or a copy — unless evidence (c) holds;
- one is embedded in the other (D7);
- references contradict: 3 or more are counted under (a) and fewer than 20 %
  resolve;
- content contradicts: a validated geometry of one side matches fewer than
  20 % of at least 3 checkable files of the other.

All pairs are evaluated and the pair with the strongest evidence merges first
— (c), then (a) by the number of resolving references, then (b), then (d) —
until no pair qualifies. An instance without evidence stays on its own.

**D5. Order of work.** (1) fragments, cuts, segments; (2) joining by evidence
(a) and (c) until nothing changes; (3) geometry (F) of every instance with at
least 16 records and of the anchor (D8); (4) the embedded test (D7), then joining by (b) and (d),
recomputing the geometry of merged instances, until nothing changes; (5)
copies and strays (D6), main (D8), kinds (D9); (6) trees (E) and health (G).

**D6. Strays and copies.** Instances with fewer than 16 records other than
the anchor (D8), and *copy sets*, are merged into one pseudo-instance
`strays`. A copy set is an instance, other than the anchor, at least 90 % of
whose records repeat the number, the sequence and the `$STANDARD_INFORMATION`
creation time (for an extension record: the base reference) of a record of a
larger instance (shadow-copy storage, an MFT that was moved). Number and
sequence alone are not enough: most records of every volume have sequence 1. Inside `strays` no record is a parent target: its
files are grouped into lost folders by their parent reference, listed under
the strays root and rated at most `uncertain`. Stray directory records can
name lost folders of other instances (E6).

**D7. Embedded instances.** Instance `X` is *embedded* in instance `Y` when
`Y` has a validated geometry, at least 90 % of `X`'s records lie inside data
extents of in-use non-directory files of `Y`, and the file that holds most of
them (the *container*) has a record time not more than a day older than
the oldest record time of `X`'s records inside it. If only the last condition
fails, or if `X` is a volume of the disk itself — its validated geometry puts
its volume at the range start or at a partition start, and fewer than 90 % of
its records repeat records of another instance as D6 compares them — the
container is a stale file whose clusters `X`'s file system has reused: the
file gets `overwritten` and `X` is not embedded. The anchor (D8) is never
embedded. An
embedded instance is labelled with the container's name; its files are
browse-only (flag `embedded`, rating `unsupported`): the way to them is to
recover the container and open it as a disk image. The user can overrule the
label (`instance_set`).

**D8. Main instance.** The *anchor* is the instance that holds old records at
the origin of a current volume's `$MFT`; when cuts split that fragment, the
one holding its segment 0. Among instances that are neither embedded nor
strays, `main` is the first that exists in this order: (1) an anchor (if there
are several, one of the current volume with the newest `format_time` first —
the accident is the latest format — then the one with most records); (2) an instance with
validated geometry whose volume offset equals the range start or a partition
start, newest record time first; (3) the instance with the newest record
time; (4) the one with most records. `instance_set` overrules the choice; the
choice is stored in the session. Two accidents on the same volume (formatted,
used, formatted again) are handled the same way: the file system lost last is
`main`, the one lost first is `older`, and the user picks it with
`instance_set` (the results screen offers this whenever an `older` instance
holds more in-use files than `main`). The same applies when the user formatted
another partition of the disk after the accident: its leftovers win (1), and
`instance_set` is the way back.

**D9. Kinds.** `main`; `older` — an instance that a kept cut puts on the
older side of `main` (it holds the segment above the cut, or is *before* it),
or whose newest record time is older than `main`'s oldest and whose geometry
is not validated at a different volume offset: a file system from before the
previous format; `other` — a different volume (whole-disk scans) or records
that nothing ties to `main`; `embedded`; `strays`. Every instance is a root of
its own with its own virtual folders. Instances that produce no catalog node
are not reported.

## E. Catalog build (per instance)

**E1. Base and extension records.** A record with a non-zero base reference
`(N, s)` is an extension record. It belongs to base record `N` of the same
instance when: the extension is in use, the base is in use and `base.seq ==
s`; or neither is in use, the base has attributes, and `base.seq` is `s` or
`next(s)`. A free extension whose base is in use is a leftover and is ignored.
Extensions that match no base form an *orphan group* per base reference (E7).

**E2. Merging attributes.** Names come from the base and all its extensions
alike (a file's only `$FILE_NAME` may sit in an extension record). Extents of
each non-resident attribute — the unnamed `$DATA`, `$INDEX_ALLOCATION:$I30` —
are merged across the group ordered by start VCN. Extent `k` must begin at
`start_vcn(k−1) + Σ run lengths of extent k−1`; the stored last VCN is only a
cross-check when it is > 0. Sizes (allocated, real, initialized) are read only
from the extent with start VCN 0. A hole in the chain, an overlap, or fewer
mapped clusters than the real size needs → `missing_fragments`. A real size
above the allocated size of an attribute that is not compressed (NTFS never
stores one; only a damaged record can) is not believed: size = min(allocated
size, clusters mapped contiguously from VCN 0 × cluster size), initialized
size capped at it, flags `missing_fragments` and `size_estimated`. If the VCN-0
extent is lost: size = (highest mapped VCN + 1) × cluster size, flags
`missing_fragments` and `size_estimated`. A base with an `$ATTRIBUTE_LIST`
but no `$DATA` extent found is `missing_fragments`; a base with neither is
`no_data` (0 bytes).

**E3. Directory or file.** A group is a directory when its base record has
header flag 0x0002 (or one of its names has `$FILE_NAME` flag 0x10000000).

**E4. Name.** Namespace preference: Win32 (1) or Win32+DOS (3), then POSIX (0),
then DOS (2). Among names of the best namespace: one whose parent reference
resolves to a surviving directory, then the lowest attribute id. Other hard
links are listed in `node_info` only.

**E5. Parent.** Parent reference `(P, s)`:

- `P == 5` → the instance root (whether or not record 5 survived).
- `P == 11` → under `$Extend`: the record and everything below it are omitted.
- otherwise the directory group whose base record `P` resolves by the
  reference rule. Never a `live`/`post_format` record, never a non-directory,
  never a record marked `older_generation` by D2.
- no such directory → a lost folder (E6).

**E6. Lost folders** are keyed by `(instance, P, s)`: `Folder #P`, or
`Folder #P.s` when several sequences share a number. A lost folder gets its
real name and its own parent reference from, in this order: (a) a usable
`IndexEntry` whose child reference is `(P, s)`; (b) an in-use directory record
with number `P` and sequence `s` among the strays that comes from a group at
least 90 % of whose records repeat records of `I` as D6 compares them (a stale
copy of `I` itself). With several
entries: skip the DOS namespace when another exists, then take the one with
the latest entry time, then the highest source LSN, then the lowest
source offset (LSNs of different file systems are not comparable: a new
`$LogFile` may restart below the old one's numbers). An entry is *usable* for instance `I` when it is not
current-volume data (C4); it belongs to `I` — it comes from an `$INDEX_ROOT`
of a record of `I`, or from an index block in which some entry names a
record of `I` exactly (number, sequence, name and parent reference equal to
one of the record's `$FILE_NAME`s) and no entry names a record of another
instance so; a block in which no entry names any record so belongs to `I`
when its owner resolves to a directory of `I`, or, failing that, when its
owner number is not present in any instance and the entry time
is not more than a day older than the oldest record time of `I`'s records
(an older file system whose records are all gone still leaves index blocks;
`strays` counts as an instance here); it is on `I`'s side of every kept cut that concerns
`I` (its entry time compared as in D3); and its source offset is not
inside a data extent of an in-use non-directory file of `I` (which would make
it part of an embedded file system). A named lost folder is attached by the
rules of E5/E6 applied to the entry's parent reference; a nameless one, and
any folder whose chain does not reach the root, lives under `[Lost folders]`.

**E7. Orphans.** An orphan group takes name and parent from its own
`$FILE_NAME` when it has one; otherwise it is `File #<base number>` (plus the
extension of the sniffed kind once content has been read) under
`[Orphaned files]`. It is dropped when its base number is below 24 or it has
no unnamed `$DATA`. Files without any usable parent reference also go to
`[Orphaned files]`.

**E8. Cycles** in parent chains are cut by attaching the node where the cycle
is detected to `[Lost folders]`.

**E9. System files.** Records 0–15 and their extension records are omitted;
records 16–23 are omitted when they are extension records or have no
`$FILE_NAME`.

**E10. Aggregates** per folder, bottom-up: `files`, `size` (non-deleted files),
`deleted_files`, `deleted_size`. They do not depend on request filters.

**E11. Node ids** are dense integers assigned per catalog *epoch* (see
PROTOCOL). An id is never reused inside an epoch.

Virtual folder names: `[Lost folders]`, `[Orphaned files]`, `[Carved files]`.
Instance labels: `[Older file system]`, `[File system N]`,
`[Inside <file name>]`, `[Stray records]`.

## F. Geometry (per instance)

A geometry is `(cluster_size, volume_offset)`; `volume_offset` is the absolute
byte offset of cluster 0 of that instance's volume.

**F1. Cluster size** = the most frequent value of `allocated_size /
(last_vcn + 1)` over non-resident attributes with start VCN 0 and
`last_vcn ≥ 0`, accepting only powers of two from 512 to 2 MiB. Source `attr`.
If no attribute qualifies: the cluster size of the boot hit at the range start
or a partition start that is nearest below the instance's lowest record, else
4096.

**F2. Offset candidates**, all required to be ≥ 0, sector-aligned and inside
the source:

- *MFT map* — if the instance has a record 0 named `$MFT`: for each record of
  number `n` at disk offset `D`, map byte `n × record_size` through the `$MFT`
  extents to a volume byte `B`; the most frequent `D − B`. Source `mft`.
- *Index votes* — for each `IndexBlock` at `D` with VCN `v` whose owner
  resolves to a directory of the instance with `$INDEX_ALLOCATION` extents:
  the block's byte position inside the attribute is `v × cluster_size` when
  `cluster_size ≤ block size`, else `v × 512`; map it through the extent that
  contains it to a volume byte `B`; vote for `D − B`. Votes are counted **per
  distinct directory**, not per block. Source `index`.
- *Boot sectors* — each `BootHit` at `X` with the instance's cluster size, both
  as a primary (`X`) and as a backup (`X − total_sectors × bytes_per_sector`),
  preferring the reading consistent with its `hidden_sectors`. Sources `boot`,
  `backup_boot`.
- *Partition starts* inside the range, and the range start. Source `default`.
- *Signature anchors*, only when there are no index votes: for up to 64 in-use
  files with a known signature and first cluster `l`, each carve hit at `D` of
  a matching kind votes for `D − l × cluster_size`; an offset needs ≥ 3 votes.
  Source `signature`.

**F3. Validation.** For the best candidates (at most 8), read the first sector
of up to 200 *checkable* files and count matches. Checkable: in use,
non-resident, not sparse, not compressed or encrypted, intact fixups, an
extension that `hds_carve::matches_extension` has an opinion on, and a first
cluster that under the candidate lies inside the session range and outside
the overwritten set. The sample is spread evenly over the record numbers.
Every head read (the first `SNIFF_LEN` bytes, keyed by absolute offset) is
stored in the session; a catalog built without an attached source uses the
stored heads and reads nothing, so it comes out the same.

**F4. Choice.** By signature matches when at least 20 files were checked; then
by distinct voting directories; then `mft`, `boot`, `backup_boot`, `default`.
`older` instances and strays without evidence of their own take `main`'s
geometry (source `inherited`).

**F5. Volume size.** `end_by_files` = the 4th-largest value of (highest mapped
cluster + 1) over the in-use files of the instance with intact fixups and
complete runlists; with fewer than 4 such files the smallest of their values,
0 without any (a few damaged runlists must not decide). `total_clusters` = the
largest value of a boot hit that agrees with the chosen geometry, provided
`end_by_files` does not exceed it — after a re-partitioning the boot sector
found at the old volume's offset is the new, smaller volume's; otherwise
`max(end_by_files, (range end − volume_offset) / cluster_size)`. The
*lost-volume range* of an instance is `[volume_offset, volume_offset +
max(total_clusters, end_of_all) × cluster_size)` clipped to the source, where
`end_of_all` is the largest (highest mapped cluster + 1) over the in-use files
of the instance with intact fixups and complete runlists; for `main` and
`other` instances with validated geometry, and for the anchor (D8), it is
protected by S2. (`total_clusters` keeps the 4th-largest rule: it decides
`out_of_bounds`.)

**F6. Validated.** With at least 20 files checked: at least 80 % matched; or
at least 50 % matched together with structural evidence. With fewer than 20
files checked: structural evidence alone. Structural evidence = at least 3
distinct voting directories, or an MFT map with at least 16 agreeing records
besides the record that supplied the runlist. Files of an instance whose
geometry is not validated are rated at most `uncertain`.

**F7. Override.** `geometry_set` replaces an instance's geometry (source
`manual`); health and the lost-volume ranges are recomputed, the tree does
not change, carved nodes are discarded. The choice is stored in the session.

## G. Health

**G1. Flags** (per file): `deleted`, `torn_record` (fixup mismatch in a record
of the group), `missing_fragments`, `size_estimated`, `out_of_bounds`,
`overwritten`, `overlap`, `compressed`, `encrypted`, `placeholder`, `sparse`,
`no_data`, `bad_sectors`, `older_generation`, `embedded`, `stray`,
`signature_ok`, `signature_mismatch`.

The extents that G1, G2, H1 and I4 test, and that I2 (a) protects, are a
file's *data* extents: each run clipped to the file bytes below
min(initialized size, real size). Clusters past the real size and bytes at
or after the valid data length are never read (H7, they read as zeros or
not at all), so damage, conflicts or bounds there do not affect the file.

- `compressed` — non-resident `$DATA` with `(flags & 0x00FF) != 0`, or reparse
  tag 0x80000017 / a `WofCompressedData` stream. Resident data is always raw.
  The compression-unit field never decides.
- `encrypted` — `$DATA` flag 0x4000.
- `placeholder` — reparse tag with `(tag & 0xFFFF0FFF) == 0x9000001A` (cloud)
  or 0x80000013 (deduplication): the content is not on this disk.
- `sparse` — the runlist has a hole or the attribute flag 0x8000 is set.
- `out_of_bounds` — a run that reaches beyond `total_clusters` (LCN + length >
  `total_clusters`), or mapped bytes outside the session range.
- `bad_sectors` — an extent intersects a bad range of the scan; set again by
  any read that meets unreadable sectors.
- `overwritten` — an extent intersects the overwritten set (C5), or the file
  lost an overlap conflict (G2), or a carved file was found inside it (I4),
  or the file is the stale container of D7.
- `older_generation` — every file of an `older` instance, and records marked
  by D2.
- `signature_ok` / `signature_mismatch` — set whenever the first bytes have
  been read (geometry validation, `read_file` at offset 0, recovery) and
  `matches_extension` has an opinion.

**G2. Overlap conflicts.** All extents of all non-embedded instances are
compared in absolute bytes (sort, sweep). For two files that share bytes, the
winner is decided by, in order: instance kind (`main` over `other` over
`older` over `strays`); in use over deleted; later record time (of the
file's base record); higher LSN.
The loser gets `overwritten`. The winner gets `overlap` only when both are in
use and of the same instance kind. A full tie gives both `overlap`.

**G3. Rating.**

| rating | condition (first match) |
|---|---|
| `unsupported` | `compressed`, `encrypted`, `placeholder` or `embedded` |
| `damaged` | `missing_fragments`, `out_of_bounds`, `overwritten`, `bad_sectors` or `signature_mismatch` |
| `uncertain` | `deleted`, `overlap`, `torn_record`, `no_data`, `older_generation`, `stray`, geometry not validated, or carved without a verified end |
| `good` | otherwise |

`sparse`, `size_estimated` and `signature_ok` never change a rating by
themselves. `hide_damaged` and `skip_damaged` mean rating `damaged`.

## H. Recovery

**H1. Preconditions.** A catalog with its source attached. The current-volume
snapshot is refreshed and `overwritten` re-evaluated before anything is read.

**H2. Selection.** `include` / `exclude` node ids; a node is selected when its
nearest ancestor-or-self that appears in either list is in `include`. The
optional `filter` (query, extensions, sizes, ratings), `include_deleted` and
`skip_damaged` apply to every selected file, explicit or inherited. The walk
passes through deleted folders. Files rated `unsupported` are never written.
The selection is made when `recover_start` answers, after H1: which files,
and each file's flags and rating context, are fixed then (`files`, `bytes`
and the `no_space` estimate come from them; a later read that adds a flag,
G1, changes neither). The job describes each file (runs, name, path,
times) from the catalog only when it writes it.

**H3. Destination.** The destination guard runs at job start, with the
protected ranges of that moment (session range and lost-volume ranges, S2),
on the created root (ARCHITECTURE §2 and §5.2). Files larger than the
destination file system's maximum file size are `skipped:too_large`.

**H4. Output paths.** `dest` + the full path from the instance root, also for
a selection deep in the tree; files of non-main instances go below a folder
named after the instance label. Virtual folders are real folders in the
output. Selected folders are created even when empty. Names are assigned
before any data is read, in-use entries first, then by record number.
Sanitizing, in this order: (1) each of `<>:"/\|?*` and each unit below 0x20 →
`_`; (2) a lone surrogate → U+FFFD; (3) strip trailing dots and spaces; empty
→ `_`; (4) a reserved device name (the part before the first dot, trailing
spaces ignored, ASCII case-insensitive: `CON PRN AUX NUL CONIN$ CONOUT$
COM0`–`COM9 COM¹ COM² COM³ LPT0`–`LPT9 LPT¹ LPT² LPT³`) → prefix `_`; names
beginning with `HDSavior-report-` or matching `~hds-*.part` → prefix `_`;
(5) case-insensitive collisions inside a folder get ` (2)`, ` (3)`… before the
extension (from the last dot to the end, when that dot is not the first
character and the part is at most 16 units long with the dot: `.gitignore`
becomes `.gitignore (2)`; PROTOCOL `ext` uses the same dot rule); (6) shorten the stem to the destination volume's component limit
without cutting a surrogate pair. A path longer than 32,767 units is
`skipped:path_too_long`. If the file system still rejects a name, retry once
with `<shortened stem>~<record number>.<ext>`.

**H5. Order.** Files are processed in ascending order of the absolute offset
of their first data byte (resident files: their record's offset).

**H6. Writing one file.** Create `~hds-<n>.part` in the final directory with
create-new semantics, write, set the times on the writing handle, close, then
rename **without replacing**. If the final name exists: a file produced by
this run → take the next ` (n)`; a file that existed before → `on_exists`
(`rename` → next ` (n)`, `skip` → `skipped:exists`, `overwrite` → only a
regular file that is not a reparse point: delete it, then rename; a
directory, a link or any other reparse point under that name is never
deleted or entered, and the new file takes the next ` (n)`). Leftover
`~hds-*.part` files in the tree are deleted when a job starts there, so a file
under its final name is always complete. Output directories are created one
level at a time; an existing name is reused only if it is a directory that is
not a reparse point. `create_dir_all`, truncating opens and replacing renames
are never used on output paths.

**H7. Content.**

- Resident: re-read the record that holds the attribute (base or extension)
  at its disk offset, compare `hash`; a mismatch is `failed:record_changed`.
  Apply fixups, copy the attribute content.
- Non-resident: for each run read `volume_offset + lcn × cluster_size` with
  `read_resilient` in large aligned chunks. Sparse runs, clusters missing from
  the chain and runs outside the session range produce zeros. Bytes at or
  beyond the initialized size are zeros. The output is truncated to the real
  size.

**H8. Times.** Creation, modification and access time from
`$STANDARD_INFORMATION`, converted with checked arithmetic; only values with
`0 < FILETIME < 2^63` are applied. Directories this run created get theirs
bottom-up at the end. A time that cannot be set is a note in the report and
never changes a status.

**H9. Verification.** The first bytes, still in memory, are checked with
`matches_extension` → `signature_ok` / `signature_mismatch`. For JPEG, ZIP
and MP3 a magic number alone is not a match: JPEG needs `FF D8` and a chain
of marker segments whose lengths fit their content as far as the bytes
reach (an APPn segment starts with an ASCII identifier; a COM segment with
text, where every byte from 0x80 up counts as text, so UTF-8 and code-page
comments match); ZIP a local header with a plausible version, flags, method and a
name without control characters (or an empty archive's end record, all
fields zero); MP3 a sane ID3v2 header, a frame with a VBR header, or two
consistent frame headers at the distance the first states (a single frame
header gives no opinion).

**H10. Report.** `HDSavior-report-<UTC yyyymmdd-hhmmss>.csv` in `dest`, created
when the job starts; one row appended per file and flushed. UTF-8 with BOM,
CRLF, RFC 4180 quoting; a cell starting with `=`, `+`, `-`, `@` or TAB is
prefixed with an apostrophe. Columns: `status, flags, size, bytes_written,
output_path, original_path, mft_record, record_offset, first_data_offset,
modified_utc, detail`. Files not reached before a cancel are not listed.

**H11. Status** of each file after the attempt:

| status | meaning |
|---|---|
| `ok` | written, rating `good`, first bytes match the extension |
| `ok_unverified` | written, rating `good`, no opinion about the extension (or empty file) |
| `warning:<flags>` | written, rating after the read is not `good` |
| `skipped:<reason>` | not written: `compressed`, `encrypted`, `placeholder`, `embedded`, `damaged` (skip_damaged), `exists`, `too_large`, `path_too_long` |
| `failed:<code>` | nothing is left at the destination: `record_changed`, `write_error`, `io_error` |

`ok + warnings + failed + skipped = files_total`; `ok` counts both ok kinds.

**H12. Ending early.** Destination full → the job ends once with error
`no_space`. Source lost → `source_lost`. Cancel → the current part file is
removed, status `cancelled`. The report keeps what was done.

## I. Carving

**I1.** Optional job after a scan that collected carve hits (otherwise
`unsupported`). Works with the `main` instance's geometry only.

**I2. Candidates.** A hit qualifies when `(offset − volume_offset)` is a
multiple of the cluster size, it is inside the scanned (done) part of the
range, and it is not inside (a) an extent of an in-use `main` file that is
neither `overwritten` nor `out_of_bounds`, (b) the overwritten set, (c) a file
already carved in this run whose end was verified by structure
(`Measured.end_verified`). A file whose length comes from a header or a guess
does not bound the candidates after it: if it is fragmented, its length runs
over foreign clusters, and a file starting there is measured like any other
candidate (both are kept).

**I3. Measuring.** Candidates are processed in ascending offset order with
`hds_carve::try_measure`, bounded by the next extent of kind (a), the next
live-allocated range, and the range end. A ZIP candidate that hits the read
limit is retried once with the limit raised to that bound; the retries of one
run share a budget of one pass over the part of the range already done, and
none is made once 1 GiB or less of that budget is left. Streamed ZIP entries
(sizes in a data descriptor) are read as far as needed when they are DEFLATE
(walked to the end of the stream) or stored and not encrypted (the
descriptor must carry the CRC-32 of the data); the search for the
descriptor of any other streamed entry gives up once 32 MiB of its data have
been searched, and such an entry leaves the archive's end not verified. Valid results
become nodes `[Carved files]/<ext>/f<sector number, 10 digits>.<ext>`.
Rating: `good` when the end was verified by structure (`Measured.end_verified`),
else `uncertain`, and at most `uncertain` without a validated geometry (F6).
An end concluded from missing data (the walker asked for bytes at or past the
bound and got none) is not verified; JPEG and GIF ends are verified only by
decoding the entropy-coded or LZW data to exactly the frame's size.

**I4.** A carved file that lies inside a deleted file's data extents (G1; every
byte of it, not merely touching them) makes that
file `overwritten`, except the file's own content: a carved file that starts
exactly at the deleted file's first data byte (its mapped run at VCN 0) and is
no longer than its real size. A deleted file whose VCN-0 run is unknown gets
no exemption.

**I5. Lifetime.** `carve_start` replaces the carved nodes of the kinds it is
given; results found before a cancel are kept; carved nodes are stored in the
session and discarded by a `geometry_set` that changes the main instance's
layout, cluster size or volume offset, by
`instance_set` and by any `scan_start`. Carved files are recovered like any file with one run.

## J. Imaging

Sequential copy of a byte range of the source to a file with `read_resilient`.
Bad sectors are zero-filled and appended to `<image>.bad.txt`. A side file
`<image>.hdsimg` (source identity, range, confirmed byte count) is rewritten
after a flush every 1 GiB and on stop. A side file (`.hdsimg`, and
`.bad.txt` when a resume trims it) is rewritten without ever leaving its
only copy in a part file: the new text is committed as `<side file>.new`,
then the old file is removed and `.new` renamed to its name; a side file
that is missing is read from its `.new` copy. A fresh image refuses the
`.new` names like the side files themselves. `resume` checks the identity,
truncates the image to the confirmed count and only then checks the free
space (the unconfirmed tail is not part of the image, and its space counts
as free); the image length alone is never trusted. A resumed image is reopened without following links and only when it
is a regular file that is not a reparse point (S6). The guard protects the
imaged range `[start, end)` in addition to the session's protected ranges. Without `resume` an existing image file is refused (`exists`). An
image larger than the destination file system's maximum file size is refused
before writing. Source lost ends the job with `source_lost` without extending
the image.
