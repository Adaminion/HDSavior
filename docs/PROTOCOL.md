# HDSavior engine protocol (version 1)

The Flutter app starts `hdsavior-engine serve` and talks to it over the
child's standard streams. The command-line subcommands print the same result
and event objects (ARCHITECTURE §5.6).

## 1. Transport

- UTF-8, one JSON object per line (`\n` terminated, no embedded newlines).
- `stdin`: requests. `stdout`: responses and events, nothing else.
  `stderr`: free-form log lines for humans.
- Both sides ignore object members they do not know.

```
request   {"id": 7, "cmd": "list_children", "args": {"node": 12, "epoch": 3}}
response  {"id": 7, "ok": true, "result": { ... }}
          {"id": 7, "ok": false, "error": {"code": "not_found", "message": "node 12 does not exist", "data": null}}
event     {"event": "scan_progress", "data": { ... }}
```

- `id` is an integer chosen by the client and echoed. `args` may be omitted.
  A line that is not a JSON object with an integer `id` is answered with
  `"id": null` and `bad_request`.
- **Ordering.** Requests are handled one at a time in arrival order and
  answered in that order. Long work is a *job*: the response to `X_start` is
  written before any event of that job; `X_done` is the job's last event and
  is written after the job slot is free, the resulting state (catalog,
  session file) is in place and can be queried.
- At most one job (scan, carve, recover, image) runs at a time; starting
  another answers `busy`. Queries are answered while a job runs, except that
  no catalog exists during a scan.
- `session_load` and `geometry_set` can take long on very large sessions.
  They stay plain requests; while they work the engine emits `task_progress`
  events and later requests wait.
- **End of input.** On `shutdown`, or when `stdin` reaches EOF: a running scan
  stops reading and saves its session (no catalog is built); other jobs are
  cancelled (the current part file is removed); then the engine exits.

### Conventions

- Byte counts and disk offsets: JSON integers (up to 2^63 − 1). All sizes are
  bytes.
- Timestamps: Unix time in **milliseconds**, UTC, integer; `null` if unknown
  or not representable.
- `speed_bps`: bytes per second averaged over the last 5 s. `eta_seconds`:
  remaining bytes / speed, rounded up; `null` in phase `analyzing` or at
  speed 0. `elapsed_seconds`: whole seconds of this run.
- Names are UTF-8; an unpaired UTF-16 surrogate in an NTFS name is sent as
  U+FFFD. Node ids are the only handles; names are never sent back.
- Paths inside the rescued tree use `/` and are for display only.
  File-system paths (`dest`, `path`) are native absolute paths.
- Extensions (`ext`, `exts`) are lower-case without the dot; `""` if none.
- In filters `null` means "any"; an empty list is `bad_request`.

### Catalog epochs

Node ids are valid for one catalog **epoch**. `CatalogInfo.epoch` changes
whenever the node set is rebuilt (every `scan_done`, `session_load`). Any
`scan_start` ends the current epoch at once. Inside an epoch an id is never
reused: `geometry_set` and `carve_start` keep the epoch, ids of discarded
carved nodes answer `not_found`. Every command that takes node ids requires
`epoch` and answers `stale_catalog` when it differs.

### Error codes

`bad_request`, `unknown_command`, `busy`, `no_source`, `no_session`,
`no_catalog`, `stale_catalog`, `not_found`, `io_error`, `access_denied`,
`destination_refused`, `source_mismatch`, `source_lost`, `no_space`, `exists`,
`unsupported`, `cancelled`, `internal`.

`error.data` is an optional object: for `destination_refused` it is the
`check_destination` result; for `source_mismatch` it names the failed checks.
`access_denied` from opening a device is the authoritative sign that the
engine is not elevated.

States: **N** no source, no session · **O** source open, no session ·
**S** scan running · **C** catalog with source attached · **B** catalog
without source (browse only) · **J** carve, recover or image running.

| command | errors by state |
|---|---|
| `hello`, `list_disks`, `job_cancel`, `shutdown` | always answered |
| `open_source`, `close_source`, `session_load`, `session_attach_source`, `geometry_set`, `session_save` | `busy` in S and J |
| `scan_start` | N, B `no_source` · resume without session `no_session` · S, J `busy` · bad range `bad_request` · refused session path `destination_refused` |
| `catalog_info`, `list_children`, `node_info`, `search`, `selection_summary` | N, O, S `no_catalog` · wrong epoch `stale_catalog` · unknown id `not_found` · answered in J |
| `check_destination` | N, B `no_source` · otherwise answered, also in S and J |
| `recover_start`, `carve_start` | N, O, S `no_catalog` · B `no_source` · J `busy` · empty selection `bad_request` · `destination_refused` · `no_space` · carve without stored hits `unsupported` |
| `image_start` | N, B `no_source` · S, J `busy` · `destination_refused` · `no_space` · `exists` |
| `read_file` | N, O, S `no_catalog` · B `no_source` · folder `bad_request` · unsupported file `unsupported` |
| `session_save` | N, O `no_session` · `destination_refused` |
| `session_attach_source` | N, O `no_session` · `source_mismatch` |

