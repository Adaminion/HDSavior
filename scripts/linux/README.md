# Linux fixtures: real NTFS volumes, really quick-formatted

This directory builds the disk images HDSavior's engine is tested against on
Linux (ARCHITECTURE.md section 7, item 3), contains an independent tool to
look into them, and a checker that tells whether a recovery of a fixture is
correct.

| File | What it is |
|---|---|
| `make-fixtures.sh` | Entry point. Runs `mkntfs`, `ntfs-3g`, `umount`, `sfdisk`, `dd` and drives the helper. Its header documents the manifest schema in full. |
| `fixture_helper.py` | Worker: populates volumes, captures ground truth, re-reads volumes raw, simulates the Windows-style rewrite of the new `$MFT`'s slots, compares before / after for every file system of the scenario, writes manifests. |
| `fixture_content.py` | Deterministic file contents and the media / document samples. |
| `inspect-ntfs.py` | Stand-alone, dependency-free dumper of FILE records, INDX blocks and NTFS boot sectors found anywhere in an image. Shares no code with HDSavior. |
| `check-recovery.py` | Checks a recovery (output folder + report CSV) against a manifest: ARCHITECTURE.md 7.3's properties, see "Checking a recovery". |
| `reference-recovery.py` | An independent, simple recovery that reads only the image (never the manifest), following ALGORITHMS.md; exists to prove the checker and the manifests. |
| `test-check-recovery.py` | Runs the reference recovery on fixtures, requires the checker to accept it, then plants faults and requires the checker to catch each. |
| `instances/` | A separate lab for the instance-separation rules (its own README); not built by `make-fixtures.sh`. |

Nothing is committed but these scripts. Images, manifests and samples are
produced by running the build; the default output directory `fixtures-out/` is
git-ignored.

## Building

```sh
sudo apt-get update
sudo apt-get install -y ntfs-3g fuse3 fdisk ffmpeg python3-pil
sudo scripts/linux/make-fixtures.sh                 # -> <repo>/fixtures-out
sudo scripts/linux/make-fixtures.sh /some/dir       # -> /some/dir
sudo scripts/linux/make-fixtures.sh --keep-pre      # also keep <name>.pre.img
sudo scripts/linux/make-fixtures.sh --only generations,live-collision
scripts/linux/make-fixtures.sh --list
```

The whole build (16 scenarios) takes about 80 seconds on a 4-core machine and
leaves about 550 MB on disk (1.7 GiB apparent size; the images are sparse). It
needs root because it mounts. Per scenario it writes `<name>.img` and
`<name>.manifest.json`; with `--keep-pre` also `<name>.pre.img`, the same image
right before the accident (for `repartition`: with the old partition table).
When run through `sudo`, the results are handed to the invoking user. A failed
run stops at the first broken promise, unmounts whatever is mounted (also on
Ctrl-C / SIGTERM) and leaves its work files and the tools' output in
`<out>/.work/` for inspection; a successful run removes that directory. Do not
edit the scripts while a build runs: bash reads its script as it goes.

### Requirements

- `ntfs-3g` package: `mkntfs`, `ntfs-3g`, `ntfsinfo` (built and tested with
  2022.10.3, the version in Ubuntu 24.04)
- `/dev/fuse` (the script tries `modprobe fuse` if it is missing)
- `sfdisk` (package `fdisk`), `mountpoint` (util-linux), coreutils
- `python3` 3.8 or newer with Pillow (`python3-pil`), `ffmpeg` and `ffprobe`
- optional: `7z` and `rar` (two more sample types), `blkid` (one more check)
- `ntfs-3g-dev` is not needed to run anything; its
  `/usr/include/ntfs-3g/layout.h` is the structure reference `inspect-ntfs.py`
  was written against

### On a GitHub-hosted runner

```yaml
  fixtures:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: |
          sudo apt-get update
          sudo apt-get install -y ntfs-3g fuse3 fdisk ffmpeg python3-pil
      - run: sudo scripts/linux/make-fixtures.sh "$RUNNER_TEMP/fixtures"
```

What this relies on, and how far it was checked:

- GitHub-hosted Linux runners are virtual machines with passwordless `sudo`,
  not containers, so the kernel's FUSE support and `/dev/fuse` are available
  to root. Ubuntu's `ntfs-3g` carries its own FUSE implementation ("integrated
  FUSE 28") and, run as root, mounts directly, without the `fusermount`
  helper. None of this could be tried on a runner from here.
- The runner images' software lists (actions/runner-images, Ubuntu 24.04 and
  26.04, November 2026) name neither `ffmpeg` nor `python3-pil`, so both must
  be installed. `ntfs-3g`, `fuse3` and `fdisk` are part of Ubuntu's standard /
  server / cloud-image package sets and are normally present already;
  installing them again is harmless. `gdisk` is not needed (sfdisk writes the
  GPT too).
- `ubuntu-latest` was Ubuntu 24.04 when this was written and was announced to
  move to 26.04. The build was run here with Python 3.13 / Pillow 13 and with
  Python 3.12.3 / Pillow 10.2.0 (the 24.04 system versions); both give
  byte-identical manifests. Ubuntu 26.04 (Python 3.14, ffmpeg 8) was not
  tried. A sample that a different ffmpeg cannot produce is skipped and listed
  under `samples_skipped`, except for MP4, MOV, AVI, WAV and MP3, without which
  the build fails.
