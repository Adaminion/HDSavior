# HDSavior

HDSavior gets files back from a hard disk whose NTFS volume was accidentally
quick-formatted (formatted as NTFS again). It looks for the old MFT `FILE`
records that the format left behind, rebuilds the old folder tree from their
parent references, and copies each file's clusters, as listed in its old
runlist, to **another** drive. Files whose record is gone can be found by
their file signature ("carving"). It can also copy the whole disk to an
image file first, so that the rescue works on the copy.

It is a Rust engine (`engine/`, the program `hdsavior-engine`) and a Flutter
Windows app (`app/`) that runs the engine as a child process. The app has
no disk access of its own.

Status: work in progress (version 0.1.0). No release has been published. CI
builds a preview of the Windows app as a build artifact.

- [What it does and does not do](#what-it-does-and-does-not-do)
- [It never writes to the disk being rescued](#it-never-writes-to-the-disk-being-rescued)
- [I just quick-formatted my disk: what to do](#i-just-quick-formatted-my-disk-what-to-do)
- [Ratings, statuses and warnings](#ratings-statuses-and-warnings)
- [Command line](#command-line)
- [Building](#building)
- [Tests and fixtures](#tests-and-fixtures)
- [Limitations and known issues](#limitations-and-known-issues)
- [Project layout](#project-layout)
- [Documentation](#documentation)

## What it does and does not do

It does:

- Read a physical disk (`\\.\PhysicalDrive<N>`), one partition of it, or a
  raw image file of either.
- Scan the source in one sequential pass for old MFT records, directory
  index blocks, NTFS boot sectors and file-signature hits. A scan can be
  stopped and resumed, and is saved in a session file, so you do not have to
  scan again to recover more files later.
- Rebuild the old folder tree, separately for each file system it finds:
  the one you lost (`main`), older ones from before an earlier format, other
  volumes, and file systems stored inside disk-image files on the drive
  (browse only).
- Recover file contents through their runlists, including fragmented files
  whose runlist spans several MFT records, sparse files and files whose
  valid data length is shorter than their size. Times are restored.
- Tell which files were overwritten since the format by reading the new
  file system's own bitmap and MFT.
- Carve 17 file types whose MFT record is gone, if they were stored in one
  piece: `jpeg png gif bmp tiff pdf zip mp4 heic cr3 avi wav webp mkv mp3 7z
  rar` (ZIP-based formats such as Office documents are found by the `zip`
  kind and keep their own extension, e.g. `.docx`).
- Copy a disk, a partition or a byte range sector by sector to an image file
  on another disk, skipping unreadable sectors and listing them. An image
  can be resumed.

It does not:

- Work on anything but NTFS as the lost file system. ReFS, exFAT and FAT
  volumes, BitLocker volumes, Storage Spaces and dynamic disks are not
  supported. (The scan runs, but HDSavior cannot tell what was written since
  the format on a current volume that is not NTFS.)
- Recover NTFS-compressed (LZNT1) or WOF-compressed files, EFS-encrypted
  files, OneDrive and other cloud placeholders, or deduplicated files. They
  are listed and marked "Can't recover" (`unsupported`), and never written.
- Recover alternate data streams.
- Help after a *full* format (Windows Vista and later write zeros over the
  whole volume) or on a device that discarded its data when it was
  formatted: SSDs and other TRIM-capable devices, which includes many SMR
  hard disks.
- Repair the disk in place. It only copies data off it.
- Read physical disks on anything but Windows. On Linux and other systems the
  engine works on image files only (which is how it is tested).

## It never writes to the disk being rescued

This is the main design rule. It is enforced in the engine, not only by the
app, and every rule below can be checked in the code (they are the safety
invariants S1 to S7 in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#2-safety-invariants)).

- **Read-only access.** A device is opened in exactly one function,
  `hds_io::open_device`, with either no access at all (for listing) or
  `GENERIC_READ`. Never with write access. Device control codes go through
  one wrapper with a closed list of six read-only queries; there is no lock,
  dismount, offline or attribute change. The `BlockSource` trait that all
  reading code uses has no write method. The engine also never opens a path
  on a volume that is not mounted, because the first mount of an NTFS volume
  writes to it.
- **Image files** used as a source are opened read-only and shared for
  reading only, so Windows refuses any program that tries to write,
  truncate or delete them while they are open.
- **Every output location is checked before anything is created** (the
  "destination guard"). The path is resolved by handle to its volume and to
  the physical disk extents it occupies, through virtual disks (VHD/VHDX)
  recursively. If it overlaps a *protected range* it is refused, with no
  override. The protected ranges are the scanned range (for imaging: the
  imaged range) and, once the scan has rebuilt a catalog, the full extent of
  every lost file system it found, which can reach into other partitions
  after a disk was re-partitioned. Before any scan exists, the whole source
  disk is protected. The check runs again at the start of every job. This
  covers recovered files, image files, session files, reports, temporary
  files and the app's own data.
- **Same disk or unknown disk needs consent.** A location on the source disk
  but outside the protected ranges is refused unless you allow it
  (`--allow-same-disk`, or a confirmation dialog in the app). A location
  whose disk cannot be determined (Storage Spaces, a share on this machine,
  WSL or WebDAV paths, and so on) needs its own, separate consent
  (`--allow-unknown-disk`). The one exception: session files on the Windows
  system volume need no consent (the app keeps them in
  `%LOCALAPPDATA%\HDSavior\Sessions`); the overlap check still applies. Do not give either consent unless
  you are sure: the safe choice is always a different physical disk.
- **Output never follows links and never replaces.** Below the destination
  folder the engine does not enter junctions, symlinks or mount points,
  creates files only if they do not exist yet, and renames without
  replacing. (The one exception you choose yourself: with "overwrite" for
  files that already exist, an existing regular file in the destination is
  deleted before the new one is renamed into place; folders and links
  never are.) All file writes of the engine go through one module,
  `hds_io::out`.
- **A disk that disappears is not a bad sector.** If the source stops
  answering, the job ends with `source_lost`; nothing is recorded as bad and
  no zero-filled file or image is written.

The app is marked `requireAdministrator` (reading raw disks needs it), shows
a permanent notice to stop using the disk being rescued, and warns when it
is itself running from that disk (`app_on_source`).

## I just quick-formatted my disk: what to do

1. **Stop using the disk now.** Every file written to it can overwrite
   something you want back. Do not copy anything to it, do not install
   HDSavior or anything else on it, do not run `chkdsk` or any repair tool on
   it. If you can, unmount it or unplug it until you are ready. If it was the
   disk Windows runs from, shut the computer down and rescue it from another
   Windows installation.
2. **Have a second disk ready** with at least as much free space as the
   formatted disk (for the image) plus room for the recovered files. Recovered
   files must go to a different physical disk.
3. **Check what kind of disk it is.** In HDSavior's Source screen each disk
   card shows its warnings. If it says the disk supports TRIM or is an SSD,
   the format most likely erased the data and there is little to recover. A
   full format (not quick) also leaves nothing.
4. **Make an image first** ("Create disk image…" on the Source screen, or
   `hdsavior-engine image`). This copies the disk once, sequentially, and
   from then on all work reads the copy. It is the safest choice for a disk
   that may be failing, and it lets you put the original away. Unreadable
   sectors are filled with zeros and listed in `<image>.bad.txt`. If the copy
   is interrupted, start it again and choose to continue it. When it is
   done, "Rescue from this image" opens it.
   (You can also scan the disk directly; HDSavior only reads it. The image is
   recommended, not required.)
5. **Scan.** Choose "Scan the whole disk" (or the image), or "Scan this
   partition". Scanning the whole disk is safer if the disk was ever
   re-partitioned. The scan goes through three phases: `mft_probe` (the
   places where an MFT is expected), `sweep` (the rest of the range) and
   `analyzing`. When the phase changes to `sweep`, most of the folder tree has
   usually been found; "Stop and show results" ends the scan early and the
   scan can be resumed later. The session is saved as it goes.
6. **Browse.** The Results screen shows the lost file system first, then any
   other file systems found. Folders whose names could not be recovered are
   under `[Lost folders]` (as `Folder #N`); files whose folder is unknown are
   under `[Orphaned files]`. Use search and the filters (deleted files, file
   type, condition). Look at each file's rating (next section); the details
   pane explains every flag and can preview pictures and text. The
   disk-layout chip shows how sure HDSavior is about the old volume's cluster
   size and position; if it says the evidence was too thin, files are rated
   at most "Uncertain".
7. **Select and recover.** Tick folders and files, then "Recover…". Choose a
   folder on another disk. The engine checks it and says why if it refuses
   it. Options: include deleted files, skip damaged files, and what to do
   with files that already exist in the destination (rename, skip, or
   overwrite). Each run writes a report `HDSavior-report-<date-time>.csv` in
   the destination with one row per file.
8. **Carve as a fallback.** For files that are not in the tree (their MFT
   record was overwritten, typically the first ~220 files and folders ever
   created on the volume), use "Find more files by signature". Found files
   appear under `[Carved files]/<extension>/` (for example `[Carved files]/jpg/`)
   and are recovered like any other.
   Only files stored in one piece come back whole.
9. **Check what you got.** Open the report. Files with `warning:` or rated
   "Uncertain" or "Damaged" may be incomplete or partly wrong; open them
   before you rely on them.

You can close the app and come back later with "Open saved scan…". Without
the disk or image attached, a saved scan can be browsed but not recovered
from; "Attach the disk…" reconnects it.

## Ratings, statuses and warnings

Every file has a **rating** (shown as its condition). The first matching row
wins ([ALGORITHMS G3](docs/ALGORITHMS.md#g-health)):

| rating | shown as | means |
|---|---|---|
| `unsupported` | Can't recover | Compressed, encrypted, a cloud placeholder, or inside a disk-image file. Never written. |
| `damaged` | Damaged | Part of it is missing or was overwritten: fragments missing, data outside the volume, overwritten since the format or claimed by a newer file, unreadable sectors, or its first bytes do not match its extension. What is left can still be recovered. |
| `uncertain` | Uncertain | Recoverable, but something is not sure: it was already deleted before the format, shares clusters with another file, its record was torn, it comes from an older file system or a stray record, the disk layout was not confirmed, or it was carved and its end could not be confirmed. |
| `good` | Good | Everything indicates it will come back intact. |

Each recovered file gets a **status** in the report
([ALGORITHMS H11](docs/ALGORITHMS.md#h-recovery)):

| status | means |
|---|---|
| `ok` | Written, rated good, and its first bytes match its extension. |
| `ok_unverified` | Written, rated good, but the type could not be checked (unknown type or empty file). |
| `warning:<flags>` | Written, but the rating after reading was not good. Check the file. |
| `skipped:<reason>` | Not written: `compressed`, `encrypted`, `placeholder`, `embedded`, `damaged` (with skip damaged), `exists`, `too_large` (bigger than the destination file system allows, e.g. FAT32's 4 GiB), `path_too_long`. |
| `failed:<code>` | Nothing left at the destination: `record_changed` (the record changed since the scan), `write_error`, `io_error`. |

**Warnings about the source** (shown on the disk card when it is opened):

| code | means |
|---|---|
| `trim_supported` | The device can discard data on format. Recovery is unlikely. |
| `system_disk` | Windows runs from this disk, so it keeps writing to it. |
| `mounted_volume` | The range is a mounted volume; Windows may write to it. |
| `page_file_on_source` | A page file lives on it and is rewritten all the time. |
| `source_reads_zero` | The start of the source reads as zeros (wiped or full format). |
| `app_on_source` | HDSavior itself may run from this disk (or that could not be ruled out). Run it from another disk. |

**Warnings about the scan result** (shown above the results):

| code | means |
|---|---|
| `scan_incomplete` | The scan was stopped before the end. Resume it to find more. |
| `out_of_range_files` | Some files have data outside the scanned range. Scan the whole disk. |
| `session_file_at_risk` | The session file lies inside the lost volume. Save it elsewhere. |
| `session_file_not_saved` | The last write to the session file failed (e.g. disk full). Save it elsewhere. |
| `mixed_generations` | Records of an older file system could not be cleanly separated; some files may be shown in an older version. |
| `writes_not_detectable`, `live_volume_incomplete` | HDSavior could not fully read the current volume, so it cannot tell everything that was overwritten since the format. |
| `carve_hits_capped` | Too many signature hits to keep them all; carving will not see every one. |
| `identity_differs` | The attached source does not look exactly like the one that was scanned. |
| `old_session_format`, `session_incomplete` | The session was written by an older engine, or the file ends early. |

The full list with exact wording is in
[PROTOCOL.md, "Warning"](docs/PROTOCOL.md#warning).

## Command line

The engine is a complete command-line tool. The app uses `serve`; the other
subcommands start the same engine in-process and print every result and
event as one JSON object per line on stdout. Logs go to stderr.

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
hdsavior-engine help    (also --help, -h)
```

| subcommand | what it does |
|---|---|
| `serve` | Runs the JSON-lines protocol server on stdin/stdout ([PROTOCOL.md](docs/PROTOCOL.md)). This is what the app starts. |
| `disks` | Lists the physical disks (model, serial, size, bus, TRIM, volumes). Windows only; elsewhere the list is empty. |
| `scan` | Scans `<source>` and writes the session to `--session`. A source starting with `\\.\` is a disk, anything else an image file; `--kind` overrides that. `--partition K` scans the partition whose `index` is K in the source description the command prints (1 is the first entry of an MBR or GPT table), `--start`/`--end` a byte range (sector-aligned); the default is the whole source. `--no-carve-hits` skips collecting signature hits (carving is then impossible). An existing session file is refused unless `--force` replaces it or `--resume` continues it. |
| `info` | Loads a session and prints it (catalog, file systems, warnings). With `--source`, fails if that path is not the session's source. |
| `ls` | Lists one folder: the lost file system's root by default, `--node ID`, or `--path P` (folder names separated by `/` or `\`, case-insensitive, from the root). `--deleted` includes deleted entries. `--limit` defaults to 200. |
| `find` | Searches file names for `<query>` (case-insensitive substring), optionally only the extensions in `--ext`. |
| `recover` | Recovers `--all` (every file system's root) or the node ids in `--include` (comma-separated, from `ls`/`find`), minus `--exclude`, into `--dest`. Deleted files are left out unless `--include-deleted`; `--skip-damaged` leaves out files rated damaged. `--on-exists` defaults to `rename`. Saves the session back afterwards. |
| `carve` | Runs signature carving over the stored hits, for all kinds or the ones in `--kinds` (names as listed above: `jpeg,png,…,7z,rar`). Saves the session back, so `ls`/`recover` see the carved files. |
| `image` | Copies `<source>` (or `--partition K`, or `--start`/`--end`) to the image file `--dest`. Writes `<dest>.hdsimg` (progress and source identity) and, if sectors were unreadable, `<dest>.bad.txt`. An existing image is refused unless `--resume` continues it. |

`--source <path>` on `info`, `recover` and `carve` attaches the disk or image
when the session's recorded source is not found by itself. The consent flags
`--allow-same-disk` and `--allow-unknown-disk` are described
[above](#it-never-writes-to-the-disk-being-rescued). Options take their
value as the next argument or after `=` (`--limit=50`); `--` ends the options.

Exit codes: `0` ok, `1` the command or job failed, `2` usage error, `130`
stopped with Ctrl-C. The first Ctrl-C stops a running job cleanly (a scan
saves its session so `--resume` can continue; a recovery removes its
unfinished file; `recover` and `carve` still save the session); a second
Ctrl-C ends the program at once.

Example (Windows, elevated prompt; disk 2 is the formatted disk, `E:` is
another disk):

```powershell
hdsavior-engine disks
hdsavior-engine image \\.\PhysicalDrive2 --dest E:\rescue\disk2.img
hdsavior-engine scan E:\rescue\disk2.img --session E:\rescue\disk2.hdsc
hdsavior-engine ls --session E:\rescue\disk2.hdsc --path Users/me/Pictures
hdsavior-engine find --session E:\rescue\disk2.hdsc holiday --ext jpg,mp4
hdsavior-engine recover --session E:\rescue\disk2.hdsc --dest E:\rescue\files --include 4711,4800
hdsavior-engine carve --session E:\rescue\disk2.hdsc --kinds jpeg,png
hdsavior-engine recover --session E:\rescue\disk2.hdsc --dest E:\rescue\all --all --include-deleted
```

### Developer tools (examples)

These are `cargo` examples for development and diagnosis, not part of the
product. Run them from `engine/`.

| command | what it does |
|---|---|
| `cargo run --release -p hds-carve --example probe -- FILE...` | Carving probe: runs signature detection and length measurement on whole, intact files and compares the measured length with the file size (`ok`, `short`, `long`, `none`, `-`). Use it on a folder of your own photos and videos; any `short`, `long` or `none` on an intact file is worth a bug report. |
| `cargo run --release -p hds-ntfs --example scan -- IMAGE [--dump N] [--bench]` | Walks an image like the scanner and prints what the NTFS parsers find. |
| `cargo run --release -p hds-core --example scanimg -- IMAGE [--start N] [--end N] [--no-carve] [--session FILE] [--dump N]` | Scans an image the way the engine does and reports speed and memory. |
| `cargo run --release -p hds-core --example bench -- DIR FIXTURE_IMAGE [RECORDS]` | Benchmarks the scanner and session file on a large synthetic image. |
| `cargo run --release -p hds-core --example labcat -- IMAGE START END OUT.json [--scan-time FILETIME] [--resume-check]` | Catalog dump for the instance-separation lab (`scripts/linux/instances/`). |
| `cargo build --release -p hds-io --example winprobe` | Windows probe for raw disks, the destination guard and output primitives (`winprobe disks`, `read`, `table`, `dest`, `out`). Used by CI through `scripts/windows/ci-io-probe.ps1`. |

## Building

Toolchain versions used by CI: Rust **1.97.0**, Flutter **3.50.1** (stable,
Dart 3.14).

### Engine (any OS)

```sh
cd engine
cargo build --release -p hds-engine      # -> target/release/hdsavior-engine(.exe)
```

The engine is pure Rust with no C dependencies. The release profile must keep
`panic = "unwind"` (the crate refuses to compile with `abort`): a panic in a
request is caught and answered as an `internal` error.

Windows-only code (raw disk access, disk listing, the Windows destination
guard) is compiled only on Windows. From Linux it can be type-checked but
not run:

```sh
rustup target add x86_64-pc-windows-msvc
cargo clippy --target x86_64-pc-windows-msvc -p hds-io -p hds-engine --all-targets -- -D warnings
```

### Windows app

Requirements: Windows 10 or 11, Flutter 3.50.1, Visual Studio 2022 with the
"Desktop development with C++" workload, and the Rust toolchain.

```powershell
cd engine
cargo build --release -p hds-engine
cd ..\app
flutter pub get
flutter build windows --release   # -> build\windows\x64\runner\Release\hdsavior.exe
```

The build copies the engine next to `hdsavior.exe`, taking the file named by
the environment variable `HDSAVIOR_ENGINE_EXE` if set, otherwise
`engine\target\release\hdsavior-engine.exe`. Without either the build still
succeeds, with a CMake warning, and the app says the engine is missing.

The app asks for administrator rights at start (one UAC prompt).
`flutter run` must therefore be started from an elevated terminal, or
Windows refuses with error 740. In debug and profile builds the environment
variable `HDSAVIOR_ENGINE` can point to another engine executable; release
builds ignore it. The app can also run against a fake engine written in
Dart, without any disk:

```powershell
flutter run -d windows -t test/fake_engine/run_app.dart
```

More in [app/README.md](app/README.md).

## Tests and fixtures

### Engine

```sh
cd engine
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Most crates have unit tests and randomized robustness tests (arbitrary input
must never make a parser panic or hang). `hds-testkit` builds synthetic NTFS
images for cases real drivers do not produce on demand.

Tests that need real NTFS volumes read them from the directory in
`HDS_FIXTURES` and print `SKIPPED` without it. The fixtures are built on Linux
with `mkntfs` and `ntfs-3g` (needs root, because it mounts; about 80 seconds
and about 510 MB on disk, 1.7 GiB apparent size, as the images are sparse):

```sh
sudo apt-get install -y ntfs-3g fuse3 fdisk ffmpeg python3-pil
sudo scripts/linux/make-fixtures.sh                 # -> fixtures-out/ (git-ignored)
scripts/linux/make-fixtures.sh --list               # the 16 scenarios
HDS_FIXTURES=$PWD/fixtures-out cargo test --manifest-path engine/Cargo.toml
```

Each scenario writes `<name>.img` and `<name>.manifest.json` (the ground
truth). `scripts/linux/check-recovery.py` checks a recovery against a
manifest. See [scripts/linux/README.md](scripts/linux/README.md).

The protocol transcript `docs/protocol-examples.jsonl` is recorded from the
real engine on the `basic` fixture and compared by the engine tests;
`HDS_TRANSCRIPT=write` regenerates it. The Dart fake engine replays it, so
both sides stay in step.

A scale test against a million-record image is ignored by default:

```sh
cd engine
HDS_SCALE_IMAGE=/path/scale-million.img \
  cargo test --release -p hds-engine --test scale -- --ignored --nocapture
```

The image is built with `hds_testkit::scenarios::scale::million_records`.

### App

```sh
cd app
flutter pub get
flutter analyze
flutter test
```

The widget tests run against the Dart fake engine and work on Linux too.
Three tests also drive the real engine binary (scan, browse, recover and
reload a session through the app's state layer, and compare the protocol
with the fake engine); they are skipped unless both `HDSAVIOR_ENGINE` (path of
`hdsavior-engine`) and `HDSAVIOR_TEST_IMAGE` (a fixture image such as
`basic.img`) are set. The Linux CI job sets both.

### CI

`.github/workflows/ci.yml` runs on every push and pull request that changes
code (changes to Markdown files and `docs/` alone do not start it):

- **Linux:** builds the fixtures, then `cargo fmt`, `cargo clippy` and
  `cargo test` with `HDS_FIXTURES`, the testkit tests, and
  `flutter analyze` / `flutter test` (including the real-engine tests on
  `basic.img`).
- **Windows:** clippy and tests on Windows, then
  `scripts/windows/ci-io-probe.ps1`, which creates real virtual disks with
  `diskpart` and checks raw reads and the destination guard (partition,
  other disk, mounted folder, junction, symlink, `SUBST` drive); then
  `scripts/windows/ci-roundtrip.ps1`, which has Windows quick-format a
  populated volume, rescues it with `hdsavior-engine scan` and
  `recover --all` through `\\.\PhysicalDriveN`, checks every file's
  SHA-256 and that the source disk is byte-identical afterwards.
- **Windows app:** builds the engine and `flutter build windows --release`,
  checks that the engine is bundled and the manifest requires administrator
  rights, and uploads the bundle as `hdsavior-windows-preview`.
- **Windows quick-format facts** (informational, may fail):
  `scripts/windows/format-facts.ps1` measures what a real Windows quick
  format leaves behind.

## Limitations and known issues

What cannot be recovered (by the nature of a quick format;
[ARCHITECTURE §8](docs/ARCHITECTURE.md#8-known-limitations)):

- A quick format on Windows rewrites MFT records 0 to 255. If the old volume
  was formatted on the same partition, the new MFT lands on the old one, so
  roughly the first 220 files and folders ever created lose their record.
  Their content can only come back through carving (if unfragmented and of a
  supported type); files small enough to live inside their MFT record are
  gone. Folders among them lose their names unless an index entry survives,
  and their contents appear under `[Lost folders]/Folder #N`.
- If the lost volume held fewer files than that, none of its records
  survive; what the tree shows then belongs to an older file system, and
  only carving reaches the lost files.
- Carving cannot see fragmentation in formats whose length comes only from a
  header; such files are rated uncertain.
- Files from before an earlier format are shown in their own tree and rated
  at most uncertain. Where nothing proves they belong to another file system
  they stay in the main tree (their dates tell them apart).
- Files written and deleted again after the format leave no trace once their
  record has been reused; what they overwrote can look intact. Types the
  engine can check are caught by the signature check; others end up
  `ok_unverified`.
- Volumes formatted by Windows NT 4 or 2000 do not store record numbers in
  records, so their files are placed in the tree only where their position
  reveals it.

Known issues in the current code:

- Loading a saved session takes about 2 seconds per million files.
- "Save as" over an existing session file has a short window in which the
  only complete copy is the temporary file `~hds-session-*.tmp` next to it.
  If the program dies in that window, that temporary file is a complete
  session: open it or rename it.
- The engine's Ctrl-C handling on Windows (console control handler) is
  implemented but has never been run on Windows; it is tested on Linux only.
- A disk recorded in a session without a serial number cannot be recognised
  again when the session is reopened without it attached, so destinations
  are judged `unknown_disk` (consent needed) in that state.
- Windows CI rescues a volume that Windows itself quick-formatted (a 1 GiB
  virtual disk, see [ARCHITECTURE §7](docs/ARCHITECTURE.md#7-testing)), but
  not yet larger disks, 4Kn sectors, dynamic disks or real USB hardware;
  [docs/hardware-test.md](docs/hardware-test.md) is a checklist for that.
  The app's Windows-only behaviour (UAC start, drive listing, UNC
  destinations) has no automated test beyond the build.
- No installer or signed release exists yet.

## Project layout

```
docs/                    the contract: ARCHITECTURE.md, ALGORITHMS.md, PROTOCOL.md,
                         protocol-examples.jsonl, notes/ (verified on-disk and Windows facts)
engine/                  Cargo workspace
  crates/hds-ntfs/       pure NTFS parser, no I/O, never panics on any input
  crates/hds-io/         block sources, image files, Windows raw disks, disk listing,
                         MBR/GPT, destination guard, the only file-writing code (out)
  crates/hds-carve/      file signatures and per-format length/validity walkers
  crates/hds-recover/    content reader, recovery writer and report, imaging
  crates/hds-core/       scanner, session file, current volumes, file-system instances,
                         catalog, geometry, health, queries, carving
  crates/hds-engine/     the hdsavior-engine binary: CLI and JSON-lines server
  crates/hds-testkit/    dev-only synthetic NTFS image builder
app/                     Flutter Windows app (lib/, test/ with the fake engine, windows/ runner)
scripts/linux/           fixture builder, independent NTFS inspector, recovery checker,
                         instance-separation lab
scripts/windows/         CI probes for real virtual disks and quick-format facts
.github/workflows/ci.yml CI
```

## Documentation

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): scope, safety invariants,
  what a quick format leaves behind, components, UI, testing, limitations.
- [docs/ALGORITHMS.md](docs/ALGORITHMS.md): the normative rules for scanning,
  sessions, instances, catalog, geometry, health ratings, recovery, carving
  and imaging.
- [docs/PROTOCOL.md](docs/PROTOCOL.md): the JSON-lines protocol between the
  app and the engine.
- [docs/notes/](docs/notes/): verified reference facts:
  [NTFS on disk](docs/notes/ntfs-on-disk.md),
  [quick-format behaviour](docs/notes/quick-format-behaviour.md),
  [Windows raw I/O](docs/notes/windows-raw-io.md).
- [docs/hardware-test.md](docs/hardware-test.md): a step-by-step rescue test
  on real hardware, with a helper script that proves the source is untouched.
- [app/README.md](app/README.md): the app's screens, build details, fake
  engine and tests.
- [scripts/linux/README.md](scripts/linux/README.md): how the fixtures are
  made and checked.