## 2. Shared objects

### SourceInfo

```json
{"kind": "disk", "path": "\\\\.\\PhysicalDrive1", "size": 4000787030016,
 "sector_size": 512, "physical_sector_size": 4096,
 "model": "WDC WD40EZRZ-00GXCB0", "serial": "WD-WCC7K0000000", "disk_number": 1}
```

`kind` is `"disk"` or `"image"`. `model`, `serial`, `disk_number` are `null`
for images. `disk_number` is what the opened device reports, queried again on
every attach.

### Partition

```json
{"index": 1, "offset": 1048576, "size": 4000785104896,
 "type": "Basic data", "type_id": "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7", "fs": "ntfs",
 "ntfs": {"cluster_size": 4096, "total_sectors": 7814033407, "serial": "A1B2C3D4E5F60718"},
 "letters": ["E:"], "label": "New Volume"}
```

`index` is 1-based in offset order; a source without partition table whose
first sector is a file-system boot sector has one pseudo-partition with index
0 covering the source. `type_id` is the MBR type byte as two hex digits or
the GPT type GUID. `fs` is sniffed from the first sector: `"ntfs"`, `"fat"`,
`"exfat"`, `"refs"`, `"bitlocker"`, `"other"` or `null` (unreadable/empty).
`ntfs` is `null` unless `fs == "ntfs"`. `letters`/`label` describe the volume
currently mounted there (empty / `null` if none; always so for images).

### Disk

`SourceInfo` members plus:

```json
{"number": 1, "bus": "USB", "removable": false, "is_system": false,
 "media": "hdd", "trim": false, "flags": [],
 "table": "gpt", "partitions": [Partition, ...]}
```

`media`: `"hdd"`, `"ssd"`, `"unknown"` (from the seek-penalty property).
`trim`: `true`, `false` or `null` (unknown) — a format on a TRIM-capable
device discards the data. `flags`: subset of `virtual`, `spaces`, `dynamic`,
`no_media`. `table`: `"mbr"`, `"gpt"`, `"none"`.

### Warning

`{"code": "source_reads_zero", "message": "..."}`. Codes: `trim_supported`,
`mounted_volume`, `page_file_on_source`, `system_disk`, `app_on_source`,
`source_reads_zero`, `carve_hits_capped`, `live_volume_incomplete`,
`writes_not_detectable`, `scan_incomplete`, `out_of_range_files`,
`identity_differs`.

### Geometry

```json
{"cluster_size": 4096, "volume_offset": 1048576,
 "cluster_source": "attr", "offset_source": "index",
 "index_dirs": 310, "index_blocks": 412,
 "signature_checked": 200, "signature_ok": 197,
 "validated": true, "total_clusters": 976754175}
```

`cluster_source`: `"attr"`, `"boot"`, `"default"`, `"manual"`.
`offset_source`: `"mft"`, `"index"`, `"boot"`, `"backup_boot"`, `"signature"`,
`"default"`, `"inherited"`, `"manual"`. `volume_offset` is the absolute byte
offset on the source of that volume's cluster 0.

### Instance

One file system found by the scan.

```json
{"id": 0, "kind": "main", "label": "", "root": 0,
 "records": 812401, "files": 790001, "dirs": 22300, "deleted_files": 5120,
 "lost_dirs": 14, "bytes": 3210987654321,
 "time_from": 1420070400000, "time_to": 1763942400000,
 "embedded_in": null,
 "geometry": Geometry, "geometry_candidates": [Geometry, ...],
 "virtual": {"lost": 3, "orphans": 4, "carved": null}}
```

`kind`: `"main"`, `"older"` (a file system from before the previous format),
`"other"` (another volume), `"embedded"` (inside a disk-image file; browse
only), `"strays"`. `label` is the display name of its root (`""` for main).
`embedded_in` is the display path of the containing file. `files`, `dirs`,
`bytes` count non-deleted items. Instance roots are separate trees: no
instance root is a child of another.

### CatalogInfo

