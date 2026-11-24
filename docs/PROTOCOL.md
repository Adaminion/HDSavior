# HDSavior engine protocol (version 1)

The Flutter app starts `hdsavior-engine serve` and talks to it over the
child's standard streams.

## 1. Transport

- UTF-8, one JSON object per line (`\n` terminated, no embedded newlines).
- `stdin`: requests. `stdout`: responses and events — nothing else is ever
  printed there. `stderr`: free-form log lines for humans.
- The engine exits when `stdin` reaches EOF or on the `shutdown` command;
  running jobs are cancelled first.
- Both sides ignore object members they do not know.

```
request   {"id": 7, "cmd": "list_children", "args": {"node": 12}}
response  {"id": 7, "ok": true, "result": { ... }}
          {"id": 7, "ok": false, "error": {"code": "not_found", "message": "node 12 does not exist"}}
event     {"event": "scan_progress", "data": { ... }}
```

- `id` is chosen by the client and echoed. `args` may be omitted.
- Every request gets exactly one response. Long-running work is a *job*:
  the response to `*_start` arrives immediately, progress and completion
  arrive as events. At most one job runs at a time; starting another yields
  `busy`. Queries (`list_children`, `search`, `node_info`, `read_file`, …)
  are answered while a job runs, except that no catalog exists during a scan.

### Conventions

- Byte counts and disk offsets: JSON integers (up to 2^63 − 1).
- Timestamps: Unix time in **milliseconds**, UTC, integer; `null` if unknown.
- Node ids: non-negative integers, valid for the lifetime of the loaded
  session/catalog.
- Paths inside the rescued tree use `/` as separator and are for display
  only. File-system paths (`dest`, `path`) are native OS paths.
- Extensions (`ext`) are lower-case without the dot; `""` if none.

### Error codes

`bad_request`, `unknown_command`, `busy`, `no_source`, `no_session`,
`no_catalog`, `not_found`, `io_error`, `access_denied`, `destination_refused`,
`source_mismatch`, `unsupported`, `cancelled`, `internal`.

## 2. Shared objects

### SourceInfo

```json
{"kind": "disk", "path": "\\\\.\\PhysicalDrive1", "size": 4000787030016,
 "sector_size": 512, "physical_sector_size": 4096,
 "model": "WDC WD40EZRZ-00GXCB0", "serial": "WD-WCC7K0000000", "disk_number": 1}
```

`kind` is `"disk"` or `"image"`. `model`, `serial`, `disk_number`,
`physical_sector_size` are `null` for images.

### Partition

```json
{"index": 1, "offset": 1048576, "size": 4000785104896,
 "type": "Basic data", "fs": "ntfs",
 "ntfs": {"cluster_size": 4096, "total_sectors": 7814033407, "serial": "A1B2C3D4E5F60718"},
 "letters": ["E:"], "label": "New Volume"}
```

`index` is 1-based in offset order. `fs` is sniffed from the partition's first
sector (`"ntfs"`, or `null`). `ntfs` is `null` unless `fs == "ntfs"`.
`letters`/`label` describe the volume currently mounted there (empty / `null`
if none, always empty for images).

### Disk

`SourceInfo` members plus:

```json
{"number": 1, "bus": "USB", "removable": false, "is_system": false,
 "table": "gpt", "partitions": [Partition, ...]}
```

`table` is `"mbr"`, `"gpt"` or `"none"`.

### Geometry

```json
{"cluster_size": 4096, "volume_offset": 1048576, "source": "index",
 "index_votes": 412, "index_blocks": 415,
 "signature_checked": 200, "signature_ok": 197, "total_clusters": 976754175}
```

`source`: `"index"`, `"boot"`, `"backup_boot"`, `"default"`, `"manual"`.
`volume_offset` is the absolute byte offset on the source of old cluster 0.

### CatalogInfo

```json
{"root": 0, "nodes": 812345, "files": 790001, "dirs": 22300,
 "deleted_files": 5120, "lost_dirs": 14, "carved": 0,
 "total_bytes": 3210987654321, "live_records": 41, "instances": 1,
 "geometry": Geometry, "geometry_candidates": [Geometry, ...],
 "virtual": {"lost": 3, "orphans": 4, "carved": null, "instances": []},
 "range": {"start": 1048576, "end": 4000786153472},
 "scanned_bytes": 4000785104896, "scan_complete": true,
 "bad_bytes": 0, "warnings": ["..."]}
```

`files`/`dirs`/`total_bytes` count non-deleted items of the main instance.
`live_records` is the number of records that belong to the *current* file
system and are therefore not shown. `virtual` holds the ids of the virtual
folders (`null` when absent).

### Row

