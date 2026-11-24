# Instance-separation lab

Real-volume scenarios, ground truth and an independent reference model for the rules of
`docs/ALGORITHMS.md` that decide which scanned records belong together:

- **C** current volumes (C1-C5), **D** instances (D1-D9), **E5/E6** parents and lost-folder names,
- **F** geometry per instance (F1-F6, including the lost-volume range of F5) and **G1/G2** flags.

Every scenario is a disk image built with the real tools (mkntfs, the ntfs-3g driver, sfdisk,
qemu-img), with a ground-truth file that says what a correct catalog must show.  The Rust engine is
meant to be checked against the same images and truth files (see *Using the truth files*).

```
run-all.py                  build missing scenarios, run the model on every scan, print the table
run-all.py --rebuild        rebuild all scenarios first (about one minute with 3 jobs)
run-all.py --fixes all      the same with the lab's proposed rule changes applied
run-all.py --fixes d4c-self,d6-copy-ctime   only some of them
run-all.py --set embed_tol=0.5              override a threshold or a reading (see hdsi/model.py Params)
run-all.py --sweep --fixes all              threshold sensitivity table
run-all.py --details        print examples for every failed check
run-all.py --only a,b       restrict to some scenarios;  --json out.json  machine-readable results
```

Needs root, `mkntfs`/`ntfs-3g` (2022.10.3 was used), `losetup`, `sfdisk`, `qemu-img`, libfaketime
(`/usr/lib/x86_64-linux-gnu/faketime/libfaketime.so.1`) and Python 3.8+ (standard library only).
Images go to `$HDSI_LAB/scenarios/<name>/` (default: the session scratchpad; set `HDSI_LAB` to move
it).  Nothing large is ever written into the repository.  A full rebuild needs about 5 GB of
(mostly sparse) image files; one evaluation of all scans takes about 20 s.

## Layout

| path | what |
|---|---|
| `run-all.py` | runner: builds (parallel), evaluates, prints the pass/fail table, sweeps thresholds |
| `scenarios/*.py` | one builder per scenario (`build(sc)`), `_common.py` shared histories |
| `hdsi/build.py` | builder framework: volumes, faketime phases, loop devices, Windows-style wipe, snapshots |
| `hdsi/pop.py` | seeded trees, churn, MFT fragmentation (fill and punch) |
| `hdsi/ntfsraw.py` | independent parser: boot sector, FILE record (fixups, attributes), INDX block |
| `hdsi/scan.py` | the A3 scan: every 512-byte offset of the session range, never a byte outside it |
| `hdsi/sniff.py` | carve signatures (A3.4, F2) and `matches_extension` (F3), subset of `hds-carve` |
| `hdsi/model.py` | reference model of C, D, E5/E6 (with E1-E4, E7-E9 as far as placement needs), F, G1/G2 |
| `hdsi/truth.py` | ground truth from the build record and the final image |
| `hdsi/compare.py` | checks a catalog against a truth file, hard failures and soft findings |

None of this shares code with the engine or with `scripts/linux/inspect-ntfs.py`.

## How a scenario is built

- **Time.** Every mkntfs and every ntfs-3g mount runs under libfaketime at a date the scenario sets
  (older generations in 2017-2020, the lost volume 2021-2023, the accident at 2024-06-01 12:00:00).
  So `$STANDARD_INFORMATION` times span years as on a real disk, and every re-format starts in a
  later whole second than the last write to the old volume (mkntfs stamps whole seconds).
- **The accident** is `mkntfs -Q` on the same region.  Windows' rewrite of `$MFT` slots 0..255 at
  format and first mount is imitated afterwards: every slot from the new `$MFT`'s initialized size
  up to 255 that lies in a cluster free in the new volume is zeroed (Windows 10 style; `windows=
  'blank'` writes unused FILE records as Windows 7 does).  Scenarios marked "no wipe" keep plain
  mkntfs behaviour.
