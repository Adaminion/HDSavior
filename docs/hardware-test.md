# Real-hardware test checklist

CI rescues a volume that Windows quick-formatted, but only on a 1 GiB
virtual disk. This checklist runs the same rescue on real hardware, plus the
things CI cannot reach: a real USB bridge, a bigger disk, the app's
administrator start and disk list, and Ctrl-C in the command-line engine.
It takes about an hour, mostly waiting.

The helper `scripts/windows/hardware-test.ps1` does the bookkeeping: it fills
the test disk with known files, records their SHA-256, hashes the whole disk
read-only, and compares everything afterwards.

## What you need

- A Windows 10 or 11 PC where you have administrator rights.
- A **test disk you can lose**: a USB hard disk or an old internal HDD is
  best. A USB stick works too, but some sticks erase their data when they
  are formatted (TRIM), and then little comes back; that is a property of
  the stick, not a bug. 16–500 GB is fine; smaller is faster.
- A **second disk** (any other drive, not the test disk) with free space of
  at least twice the test disk's size: for the disk image and the recovered
  files. Below it is `D:`; use your own letter.
- The app. Either download the build artifact `hdsavior-windows-preview` of
  the latest green CI run (Actions tab of the repository, signed in to
  GitHub; artifacts expire after 7 days), or build it as described in the
  README under "Building". The folder contains `hdsavior.exe` and
  `hdsavior-engine.exe`. Put it on `C:` or `D:`, **never on the test disk**.

Open **PowerShell as administrator** for every command below, in the
repository's `scripts\windows` folder (or wherever you copied
`hardware-test.ps1`). If scripts are blocked:

```powershell
Set-ExecutionPolicy -Scope Process Bypass
```

## 1. Prepare the test disk

1. Find the test disk's number and make sure it is the right one:

   ```powershell
   Get-Disk | Format-Table Number, FriendlyName, Size, BusType, IsBoot
   ```

   Below it is disk `3` with volume `E:`; use your own number and letter.
2. Give it one NTFS volume. In Disk Management, delete its partitions,
   create one simple volume, NTFS, letter `E:`. A quick format is fine for
   this first format. This erases the test disk.
3. Fill it and write the manifest to the second disk. `-CopyFrom` is
   optional but recommended: a folder of your own photos, PDFs or music makes
   the test more realistic (it is copied, not moved):

   ```powershell
   .\hardware-test.ps1 Fill -Drive E -Manifest D:\hds-test\manifest.json -CopyFrom "$env:USERPROFILE\Pictures\SomeFolder"
   ```

   Note how many counted files it reports.
4. Optional: open `hdsavior.exe` now. Write down the warnings on the test
   disk's card; `mounted_volume` is expected at this point.

## 2. The accident

1. Quick-format the test volume, the way a user would by mistake: in
   Explorer, right-click `E:` → Format… → NTFS, **Quick Format ticked** →
   Start. (Or `Format-Volume -DriveLetter E -FileSystem NTFS -Confirm:$false`,
   which is a quick format.)
2. Take the volume offline right away, as the README advises after an
   accident, so Windows stops writing to it:

   ```powershell
   mountvol E: /p
   ```

3. Take the "before" hash of the whole test disk (read-only; on a big HDD
   this takes a while, about 1 minute per 8 GB over USB 3):

   ```powershell
   .\hardware-test.ps1 HashDisk -Disk 3 -Out D:\hds-test\before.txt
   ```

## 3. Rescue with the app

1. Start `hdsavior.exe`. Check: **one UAC prompt** appears and the app
   starts. Note anything confusing.
2. Source screen: is the test disk in "Disks on this computer" (try "Look
   for disks again")? Are its model, size and warnings plausible?
   `mounted_volume` should be gone now; a TRIM warning on a stick is
   possible.
3. "Create disk image…" of the whole test disk to `D:\hds-test\disk.img`.
   When it is done, choose "Rescue from this image".
4. "Scan the whole image". Note the time it takes and whether the progress
   looks sane.
5. Results: are `Documents`, `Photos`, `Music`, `Video` (and `Real` if you
   used `-CopyFrom`) there with their subfolders? Is the disk-layout chip
   confirmed? Open a few files in the details pane (pictures and text have a
   preview).
6. Tick everything, "Recover…" to `D:\hds-test\recovered`. Keep the default
   options.
7. Optional: "Find more files by signature". Carved files appear under
   `[Carved files]`; recover them into the same folder. With `-CopyFrom`
   photos this can bring back some of the uncounted "early" content.
8. Close the app.

## 4. Check the results

1. The "after" hash, then compare. **This must say IDENTICAL.**

   ```powershell
   .\hardware-test.ps1 HashDisk -Disk 3 -Out D:\hds-test\after.txt
   ```

   ```powershell
   .\hardware-test.ps1 CompareDisk -Before D:\hds-test\before.txt -After D:\hds-test\after.txt
   ```

2. Compare the recovered files with the manifest:

   ```powershell
   .\hardware-test.ps1 Compare -Manifest D:\hds-test\manifest.json -Recovered D:\hds-test\recovered
   ```

   Expected: every counted file is recovered byte-identical, under its old
   path, and the report shows mostly `ok` / `ok_unverified`. The 300 files in
   `early\` are not counted: a quick format destroys the records of the first
   files ever written to a volume.

## 5. Command line and Ctrl-C (optional, 10 minutes)

In the app's folder, scan the **disk** directly this time (not the image):

```powershell
.\hdsavior-engine.exe disks
```

```powershell
.\hdsavior-engine.exe scan \\.\PhysicalDrive3 --kind disk --session D:\hds-test\direct.hds
```

1. Press **Ctrl-C once** during the scan. Expected: it stops within a
   second or two, prints a `scan_done` line with status `stopped`, and
   `$LASTEXITCODE` is `130`.
2. Continue it: run the same command with `--resume` added. Expected: it
   completes.
3. Hash the disk again (`after2.txt`) and `CompareDisk` it with `before.txt`.
   It must still be IDENTICAL.

## 6. Clean up

Give the test volume its letter back (or simply format the disk in Disk
Management):

```powershell
diskpart
```

Then, inside diskpart: `list volume`, `select volume <n>` (the test
volume), `assign letter=E`, `exit`.

Delete `D:\hds-test` when you are done.

## What to send back

- The output of `CompareDisk` (both, if you did step 5) and of `Compare`.
- The test disk's `BusType`, size and model (from `Get-Disk`), and Windows
  version (`winver`).
- How long the image and the scan took.
- Anything that confused you, with a screenshot if possible: wording,
  missing buttons, warnings that made no sense.

If `CompareDisk` ever says CHANGED while nothing but HDSavior used the disk,
stop and send that first: it would break HDSavior's main promise.