```json
{"id": 4711, "name": "IMG_0042.JPG", "kind": "file", "ext": "jpg",
 "size": 3456789, "modified": 1714566896000, "created": 1714566896000,
 "deleted": false, "rating": "good", "flags": ["signature_ok"],
 "files": 0, "deleted_files": 0, "has_children": false}
```

- `kind`: `"dir"`, `"file"`, `"lost_dir"`, `"virtual"`, `"carved"`.
- `size`: files — real size; folders — total size of the non-deleted files
  below them.
- `files` / `deleted_files`: folders — recursive counts; files — 0.
- `has_children`: folders — whether there is at least one sub-folder (for
  tree expanders), under the request's filters.
- `rating`: `"good"`, `"uncertain"`, `"damaged"`, `"unsupported"`; `null`
  for folders.
- `flags`: subset of `deleted`, `torn_record`, `missing_fragments`,
  `out_of_bounds`, `overwritten`, `overlap`, `compressed`, `encrypted`,
  `sparse`, `no_data`, `bad_sectors`, `signature_mismatch`, `signature_ok`.

## 3. Commands

### hello

→ `{"engine_version": "0.1.0", "protocol": 1, "os": "windows", "elevated": true, "raw_disk_support": true}`

### list_disks

→ `{"disks": [Disk, ...]}` (empty on non-Windows hosts).

### open_source

args `{"kind": "disk"|"image", "path": "..."}` — for images an optional
`"sector_size"` (default 512).

→ `{"source": SourceInfo, "table": "gpt", "partitions": [Partition, ...]}`

Closes any previously open source and drops its session and catalog.

### close_source

→ `{}`

### scan_start

args

```json
{"start": 1048576, "end": 4000786153472,
 "session_path": "C:\\Users\\me\\AppData\\Local\\HDSavior\\sessions\\disk1.hdsc",
 "resume": false, "collect_index": true, "collect_carve_hits": true}
```

`start`/`end`: absolute byte range, sector-aligned; default the whole source.
`session_path` (optional): the session is saved there periodically and at the
end; it is subject to the destination guard. `resume: true` continues the
currently loaded session (its own range is used).

→ `{"job": "scan"}`

Events

`scan_progress`

```json
{"phase": "mft_probe", "bytes_done": 1073741824, "bytes_total": 4000785104896,
 "position": 4294967296, "speed_bps": 157286400, "eta_seconds": 25430,
 "elapsed_seconds": 7, "records": 120344, "files": 118002, "dirs": 2342,
 "index_blocks": 904, "carve_hits": 31, "bad_bytes": 0}
```

`phase`: `"mft_probe"` (likely-MFT region first), `"sweep"` (rest of the
range), `"analyzing"` (building the catalog after the read pass).

`scan_done`

```json
{"status": "completed", "error": null, "session_path": "...", "catalog": CatalogInfo}
```

`status`: `"completed"`, `"stopped"` (cancelled by the user — results so far
are kept and a catalog is still built), `"failed"` (`error` set, `catalog`
`null` unless partial results exist).

### job_cancel

→ `{}` immediately; the running job then finishes with its `*_done` event
(`"stopped"` for a scan, `"cancelled"` for the others). No job → `{}`.

### session_save

args `{"path": "..."}` → `{"path": "...", "bytes": 12345678}`

### session_load

args `{"path": "...", "source": {"kind": "disk", "path": "..."}}` (`source`
optional: by default the source recorded in the session is reopened).

→

```json
{"source": SourceInfo, "source_open": true, "source_error": null,
 "catalog": CatalogInfo}
```

If the source cannot be opened or does not match the session (size, serial,
and a sample of stored records re-read from disk), `source_open` is `false`
and `source_error` explains why; the tree can be browsed but `recover_start`,
`read_file`, `carve_start` answer `no_source` until `session_attach_source`
succeeds.

### session_attach_source

args `{"kind": "disk"|"image", "path": "...", "force": false}` →
`{"source": SourceInfo}` or error `source_mismatch` (`force: true` skips the
identity check).

### catalog_info

→ `CatalogInfo`

### geometry_set

args `{"cluster_size": 4096, "volume_offset": 1048576}` → `CatalogInfo`

Health flags are recomputed. Ids of `dir`/`file`/`lost_dir`/`virtual` nodes
stay valid; carved nodes are discarded (carving must be re-run).

### list_children

args

```json
{"node": 0, "offset": 0, "limit": 200, "sort": "name", "desc": false,
 "dirs_only": false, "show_deleted": false, "hide_damaged": false}
```

`sort`: `"name"` (case-insensitive), `"size"`, `"modified"`, `"rating"`,
`"ext"`. Folders always sort before files. `limit` ≤ 1000.

→ `{"total": 5321, "rows": [Row, ...]}`

### node_info

