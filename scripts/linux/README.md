# Linux fixtures: real NTFS volumes, really quick-formatted

This directory builds the disk images HDSavior's engine is tested against on
Linux (ARCHITECTURE.md section 8, item 3) and contains an independent tool to
look into them.

| File | What it is |
|---|---|
| `make-fixtures.sh` | Entry point. Runs `mkntfs`, `ntfs-3g`, `umount`, `sfdisk` and drives the helper. |
| `fixture_helper.py` | Worker: populates volumes, captures ground truth, re-reads volumes raw, simulates the Windows-style wipe, compares before / after, writes manifests. |
| `fixture_content.py` | Deterministic file contents and the media / document samples. |
| `inspect-ntfs.py` | Stand-alone, dependency-free dumper of FILE records, INDX blocks and NTFS boot sectors found anywhere in an image. Shares no code with HDSavior. |

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
sudo scripts/linux/make-fixtures.sh --only wiped,post-write
scripts/linux/make-fixtures.sh --list
```

The whole build takes about 40 seconds on a 4-core machine and leaves about
280 MB on disk (1.0 GiB apparent size; the images are sparse). It needs root
because it mounts. Per scenario it writes `<name>.img` and
`<name>.manifest.json`; with `--keep-pre` also `<name>.pre.img`, the same image
right before the accident. When run through `sudo`, the results are handed to
the invoking user. A failed run stops at the first broken promise, unmounts
whatever is mounted (also on Ctrl-C / SIGTERM) and leaves its work files and
the tools' output in `<out>/.work/` for inspection; a successful run removes
that directory.

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
   freed MFT records, see "deleted entries" below). All content comes from a
   fixed seed. Creation, modification and access time of every entry are set to
   fixed values through the driver (`system.ntfs_times`). Entries that are to be
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
   `sparse`, `fragmented`, `contiguous`; "the MFT is fragmented"; ...) are
   checked here and the build fails loudly if one does not hold.
5. **Cross-check the inspector** against `ntfsinfo -v` for about a hundred
   records per scenario: every header field, every attribute header, all
   runlists, names, attribute lists, index roots and (for small folders) INDX
   blocks; the boot sector against `ntfsinfo -m` and `blkid`.
6. **Accident.** `mkntfs -Q -F` again on the same bytes, then whatever else the
   scenario calls for.
7. **Compare** the pre-accident copy with the result: which old MFT record
   slots changed, which files still read back correctly from their old place,
   which old INDX blocks are still there, which clusters the new file system
   has allocated. Scenario promises about the outcome are checked.
8. **Blind scan.** The final image is scanned at every 512-byte offset for
   FILE records, without boot sector or `$MFT` runlist, the way HDSavior has to
   work (only the old cluster size and volume offset are taken as known). For
   every live file a matching in-use record is looked for, extension records
   are attached through their base references, and the content is read through
   the runs found. A file must come back bit-identical exactly when the
   manifest says `after.record_intact` and `after.content_intact`.
9. **Manifest**, and the image's SHA-256.

Volumes are mounted straight from image files, no loop device. `ntfs-3g` is run
with `no_detach` as a job of the script and `umount` is followed by `wait`: for
an image file, `umount` returns while the driver is still writing.

## Scenarios

Numbers are from the toolchain named above; they are the same in every build
on it.

| Scenario | Image | Old volume | Accident | Files / folders / deleted | Old MFT records destroyed |
|---|---|---|---|---|---|
| `basic` | 128 MiB volume | 4 KiB clusters | quick format, same parameters | 3288 / 46 / 14 | 0-27 |
| `fragmented` | 64 MiB volume | 4 KiB clusters, `$MFT` in 27 fragments | quick format, same parameters | 2968 / 17 / 1202 | 0-27 |
| `wiped` | 128 MiB volume | 4 KiB clusters | quick format, same parameters, then record slots 0-255 overwritten | 3509 / 123 / 14 | 0-27, 64-255 |
| `cluster-change` | 128 MiB volume | 4 KiB clusters | quick format with 64 KiB clusters | 1596 / 109 / 14 | 0-175 |
| `offset-mbr` | 72 MiB disk, MBR, partition at sector 2048 | 4 KiB clusters, 64 MiB | quick format, same parameters | 517 / 30 / 14 | 0-27 |
| `offset-mbr63` | 70 MiB disk, MBR, partition at sector 63 | 4 KiB clusters, 128457 sectors | quick format, same parameters | 517 / 30 / 14 | 0-27 |
| `offset-gpt` | 87 MiB disk, GPT, partition at sector 32768 | 4 KiB clusters, 64 MiB | quick format, same parameters | 517 / 30 / 14 | 0-27 |
| `post-write` | 128 MiB volume | 4 KiB clusters | quick format, same parameters, then 146 new entries (27 MB) written, 6 deleted again | 1596 / 109 / 14 | 0-26, 64-211 |
| `small-clusters` | 64 MiB volume | 512-byte clusters | quick format, same parameters | 517 / 30 / 14 | 0-26 |
| `big-clusters` | 64 MiB volume | 64 KiB clusters | quick format, same parameters | 514 / 29 / 14 | 0-26 |
| `4k-records` | 64 MiB volume | 4096-byte sectors and MFT records, 4 KiB clusters | quick format, same parameters | 517 / 30 / 14 | 0-26 |

What each one is for:

- **basic** - the general round trip. A same-geometry `mkntfs` quick format
  overwrites only old MFT records 0-26 (system files) and the unused slot 27;
  ntfs-3g gives the first user file record 64. So every file and folder must
  come back bit-identical at its path.
- **fragmented** - `frag/big-fragmented.bin` was written into the holes of a
  completely full volume: 882 data runs, a non-resident `$ATTRIBUTE_LIST`, five
  extension records, four of them with `$DATA` pieces and one holding the
  file's only `$FILE_NAME`; its six records lie in two different fragments of
  the `$MFT`. Three sparse files with real hole runs. Two files whose
  initialized size is smaller than their size; one of them
  (`valid-data/stale-bytes-after-initialized-size.bin`) has 3181 non-zero stale
  bytes on disk behind the initialized size, so a reader that ignores the
  initialized size produces the wrong hash. The old `$MFT` is in 27 fragments
  (the volume was full when it had to grow), so most of it can only be found
  by scanning. `ballast.bin`, 45 MB in 12 fragments, was deleted last and is
  fully intact as a deleted file; 1201 other deleted files had their records
  reused.
- **wiped** - what `basic` would look like after a Windows format, which ends
  up with a 256-record `$MFT` where `mkntfs` writes 27 records. The first user
  files are the 31 samples (JPEG, PNG, GIF, BMP, TIFF, WebP, MP3, WAV, M4A,
  FLAC, Ogg, MP4, MOV, 3GP, AVI, MKV, WebM, PDF, DOCX, XLSX, PPTX, ODT, EPUB,
  ZIP, 7z, RAR), each in one fragment; then come 76 folders whose children are
  created only after record 255. After the accident 192 live entries have lost
  their record. All 31 samples keep intact, contiguous content (carving). Of
  the destroyed folders with surviving children, 54 are still named by a
  surviving INDX block of their parent and 21 are not named anywhere. The old
  root directory had three index blocks: of its 54 destroyed folders, 38 are
  named by the two blocks that survived (`Holiday Plans` ...) and 16 are not
  (`Accounting 2019` ...), because their entries were in the first block, which
  the new root index overwrote, or in the index root inside old record 5.
  Below the root, `Projects/p01-alpha` keeps its name through `Projects`' INDX
  block, while `Projects/p01-alpha/src` and `small-parent/inner` were listed
  only in their parents' resident index roots and are nameless.
- **cluster-change** - the new boot sector says 64 KiB clusters, the old
  runlists count 4 KiB clusters. The new `$MFT` starts at byte 131072 instead
  of 16384; with the new boot cluster and MFT bitmap in front of it the format
  overwrites old records 0-175, among them 112 live entries (76 folders, the
  31 samples and five other files). 94 of 95 old INDX blocks survive, among
  them all three of the old root directory (the new root index lies
  elsewhere), so 53 of the 55 top-level folders that lost their record keep
  their names; `Games` and `Presentations` do not, their entries were the
  separators in the index root inside old record 5. The one INDX block that is
  gone belonged to `Photos`. No live file loses content here: what the new
  metadata lands on is old metadata and that block.
- **offset-mbr**, **offset-mbr63**, **offset-gpt** - the volume sits inside a
  whole-disk image. `offset-mbr63` is the Windows XP layout: partition at
  sector 63 with an odd number of sectors, so every FILE record of the image
  lies at an odd multiple of 512 bytes. `offset-gpt` has a Microsoft reserved
  partition at sector 34 and the NTFS partition at 16 MiB, as Windows lays out
  a GPT data disk. All three have one spare "cylinder" of unallocated space
  behind the partition.
- **post-write** - the new volume was used. 378 old files have lost content,
  148 live entries their record, and only 22 of 95 old INDX blocks are left.
  Of the old files whose MFT record survived, 174 are overwritten by data the
  new file system still has allocated, and 132 by new files that were deleted
  again: their clusters are free in the new `$Bitmap`, yet the old content is
  gone (the deleted records in the new MFT still hold the runlists). Nothing
  may be reported as recovered-OK unless it matches the old hash. 1218 old
  files are fully intact.
- **small-clusters**, **big-clusters** - an MFT record spans two clusters /
  sixteen index blocks share a cluster. Index block VCNs count clusters in the
  first case and 512-byte units in the second; in both cases they run 0, 8,
  16, ...
- **4k-records** - `mkntfs` has no option for the MFT record size; it uses
  max(1024, sector size). A volume formatted with `-s 4096` therefore has
  4096-byte records, like a 4Kn disk. Records still have one fixup per 512
  bytes (update sequence count 9), the resident limit moves from 640 to 3696
  bytes, and only one folder is big enough to need INDX blocks.

`offset-mbr63` is not on the list the fixtures were specified with; it was
added because ARCHITECTURE.md 6.1 justifies scanning every 512-byte offset
with exactly this layout.

## What is on the old volumes

`basic` and `wiped` carry the full tree described here, `cluster-change` and
`post-write` a medium one, the others a small one: the same folders with fewer
names in each `names/` folder and fewer, smaller files elsewhere. `fragmented`
has its own tree (see the scenario list) plus `typed/`, `names/` and `sizes/`.

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
| `many/` | 400 / 1200 / 3000 small files in one folder (21 / 63 / 159 INDX blocks) |
| `special/streams/` | files with named data streams (`Zone.Identifier`, a 20 KB one) |
| `special/times/` | timestamps before 1970, at the DOS epoch, after 2038, in 2100, on whole seconds, one tick apart |
| `special/compressed/` | (not with 64 KiB clusters, where NTFS has no compression) an LZNT1-compressed text, an incompressible file stored raw with the compressed flag, a resident file with the compressed flag |
| `trash/` | deleted entries, see below |
| `Photos/`, `Music/`, `Videos/`, `Documents/`, `Projects/`, `small-parent/`, 48 more top-level folders (`Accounting 2019` ... `work`), `early-junk/` | only where records get destroyed (`wiped`, `cluster-change`, `post-write`): the first user records. The folders are created first and get their children after record 255; the 48 extra ones make the root directory's index three blocks long. |

**Deleted entries.** Each tree deletes six entries "early" (two files and a
folder with three files) and eight "late" (three files and a folder with two
files and a sub-folder with one file). The early ones are followed by an
unmount, a mount and four new files, which take over the four lowest freed
record numbers; the late ones are the last thing that happens to the volume.
Result in every scenario: deleted files and folders whose record and content
are intact (flags 0, sequence number one higher than in the references to
them), deleted entries whose record now belongs to another file, and two
deleted files whose record is intact but whose parent folder's record was
reused by a file. In `small-clusters` one of those two
(`trash/early-folder/e-c.bin`) additionally has its clusters reused by a live
file (`trash/reuser-0.bin`).

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
`\u`-escaped), one top-level member per line and one entry per line.

Conventions:

- Byte offsets are absolute offsets in the image file.
- LCNs and cluster counts are in units of the **old** cluster size and count
  from `old_volume.offset`.
- Timestamps are FILETIME integers: 100 ns ticks since 1601-01-01 00:00:00
  UTC. `*_unix_ms` is the same instant in Unix milliseconds, rounded towards
  minus infinity (`(filetime - 116444736000000000) // 10000`).