- **Partitions** are written with sfdisk; volumes inside a disk image are formatted and mounted
  through `losetup -o OFFSET --sizelimit LEN` (always detached again, also on failure).
- **Embedded volumes** are built in a separate image file and copied into a file of the host
  through the driver (raw), or converted to a dynamic VHD with `qemu-img` first.
- **Names.** Every name a scenario creates starts with its file system's tag (`L_` lost volume,
  `O_`/`G0_`/`G1_` older generations, `G_` guest image, `C_` current volume ...), which lets the
  truth attribute index entries to file systems.
- **Snapshots.** Before a file system is destroyed (re-format of its region, copy into a container)
  and at the end, it is read through its own structures: boot sector, `$MFT` runlist (including an
  `$ATTRIBUTE_LIST` in extension records), every slot below the initialized size with its raw FNV-1a
  64 hash, the `$MFT` bitmap, the cluster bitmap and the `$MFTMirr` runs.  Content is seeded, and the
  SHA-256 of every file written is recorded.  `build.json` in the scenario directory holds all of
  it.

Builds are deterministic in layout and content; sub-second timestamps vary between builds.

## Scenarios

"Expected" is what the truth demands.  All scans are the whole image unless a second scan is listed.

| scenario | situation | rules | expected |
|---|---|---|---|
| basic | one file system, three years of use, quick format | C D E F G | one main instance |
| basic_win7 | as basic, unused FILE records (Windows 7 style) instead of zeros in slots up to 255 | A3 C3 D1 | one main instance |
| fragmft | lost `$MFT` in dozens of fragments | D1 D4 D5 F | one main instance |
| churn_fragmft | fragmented `$MFT` plus folder churn (deleted folders' slots reused by new folders) | D4(a) thresholds | one main instance |
| offset_mbr (+ `part`) | partition at 1 MiB, scanned as disk and as partition | C1 F2 F5 D8 | main at 1 MiB |
| offset_mbr63 (+ `part`) | partition at sector 63 (nothing 1 KiB-aligned) | C1 F2 F5 | main at 32256 |
| offset_gpt (+ `part`) | GPT, data partition at 16 MiB behind an MSR partition | C1 F2 F5 | main at 16 MiB |
| cluster_8k | old 4 KiB clusters, accident with 8 KiB | F1 F2 F4 | main (4096, 0) |
| cluster_64k | old 4 KiB, accident with 64 KiB: new `$MFT` moves, old record 0 survives | F2(mft) D4(c) D8 | main (4096, 0) |
| cluster_64k_fragmft | same with a fragmented old `$MFT` | D4(c) F2 F6 | one main instance |
| post_write | 150 files written after the accident, 40 deleted again | C2-C5 G1 | live/post_format hidden, hit files overwritten |
| postformat_churn | 60 files copied after the accident and deleted | C3 C5 D2 | one instance, nothing older |
| one_replace | one file system: delete the oldest folder, copy a new one | D2 D9 | one instance, nothing older |
| one_rename | one file system: oldest files renamed last | D2 D9 | one instance, nothing older |
| one_rewrite | one file system: oldest tree re-saved via temp + rename | D2 D9 | one instance, nothing older |
| gen_replace | older generation + lost generation with a replace history | D2-D4 D8 D9 E5 G2 | lost = main, tail = older |
| gen_rename | older generation + lost generation with a rename history | D2-D4 D8 D9 | lost = main, tail = older |
| gen_random | two generations, random trees with churn (reviewer gw1) | D2-D4 E5 E6 G2 | lost = main, tail = older |
| gen_mk | two generations, plain mkntfs (reviewer gm1) | D2-D4 E5 | lost = main, tail = older |
| gen3 | three generations | D2-D4 D9 | newest = main, both others older |
| gen_big_old | older tail larger (records and data) than the newer generation | D2 D8 D9 F | newer = main |
| gen_tiny_new | newer generation with about 20 surviving records | D2 D6 D8 D9 | newer = main |
| gen_tiny_new_mk | newer generation of 20 records, no wipe | D2 D6 D8 D9 | newer = main |
| oldmft_frag | older generation's `$MFT` fragmented, lost generation on top | D2-D4 D9 | older fragments join older, never main |
| isolated_fragments | lost `$MFT` fragments whose files' folders were all wiped | D4(b)(d) D5 F | one main instance |
| isolated_fragments_resident | the same with resident files only | D4 D9 | limit case, see below |
| embed_big | raw image with more records than its host (reviewer vm-win) | D4 D7 D8 E6 G2 | host = main, guest = embedded |
| embed_vhd | dynamic VHD with more records than its host (reviewer E3) | D1 D4 D7 D8 | host = main, guest = embedded |
| embed_mid | image with fewer records than the host, numbers overlapping | D6 D7 | guest = embedded |
| embed_contig_64k | contiguous image on a 64 KiB-cluster host (reviewer E1c) | F2 F4 F6 D7 D8 | host = main (65536, 0) |
| embed_small_fragmft | image under 256 records on a host with fragmented `$MFT` (mf1) | D4 D5 D7 | stick = embedded, host one instance |
| embed_contig_intact | small contiguous image on the lost volume (C2 verdict) | C1 D7 G1 | embedded, image not overwritten |
| embed_orphan | image whose container record was destroyed | D4 D7 D9 | guest = other, never mixed |
| embed_skew | guest clock 3 days ahead of the host | D7 | guest = embedded, container intact |
| embed_copies | image holding copies of its own MFT blocks | D6 E6(b) | copies strays, no host folder named by them |
| equal_recnums | host and image with folders at equal numbers above 255 (E11) | D4(a) D7 E6 | no mixing |
| equal_recnums_churn | host and image with the same history (equal owners and children) | E6 F2 | host folders get host names only |
| stale_layout | 2019 layout at sector 63 with a volume-filling file, 2022 volume at 1 MiB (E6) | C1 D7 D8 D9 G1 | 2022 = main, 2019 = other/older, stale file overwritten |
| repart_intact (+ `part`) | re-partitioned disk, old volume intact at 129 MiB inside the new partition | C1 D8 F2 F5 | old volume = main, not hidden |
| repart_split (`part1` + whole) | one old volume re-partitioned into two (S2 verdicts) | C1 F5 G1 | range reaches partition 2 |
| mft_copies | copies of MFT blocks inside files, one taken before renames | D6 D7 E6 | copies are strays |
| no_index | no index blocks, no known file types (E7) | F1 F2 F4 F6 | right geometry, not validated |
| tiny_anchor | red team: nearly empty lost volume (9 surviving records) over a large older generation | D5 D6 D8 D9 G2 | lost = main (anchor), older = older |
| tiny_anchor_fresh | red team: the same on a new disk | D5 D6 D8 | lost = main, not strays |
| two_parts_anchor | red team: whole disk, intact partition 2 formatted over a 2017 file system (a second anchor with more records) | C1 C3 D8 | lost = main |
| same_day_repart | red team: stale container of an old layout, re-partitioned and re-filled the same day, accident with 64 KiB clusters | D7 D8 G1 | lost = main, never embedded |
| same_day_repart_static | red team: the same, disk only read after the re-fill | D7 D8 G1 | lost = main, never embedded |
| repart_shrink (`part1` + whole) | red team: partition shrunk at the same start; the old volume's mid-volume `$MFTMirr` survives inside the lost one | D4(c) E6 | no record of the old volume in main, no old names |
| clock_reset | red team: two generations, one session of the newer under a reset clock (2009) | D2 D3 D4 E5 | lost = main, older = older, no mixing |
| few_big_repart (whole + `part`) | red team: three big videos survive, partition re-made smaller at the same start | D6 F5 G1 | lost = main, videos not out of bounds (whole) |
| few_big_split (whole + `part`) | red team: the same, freed space made a second formatted partition (old backup boot gone) | F5 G1 S2 | videos not out of bounds (whole) |
| late_fill | red team: folders made at the start, every surviving record two months younger (cost of `e6-floor`) | E6 | names kept |
| second_accident | red team: the disk quick-formatted twice ten days apart, 600 files in between | D2 D8 D9 E5 | V1 = main, first lost volume = older, no mixing |

## Ground-truth format (`hds-instance-truth/1`)

One file per scan: `$HDSI_LAB/scenarios/<scenario>/<scenario>.<scan>.truth.json`.  JSON, one
top-level member per line, one lost folder or record per line.  Offsets are absolute byte offsets
in the image file; times are FILETIME.

```
format          "hds-instance-truth/1"
scenario, scan, description, image, image_size
session_range   [start, end)            what to scan
partition_table null | {kind: "mbr"|"gpt", parts: [[start_sector, sectors], ...]}
current_volumes [{volume, offset, format_time}]   the volumes C1 must find
main            id of the file system that must be main (null: no expectation)
volumes         {id: VOLUME}            every file system ever created on the disk
lost_folders    [LOST]                  every lost folder a correct catalog has
records         [RECORD]                every FILE record the lab's scan finds in the range
notes           [text]
```

`VOLUME` (ids are stable names chosen by the scenario: `lost`, `older`, `gen0`, `guest`,
`layout2019`, `current` ...):

```
tag                 name prefix of everything created on it
expected_kind       main | older | other | embedded | strays | absent (no surviving record)
accept_kinds        kinds that are acceptable (first = expected_kind)
records_surviving   its old records found by the scan
current             true for a current volume (C1)
format_time         FILETIME of its format (creation time of its $Volume)
cluster_size, record_size
embedded_in         [host id, container file name, "raw"|"vhd"] or null
geometry            {cluster_size, volume_offset, validated: true|false|null}   (not for images)
lost_volume_range   {true: [a, b)  the volume's real extent (total_sectors x bytes per sector),
                     must_cover: [a, b)  from the volume start to the end of the highest data run
                                 of any of its surviving in-use files (null when none),
                     protected: true|false|null}
```

`RECORD` (every scanned FILE record, keyed by `off`):

```
off, num, seq, in_use, dir, base (true for a base record), name (best name or null)
volume      file system that wrote these bytes (null if unknown)
role        primary         at its own place in its file system's $MFT (or inside the container
                            for an embedded file system)
            mirror          a $MFTMirr copy
            copy            any other copy (copies of MFT blocks in files)
            artifact        mkntfs's never-used slots 16-23 (record number field 0)
            primary-torn    at its own place but changed later (e.g. a sector overwritten)
            current-live / current-deleted   written by a current volume, in use / free
class       old | live | post_format        what C3 must say
kind        the kind of instance that must hold it (null for live/post_format);
            copies, mirrors and artifacts may also stay in strays or join their own file system
parent      (base records with a name, primary role only)
            ["root"] | ["extend"] | ["system"] | ["dir", OFFSET of the parent's record] |
            ["lost", P, s]   lost folder keyed by the parent reference
flags       (non-directory files with content) {overwritten: true|false|null,
            older_generation: true|false|null, content: "intact"|"damaged"|null,
            evidence: "current"|"overlap"|"records-inside"|null}
```

`overwritten` is true when the file's runs intersect what the current volume allocated or what a
post-format record maps, or when the file is damaged (its content no longer matches what was
written) and a surviving intact file or another file system's records lie in its runs; false when
the content is intact and nothing current lies in it; null when damaged without surviving evidence
(no rule can know).  `older_generation` is true for every file of an `older` file system, false for
`main` and `other`, null otherwise.

`LOST`:

```
volume, key: [P, s]     the lost folder (instance of `volume`, parent reference (P, s))
names                   names that index entries of that file system still supply for (P, s)
                        (any one is right; [] means "Folder #P")
parents                 acceptable parents of the lost folder: ["root"], ["dir", OFFSET],
                        ["lost", P2, s2] or ["lostroot"] ([Lost folders]) for a nameless one
```

### Using the truth files for another implementation

Build the scenarios (`run-all.py --no-eval`), run the implementation on `image` with
`session_range`, export per record offset: class, instance id and instance kind; per file
(base record offset): parent node and flags; per instance: geometry (cluster size, volume offset,
validated, lost-volume range); per lost folder (instance, P, s): name and parent; the main
instance.  `hdsi/compare.py` (`compare(truth, catalog)`) documents the expected shape
(`catalog_from_model` builds it from the reference model) and the checks.  Instances are matched
to file systems by majority, so instance ids may be anything.

## Checks (`hdsi/compare.py`)

Hard failures (verdict FAIL): `class` (C3), `mix` (a non-strays instance holds records of two file
systems), `main`, `kind`, `parent` / `foreign` (a file attached to a directory of another file
system), `lostname` (a name no entry of that file system supplies), `lostparent`, `geometry` (main
and other file systems), `range` (lost-volume range does not cover its files), `overwritten_missed`
/ `overwritten_false`, `older_missed` / `older_false`, `validated` where the truth expects a value.
Soft findings: `lostname_missing` (left as `Folder #P` although an entry exists), `split` (records of
a file system in strays), `range_over`.

## The reference model (`hdsi/model.py`)

Steps follow the text in order: C1/C2 current volumes and their snapshot (reads only inside the
session range), C3 classes, C4 current index data, C5 overwritten set; D1 fragments, D2 cuts
(changepoints, contradictions A and B, choosing and keeping), D3 boundary times, D4 joining with
evidence (a)-(d) and every forbidding condition, D5 order of work, D6 strays and copy sets, D7
embedded and stale containers, D8 main, D9 kinds; E1/E2 groups, E4 names, E5 parents, E6 lost-folder
names, E7 orphans, E9 system files; F1-F6; G1 flags and the G2 sweep.

`Params` holds every threshold (`min_records` 16, `ref_ratio` 0.9, `content_ratio` 0.8,
`contra_ratio` 0.2, `contra_refs_min` 3, `mftmap_min` 16, `copy_ratio` 0.9, `embed_ratio` 0.9,
`embed_tol` 1 day, `cut_min_hi` 2, `f5_rank` 4, `f6_*`, ...) and the reading chosen for every
ambiguous sentence (defaults = the most literal reading):

| parameter | default reading | other reading |
|---|---|---|
| `a_dir_only` | D4(a) counts a parent reference only when the target on the other side is a directory | count every parent reference |
| `ambiguity_scope` | D4(a) "more than one scanned record could resolve": records of D's input | every stored record (live included) |
| `anchor_lowest` | D4 anchor rule: lowest old record at the origin held by that instance | lowest at the origin overall |
| `copy_any_larger` | D6: a record may repeat one of any larger instance | (not implemented otherwise) |
| `boot_readings` | F2: both readings of a boot hit, the hidden-sectors-consistent one ranked first | only the consistent one |
| `lost_chain` | E6: a named lost folder below a nameless lost parent stays below it | moved directly under [Lost folders] (the truth files encode the default reading; `flat` therefore fails the `lostparent` check by construction) |
| `g2_dirs` | G2 compares file data only | also directories' index allocation |
| `strays_is_instance` | E6 "owner number not present in any instance": strays count | strays do not count |
| `seq0_wildcard` | reference rule as written: sequence 0 has no special meaning | sequence 0 resolves to anything (first model) |
| `f2_mft_ext` | F2 MFT map through every `$MFT` extent, also those in record 0's extension records | record 0's own extents only (first model) |
| `oob_literal` | G1 out_of_bounds: a run whose LCN is >= total_clusters | a run reaching beyond total_clusters (first model) |
| `c5_embedded` | G1 overwritten from C5 also for embedded instances | embedded instances skipped (first model) |

The lab's proposed rule changes are switches (`Params.fixes`, `run-all.py --fixes`), off by
default, so a run without `--fixes` is the text as written:

| fix | paragraph | change |
|---|---|---|
| `d4c-self` | D4(c) | the volume offset of the MFT map must also be the one at which the record-0 side itself lies |
| `d6-copy-ctime` | D6 | a copy repeats number, sequence and `$STANDARD_INFORMATION` creation time |
| `d4-older-side` | D4 | the instance holding the segment above a cut may not join an instance that is *after* the cut (instead of: that is not *before* it) |
| `d7-oldest` | D7 | the container's time is compared with the oldest change time of the instance, not the newest |
| `f5-hull` | F5 | the lost-volume range reaches at least to the end of the highest run of any in-use file |
| `e6-match` | E6 | an index block belongs to the instance whose records its entries name exactly |
| `e6b-copyset` | E6(b) | only a stray copy of the instance itself can name its lost folders |
| `d6-anchor` | D5(3), D6 | the anchor is never merged into strays, however few records it has, and gets a geometry |
| `d8-newest-cv` | D8(1) | with several anchors, the one of the current volume with the newest `format_time` first |
| `d7-volstart` | D7 | an instance validated at the range start or a partition start that is not a copy is never embedded (its container is stale) |
| `d4c-size` | D4(c) | the `$MFT` initialized size of the record 0 covers every record number of the other side |
| `e6-floor` | E6 | a block attributed only by an owner number that no instance holds names nothing older than the instance's oldest record (1 day tolerance) |
| `d2-fallback` | D2, D3 | a contradiction without a changepoint chooses the number with fewest out-of-order timed records (at most a quarter); A_c ignores records not newer than B_c |
| `f5-few` | F5 | with 1 to 3 qualifying files, end_by_files is the smallest of their ends instead of 0 |

`--fixes all` applies the first lab's changes, `--fixes red` the red team's, `--fixes all,red,-name` all but one.

Rejected variants kept for reference: `d4c-not-first` (D4(c) counting only records outside the first
`$MFT` extent: also blocks the lost volume's own `$MFTMirr` copy, embed_contig_64k), `d6-copy-name`
(copy test by names: a copy taken before renames stops being a copy set, mft_copies) and
`d3-before-A` (D3 "before" = newest < A_c: newer-generation index entries older than the oldest
surviving record then count as "before" and name lost folders of the older instance, gen_tiny_new).
`Params(trace=True)` records the margins to every threshold (`Model.tr`).

## Limits (what Linux cannot imitate)

- mkntfs puts every `$MFT` at 16 KiB (LCN 2 above 8 KiB clusters) and the `$MFTMirr` in the middle
  of the volume; Windows uses one third of the volume or 3 GiB and byte 8192.  The wipe imitates
  only the loss of slots up to 255.  First user record is 64 here, about 38 on Windows.
- ntfs-3g writes LSN 0 everywhere (LSN tie-breaks of E6 and G2 are untested), POSIX names only
  (no Win32/DOS pairs), non-resident attribute lists only, and reuses the lowest free record only
  after a remount.
- Windows' MFT-change time after moves, robocopy and Explorer copies is not reproduced; the
  scenarios assume the driver sets it at every change (ntfs-3g does).
- The XP-era case of an old `$MFTMirr` that survives in the middle of the volume while the new
  volume mirrors at byte 8192 cannot be built (mkntfs puts both mirrors at the same place).
- `isolated_fragments_resident` fails by design: fragments of the lost `$MFT` whose only files are
  resident and whose folders were wiped carry no evidence that ties them to main.
