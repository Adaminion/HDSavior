# What a Windows quick format overwrites (reference sheet)

Facts collected while reviewing ARCHITECTURE.md sections 3, 6.1, 6.3–6.7 and 9. On-disk structure
details (record header, fixups, runlists, index blocks) are in [ntfs-on-disk.md](ntfs-on-disk.md);
this sheet is only about *what is written where* by a format and by Windows afterwards.

Evidence level of every statement:

- **[D]** documented: Microsoft documentation or a header/source file, read in full (section 9).
- **[W]** measured on NTFS volumes that were formatted and mounted by real Windows (public test
  images, section 8). All of them are small (3 MiB – 1 GiB) and their media were blank before the
  format.
- **[L]** measured here with `mkntfs` / `ntfs-3g` 2022.10.3. This is **not** Windows behaviour.
- **[R]** reported by vendors, forums or forensic write-ups. Read only as search-engine excerpts
  (the pages could not be opened from the review environment), so treat wording as approximate.
- **[I]** inference. Not verified. Section 7 lists what the Windows CI job should pin down.
- **[CI]** measured by the Windows CI job (`scripts/windows/format-facts.ps1`, section 7a) with
  Windows' own `Format-Volume -Full:$false` on virtual disks, Windows Server 2025 build 26100 (the
  Windows 11 24H2 kernel), raw bytes read back and compared slot by slot.

## 1. Summary

After `format /q` (or the Explorer dialog with "Quick Format") **and the automatic mount that follows**:

| Area | Rewritten? | Where | Level |
|------|-----------|-------|-------|
| Boot sector + backup copy | yes | first sector of the volume; last sector of the *partition* | [W][D] |
| `$MFT` records 0–15 | yes, by `format` | MFT start (section 3) | [W] |
| `$MFT` records 16–255 | yes, by the NTFS driver at first mount (zeros where unused) | directly behind, same extent when free | [W][CI] |
| `$MFT` records 256 and up | **no** | – | [CI]: 1368 of 1368 intact on a used volume |
| `$MFTMirr` (records 0–3) | yes | byte 8192 of the volume (Windows 7 and 10 samples) | [W] |
| `$LogFile` | yes, **completely** (0xFF fill) | before the MFT | [W] |
| `$Bitmap`, MFT bitmap | yes, completely | directly before the MFT | [W] |
| `$UpCase`, `$AttrDef`, `$Secure` | yes | start of volume / before the MFT | [W] |
| Root directory index block VCN 0 | yes | fixed place near the volume start | [W] |
| TxF log, `$Repair` streams, `System Volume Information` | yes, at first mount, about 22–25 MiB | first free clusters | [W] |
| File data, later MFT fragments, other index blocks | no | – | [R] |
| **Whole volume on TRIM-capable devices** | **discarded (TRIM / UNMAP)** | everything | [D][R] |

The same format on the same partition puts every structure at the same place again (section 3), so
a re-format normally destroys old *metadata* only. It destroys old *file data* when the previous
file system was laid out differently (other Windows generation, other tool, other partition start
or size): then the new `$LogFile` (64 MiB), `$Bitmap` (volume size / cluster size / 8 bytes, 122 MiB
for 4 TB at 4 KiB clusters) and the first-mount files land on old user data. All of these are
allocated in the new `$Bitmap`. [I]

## 2. Boot sectors