```json
{"epoch": 3, "instances": [Instance, ...], "nodes": 812345,
 "live_records": 41, "post_format_records": 6,
 "live_volumes": [{"offset": 1048576, "size": 4000785104896, "format_time": 1763900000000, "snapshot_time": 1763990000000}],
 "range": {"start": 1048576, "end": 4000786153472},
 "scanned_bytes": 4000785104896, "scan_complete": true, "bad_bytes": 0,
 "out_of_range_files": 0, "out_of_range_bytes": 0,
 "carve_hits": 48213, "carved": 0,
 "source_attached": true, "warnings": [Warning, ...]}
```

`instances[0]` is the main instance. `nodes` counts all nodes of all roots.
`live_records` belong to the current file system and are not shown.
`out_of_range_files` are files with data outside the session range (scan the
whole disk to reach them).

### Row

```json
{"id": 4711, "parent": 17, "name": "IMG_0042.JPG", "kind": "file", "ext": "jpg",
 "size": 3456789, "modified": 1714566896000, "created": 1714566896000,
 "deleted": false, "rating": "good", "flags": ["signature_ok"],
 "files": 0, "deleted_files": 0, "deleted_size": 0, "has_children": false}
```

- `kind`: `"dir"`, `"file"`, `"lost_dir"`, `"virtual"`, `"carved"`.
- `parent`: node id, `null` for an instance root.
- `size`: files — real size; folders — total size of the non-deleted files
  below. `files` / `deleted_files` / `deleted_size`: folders — recursive, by
  each file's own deleted flag, independent of request filters; files — 0.
- `has_children`: folders — at least one child folder that the same request
  with `dirs_only` would return.
- `rating`: `"good"`, `"uncertain"`, `"damaged"`, `"unsupported"`; `null` for
  folders. Mapping from flags: ALGORITHMS G3.
- `flags`: subset of `deleted`, `torn_record`, `missing_fragments`,
  `size_estimated`, `out_of_bounds`, `overwritten`, `overlap`, `compressed`,
  `encrypted`, `placeholder`, `sparse`, `no_data`, `bad_sectors`,
  `older_generation`, `embedded`, `stray`, `signature_ok`,
  `signature_mismatch`.

### Filter

```json
{"query": "img_", "exts": ["jpg", "png"], "min_size": null, "max_size": null, "ratings": ["good", "uncertain"]}
```

All members optional. `query` is a case-insensitive substring of the name.

## 3. Commands

Defaults are given with each argument; arguments without a default are
required.

### hello

→ `{"engine_version": "0.1.0", "protocol": 1, "os": "windows", "elevated": true, "raw_disk_support": true, "carve_kinds": ["jpeg", "png", ...]}`

`elevated` is advisory (for messages only).

### list_disks

→ `{"disks": [Disk, ...]}` (empty on non-Windows hosts).

### open_source

args `{"kind": "disk"|"image", "path": "...", "sector_size": null}`

`kind` `"disk"` accepts only `\\.\PhysicalDrive<N>`. `sector_size` (images
only; default: detected, else 512).

→ `{"source": SourceInfo, "table": "gpt", "partitions": [Partition, ...], "warnings": [Warning, ...]}`

Closes any previously open source and drops its session and catalog.

### close_source

→ `{}`

### scan_start

args

```json
{"start": 0, "end": null, "session_path": null, "resume": false,
 "collect_carve_hits": true, "allow_same_disk": false, "allow_unknown_disk": false}
```

`start` / `end`: absolute byte range, sector-aligned (default: the whole
source). `session_path`: where the session is saved every 5 minutes and at
the end; `null` = nothing is saved automatically. It is checked with the
destination guard before anything is read (`destination_refused`).

Any `scan_start` discards the current catalog and carved nodes at once; until
`scan_done`, catalog queries answer `no_catalog`. With `resume: true` the
loaded session continues: `start` / `end` / `collect_carve_hits` must be
absent or equal to the session's; `session_path` defaults to the file the
session was loaded from or last saved to; resuming a complete session just
emits `scan_done`.

→ `{"job": "scan", "bytes_total": 4000785104896}`

Events

`scan_progress`

```json
{"phase": "mft_probe", "bytes_done": 1073741824, "bytes_total": 4000785104896,
 "position": 4294967296, "speed_bps": 157286400, "eta_seconds": 25430,
 "elapsed_seconds": 7, "records": 120344, "files": 118002, "dirs": 2342,
 "index_blocks": 904, "carve_hits": 31, "bad_bytes": 0, "warnings": []}
```

`phase`: `"mft_probe"`, `"sweep"`, `"analyzing"` (building the catalog; in
this phase `bytes_*` describe the analysis steps and `job_cancel` is ignored).
`position`: absolute offset of the last chunk read. `bad_bytes`: a multiple of
the logical sector size.