- Two builds with the same tool versions produce identical manifests except
  for `generated_utc`, `image.sha256` and the two `serial` members. Record
  numbers, sequence numbers, runs, times and all before / after facts are
  reproducible. The images themselves are not bit-identical between builds
  (volume serial numbers and the driver's MFT-change times differ).

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
| `new_volume` | the same members for the file system the accident created: the parameters of the new format |
| `accident` | `steps` in order (`quick-format`, `wipe-mft-records`, `new-files-written`); `wiped_records` (null, or `first`, `last`, `overwritten_by_format`, `overwritten_by_wipe`, `method`) |
| `destroyed_records` | old MFT record numbers whose bytes differ between the pre-accident image and the final image, whichever step changed them. A never-used record that was rewritten with identical bytes is not listed. |
| `summary` | counts: `files`, `folders`, `deleted_entries`, `bytes_in_files`, `old_mft_records`, `old_mft_fragments`, `destroyed_records`, `live_entries_with_destroyed_record`, `live_files_with_damaged_content`, `live_files_fully_intact`, `samples_with_destroyed_record_still_contiguous_and_intact`, `old_indx_blocks`, `old_indx_blocks_surviving`, `new_files` |
| `verified` | what the build checked and found, in words |
| `notes` | free-form remarks |
| `samples_skipped` | samples this toolchain could not produce, with the reason |
| `new_files` | `post-write` only: what was written to the new volume (`path`, `type`, `size`, `sha256`, `mft_record` in the new MFT, `deleted` = removed again before the end, `created`, `modified`, `accessed`, `content_type`) |
| `entries` | the old volume, sorted by path |

### Entry

| Member | Meaning |
|---|---|
| `path` | relative to the volume root, `/` separators. Exactly as stored: names may contain backslashes, control characters, trailing dots. |
| `type` | `file` or `dir` |
| `size`, `sha256` | of the content as read through the mounted driver; null for folders |
| `mft_record` | record number (ntfs-3g's inode number, `stat -c %i`) |
| `mft_sequence` | sequence number that references to the entry carry (parent references of its children, index entries); null for a deleted entry whose record was reused. A deleted entry with an intact record has this value plus one in the record itself. |
| `created`, `modified`, `accessed` and `*_unix_ms` | trustworthy to the tick, see below |
| `deleted` | true = deleted before the accident; the other members are the last state seen through the driver |
| `content_type` | `jpeg`, `png`, `gif`, `bmp`, `tiff`, `webp`, `mp3`, `wav`, `m4a`, `flac`, `ogg`, `mp4`, `mov`, `3gp`, `avi`, `mkv`, `webm`, `pdf`, `docx`, `xlsx`, `pptx`, `odt`, `epub`, `zip`, `7z`, `rar`, `text`; null for arbitrary bytes |
| `tags` | promises checked on the raw volume: `sample`, `contiguous`, `resident`, `limit`, `sparse`, `compressed`, `initialized-short`, `stale-tail`, `fragmented`, `ads`, `dos-name`, `deleted-early`, `deleted-late` |
| `name_issues` | only when the name cannot be re-created unchanged: `win32-illegal-char`, `control-char`, `trailing-dot-or-space`, `reserved-device-name`, `case-collision`, `utf8-over-255-bytes`. A recovered copy of such an entry, or of anything below such a folder, cannot sit at exactly this path (ARCHITECTURE.md 6.8 step 3; the last one concerns Linux destinations only). |
| `dos_name`, `streams` | only when a DOS name / named data streams exist |
| `ondisk` | read from the raw volume before the accident (next table) |
| `after` | the final image compared with the pre-accident image (table after that) |

`ondisk`:

| Member | Meaning |
|---|---|
| `record_offset` | byte offset of the base MFT record in the image |
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
| `overlaps_new_allocation` | files: the new file system's `$Bitmap` marks at least one of the file's clusters (resident data: its record's cluster) as allocated |
| `index_entry_intact` | an old INDX block that names the entry with this record number, sequence number and name is byte-identical. Entries of a folder with a resident index never were in an INDX block. |

A file can be recovered bit-identical through its MFT record exactly when
`after.record_intact` and `after.content_intact` are both true; its path can be
rebuilt as far as its ancestors' records or index entries survive.
`after.content_intact` without `after.record_intact`, with
`ondisk.fragments == 1`, is the carving case.

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
about 1300 records in total on every build (78,000 header and attribute
fields, 1,700 data runs, 1,300 `$FILE_NAME` attributes, 1,800 index entries in
index roots and 138 INDX blocks), verifies sequence numbers, references and
timestamps against other data, and above all reads every live file of every
fixture (16,000 files) through the runlists it decoded and gets the SHA-256
the mounted driver gave: once from the intact volume, finding the records
through the `$MFT`, and once more from the final image, finding them by its
own scan. Each field in the source is marked `[v]` (compared with ntfsinfo),
`[x]` (cross-checked otherwise) or `[l]` (decoded as `layout.h` says, nothing
to compare with). A mutation test over 100,000 damaged records and blocks
raised no exception. It has only ever seen structures written by mkntfs and
ntfs-3g.

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
  `sgdisk -v` accept it.
- **The Windows-style wipe is a model, not a Windows format.** After the
  `mkntfs` quick format, each old record slot from 0 to 255 that `mkntfs` had
  not already overwritten (28-255) gets an empty, unused FILE record with its
  own number, byte for byte what ntfs-3g writes when it formats a fresh record
  (sequence 1, flags 0, no attributes, LSN 0). The location is computed from
  the old `$MFT`'s first run, which is checked to cover those records and to
  coincide with the new `$MFT`. The new file system's own `$MFT` still says 27
  records, so the 228 written records lie in clusters it regards as free.
  Blank FILE records are what the reference notes in `docs/notes/` report for
  the unused slots of a Windows 7-era format; the Windows 10 samples there
  have zero bytes instead, which is the easier case for a scanner. Nothing
  about Windows was measured here; the Windows CI job (ARCHITECTURE.md
  section 8, item 4) is the place for that.
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

## Limits

- Everything is written by one driver family. Structures only Windows produces
  are absent: Win32-namespace names by default, 72-byte
  `$STANDARD_INFORMATION` on user files, security through `$Secure` (and with
  it INDX blocks whose keys are not file names), a used `$LogFile`,
  `$UsnJrnl`, resident attribute lists, an `$MFT` that does not start at
  byte 16384, a 256-record initial MFT.
- No re-partitioning scenario: old and new volume always cover the same bytes,
  so no old boot sector survives anywhere.
- No bad sectors (the images read perfectly) and no torn records.
- Samples of types that need tools not in Ubuntu main / universe (HEIC, camera
  RAW) are missing; RAR needs the non-free `rar`.