- The NTFS volume is one sector smaller than its partition: BPB `total_sectors` = partition sectors − 1.
  The backup boot sector is the last sector of the partition, i.e. at
  `volume_start + total_sectors × bytes_per_sector`, and is byte-identical to the primary.
  [W: 2 097 151 / 2 097 152, 161 663 / 161 664, 6 015 / 6 016 sectors] [L: same for mkntfs]
  [D: layout.h lines 34–43: "On NT4 and above there is one backup copy of the boot sector to be found on
  the last sector of the partition"; NT 3.51 and earlier kept it at `number of sectors / 2`]
- `hidden_sectors` (0x1C) holds the partition start LBA at format time (2048 and 128 in the samples). [W]
  It can confirm whether a boot sector found mid-scan is a primary or a backup. [I]

## 3. `$MFT`

Size and contents

- `format` itself writes a 16-record MFT: the `$FILE_NAME` attribute of record 0, which is written at
  format time and not updated later, says allocated = real = 16 384 bytes in all three Windows samples. [W]
- After the first mount `$MFT` `$DATA` has allocated = real = initialized size = 262 144 bytes
  (256 records of 1 KiB) in all samples. [W] In the 3 MiB sample the extra 60 clusters are a second
  extent (`LCN 250 × 4, LCN 639 × 60`), i.e. they were added after format had laid out its files. [W]
  Vendor description of the end state: "32KB for Windows XP, 64KB for Windows Vista, and 256KB for
  Windows 7/8/8.1/10/11", "229" user records lost on Windows 7 and later. [R: R-Studio]
- State of the 256 slots after format + mount, nothing else done: [W]

  | Slots | Windows 7-era sample (2013) | Windows 10 samples (2020, 2021) |
  |-------|-----------------------------|---------------------------------|
  | 0–11 | system files, in use | same |
  | 12–15 | in use, no name, sequence = record number | same |
  | 16–23 | `FILE`, flags 0, sequence 1, no attributes, LSN ≠ 0 | all zero bytes |
  | from 24 | 24–34: `$Extend` children and TxF files | 24–37: the same plus `$Extend\$Deleted`, `System Volume Information`, `WPSettings.dat` |
  | first user record | 35 | 38 |
  | unused up to 255 | `FILE`, flags 0, sequence 1, no attributes, LSN ≠ 0 (logged initialisation) | all zero bytes |

  So about 220 user records (the first files and folders ever created on the old volume) are lost,
  plus whatever the new MFT grows over when new files are written.
- **Settled [CI]:** on a *used* 1 GiB volume (1600 user records) a quick format of the same size put
  the new `$MFT` at the same LCN (87 290) with 256 records, and every old slot below 256 was
  destroyed: 0–37 rewritten as the new system records, 38–41 other new records, 42–255 **zero
  bytes**. Every old record from 256 up (1368) was byte-identical afterwards. 232 of the 1600 user
  records were lost to the format itself. Older text of this point, kept for context: the Windows 10
  samples were made on blank virtual disks, so the zeros in unused slots did not prove that Windows
  10 writes them. It has to (the slots are below the valid data
  length, and stale in-use records there would be MFT corruption after every re-format), but this is
  [I]. Do not build logic that depends on either answer; classify records in live MFT slots by
  content and time (in use → live; no attributes → discard; otherwise compare the `$SI` MFT-change
  time with the creation time of the live `$Volume` record).
- With 4 KiB file records (`format /L`, 4Kn disks, tiered volumes [D: format `/L`]) the initial size
  is unknown (256 KiB or 256 records). [I]
- With 64 KiB clusters the MFT after format + mount is only 64 KiB (64 records; `fsutil` "Mft Valid
  Data Length 64.00 KB"), so old records 64–255 can survive such a format. [CI] The engine
  classifies slots by content (C3), so nothing depends on the number.

Placement

- Volumes up to 1 GiB: MFT at one third of the volume (LCN 250 of 751, 53 888 of 161 663,
  87 381 of 262 143). [W]
- Measured [CI], 4 KiB clusters unless noted (volume size → MFT start): 256 MB → size / 3
  (LCN 0x54FA); 1 GB → size / 3 (LCN 0x154FA); 4 GB → **1 GiB** (LCN 0x40000); 16 GB → 3 GiB
  (LCN 0xC0000); 64 GB → 3 GiB; 16 GB with 64 KiB clusters → 3 GiB (LCN 0xC000). So: one third,
  rounded down to whole GiB once that is at least 1 GiB, at most 3 GiB. `$MFTMirr` at LCN 2
  (LCN 1 with 64 KiB clusters). ALGORITHMS A2 probes both readings.
- Large volumes: byte offset 3 GiB from the volume start, LCN 0xC0000 = 786 432 at 4 KiB clusters,
  independent of the cluster size, also on Windows XP. [R: `fsutil fsinfo ntfsinfo` output
  "Mft Start Lcn 0x00000000000c0000 / Mft2 Start Lcn 0x2" quoted on Microsoft Q&A; MSFN "default NT
  format will tend to create the $MFT @#786432", "insensitive to cluster size"; a vendor lab
  test "post-format MFT starts at LCN 786,432"]. The size where the rule changes from "one third"
  to "3 GiB" is not documented (they meet at 9 GiB). [I]
- mkntfs puts the MFT at 16 KiB (LCN 4 at 4 KiB clusters) [L] [D: mkntfs.c "We start at the higher
  value out of 16kiB and just after the mft bitmap"]; Windows NT4/2000 are said to have done the
  same (an older Microsoft `fsutil fsinfo ntfsinfo` sample shows "Mft Start Lcn 0x4"). [R]
- The old MFT is **not** always where the new one goes: volumes made by other tools, converted from
  FAT, or with a moved MFT have it elsewhere ([R]: lab test with the old MFT at LCN 175 724 968 and
  the new one at 786 432). Then old records 0–255, including old `$MFT` (record 0, with the MFT's
  own runlist) and the old root directory (record 5), survive.
- Growth: the MFT zone is "a multiple of 200MB units", default 200 MB, "based on the contiguous range
  past the end of the MFT"; a new zone is chosen only when the current one is used up. [D: fsutil
  behavior `mftzone`] So the first fragment is normally contiguous for at least ~200 000 records;
  volumes with more files usually have further fragments elsewhere. XP reserved 12.5 % of the volume. [R]

## 4. Other metadata

| File | Windows 7-era, 1 GiB, 4 KiB clusters [W] | Windows 10, 79 MiB, 512-byte clusters [W] | Remarks |
|------|------------------------------|--------------------------------|---------|
| `$Boot` | LCN 0–1 | LCN 0–15 | 8192 bytes |
| `$MFTMirr` | LCN 2 | LCN 16 | byte 8192, 4 records. XP and earlier: middle of the volume [R], as mkntfs does [L] |
| `$UpCase` | LCN 3–34 | LCN 24–279 | byte 12 288, 128 KiB |
| `$AttrDef` | LCN 85 613 (near MFT) | LCN 280–284 | |
| Root `$I30` block VCN 0 | LCN 44 (byte 180 224) | LCN 285 (byte 145 920) | 4 KiB W10 sample: LCN 36 |
| `$LogFile` | LCN 83 723 × 1824 (7.1 MiB) | LCN 49 751 × 4096 (2 MiB) | ends 1824 clusters / 0 clusters before `$Bitmap` |
| `$Bitmap` | LCN 87 371 × 8 | LCN 53 847 × 40 | ends 1 cluster / 0 clusters before the MFT bitmap cluster |
| `$MFT` `$BITMAP` | LCN 87 380 (+1 later) | LCN 53 887 (+15 later) | the cluster before the MFT |
| `$MFT` | LCN 87 381 × 64 | LCN 53 888 × 512 | |
| `$Secure:$SDS` | LCN 85 548 × 65 | LCN 5 132 × 513 | 263 KiB |

- Every position is a simple function of volume size and cluster size; nothing is randomised. [I from W]
  A re-format by the same Windows generation therefore rewrites exactly the old locations.
- `$LogFile`: 2 MiB minimum [D: format `/LogSize`], 64 MiB "upwards from 12GiB" [D: comment in
  mkntfs.c, which copies Windows' table]. Every page that the new file system has not used yet is
  filled with 0xFF (1710 of 1824, 450 of 512, 474 of 512 pages) – the old log at the same place is
  gone completely. [W]
- Root directory: after a format all root entries are in one `INDX` block (VCN 0) plus the resident
  `$INDEX_ROOT` in record 5. Both are rewritten by a re-format. Further root index blocks (VCN ≥ 1)
  are allocated later, elsewhere, and survive. [W for the layout] [L: a root with 60 folders has 25
  entries in VCN 0 at the format-time cluster and 45 in VCN 1–2 at later clusters]
- Directories with up to about 4–5 entries keep them resident in `$INDEX_ROOT`; no `INDX` block
  exists for them. [L: 1–4 entries resident, 5 and more in an `INDX` block] [W: `a_directory`, 2 entries]

## 5. What Windows writes on its own after the format

Created within one or two seconds of the format, without any user action [W]:

- `$Extend\$RmMetadata\$TxfLog\$TxfLogContainer…0001` and `…0002`: 2 × 10 MiB on the 1 GiB sample
  (2 × 2 MiB on tiny volumes), valid data length = size, placed in the first free clusters after
  the root index block (LCN 45 and 2605); `$Tops:$T` 1 MiB; `$TxfLog.blf` 64 KiB.
  `format /TXF:disable` exists [D].
- Windows 10 only: `$Extend\$RmMetadata\$Repair:$Corrupt` 2 MiB and `:$Verify` 300 KiB;
  `$Extend\$Deleted`; `System Volume Information` with `WPSettings.dat`.
- `IndexerVolumeGuid` follows when Windows Search touches the volume. [R] `$RECYCLE.BIN` is absent
  in all samples (created on first use). [W]
- A USN journal is not created by the format ("change journals are not necessarily created at
  startup … an administrator may do so explicitly or start another service that requires a change
  journal") [R: Microsoft documentation excerpt], none of the samples has one [W].

Typical damage: about 22–25 MiB, all at places where a volume formatted by the same Windows
generation had the same files. Everything is visible in the new `$Bitmap`. Not visible there:
clusters of files that were created **and deleted** after the format. [L: 489 clusters written by
such a file, 0 of them allocated in the live `$Bitmap`; the deleted record in the live MFT still
holds the runlist]

Scheduled "Optimize Drives" runs weekly [R: Microsoft support page]; on drives it classifies as
solid-state or thin-provisioned it re-trims the free space of the *new* volume, which is nearly
the whole old volume.

Windows generations (old volume formatted by one, re-formatted by another):

| | NT4 / 2000 | XP / 2003 | Vista | 7 – 11 |
|---|---|---|---|---|
| MFT start (large volume) | 16 KiB [R] | 3 GiB [R] | 3 GiB [R] | 3 GiB [R], one third on small volumes [W] |
| MFT size after format + mount | ? | 32 KiB [R] | 64 KiB [R] | 256 KiB [W] |
| `$MFTMirr` | middle [R] | middle [R] | byte 8192 [I] | byte 8192 [W] |
| Records 16–23 | ? | zero [D: libfsntfs] | ? | blank `FILE` (7-era), zero (10) [W] |
| TxF log containers | no | no | yes [D: libfsntfs] | yes [W] |
| Record number in the record (0x2C) | no | yes | yes | yes [D: layout.h 402] |

An old volume formatted by XP and re-formatted by Windows 7 or later keeps its old `$MFTMirr` in the
middle of the volume: copies of old records 0–3, including old `$MFT` with the old MFT's runlist. [I]

## 6. TRIM / UNMAP

- `format` has a switch `/NoTrim`: "Skips sending trim (delete notification) during a format." [D]
  Without it a format sends TRIM for the volume. Windows 7 announcement: "The Trim operation is
  fully integrated with partition- and volume-level commands like Format and Delete". [R]
- Which devices get it is decided by the device's capability report, not by "SSD or HDD":
  "If your hard disk drive or SAN reports that it doesn't support trim, then your hard disk drive
  and SANs don't get trim notifications." [D: fsutil behavior `disabledeletenotify`]
  `fsutil fsinfo sectorinfo` prints "Trim Supported" / "Trim Not Supported". [D]
- Device-managed SMR hard disks report TRIM support: Western Digital lists internal and external
  SMR hard drives as supporting TRIM, CMR drives as not; over USB the bridge translates UNMAP. [R: WD
  knowledge base 26014] Recovery labs report that such drives return zeros after a quick format. [R]
- Thin-provisioned LUNs and VHDX virtual disks support unmap as well [R] – relevant for test rigs.
- Read-only capability query for the engine: `IOCTL_STORAGE_QUERY_PROPERTY` with
  `StorageDeviceTrimProperty` (8, `DEVICE_TRIM_DESCRIPTOR.TrimEnabled`) and
  `StorageDeviceSeekPenaltyProperty` (7, `DEVICE_SEEK_PENALTY_DESCRIPTOR.IncursSeekPenalty`).
  [D: windows-sys 0.61.2]
- A full format (no `/Q`) on Vista and later writes zeros to the whole volume. [R: Microsoft KB 941961]
  `/P:count` is documented as "Zero every sector on the volume. After that, the volume will be
  overwritten count times…"; "This switch is ignored when /Q is specified." [D]

## 7. mkntfs is not Windows format

| | Windows 7 / 10 [W] | mkntfs + ntfs-3g [L] |
|---|---|---|
| MFT start | one third of the volume or 3 GiB | LCN 4 (16 KiB) |
| MFT after format + mount | 256 records, unused slots blank or zero | 27 records; ntfs-3g later initialises 27–63 as blank `FILE` |
| First user record | 35 / 38 | 64 |
| Old user records overwritten by a re-format | about 220 | **none** |
| `$MFTMirr` | byte 8192 | middle of the volume |
| `$LogFile` | used (RSTR/RCRD), rest 0xFF | never used, LSN 0 in every record |
| TxF, `$Repair`, `System Volume Information` | yes | no |
| Records 16–23 | blank or zero | flags 0, sequence = number, self-number 0 |

An mkntfs re-format of an 800 MiB test volume changed 52 KiB; all 303 old user records survived. [L]

To be pinned by the Windows CI job (items 1 and 2 are now measured, see section 7a; item 3 on
virtual disks only):

1. On a ≥ 16 GiB virtual disk: `fsutil fsinfo ntfsinfo` shows `Mft Start Lcn 0xC0000` (4 KiB clusters;
   `0xC000` at 64 KiB), `Mft2 Start Lcn 0x2`, `Mft Valid Data Length 0x40000`.
2. Populate, quick format again, read raw: old records 0–255 are gone (new, blank or zero), record 256
   and up are unchanged, root `INDX` VCN 0 is at the same LCN as before.
3. `fsutil fsinfo sectorinfo` of the test disk: if it says "Trim Supported", the format discards the
   old data and a round-trip test is void – use a disk without TRIM or `format /NoTrim`, and assert
   a minimum number of recovered files.

## 7a. Measured by the Windows CI job

`scripts/windows/format-facts.ps1` (job "Windows quick-format facts", artifact `format-facts`):
placement and the accident are in sections 1 and 3 above. TRIM: a marker file filled the volume, the
volume was quick-formatted, the raw disk was searched for the marker afterwards. On a fixed VHD and
on a dynamic VHDX, with `DisableDeleteNotify` 0 and 1, all 256 marker blocks survived in all four
cases, although `fsutil` reports trim limits for these volumes. This shows only that Windows' quick
format did not discard data on these virtual disks; it says nothing about a physical SSD, where the
summary's TRIM row stands. Per file (second run, file ids fixed): every one of the 606 intact old files that
were checked had byte-identical content through its old runlist after the format; the 10 deleted
files' slots (471–480) were taken at once by the next 10 files created, so a deleted file's record is
normally gone as soon as anything new is written. Copies from another volume: robocopy `/COPY:DAT`
keeps creation and modified time, Copy-Item and Move-Item (copy + delete across volumes) keep only the
modified time. **Open and important for ALGORITHMS D2/D3:** in the raw `$SI` of all 60 copies the
MFT-change time was *older* than the start of the copy. The third run records the four times the
system reports for source and copy, to tell "the copy tools carry the MFT-change time over" from "the
raw record was not flushed yet". If copies do carry old MFT-change times, a volume restored from a
backup with robocopy has a whole generation of records whose times say nothing about when they were
written, and the D2 fallback (at most a quarter of timed records out of order) would not cover it.
The file meant to have thousands of runs got one run (sparse files allocate in 64 KiB units); the
third run writes one block every 128 KiB.

## 8. Samples and how to re-check

| Sample | Source | Made by |
|--------|--------|---------|
| 1 GiB, 4 KiB clusters, 2013-12-03 | `log2timeline/plaso` `test_data/vsstest.qcow2` | Windows 7-era (TxF present, no `$Extend\$Deleted`), two shadow copies |
| 79 MiB, 512-byte clusters, 2021-05-01 | `log2timeline/dfvfs` `test_data/vss.raw` | Windows 10 (`$Extend\$Deleted`, `$Repair:$Corrupt`) |
| 3 MiB, 4 KiB clusters, 2020-10-11 | `log2timeline/dfvfs` `test_data/ntfs-fixed.vhd`, `ntfs-parent.vhdx` (partition at sector 128) | Windows 10 |

```
curl -LO https://raw.githubusercontent.com/log2timeline/plaso/main/test_data/vsstest.qcow2
qemu-img convert -O raw vsstest.qcow2 vsstest.raw
fsstat vsstest.raw                 # MFT / MFTMirr clusters, sector range
istat  vsstest.raw 0               # $MFT: $FILE_NAME sizes (16384), $DATA size/init_size (262144), runs
istat  vsstest.raw 2 ; istat vsstest.raw 5 ; istat vsstest.raw 6     # $LogFile, root, $Bitmap
curl -LO https://raw.githubusercontent.com/log2timeline/dfvfs/main/test_data/vss.raw
curl -LO https://raw.githubusercontent.com/log2timeline/dfvfs/main/test_data/ntfs-fixed.vhd   # istat -o 128
```

Stale copies are normal on real volumes: the two samples with shadow copies contain 88 and 92
`FILE` records outside the live MFT (4 of them are the live `$MFTMirr`), in same-origin groups of
at most 16 records (copy-on-write blocks), among them several copies of records 0–15, and each
sample has four old copies of the root `INDX` block. [W]

## 9. Sources

Read in full:

- Microsoft, `format` command: <https://learn.microsoft.com/windows-server/administration/windows-commands/format>
  (source file `MicrosoftDocs/windowsserverdocs`, `administration/windows-commands/format.md`).
- Microsoft, `fsutil behavior` (`mftzone`, `disabledeletenotify`) and `fsutil fsinfo` (`sectorinfo`):
  same repository, `fsutil-behavior.md`, `fsutil-fsinfo.md`.
- ntfs-3g 2022.10.3 `layout.h` (`/usr/include/ntfs-3g/layout.h`), `ntfsprogs/mkntfs.c`,
  `libntfs-3g/index.c` (<https://github.com/tuxera/ntfs-3g>).
- libyal, "New Technologies File System (NTFS)" format documentation
  (<https://github.com/libyal/libfsntfs/tree/main/documentation>): reserved records, `$Extend` children
  per Windows version, zero-filled records 16–23 on XP.
- TestDisk `src/ntfs_adv.c` (<https://github.com/cgsecurity/testdisk>): rebuilds the boot sector from
  `$MFT` / `$MFTMirr` records found by scanning.
- `windows-sys` 0.61.2 (storage property ids, `SetFileTime` takes creation / access / write time only).

Search excerpts only:

- R-Studio, "Specifics of File Recovery After a Quick Format": <https://www.r-studio.com/ntfs-recovery-after-quick-format.html>
- Western Digital knowledge base 26014 (TRIM support by product type): <https://support.wdc.com/knowledgebase/answer.aspx?ID=26014>
- Microsoft, "Support and Q&A for Solid-State Drives": <https://learn.microsoft.com/en-us/archive/blogs/e7/support-and-qa-for-solid-state-drives>
- Microsoft Q&A with `fsutil fsinfo ntfsinfo` output: <https://learn.microsoft.com/en-us/answers/questions/2751270/how-to-determine-file-system-block-size>
- MSFN, "Moving the beginning of a Partition": <https://msfn.org/board/topic/154982-moving-the-beginning-of-a-partition/>
- Amagicsoft lab note, "NTFS Metadata After Quick Format Explained": <https://www.amagicsoft.com/kb/lab/ntfs-quick-format-test.html>
- Rossmann Repair Group, "Formatted Drive Recovery" (SMR and TRIM): <https://rossmanngroup.com/formatted-drive-recovery>
- Microsoft, "Creating, Modifying, and Deleting a Change Journal": <https://learn.microsoft.com/en-us/windows/win32/fileio/creating-modifying-and-deleting-a-change-journal>
- Microsoft KB 941961, "Change in the behavior of the format command in Windows Vista and later versions":
  <https://learn.microsoft.com/en-us/troubleshoot/windows-server/backup-and-storage/format-command-not-write-zeros-to-disk>
- Microsoft, "How NTFS reserves space for its Master File Table (MFT)" (KB 174619): <https://support.microsoft.com/kb/174619>
- DMDE manual (full scan, volume variants, verification of MFT fragments): <https://dmde.com/manual/>