`scan_done`

```json
{"status": "completed", "error": null, "session_path": "...", "catalog": CatalogInfo}
```

`status`: `"completed"`; `"stopped"` (cancelled by the user: the catalog holds
what was read, `scan_complete` is false, carving works on the scanned part);
`"failed"` (`error` set; `source_lost` keeps the session resumable; `catalog`
is `null`).

### job_cancel

→ `{}` immediately; the job then ends with its `*_done` event (`"stopped"`
for a scan, `"cancelled"` for the others). No job → `{}`.

### session_save

args `{"path": "...", "allow_same_disk": false, "allow_unknown_disk": false}`
→ `{"path": "...", "bytes": 12345678}`

Writes a compacted copy. Guarded like every destination.

### session_load

args `{"path": "...", "source": null}` (`source`: `{"kind", "path"}` to open
instead of the recorded one)

→

```json
{"source": SourceInfo, "source_attached": true, "source_error": null,
 "catalog": CatalogInfo}
```

The engine tries the recorded path, then (for disks) every disk of
`list_disks`, and attaches the first whose identity matches (ALGORITHMS B4).
Without a matching source `source_attached` is false, `source` is the
recorded one, the tree can be browsed, and `recover_start`, `read_file`,
`carve_start`, `scan_start` answer `no_source` until `session_attach_source`
succeeds. Model or serial differences are reported as warning
`identity_differs`.

### session_attach_source

args `{"kind": "disk"|"image", "path": "...", "force": false}` →
`{"source": SourceInfo, "catalog": CatalogInfo}` or `source_mismatch`.

The catalog is rebuilt with a fresh current-volume snapshot (new epoch).

### catalog_info

→ `CatalogInfo`

### geometry_set

args `{"instance": 0, "cluster_size": 4096, "volume_offset": 1048576}` →
`CatalogInfo`

`cluster_size`: a power of two from 512 to 2 MiB. `volume_offset`: a multiple
of 512 inside the source. A pair equal to one of the candidates selects that
candidate. Health is recomputed; the tree and the epoch stay; carved nodes are
discarded when the main instance changes.

### list_children

args

```json
{"epoch": 3, "node": 0, "offset": 0, "limit": 200, "sort": "name", "desc": false,
 "dirs_only": false, "show_deleted": false, "exts": null, "ratings": null}
```

`sort`: `"name"`, `"size"`, `"modified"`, `"rating"`, `"ext"`. Folders always
sort before files; ties are broken by name (case-insensitive), then id;
`null` values last. `limit` ≤ 1000. `exts` and `ratings` filter files;
folders are always listed, except that with `show_deleted: false` a deleted
folder with no non-deleted file below it is hidden, as are deleted files.
`total` counts rows after filters.

→ `{"total": 5321, "rows": [Row, ...]}`

### node_info

args `{"epoch": 3, "node": 4711}` →

```json
{"row": Row,
 "ancestors": [{"id": 0, "name": ""}, {"id": 17, "name": "Photos"}],
 "path": "Photos/IMG_0042.JPG", "instance": 0,
 "accessed": 1714566896000,
 "other_names": [],
 "record": {"number": 91234, "seq": 3, "disk_offset": 3314748416, "extension_records": 0},
 "data": {"resident": false, "real_size": 3456789, "initialized_size": 3456789,
          "allocated_size": 3457024, "runs": 1, "first_lcn": 812345,
          "first_offset": 3328413696},
 "flag_details": [{"flag": "signature_ok", "text": "The file starts with a valid JPEG header."}]}
```

`record` is `null` for virtual, lost and carved nodes; `data` is `null` for
folders. `flag_details` explains every flag of the row in plain language.

### search

args

```json
{"epoch": 3, "under": 0, "filter": Filter, "show_deleted": false,
 "sort": "name", "desc": false, "offset": 0, "limit": 200}
```

`under`: the subtree to search (default: the main root). Only `file` and
`carved` nodes are returned. `limit` ≤ 1000.

→ `{"total": 1234, "rows": [Row + {"path": "Photos/IMG_0042.JPG", "ancestor_ids": [0, 17]}, ...]}`

`ancestor_ids` are root-first (so the UI can show folder tri-states for rows
checked in search results).

### selection_summary

args

```json
{"epoch": 3, "include": [17, 99], "exclude": [4711], "filter": null,
 "include_deleted": false, "skip_damaged": false}
```

A node is selected when its nearest ancestor-or-self that appears in
`include` or `exclude` is in `include`. `filter`, `include_deleted` and
`skip_damaged` apply to every selected file, explicit or inherited.