args `{"node": 4711}` →

```json
{"row": Row,
 "ancestors": [{"id": 0, "name": ""}, {"id": 17, "name": "Photos"}],
 "path": "Photos/IMG_0042.JPG",
 "accessed": 1714566896000,
 "record": {"number": 91234, "seq": 3, "disk_offset": 3314748416, "instance": 0,
            "extension_records": 0},
 "data": {"resident": false, "real_size": 3456789, "initialized_size": 3456789,
          "allocated_size": 3457024, "runs": 1, "first_lcn": 812345,
          "first_offset": 3328413696},
 "flag_details": [{"flag": "signature_ok", "text": "The file starts with a valid JPEG header."}]}
```

`record` is `null` for virtual, lost and carved nodes; `data` is `null` for
folders.

### search

args

```json
{"query": "img_", "under": null, "exts": ["jpg", "png"], "min_size": null,
 "max_size": null, "show_deleted": false, "ratings": null,
 "sort": "name", "desc": false, "offset": 0, "limit": 200}
```

`query`: case-insensitive substring of the name; `""` matches everything.
Only `file` and `carved` nodes are returned.

→ `{"total": 1234, "rows": [Row + {"path": "Photos/IMG_0042.JPG"}, ...]}`

### selection_summary

args `{"include": [17, 99], "exclude": [4711], "include_deleted": false, "skip_damaged": false}`

A node is selected when its nearest ancestor-or-self that appears in
`include` or `exclude` is in `include`.

→ `{"files": 5320, "bytes": 18234567890, "dirs": 12, "good": 5300, "uncertain": 12, "damaged": 5, "unsupported": 3}`

### check_destination

args `{"path": "D:\\Rescued", "purpose": "recover", "need_bytes": 18234567890}`
(`purpose`: `"recover"`, `"image"`, `"session"`)

→

```json
{"verdict": "ok", "message": "", "free_bytes": 500000000000, "enough_space": true}
```

`verdict`: `"ok"`, `"overlaps_source"` (always refused), `"same_disk"` and
`"unknown_disk"` (need `allow_same_disk`), `"not_found"`, `"not_writable"`.

### recover_start

args

```json
{"include": [17, 99], "exclude": [4711], "dest": "D:\\Rescued",
 "include_deleted": false, "skip_damaged": false,
 "allow_same_disk": false, "on_exists": "rename"}
```

`on_exists`: `"rename"` (default), `"skip"`, `"overwrite"`.

→ `{"job": "recover", "files": 5320, "bytes": 18234567890}` or error
`destination_refused`.

Events

`recover_progress`

```json
{"files_done": 120, "files_total": 5320, "bytes_done": 734003200,
 "bytes_total": 18234567890, "current": "Photos/IMG_0042.JPG",
 "speed_bps": 104857600, "eta_seconds": 167,
 "ok": 118, "warnings": 2, "failed": 0, "skipped": 0}
```

`recover_done`

```json
{"status": "completed", "error": null, "ok": 5300, "warnings": 12, "failed": 5,
 "skipped": 3, "bytes_written": 18230000000,
 "dest": "D:\\Rescued", "report_path": "D:\\Rescued\\HDSavior-report.csv"}
```

`status`: `"completed"`, `"cancelled"`, `"failed"`.

### carve_start

args `{"kinds": null}` (`null` = all supported kinds, else a list such as
`["jpeg", "mp4"]`)

→ `{"job": "carve", "candidates": 48213}`

Events: `carve_progress` `{"candidates_done": 100, "candidates_total": 48213, "found": 61, "bytes_found": 310000000}`;
`carve_done` `{"status": "completed", "error": null, "found": 913, "bytes_found": 4100000000, "catalog": CatalogInfo}`.

### image_start

args `{"start": 0, "end": 4000787030016, "dest": "D:\\disk1.img", "allow_same_disk": false, "resume": false}`

→ `{"job": "image", "bytes": 4000787030016}`

Events: `image_progress` `{"bytes_done", "bytes_total", "speed_bps", "eta_seconds", "bad_bytes"}`;
`image_done` `{"status", "error", "bytes_written", "bad_bytes", "path", "bad_map_path"}`
(`bad_map_path` is `null` when there were no unreadable sectors).

### read_file

args `{"node": 4711, "offset": 0, "length": 65536}` (`length` ≤ 8 MiB)

→ `{"data_b64": "...", "size": 3456789, "eof": false, "kind": "jpeg"}`

Reads through the same code path as recovery. `kind` is the sniffed file
kind of the returned bytes when `offset == 0` (else `null`).

### shutdown

→ `{}` and the engine exits.

## 4. Other events

`log` — `{"level": "info"|"warn"|"error", "message": "..."}`; informational
only.