- Run under `sudo`, the script uses `/usr/bin/python3` (sudo's `secure_path`),
  so Pillow must come from `apt`, not from a `setup-python` interpreter.
- In a job that uses `container:`, or in Docker generally, `/dev/fuse` and the
  right to mount are missing unless the container is started with
  `--device /dev/fuse --cap-add SYS_ADMIN` (and, with AppArmor,
  `--security-opt apparmor:unconfined`). No loop device is needed anywhere.

## How a fixture is made

Every scenario goes through the same steps; the scenario functions in
`make-fixtures.sh` read top to bottom as this list.

1. **Format.** A sparse file of zeros is formatted with `mkntfs -Q -F` and
   explicit geometry (`-c`, `-s`, `-p`, `-H`, `-S`). `-Q` because a full format
   of an all-zero file would only write the same zeros again and un-sparse the
   image. The boot sector is checked against the arguments, and the last sector
   against the boot sector (backup copy).
2. **Populate** through `ntfs-3g` mounted read-write on the image file, in two
   phases with an unmount in between (the remount is what makes ntfs-3g reuse
   freed MFT records, see "deleted entries" below). Every lost volume starts
   with the *prelude* (see "Wipe policy"). All content comes from a fixed seed.
   Creation, modification and access time of every entry are set to fixed
   values through the driver (`system.ntfs_times`). Entries that are to be
   deleted are read back and hashed immediately before they are removed.
3. **Capture** through a second, read-only mount: every file and folder is
   listed, stat-ed, read and hashed, its three times, DOS name and named
   streams are read from the driver, and all of it must equal what step 2 wrote.
   This is the ground truth: path, type, size, SHA-256, MFT record number
   (inode number), times.
4. **Re-read raw** without the driver, using `inspect-ntfs.py`'s parser: the
   boot sector, the `$MFT` runlist, every MFT record. For every live entry the
   record must be in use, carry the right name, namespace, parent reference
   (record and sequence), link count and times; every file's content, read from
   the fixed-up record (resident) or through its runlists (including extension
   records, sparse runs, the initialized-size rule and LZNT1 for the compressed
   files), must give the driver's SHA-256; every folder's index entries (index
   root plus every INDX block, with the VCN each block must carry) must equal
   the driver's listing; an `$ATTRIBUTE_LIST` must name exactly the extension
   records that base references lead to; every in-use record of the volume must
   belong to a manifest entry. The promises of the scenario (tags such as
   `sparse`, `fragmented`, `contiguous`, `split`; "the MFT is fragmented"; ...)
   are checked here and the build fails loudly if one does not hold.
5. **Cross-check the inspector** against `ntfsinfo -v` for about a hundred
   records per file system: every header field, every attribute header, all
   runlists, names, attribute lists, index roots and (for small folders) INDX
   blocks; the boot sector against `ntfsinfo -m` and `blkid`.
6. **Accident.** The latest MFT-change time in the old volume's records is
   recorded and the script waits until the clock is in a later whole second
   (mkntfs stamps whole seconds; ALGORITHMS.md C3 compares MFT-change times with
   the new volume's creation time). Then `mkntfs -Q -F` again on the same bytes,
   then the Windows-like rewrite of the new `$MFT`'s slots up to 255, then
   whatever else the scenario calls for. Scenarios with several file systems
   build them one after the other the same way (steps 1-5 for each); a
   re-format inside the history (generations) gets the same wait and rewrite.
7. **Compare**, for every old file system of the scenario, its image right
   before it was lost with the final image: which old MFT record slots
   changed, which files still read back correctly from their old place, which
   old INDX blocks and index roots still name which folders, where each path
   breaks, what each destroyed parent's slot holds now, what on the disk shows
   the damage to each damaged file. Scenario promises about the outcome are
   checked.
8. **Blind scan.** The final image is scanned at every 512-byte offset for
   FILE records, without boot sector or `$MFT` runlist, the way HDSavior has to
   work (only the old cluster size and volume offset are taken as known). For
   every live file a matching in-use record is looked for, extension records
   are attached through their base references, and the content is read through
   the runs found. A file must come back bit-identical exactly when the
   manifest says `after.record_intact` and `after.content_intact`. (Done for the
   lost file system and an older generation; an embedded one lies behind its
   container's runlist.)
9. **Manifest**, and the image's SHA-256.

Volumes are mounted straight from image files, no loop device. `ntfs-3g` is run
with `no_detach` as a job of the script and `umount` is followed by `wait`: for
an image file, `umount` returns while the driver is still writing. Partitions
are formatted as separate files and copied into place (see "Decisions").

## Wipe policy

`mkntfs` writes a 27-record `$MFT` (64 records with 64 KiB clusters) where
Windows' format and first mount leave 256 records behind (docs/notes,
quick-format-behaviour.md section 3). Every scenario therefore ends its
accident with a rewrite of the new `$MFT`'s slots from the end of what mkntfs
initialised up to slot 255, at the place those slots have when the `$MFT` is
contiguous from its first cluster, as Windows lays it out:

- **zeros** (the Windows 10/11 form) everywhere, except
- **`wiped`**, which writes blank FILE records in the Windows 7 form: byte for
  byte the unused records of the Windows 7-era sample in docs/notes (`FILE`,
  update sequence number 2, sequence 1, flags 0, no attributes, own record
  number at 0x2C, a non-zero LSN).

The rewrite checks two promises and stops the build if either fails: the new
volume uses none of those bytes for anything but its own `$MFT`, and the bytes
held nothing of the old file system but MFT records. The second promise keeps
the model honest: the rewritten slots are free in the new `$Bitmap` (mkntfs'
`$MFT` does not own them), so damage to old *file data* there would be
invisible to the engine, which on Windows it is not.

No scenario is exempt. Where the rewrite lands differs:

- same geometry (most scenarios): new slots 27-255 = old records 27-255;
- `cluster-change`: the new `$MFT` starts at byte 131072 instead of 16384, so
  its slots 64-255 cover old records 176-367 (the format itself writes old
  records 0-175);
- `4k-records`: 256 records of 4 KiB (1 MiB); whether Windows makes the
  initial `$MFT` 256 records or 256 KiB with 4 KiB records is not known;
- `generations`: also the format that turned g1 into g2;
- `repartition`: only the first new partition, which starts where the old
  volume did. The second one lies over old file data: Windows would allocate
  its 256 slots to its `$MFT` (visible in its `$Bitmap`), here they would only
  destroy old data that nothing in the new file systems accounts for.

**The prelude.** Every lost volume (and g2) begins with the same first files and
folders, so that they land in the destroyed slots while their children,
created afterwards, survive: the 31 samples (one fragment each: the carving
case), `Projects` with 16 sub-folders and 4 `src` folders below them,
`small-parent/inner`, `Moved Early`, 48 more top-level folders (enough to make
the root index three INDX blocks long) and `early-junk/` files until the MFT
passes the last destroyed record. Then the children: `Photos/2019-summer/...`,
`Projects/pNN-*/README.txt` and `build.bin`, `src/main.c`, one `about.txt` per
top-level folder, ... and `Later Parent`, into which `Moved Early` is moved
(it keeps its low record). Afterwards each destroyed folder with surviving
children is one of:

- named by a surviving INDX block: the later blocks of the old root index
  (VCN 1, 2 lie in data clusters) name most top-level folders, `Projects`' block
  names `p01-alpha` ...;
- named by the index root of a surviving parent: `Later Parent/Moved Early`;
- not named anywhere: folders whose entry was in the root's first block (which
  the new root index overwrites) or in the index root of old record 5, and
  `Projects/p01-alpha/src`, `small-parent/inner`, listed only in the resident
  index root of a parent whose record is gone.

Their children get `after.ancestry`: whether the full path can be rebuilt and,
if not, which suffix survives (`Accounting 2019/about.txt` keeps `about.txt`).

## Scenarios

Numbers are from the toolchain named above; they are the same in every build
on it. "Lost" counts live entries whose record was destroyed; "lost folders"
are destroyed folders with surviving children (named by a usable index entry
inside the scan range / all).

| Scenario | Image | Old volume | Accident | Files / folders / deleted | Destroyed old records | Lost entries, lost folders (named) | Damaged content (record survived) |
|---|---|---|---|---|---|---|---|
| `basic` | 128 MiB volume | 4 KiB clusters | quick format, same parameters | 3511 / 125 / 14 | 0-255 | 192, 76 (55) | 0 |
| `fragmented` | 64 MiB volume | 4 KiB clusters, `$MFT` in 24 fragments | quick format, same parameters | 3191 / 96 / 1202 | 0-255 | 192, 76 (55) | 0 |
| `wiped` | 128 MiB volume | 4 KiB clusters | as basic, blank Windows 7 records | 3511 / 125 / 14 | 0-255 | 192, 76 (55) | 0 |
| `cluster-change` | 128 MiB volume | 4 KiB clusters | quick format with 64 KiB clusters | 1710 / 111 / 14 | 0-367 | 304, 76 (69) | 0 |
| `offset-mbr` | 72 MiB disk, MBR, partition at sector 2048 | 4 KiB clusters, 64 MiB | quick format, same parameters | 740 / 109 / 14 | 0-255 | 192, 76 (55) | 0 |
| `offset-mbr63` | 70 MiB disk, MBR, partition at sector 63 | 4 KiB clusters, 128457 sectors | quick format, same parameters | 740 / 109 / 14 | 0-255 | 192, 76 (55) | 0 |
| `offset-gpt` | 87 MiB disk, GPT, partition at sector 32768 | 4 KiB clusters, 64 MiB | quick format, same parameters | 740 / 109 / 14 | 0-255 | 192, 76 (55) | 0 |
| `post-write` | 128 MiB volume | 4 KiB clusters | quick format, then 146 new entries (27 MB), 6 deleted again | 1598 / 111 / 14 | 0-255 (219 changed) | 192, 76 (1) | 269 |
| `small-clusters` | 64 MiB volume | 512-byte clusters | quick format, same parameters | 740 / 109 / 14 | 0-255 | 192, 76 (55) | 0 |
| `big-clusters` | 64 MiB volume | 64 KiB clusters | quick format, same parameters | 737 / 108 / 14 | 0-255 (219 changed) | 192, 76 (17) | 0 |
| `4k-records` | 64 MiB volume | 4096-byte sectors and MFT records | quick format, same parameters | 740 / 109 / 14 | 0-255 | 192, 76 (39) | 0 |
| `generations` | 128 MiB volume | g2 on top of g1, 4 KiB clusters | quick format, same parameters | g2: 349 / 82 / 14; g1: 2599 / 32 | 0-255 | 192, 76 (55) | g1: 284 |
| `generations-restored` | as generations | g2's files with g1-era creation times | as generations | as generations | 0-255 | 192, 76 (55) | g1: 284 |
| `embedded` | 128 MiB volume | host 4 KiB clusters, 32 MiB inner volume in a file | quick format of the host | host 365 / 83 / 14; inner 691 / 13 | 0-255 | 192, 76 (55) | 0 |
| `repartition` | 136 MiB disk, MBR | 128 MiB partition at sector 2048 | two 56 MiB partitions, both quick-formatted | 1364 / 98 / 14 | 0-255 | 192, 76 (15 in scan range) | 15 (outside the scan range) |
| `live-collision` | 128 MiB volume | 4 KiB clusters | quick format, then 144 new entries (76 folders in old folders' slots), 5 deleted again | 1598 / 111 / 14 | 0-255 (219 changed) | 192, 76 (1) | 239 |

"219 changed": slots 27-63 of the old `$MFT` held never-used blank records that
ntfs-3g (or, with 64 KiB clusters, mkntfs) rewrote byte for byte; they are not
listed in `destroyed_records`.

What each one is for:

- **basic** - the general round trip. Everything whose record survived must
  come back bit-identical, at its path where `after.ancestry` says it can be
  rebuilt, otherwise below a lost folder; the prelude's samples are the carving
  case.
- **fragmented** - `frag/big-fragmented.bin` was written into the holes of a
  completely full volume: 878 data runs, a non-resident `$ATTRIBUTE_LIST`, five
  extension records, four of them with `$DATA` pieces and one holding the
  file's only `$FILE_NAME`; its six records lie in two different fragments of
  the `$MFT`. Three sparse files with real hole runs. Two files whose
  initialized size is smaller than their size; one of them
  (`valid-data/stale-bytes-after-initialized-size.bin`) has 3181 non-zero stale
  bytes on disk behind the initialized size, so a reader that ignores the
  initialized size produces the wrong hash. The old `$MFT` is in 24 fragments
  (the volume was full when it had to grow), so most of it can only be found
  by scanning. `ballast.bin`, deleted last, is fully intact as a deleted file;
  1201 other deleted files had their records reused.
- **wiped** - `basic` with the Windows 7 form of the rewrite: 229 blank FILE
  records where `basic` has zeros. A scanner must count and drop them (never
  used: not in use, no attributes).
- **cluster-change** - the new boot sector says 64 KiB clusters, the old
  runlists count 4 KiB clusters; the old cluster size has to come from the old
  structures. Old records 0-367 are gone (see "Wipe policy"); all three blocks
  of the old root index survive (the new root index lies elsewhere), so 52 of
  the 54 top-level folders keep their names.
- **offset-mbr**, **offset-mbr63**, **offset-gpt** - the volume sits inside a
  whole-disk image. `offset-mbr63` is the Windows XP layout: partition at
  sector 63 with an odd number of sectors, so every FILE record of the image
  lies at an odd multiple of 512 bytes. `offset-gpt` has a Microsoft reserved
  partition at sector 34 and the NTFS partition at 16 MiB, as Windows lays out
  a GPT data disk. All three have one spare "cylinder" of unallocated space
  behind the partition.
- **post-write** - the new volume was used. 384 old files have lost content
  (also among the destroyed prelude), 192 live entries their record, and new
  data overwrote the old INDX blocks of the destroyed folders, which are among
  the first old data. Of the old files whose record survived, 180 are
  overwritten by data the new file system still has allocated and 89 by new
  files that were deleted again: their clusters are free in the new
  `$Bitmap`, but the deleted records in the new MFT still map them
  (`damage_evidence: post-format-record`). 1214 old files are fully intact.
- **small-clusters**, **big-clusters** - an MFT record spans two clusters /
  sixteen index blocks share a cluster. Index block VCNs count clusters in the
  first case and 512-byte units in the second. With 64 KiB clusters the format
  rewrites the whole cluster of the new root index, and with it all three old
  root index blocks: only 17 lost folders keep a name.
- **4k-records** - `mkntfs` has no option for the MFT record size; it uses
  max(1024, sector size). A volume formatted with `-s 4096` therefore has
  4096-byte records, like a 4Kn disk. The prelude's small junk files are
  resident there, so their content went down with their records.
- **generations** - g1 (2599 files, 32 folders, created 2008-2013) was
  quick-formatted, with the same rewrite, and became g2 (349 files, 82
  folders, created 2019-2022), the lost file system; then the accident. Both
  sit at the same MFT origin: g1's records 64-504 were overwritten by g2's own
  records, 2187 g1 entries survive behind g2's last record. 274 g1 files had
  clusters reused by g2 (`clusters_reused_by_newer`, evidence `newer-file`),
  10 more lost content to g2 files deleted before the accident (evidence
  `none`). Six g1 folders were created around record 400, so g2 took their
  slots while their children survive: 48 g1 entries name a parent slot that now
  holds a g2 record, either one their reference resolves to
  (`parent_reference_taken_by`) or one that contradicts it
  (`parent_slot_contradiction`, the D2 (B) case). g1 entries are in
  `other_entries` (file system `g1`, kind `older`); `history` has the earlier
  re-format with its times.
- **generations-restored** - the same, but g2's files carry creation and
  modification times as old as g1's (as when files are restored from a
  backup): 185 of 349 g2 files are older than g1's median. Only the MFT-change
  times (not settable) separate the generations.
- **embedded** - `Virtual Machines/old-laptop.img` (32 MiB, 5 fragments, 4
  sparse holes) is a raw NTFS volume with 704 entries, more than its host's
  448. It was written in 64 KiB blocks, skipping zero blocks (holes), taking
  turns with a companion file (fragments). The inner volume's entries are in
  `other_entries` (file system `inner`, kind `embedded`, `embedded_in`); their
  `ondisk.record_offset` is absolute in the image (mapped through the
  container's runs), their runs count the inner volume's clusters.
  `file_systems` gives the container and its runs. Its files are browse-only
  (ARCHITECTURE.md section 1): the way to them is to recover the container.
- **repartition** - one 128 MiB volume at sector 2048 was replaced by two
  56 MiB partitions (the last 16 MiB stay unpartitioned; the old backup boot
  sector survives at the old volume's last sector); both were quick-formatted.
  `scan_range` is the first partition. 98 intact old files lie wholly inside
  it, 361 reach beyond it (346 of them intact, 15 damaged by the second
  partition's format): `after.data_inside_scan_range`. The smaller first
  partition put its root index elsewhere, so the old root's first INDX block
  survives too; but INDX blocks outside the scan range are invisible to a scan
  of it, so `after.ancestry` (scan range) and `after.ancestry_whole_image`
  differ. `new_volumes` lists both new file systems.
- **live-collision** - after the accident the new volume was mounted and 76
  new folders were created in exactly the record slots (64, 73, 80, 88,
  100-171) of destroyed old folders whose children survive; new and old
  records both start at sequence 1, so 103 surviving old entries carry a
  parent reference that now resolves to a live folder of the new volume
  (`after.parent_reference_taken_by` names it). The lost folder `Later
  Parent/Moved Early` is still named by its surviving parent's index root
  while a new folder occupies its slot. Files were written into some new
  folders, four large files written and five files written and deleted again
  (deleted records of the new MFT; 72 old files lost content only to them).
  The existing `post-write` scenario had four such collisions by chance; this
  one makes them the point.

`offset-mbr63` is not on the list the fixtures were specified with; it was
added because ALGORITHMS.md A3 justifies scanning every 512-byte offset
with exactly this layout.

## What is on the old volumes

`basic` and `wiped` carry the full tree described here, `cluster-change`,
`post-write` and `live-collision` a medium one, `repartition` a medium one with
ten 3-4 MiB files and 200 typed files written last, the others a small one: the
same folders with fewer names in each `names/` folder and fewer, smaller
files elsewhere. `fragmented` has its own tree (see the scenario list) plus
`typed/`, `names/` and `sizes/`. All of them start with the prelude.

| Folder | Contents |
|---|---|
| `media/` (`Photos`, `Music`, `Videos`, `Documents`) | the samples, `content_type` set |
| `typed/` | valid PNG, BMP, WAV, TIFF, PDF, ZIP and text files, 6 KB to 260 KB, written byte by byte by `fixture_content.py` (hashes identical on every machine) |
| `names/unicode/` | accented Latin, Polish, CJK, Greek, Cyrillic, Hebrew, Arabic, emoji (surrogate pairs), NFC and NFD forms of the same letter |
| `names/long/` | names of 234 and 255 characters, 255 CJK characters (757 bytes in UTF-8), 125 emoji (254 UTF-16 units), a 224-character folder |
| `names/case/` | `Readme.txt` / `README.TXT` / `readme.txt`, `Folder` / `FOLDER` / `folder` |
| `names/spaces-dots/` | leading and trailing spaces and dots, `...`, folders with a trailing dot / space |
| `names/win32-illegal/` | `:` `?` `*` `\|` `"` `<` `>` `\`, control characters, `CON`, `nul.txt`, `COM1`, `LPT1.txt`, `AUX.tar.gz` |
| `names/dos/` | WIN32 + DOS name pairs and one WIN32+DOS name (everything else is in the POSIX namespace, which is what ntfs-3g writes) |
| `sizes/` | empty; 10, 300, 600 bytes (resident); `limit/` around the resident limit; one and two clusters, each minus one, exact and plus one byte; several MiB |
| `deep/` | 6 to 14 nested levels, full path over 400 characters in the full tree |
| `many/` | 400 / 800 / 1200 / 3000 small files in one folder |
| `special/streams/` | files with named data streams (`Zone.Identifier`, a 20 KB one) |
| `special/times/` | timestamps before 1970, at the DOS epoch, after 2038, in 2100, on whole seconds, one tick apart |
| `special/compressed/` | (not with 64 KiB clusters, where NTFS has no compression) an LZNT1-compressed text, an incompressible file stored raw with the compressed flag, a resident file with the compressed flag |
| `trash/` | deleted entries, see below |
| prelude: `Photos/` ... `Documents/`, `Projects/`, `small-parent/`, `Moved Early` (later in `Later Parent/`), 48 more top-level folders (`Accounting 2019` ... `work`), `early-junk/` | the first user records, destroyed by the accident; see "Wipe policy" |
| g1: `G1 Archive 00` ... `23`, `G1 Mail/`, `G1 Late 0` ... `5` | generations only: the older file system |
| g2: `G2 Documents/`, `G2 Photos/` | generations only, besides the prelude |
| inner: `Guest Documents/`, `Guest Pictures/`, `Guest Projects/` | embedded only: the volume inside the container |
| host: `Host Documents/`, `typed/`, `Virtual Machines/` | embedded only, besides the prelude |

**Deleted entries.** Each tree deletes six entries "early" (two files and a
folder with three files) and eight "late" (three files and a folder with two
files and a sub-folder with one file). The early ones are followed by an
unmount, a mount and four new files, which take over the four lowest freed
record numbers; the late ones are the last thing that happens to the volume.
Result in every scenario: deleted files and folders whose record and content
are intact (flags 0, sequence number one higher than in the references to
them), deleted entries whose record now belongs to another file, and two
deleted files whose record is intact but whose parent folder's record was
reused by a file. (g1 and the inner volume have no deleted entries.)

**Samples.** Images come from Pillow (one JPEG has an Exif block with an
embedded JPEG thumbnail, so the file contains two SOI / EOI pairs; one is
progressive), audio and video from ffmpeg (`-bitexact`, one thread), PDF and
the ZIP-based documents from `fixture_content.py` (one PDF carries an
incremental update with a second `%%EOF`; DOCX, XLSX, PPTX, ODT and EPUB are
minimal packages that were checked as ZIP archives with well-formed XML parts,
not opened in an office suite), 7z and RAR from `7z` and `rar` when installed.
All are under 256 KiB. FLAC and Ogg are there on purpose although HDSavior
does not carve them.

## Manifest

One JSON object per scenario, plain ASCII (non-ASCII characters are
`\u`-escaped), one top-level member per line and one entry per line. The full
schema, member by member, is in the header of `make-fixtures.sh`; this is the
overview.

Conventions:

- Byte offsets are absolute offsets in the image file.
- LCNs and cluster counts are in units of the **old** cluster size and count
  from `old_volume.offset` (entries of another file system: from that file
  system's own start, see `file_systems`).
- Timestamps are FILETIME integers: 100 ns ticks since 1601-01-01 00:00:00
  UTC. `*_unix_ms` is the same instant in Unix milliseconds, rounded towards
  minus infinity (`(filetime - 116444736000000000) // 10000`).
- Two builds with the same tool versions produce identical manifests except
  for the volatile members: `generated_utc`, `image.sha256`, every `serial`,
  every `mft_changed`, `format_time`, `last_write` and `last_old_write` (with
  their `_unix_ms`), and in `embedded` the container file's `sha256` (its
  content is a volume image, which holds a serial number and MFT-change
  times). Record numbers, sequence numbers, runs, times and all before / after
  facts are reproducible. The images themselves are not bit-identical between
  builds.

**This revision** (still `manifest_version` 1: members were only added, none
changed its meaning, so existing readers keep working): `new_volumes`,
`scan_range`, `history`, `file_systems`, `other_entries`; in `accident`
`last_old_write`, `format_time` and in `wiped_records` `fill`,
`slots_written`, `byte_range`; in `new_volume` `format_time`; in `new_files`
`mft_sequence`, `record_offset`, `tags`, `file_system`; in entries
`mft_changed`, `file_system`, `embedded_in` and in `after`
`data_inside_scan_range`, `named_by`, `foreign_index_entries`, `ancestry`,
`ancestry_whole_image`, `parent_reference_taken_by`,
`parent_slot_contradiction`, `damage_evidence`, `clusters_reused_by_newer`;
new summary counters. `entries` still holds only the lost file system; other
file systems' entries are in `other_entries`. What did change are values: every
scenario now has the prelude and the rewrite of slots up to 255.

### Top level

| Member | Meaning |
|---|---|
| `manifest_version` | 1 |
| `scenario`, `description` | name and what happened |
| `seed` | seed of all generated content |
| `generated_utc` | build time (volatile) |
| `tools` | versions of mkntfs, ntfs-3g, ffmpeg, python, pillow |
| `image` | `file`, `size`, `sector_size`, `sha256` (volatile), `pre_image` (null unless `--keep-pre`), `partition_table` (null for bare volumes; else `type` `mbr` / `gpt` and `partitions` with `index`, `type`, `start_sector`, `sectors`, `offset`, `length`, for GPT also `guid`, `name`), read back from the image |
| `old_volume` | the lost volume: `offset` (byte offset of its boot sector in the image, 0 for bare volumes), `length` (bytes of the partition it filled; the last sector of that range held the backup boot sector, so `total_sectors` = `length / sector_size - 1`), `sector_size`, `cluster_size`, `mft_record_size`, `index_block_size`, `total_sectors`, `hidden_sectors`, `mft_lcn`, `mftmirr_lcn`, `label`, `serial` (volatile), `mft_runs` (`[[lcn, clusters], ...]` of the old `$MFT`), `mft_fragments`, `mft_records`, `mkntfs_args` |
| `new_volume` | the same members for the (first) file system the accident created, plus `format_time` (its creation time: whole seconds) |
| `new_volumes` | every file system the accident created (two in `repartition`) |
| `scan_range` | `offset`, `length`, `what`: the nominal session range (the old partition; the whole image for a bare volume; `repartition`: the first new partition) |
| `accident` | `steps` in order (`quick-format`, `wipe-mft-records`, `new-files-written`, `live-collision`, `repartition`); `wiped_records` (`first`, `last`, `fill` zeros / blank, `slots_written`, `byte_range`, `overwritten_by_format`, `overwritten_by_wipe` as old record numbers, `method`); `last_old_write` and `format_time` (always a later whole second) |
| `history` | earlier re-formats (`generations`): what was formatted over, its last write, the new format's time, its rewrite |
| `file_systems` | every old file system: `name`, `kind` (`main`, `older`, `embedded`), the `old_volume` members, `format_time`, `last_write`; an embedded one has `offset` null and `container`, `container_runs`, `container_cluster_size` |
| `destroyed_records` | old MFT record numbers whose bytes differ between the pre-accident image and the final image, whichever step changed them. A never-used record that was rewritten with identical bytes is not listed. |
| `summary` | counts: `files`, `folders`, `deleted_entries`, `bytes_in_files`, `old_mft_records`, `old_mft_fragments`, `destroyed_records`, `live_entries_with_destroyed_record`, `live_files_with_damaged_content`, `live_files_fully_intact`, `samples_with_destroyed_record_still_contiguous_and_intact`, `old_indx_blocks`, `old_indx_blocks_surviving`, `new_files`, `live_entries_with_reconstructible_path`, `live_entries_below_a_lost_folder`, `destroyed_folders_named_by_index_entries`, `entries_whose_parent_reference_is_taken`, `other_entries` |
| `verified` | what the build checked and found, in words |
| `notes` | free-form remarks |
| `samples_skipped` | samples this toolchain could not produce, with the reason |
| `new_files` | `post-write` and `live-collision`: what was written to the new volume (`path`, `type`, `size`, `sha256`, `mft_record` and `mft_sequence` in the new MFT, `record_offset`, `deleted` = removed again before the end, `created`, `modified`, `accessed`, `content_type`, `tags`, `file_system` "new") |
| `entries` | the lost file system, sorted by path |
| `other_entries` | the other old file systems (g1, the embedded volume), sorted by file system and path |

### Entry

| Member | Meaning |
|---|---|
| `path` | relative to its file system's root, `/` separators. Exactly as stored: names may contain backslashes, control characters, trailing dots. |
| `type` | `file` or `dir` |
| `size`, `sha256` | of the content as read through the mounted driver; null for folders |
| `mft_record` | record number (ntfs-3g's inode number, `stat -c %i`) |
| `mft_sequence` | sequence number that references to the entry carry (parent references of its children, index entries); null for a deleted entry whose record was reused. A deleted entry with an intact record has this value plus one in the record itself. |
| `created`, `modified`, `accessed` and `*_unix_ms` | trustworthy to the tick, see below |
| `mft_changed` | the MFT-change time in the record (the build's clock; volatile) |
| `file_system`, `embedded_in` | which file system (`file_systems`); the container's path for an embedded one |
| `deleted` | true = deleted before the accident; the other members are the last state seen through the driver |
| `content_type` | `jpeg`, `png`, `gif`, `bmp`, `tiff`, `webp`, `mp3`, `wav`, `m4a`, `flac`, `ogg`, `mp4`, `mov`, `3gp`, `avi`, `mkv`, `webm`, `pdf`, `docx`, `xlsx`, `pptx`, `odt`, `epub`, `zip`, `7z`, `rar`, `text`; null for arbitrary bytes |
| `tags` | promises checked on the raw volume: `sample`, `contiguous`, `resident`, `limit`, `sparse`, `compressed`, `initialized-short`, `stale-tail`, `fragmented`, `ads`, `dos-name`, `deleted-early`, `deleted-late`, `split` (two or more fragments), `container` |
| `name_issues` | only when the name cannot be re-created unchanged: `win32-illegal-char`, `control-char`, `trailing-dot-or-space`, `reserved-device-name`, `case-collision`, `utf8-over-255-bytes`. A recovered copy of such an entry, or of anything below such a folder, cannot sit at exactly this path (ALGORITHMS.md H4; the last one concerns Linux destinations only). |
| `dos_name`, `streams` | only when a DOS name / named data streams exist |
| `ondisk` | read from the raw volume before the accident (next table) |
| `after` | the final image compared with the pre-accident image (table after that) |

`ondisk`:

| Member | Meaning |
|---|---|
| `record_offset` | absolute byte offset of the base MFT record in the image |
| `names` | every `$FILE_NAME` attribute: `name`, `namespace`, and the `record` that holds it (can be an extension record). Sorted WIN32, POSIX, DOS: which of a WIN32 / DOS pair comes first on disk differs from build to build. |
| `extension_records` | record numbers |
| `attribute_list` | `none`, `resident` or `non-resident` |
| `resident` | files: the unnamed `$DATA` is resident |
| `data_flags` | files: `sparse`, `compressed`, `encrypted` |
| `fragments` | files: allocated extents (0 when resident) |
| `runs` | non-resident files: `[[lcn, clusters], ...]`, `lcn` null for a sparse hole |
| `allocated_size`, `initialized_size` | non-resident files |
| `compressed_size`, `compression_unit` | only when the attribute has the 72-byte header (sparse or compressed) |
| `stale_bytes_after_initialized_size`, `data_pieces`, `data_runs`, `data_extension_records`, `name_in_extension_record`, `mft_fragments_spanned` | on the tagged files they describe |
| `index`, `index_blocks` | folders: `resident` / `non-resident`, number of INDX blocks in use |
| `record_state` | deleted entries: `intact` (the record still describes the entry), `reused`, `extension-records-lost` |
| `content_intact_before_accident` | deleted files with an intact record |

`after`:

| Member | Meaning |
|---|---|
| `record_intact` | base record and extension records are byte-identical (false for a deleted entry whose record had been reused before) |
| `content_intact` | files: reading the file from where the old MFT put it (old runs, old cluster size, initialized-size rule; resident data from its record) out of the final image still gives `sha256`; null for folders |
| `clusters_changed` | non-resident files: number of the file's allocated clusters that are not byte-identical (can be non-zero with `content_intact` true when only bytes behind the end of the data changed) |
| `overlaps_new_allocation` | files: a new file system's `$Bitmap` marks at least one of the file's clusters (resident data: its record's cluster) as allocated |
| `index_entry_intact` | an old INDX block of this file system that names the entry with this record number, sequence number and name is byte-identical. Entries of a folder with a resident index never were in an INDX block. |
| `data_inside_scan_range` | files with a surviving record: all their data lies in `scan_range` |
| `named_by` | folders whose record is destroyed: every surviving index entry with their (record, sequence): `name`, `namespace`, `source` (`indx` or `index_root` of a surviving record), `offset` (absolute), `parent_record`, `parent_sequence`, `usable` (ALGORITHMS C4 / E6: older than the accident's format, not in a cluster a new file system allocated), `inside_scan_range` |
| `foreign_index_entries` | surviving index entries of another file system of the scenario with the same (record, sequence): names that must not be given to this folder |
| `ancestry` | `path_reconstructible` (every ancestor, and the entry itself, is known to a scan of `scan_range`: its record survives there, or a usable index entry there names it), `lost_ancestor` (the deepest unknown one), `surviving_suffix` (the path below it: where a correct recovery puts the entry when the path cannot be rebuilt) |
| `ancestry_whole_image` | the same for a scan of the whole image, where it differs |
| `parent_reference_taken_by` | the parent's record is destroyed and its slot now holds a directory record the entry's parent reference resolves to: `file_system` (`new`, or the newer generation), `path`, `mft_record`, `mft_sequence`, `in_use`, `is_directory`, `record_offset`. The folder the entry must not be put into. |
| `parent_slot_contradiction` | the same members when the slot holds an in-use record the reference does not resolve to (the D2 (B) contradiction) |
| `damage_evidence` | files with a surviving record and damaged content: what on the disk shows it: `current-allocation`, `post-format-record`, `current-boot-sector`, `newer-file`, or `none` |
| `clusters_reused_by_newer` | entries of an `older` file system: the newer generation had allocated at least one of the file's clusters |

A file can be recovered bit-identical through its MFT record exactly when
`after.record_intact` and `after.content_intact` are both true; where it
belongs in the output is `after.ancestry`. `after.content_intact` without
`after.record_intact`, with `ondisk.fragments == 1`, is the carving case.

### Timestamps

`created`, `modified` and `accessed` are trustworthy to the 100 ns tick. The
build sets all three to fixed values through ntfs-3g, reads them back through
the driver and compares them with `$STANDARD_INFORMATION` in the raw record.
They are read from the `system.ntfs_times` extended attribute, which returns
the raw FILETIME values; `stat()` agrees from 1970 on but reports 0 for earlier
times (the kernel clamps the timestamps of a FUSE file system).

Not recorded: the MFT-change time (the wall clock of the build), and the
copies of the times inside `$FILE_NAME` attributes and index entries. On these
volumes those copies equal `$STANDARD_INFORMATION`, because setting times
through ntfs-3g updates them too; that is not the case on volumes written by
Windows.

## inspect-ntfs.py

```sh
scripts/linux/inspect-ntfs.py IMAGE                  # everything, as text
scripts/linux/inspect-ntfs.py --json IMAGE           # one JSON document
scripts/linux/inspect-ntfs.py --summary IMAGE        # counts
scripts/linux/inspect-ntfs.py --only file --record 5 --record 64 IMAGE
scripts/linux/inspect-ntfs.py --only indx --no-entries IMAGE
scripts/linux/inspect-ntfs.py --start 1048576 --length 67108864 -v IMAGE
```

It tests every 512-byte offset of the image for `FILE`, `INDX` and an NTFS
boot sector and decodes each hit from its own bytes only: no partition table,
no boot sector, no `$MFT` runlist is consulted. For a FILE record it reports
the offset, record number, sequence number, flags, base reference, link count,
fixup status, every attribute with its header, all `$FILE_NAME` attributes
with parent references, all `$DATA` attributes with decoded runs, attribute
lists, index roots with entries; for an INDX block the VCN, node header and
every entry (file reference, name, parent, sizes, times, sub-node VCN); for a
boot sector every field HDSavior needs. A hit whose header makes no sense is
still listed, with `"valid": false` and the reason. The JSON schema is in the
comment at the top of the file.

How far it can be trusted: `make-fixtures.sh` compares it with `ntfsinfo -v` on
about 1900 records in total on every build (114,000 header and attribute
fields, 2,300 data runs, 1,900 `$FILE_NAME` attributes, 3,900 index entries in
index roots and 238 INDX blocks), verifies sequence numbers, references and
timestamps against other data, and above all reads every live file of every
file system of every fixture (28,000 files) through the runlists it decoded and gets the SHA-256
the mounted driver gave: once from the intact volume, finding the records
through the `$MFT`, and once more from the final image, finding them by its
own scan. Each field in the source is marked `[v]` (compared with ntfsinfo),
`[x]` (cross-checked otherwise) or `[l]` (decoded as `layout.h` says, nothing
to compare with). A mutation test over 100,000 damaged records and blocks
raised no exception. It has only ever seen structures written by mkntfs and
ntfs-3g.

## Checking a recovery

```sh
scripts/linux/check-recovery.py --manifest fixtures-out/basic.manifest.json --output /tmp/rescue
scripts/linux/check-recovery.py --manifest M --output DIR --report DIR/HDSavior-report-20261124-101500.csv \
    --main-root "" --fs-root "g1=[Older file system]" --include-deleted --scan-range whole \
    --allow "names/long/*" --exceptions known-gaps.txt
```

It reads the manifest, the output folder and the report CSV (ALGORITHMS.md
H10: UTF-8 with BOM, CRLF, RFC 4180, apostrophe-protected cells; statuses of
H11), ties report rows to manifest entries by `record_offset` (carved files by
`first_data_offset`), and checks ARCHITECTURE.md 7.3:

- **(a)** every file with `record_intact` and `content_intact` (not deleted
  unless `--include-deleted`; not compressed, encrypted or embedded, which are
  never written; all data inside the scan range unless `--scan-range whole`)
  is in the output with an identical SHA-256: at its original path when
  `after.ancestry.path_reconstructible`, otherwise anywhere whose path ends in
  `after.ancestry.surviving_suffix`. Files of an older file system may be in
  the main tree or below their own root (`--fs-root`, default
  `[Older file system]`). A file found where expected but missing from the
  report is a violation too.
- **(b)** nothing reported `ok` or `ok_unverified` differs from the manifest's
  hash (rows tied to no entry must at least equal some manifest file).
- **(c)** no file with `content_intact` false is reported `ok` or
  `ok_unverified`. Tolerated (listed separately): `ok_unverified` for a file
  whose `damage_evidence` is `none` (ARCHITECTURE.md section 8).
- **(d)** names with `name_issues` are compared after the sanitizing rules of
  H4 (characters, lone surrogates, trailing dots and spaces, reserved names,
  `(n)` suffixes for case collisions, shortening to `--component-limit` UTF-8
  bytes, the `~<record>` retry).

It prints counts per category and every violation, and exits 1 on a
violation. `--allow GLOB` / `--exceptions FILE` turn violations for matching
manifest paths into listed tolerations.

**Proof.** `reference-recovery.py IMAGE --dest DIR` is an independent recovery
(reads only the image; ALGORITHMS C1-C5, D1/D2/D4a, E1-E9, F1, G, H4-H11 as
far as these fixtures need) and `test-check-recovery.py --fixtures DIR` runs
it on every scenario, requires the checker to accept the result, then plants
faults in copies and requires each to be caught: a flipped byte in a file
reported `ok`, a deleted file, a file moved to a wrong folder, a file below a
lost folder renamed, a sanitized name written differently, a damaged file
reported `ok_unverified`, an invented `ok` row. On the current build: all 16
correct recoveries accepted, all 24 planted faults caught (about 40 s).

## Decisions

- **The partition images are assembled by copying.** The volume is built,
  populated and re-formatted as a file of its own (`mkntfs -p <start sector>`
  puts the partition's start into the boot sector's hidden-sectors field) and
  then copied into its partition; `sfdisk` writes the MBR or GPT with fixed
  identifiers. This needs no loop device, which is what containers usually
  lack. It is byte-identical to formatting in place: `mkntfs -T` (which fakes
  the time and thereby also fixes the serial number) run on
  `losetup --offset 1048576 --sizelimit 67108864` of a disk image and on a
  separate 64 MiB file gave the same SHA-256. The finished image is checked to
  contain the volume byte for byte, and its partition table is read back by
  the helper's own MBR / GPT reader (CRCs included); `sfdisk -l` and
  `sgdisk -v` accept it. `repartition` cuts its two new partitions out of the
  old volume file with `dd`, formats the pieces and puts them back the same way.
- **The Windows-style rewrite is a model, not a Windows format** (see "Wipe
  policy"). The new file system's own `$MFT` still says 27 records, so the
  rewritten slots lie in clusters it regards as free; the model is only applied
  where those bytes held old MFT records, never old file data.
- **Every re-format starts in a later whole second than the last write** to the
  file system it goes over, measured as the latest MFT-change time in that
  file system's records; the build checks the new volume's creation time
  against it.
- **An embedded volume is a real file.** The inner volume is built, populated
  and verified like any other, then copied into the host through the driver;
  its truth (record and content survival, names, ancestry) is computed on the
  bytes read back through the container's runlist from the final image.
- **Compressed files are in the fixtures** although HDSavior does not recover
  them, because they are the easiest way to violate "nothing reported OK may
  differ from the manifest".
- **No hard links, reparse points or encrypted files.**

## What these volumes look like (mkntfs / ntfs-3g 2022.10.3)

Things worth knowing before pointing a parser at the images. All of it is
visible with `inspect-ntfs.py`.

- `mkntfs` puts the `$MFT` at byte 16384 for clusters up to 8 KiB and at LCN 2
  above; `$MFTMirr` and `$LogFile` sit in the middle of the volume. Its initial
  `$MFT` has 27 records. A quick format of a 64 MiB / 4 KiB volume writes
  bytes 0-12287, 16384-45055 (the MFT), 102 clusters right behind the MFT zone
  (LCN 2051) and 2 MiB plus one cluster in the middle, and the last sector.
- Slots 16-23 of a fresh `mkntfs` MFT (and 27-63 with 64 KiB clusters) hold
  unused FILE records whose own record-number field is 0. Records 12-15 are in
  use and have no name. `$Quota`, `$ObjId`, `$Reparse` are records 24-26.
- `$MFTMirr` holds one cluster's worth of records when a cluster is larger
  than four records: 64 FILE records with 64 KiB clusters.
- ntfs-3g gives the first user file record 64; records 27-63 are formatted
  but unused (sequence 1, flags 0, no attributes). Within one mount it looks
  for a free record upwards from the last one it handed out and only wraps
  around to the lowest free one at the end of the allocated MFT; after a
  remount it starts with the lowest free one.
- User files written by ntfs-3g have a 48-byte `$STANDARD_INFORMATION`, their
  own `$SECURITY_DESCRIPTOR` attribute in the record, names in the POSIX
  namespace, and LSN 0. With a 14-character name the largest resident file is
  640 bytes (3696 with 4096-byte records).
- The size fields of a `$FILE_NAME` attribute inside the record are stale
  (data size 0 for almost every file, allocated size from some earlier
  moment). The copies in index entries are current.
- A file with a WIN32 and a DOS name has two `$FILE_NAME` attributes; which
  comes first differs from build to build for the same file.
- In a folder whose index has more than one level, a few entries (the
  separators) live in the index root inside the folder's MFT record and in no
  INDX block: the old root directory of `wiped` and `cluster-change` keeps
  `Games` and `Presentations` there.
- When a record overflows, ntfs-3g makes the `$ATTRIBUTE_LIST` non-resident at
  once (seen with lists of 192 to 800 bytes) and moves `$FILE_NAME` to an
  extension record. Pieces of `$DATA` other than the first carry sizes of 0.
- A sparse file has attribute flag 0x8000, compression unit 4 and the 72-byte
  attribute header, without being compressed. `$BadClus:$Bad` is one hole run
  without the sparse flag and with initialized size 0.
- `ftruncate` to a larger size leaves the initialized size where it was.
- In a compressed folder even resident `$DATA` carries flag 0x0001.
- Deleting clears the in-use flag, raises the sequence number by one, sets
  the link count to 0 and leaves the attributes; a deleted folder's index root
  is empty by then.
- The root directory is record 5 with the name `.`, parent 5, and an index
  entry for itself.
- Data is not allocated front to back: consecutive new files usually land in
  different regions of the volume (three of them on a 64 MiB volume).

- A re-format with the same geometry puts the new root index's first INDX
  block exactly where the old one was; later root blocks (VCN 1, 2) lie in
  ordinary data clusters and survive. With 64 KiB clusters all three 4 KiB
  root blocks share one cluster, and the format rewrites all of it. A smaller
  volume (`repartition`) or another cluster size (`cluster-change`) puts the
  new root index elsewhere, and the old first block survives too.
- The old INDX blocks of the first folders lie among the first data the old
  volume allocated, which is where a fresh volume puts its first new data:
  a few MiB written after the accident (`post-write`, `live-collision`) take
  every name of the prelude's lost folders that INDX blocks carried.
- On a fresh volume ntfs-3g gives out records 64, 65, ... in order within one
  mount, starting with sequence number 1, the same sequence the first use of
  an old record had: a new folder in the slot of a destroyed old one takes
  over exactly the reference (N, 1) that the old folder's children carry.
- ntfs-3g extends a fresh `$MFT` by formatting slots 27-63 as never-used
  records, byte-identical to the ones the old volume had there.

## Limits

- Everything is written by one driver family. Structures only Windows produces
  are absent: Win32-namespace names by default, 72-byte
  `$STANDARD_INFORMATION` on user files, security through `$Secure` (and with
  it INDX blocks whose keys are not file names), a used `$LogFile`,
  `$UsnJrnl`, resident attribute lists, an `$MFT` that does not start at
  byte 16384, a 256-record `$MFT` that owns its slots in the `$Bitmap`.
- The rewrite of slots up to 255 only models Windows (see "Wipe policy"); the
  rest of what Windows writes at format and first mount (TxF logs, `$Repair`,
  System Volume Information, 22-25 MiB) is not imitated.
- No bad sectors (the images read perfectly) and no torn records.
- The embedded volume is a raw image; a VHD / VHDX container (header in
  front, blocks in another order) is in the instance lab, not here.
- Samples of types that need tools not in Ubuntu main / universe (HEIC, camera
  RAW) are missing; RAR needs the non-free `rar`.
- `reference-recovery.py` implements only as much of ALGORITHMS.md as these
  fixtures need (no geometry validation, no carving, no strays, no orphan
  groups, the volume offset is taken from the current volume); it is a test
  tool, not a second engine.