→

```json
{"files": 5320, "bytes": 18234567890, "dirs": 12,
 "good": 5300, "uncertain": 12, "damaged": 5, "unsupported": 3}
```

`files` = selected files after `filter` and `include_deleted`; the four
rating counts add up to it. `bytes` = sizes of the files that will be
attempted (not `unsupported`; not `damaged` when `skip_damaged`). `dirs` =
directories that will be created.

### check_destination

args `{"path": "D:\\Rescued", "purpose": "recover", "need_bytes": null}`
(`purpose`: `"recover"`, `"image"`, `"session"`, `"app_data"`)

→

```json
{"verdict": "ok", "message": "", "free_bytes": 500000000000, "enough_space": true,
 "fs": "NTFS", "max_file_bytes": null, "disk_numbers": [0]}
```

`verdict`: `"ok"`; `"overlaps_source"` (always refused); `"same_disk"` (needs
`allow_same_disk`); `"unknown_disk"` (needs `allow_unknown_disk`);
`"not_found"`, `"not_writable"`, `"invalid_path"` (not an absolute drive or
UNC path). `message` explains the verdict in plain language. `enough_space`
is `null` without `need_bytes`. `max_file_bytes`: the file system's maximum
file size if it is a practical limit (FAT32: 4294967295), else `null`.
Creates nothing.

### recover_start

args

```json
{"epoch": 3, "include": [17, 99], "exclude": [4711], "filter": null, "dest": "D:\\Rescued",
 "include_deleted": false, "skip_damaged": false,
 "allow_same_disk": false, "allow_unknown_disk": false, "on_exists": "rename"}
```

`on_exists` applies to files that existed before this run: `"rename"`,
`"skip"`, `"overwrite"`.

→ `{"job": "recover", "files": 5320, "bytes": 18234567890}` (the numbers of
`selection_summary`) or `destination_refused`.

Events

`recover_progress`

```json
{"files_done": 120, "files_total": 5320, "bytes_done": 734003200,
 "bytes_total": 18234567890, "current": "Photos/IMG_0042.JPG",
 "speed_bps": 104857600, "eta_seconds": 167, "elapsed_seconds": 7,
 "ok": 118, "warnings": 2, "failed": 0, "skipped": 0}
```

`recover_done`

```json
{"status": "completed", "error": null, "files_total": 5320,
 "ok": 5298, "warnings": 17, "failed": 2, "skipped": 3, "bytes_written": 18230000000,
 "dest": "D:\\Rescued", "report_path": "D:\\Rescued\\HDSavior-report-20261124-031500.csv"}
```

`status`: `"completed"`, `"cancelled"`, `"failed"` (`error`: `no_space`,
`source_lost`, …). `ok + warnings + failed + skipped = files_total` when
completed. Statuses per file: ALGORITHMS H11.

### carve_start

args `{"kinds": null}` (`null` = all kinds of `hello.carve_kinds`)

→ `{"job": "carve", "candidates": 48213}`

Events: `carve_progress` `{"candidates_done": 100, "candidates_total": 48213, "found": 61, "bytes_found": 310000000, "eta_seconds": 900}`;
`carve_done` `{"status": "completed", "error": null, "found": 913, "bytes_found": 4100000000, "catalog": CatalogInfo}`.

Replaces the carved nodes of the given kinds (same epoch). Results found
before a cancel are kept.

### image_start

args

```json
{"start": 0, "end": null, "dest": "D:\\disk1.img", "resume": false,
 "allow_same_disk": false, "allow_unknown_disk": false}
```

→ `{"job": "image", "bytes": 4000787030016}`

Events: `image_progress` `{"bytes_done", "bytes_total", "speed_bps", "eta_seconds", "elapsed_seconds", "bad_bytes"}`;
`image_done` `{"status", "error", "bytes_written", "bad_bytes", "path", "bad_map_path"}`
(`bad_map_path` is `null` when there were no unreadable sectors).

### read_file

args `{"epoch": 3, "node": 4711, "offset": 0, "length": 65536}`
(`length` ≤ 8388608)

→ `{"data_b64": "...", "size": 3456789, "eof": false, "kind": "jpeg"}`

Reads through the same code path as recovery. `kind` is the sniffed file kind
of the returned bytes when `offset == 0` (else `null`).

### shutdown

→ `{}` and the engine exits (see "End of input").

## 4. Other events

- `task_progress` — `{"task": "session_load"|"geometry_set", "fraction": 0.42}`.
- `log` — `{"level": "info"|"warn"|"error", "message": "..."}`; informational.
