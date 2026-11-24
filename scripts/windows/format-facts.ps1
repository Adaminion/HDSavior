<#
.SYNOPSIS
  Measures what a Windows quick format leaves of an NTFS volume (CI, elevated).

.DESCRIPTION
  HDSavior's design rests on facts about Windows' quick format that were
  derived from documentation and imitated with mkntfs on Linux
  (docs/notes/quick-format-behaviour.md). This script measures them on
  volumes Windows writes itself, on virtual disks it creates and removes:

   1. Placement: where Windows puts $MFT on volumes of 256 MiB to 64 GiB,
      with 4 KiB and 64 KiB clusters (ALGORITHMS A2 probes there).
   2. TRIM: whether a quick format discards the old data on a VHD and on a
      VHDX, with and without DisableDeleteNotify.
   3. The accident: a populated volume is quick-formatted again; then every
      old MFT record is classified (intact, zeroed, blank, rewritten), the
      content of every surviving file is hashed through its old runs,
      timestamps after robocopy / Move-Item / Copy-Item from another volume
      are compared with the copy time, slot reuse after deletions is
      recorded, and the attribute list of a file with thousands of runs is
      inspected.

  Results go to <Out>/facts.json and the log. The script fails only when it
  cannot carry out a step, not when a fact differs from the documents.
#>
param(
    [string]$WorkDir = $env:RUNNER_TEMP,
    [string]$Out = (Join-Path $env:RUNNER_TEMP 'format-facts')
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$py = (Get-Command python).Source
$analyzer = Join-Path $PSScriptRoot 'format_facts.py'
New-Item -ItemType Directory -Force -Path $Out | Out-Null

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;
public static class HdsFileId {
    [StructLayout(LayoutKind.Sequential, Pack = 4)]  // FILETIMEs are 4-byte aligned in the C struct
    struct Info {
        public uint Attributes;
        public long Created, Accessed, Written;
        public uint VolumeSerial, SizeHigh, SizeLow, Links, IndexHigh, IndexLow;
    }
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool GetFileInformationByHandle(SafeFileHandle h, out Info info);
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern SafeFileHandle CreateFileW(string name, uint access, uint share, IntPtr sa,
                                             uint disposition, uint flags, IntPtr template);
    // The NTFS file reference: record number in the low 48 bits, sequence in the high 16.
    public static ulong Get(string path) {
        using (var h = CreateFileW(path, 0x80, 7, IntPtr.Zero, 3, 0x02200000, IntPtr.Zero)) {
            if (h.IsInvalid) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error(), path);
            Info i;
            if (!GetFileInformationByHandle(h, out i))
                throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error(), path);
            return ((ulong)i.IndexHigh << 32) | i.IndexLow;
        }
    }
}
'@

function Invoke-Diskpart([string[]]$lines) {
    $script = Join-Path $WorkDir ('dp-' + [guid]::NewGuid().ToString('N') + '.txt')
    Set-Content -Path $script -Value $lines -Encoding ascii
    $o = & diskpart.exe /s $script 2>&1
    $code = $LASTEXITCODE
    Remove-Item $script -Force
    if ($code -ne 0) { $o | ForEach-Object { Write-Host "  diskpart: $_" }; throw "diskpart exit $code" }
}

# Creates a virtual disk with one NTFS partition and returns @{File; Disk; Offset; Size; Letter}.
function New-TestDisk([string]$name, [int]$mb, [string]$type, [string]$letter, [string]$unit = '') {
    $file = Join-Path $WorkDir $name
    if (Test-Path $file) { Remove-Item $file -Force }
    $fmt = 'format fs=ntfs quick label=OLDVOL'
    if ($unit) { $fmt += " unit=$unit" }
    Invoke-Diskpart @("create vdisk file=""$file"" maximum=$mb type=$type", "select vdisk file=""$file""",
        'attach vdisk', 'convert mbr', 'create partition primary', $fmt, "assign letter=$letter")
    $disk = (Get-DiskImage -ImagePath $file).Number
    $part = Get-Partition -DiskNumber $disk | Where-Object { $_.DriveLetter -eq $letter }
    return @{ File = $file; Disk = $disk; Offset = [UInt64]$part.Offset; Size = [UInt64](Get-Disk -Number $disk).Size; Letter = $letter }
}

function Remove-TestDisk($d) {
    try { Invoke-Diskpart @("select vdisk file=""$($d.File)""", 'detach vdisk') } catch { Write-Host "detach: $_" }
    Remove-Item $d.File -Force -ErrorAction SilentlyContinue
}

function Run-Analyzer([string[]]$a) {
    & $py $analyzer @a
    if ($LASTEXITCODE -ne 0) { throw "analyzer failed: $($a -join ' ')" }
}

function Raw-Path($d) { "\\.\PhysicalDrive$($d.Disk)" }
function Now-FileTime { [DateTime]::UtcNow.ToFileTimeUtc() }

# Deterministic content: a header naming the file plus bytes from a seeded generator.
function Write-TestFile([string]$path, [int]$size, [int]$seed) {
    $rng = New-Object System.Random $seed
    $bytes = New-Object byte[] $size
    $rng.NextBytes($bytes)
    $head = [Text.Encoding]::ASCII.GetBytes("HDSAVIOR-TEST $seed ")
    [Array]::Copy($head, $bytes, [Math]::Min($head.Length, $size))
    [IO.File]::WriteAllBytes($path, $bytes)
}

$facts = [ordered]@{}
$facts['windows'] = (Get-CimInstance Win32_OperatingSystem | Select-Object Caption, Version, BuildNumber)
$facts['trim_setting_before'] = (fsutil behavior query DisableDeleteNotify | Out-String).Trim()

# ---------------------------------------------------------------- 1. placement
Write-Host '== 1. MFT placement'
$placement = @()
foreach ($p in @(@{mb = 256}, @{mb = 1024}, @{mb = 4096}, @{mb = 16384}, @{mb = 65536}, @{mb = 16384; unit = '64K'})) {
    $unit = if ($p.ContainsKey('unit')) { $p.unit } else { '' }
    $d = New-TestDisk "place-$($p.mb)$unit.vhdx" $p.mb 'expandable' 'P' $unit
    try {
        $f = Join-Path $Out "placement-$($p.mb)$unit.json"
        Run-Analyzer @('placement', '--raw', (Raw-Path $d), '--size', "$($d.Size)", '--partition-offset', "$($d.Offset)", '--out', $f)
        $r = Get-Content $f -Raw | ConvertFrom-Json
        $r | Add-Member -NotePropertyName size_mb -NotePropertyValue $p.mb
        $r | Add-Member -NotePropertyName unit -NotePropertyValue $unit
        $r | Add-Member -NotePropertyName partition_offset -NotePropertyValue $d.Offset
        $r | Add-Member -NotePropertyName fsutil -NotePropertyValue ((fsutil fsinfo ntfsinfo P: | Out-String).Trim())
        $placement += $r
    } finally { Remove-TestDisk $d }
}
$facts['placement'] = $placement

# ---------------------------------------------------------------- 2. TRIM
Write-Host '== 2. TRIM on quick format'
$marker = 'HDSAVIOR-TRIM-MARKER-0123456789ABCDEF'
$markerHex = -join ([Text.Encoding]::ASCII.GetBytes($marker) | ForEach-Object { $_.ToString('x2') })
$trim = @()
foreach ($t in @(@{type = 'fixed'; ext = 'vhd'}, @{type = 'expandable'; ext = 'vhdx'})) {
    foreach ($notify in @(0, 1)) {
        fsutil behavior set DisableDeleteNotify NTFS $notify | Out-Null
        $d = New-TestDisk "trim-$($t.ext)-$notify.$($t.ext)" 256 $t.type 'R'
        try {
            $block = [Text.Encoding]::ASCII.GetBytes($marker.PadRight(4096, '.'))
            $fs = [IO.File]::Open('R:\marker.bin', 'CreateNew', 'Write')
            for ($i = 0; $i -lt 256; $i++) { $fs.Write($block, 0, $block.Length) }
            $fs.Flush($true); $fs.Close()
            Write-VolumeCache R
            $before = Join-Path $Out "trim-$($t.ext)-$notify-before.json"
            Run-Analyzer @('find', '--raw', (Raw-Path $d), '--size', "$($d.Size)", '--marker', $markerHex, '--out', $before)
            Format-Volume -DriveLetter R -FileSystem NTFS -NewFileSystemLabel NEWVOL -Force -Confirm:$false | Out-Null
            Start-Sleep -Seconds 2
            $after = Join-Path $Out "trim-$($t.ext)-$notify-after.json"
            Run-Analyzer @('find', '--raw', (Raw-Path $d), '--size', "$($d.Size)", '--marker', $markerHex, '--out', $after)
            $trim += [ordered]@{ disk = $t.ext; disable_delete_notify = $notify
                before = (Get-Content $before -Raw | ConvertFrom-Json).marker_hits
                after = (Get-Content $after -Raw | ConvertFrom-Json).marker_hits }
        } finally { Remove-TestDisk $d }
    }
}
$facts['trim'] = $trim

# ---------------------------------------------------------------- 3. the accident
Write-Host '== 3. The accident'
fsutil behavior set DisableDeleteNotify NTFS 1 | Out-Null
$src = $null
$oth = $null
$entries = New-Object System.Collections.Generic.List[object]
$phaseOf = @{}
try {
    $src = New-TestDisk 'accident.vhd' 1024 'fixed' 'T'
    $oth = New-TestDisk 'other.vhdx' 256 'expandable' 'U'
    $seed = 1
    # Top-level folders first, so that they land in slots the format destroys.
    $tops = @('A_Photos', 'B_Documents', 'C_Music', 'D_Projects', 'E_Archive')
    foreach ($t in $tops) { New-Item -ItemType Directory -Path "T:\$t" | Out-Null }
    foreach ($t in $tops) {
        foreach ($s in 1..3) {
            $dir = "T:\$t\sub$s"
            New-Item -ItemType Directory -Path $dir | Out-Null
            foreach ($i in 1..60) {
                $size = @(180, 900, 3000, 48000, 210000)[$i % 5]
                Write-TestFile "$dir\file-$i.bin" $size $seed; $seed++
            }
        }
    }
    # A folder large enough for INDX blocks.
    New-Item -ItemType Directory -Path 'T:\B_Documents\Inbox' | Out-Null
    foreach ($i in 1..600) { Write-TestFile ("T:\B_Documents\Inbox\message-{0:D4}.eml" -f $i) 700 $seed; $seed++ }
    # A file with thousands of runs: sparse, every other cluster written.
    $many = 'T:\D_Projects\many-runs.bin'
    New-Item -ItemType File -Path $many | Out-Null
    fsutil sparse setflag $many | Out-Null
    $fs = [IO.File]::Open($many, 'Open', 'Write')
    $chunk = New-Object byte[] 4096
    (New-Object System.Random 4242).NextBytes($chunk)
    for ($i = 0; $i -lt 3000; $i++) { $fs.Position = [int64]$i * 8192; $fs.Write($chunk, 0, 4096) }
    $fs.Flush($true); $fs.Close()
    $phaseOf[$many] = 'many_runs'
    # A large contiguous file.
    Write-TestFile 'T:\E_Archive\big.bin' (20MB) 777
    # Slot reuse: delete files created early, then create new ones.
    $victims = 1..10 | ForEach-Object { "T:\C_Music\sub2\file-$_.bin" }
    foreach ($v in $victims) {
        $entries.Add([ordered]@{ path = $v; frn = [HdsFileId]::Get($v); phase = 'deleted'; directory = $false })
        Remove-Item $v
    }
    foreach ($i in 1..10) { $f = "T:\E_Archive\reuse-$i.bin"; Write-TestFile $f 500 (9000 + $i); $phaseOf[$f] = 'reuse' }
    # Files from another volume, with old timestamps at the source.
    $old = [DateTime]::new(2015, 6, 1, 12, 0, 0, [DateTimeKind]::Utc)
    foreach ($kind in @('robo', 'move', 'copy')) {
        New-Item -ItemType Directory -Path "U:\src-$kind" | Out-Null
        foreach ($i in 1..20) {
            $f = "U:\src-$kind\doc-$i.bin"
            Write-TestFile $f 5000 (5000 + $i)
            $item = Get-Item $f
            $item.CreationTimeUtc = $old; $item.LastWriteTimeUtc = $old.AddDays($i); $item.LastAccessTimeUtc = $old.AddDays(30)
        }
    }
    $sourceTimes = @{}
    Get-ChildItem U:\ -Recurse -File | ForEach-Object { $sourceTimes[$_.Name + '|' + $_.Directory.Name] = @($_.CreationTimeUtc.ToFileTimeUtc(), $_.LastWriteTimeUtc.ToFileTimeUtc()) }
    $copyStarted = Now-FileTime
    robocopy U:\src-robo T:\Robo /E /COPY:DAT /DCOPY:DAT /NJH /NJS /NP | Out-Null
    if ($LASTEXITCODE -ge 8) { throw "robocopy failed: $LASTEXITCODE" }
    New-Item -ItemType Directory -Path 'T:\Moved' | Out-Null
    Get-ChildItem U:\src-move -File | Move-Item -Destination 'T:\Moved'
    Copy-Item U:\src-copy T:\Copied -Recurse
    Get-ChildItem T:\Robo, T:\Moved, T:\Copied -File | ForEach-Object { $phaseOf[$_.FullName] = @{ 'Robo' = 'robocopy'; 'Moved' = 'moved'; 'Copied' = 'copyitem' }[$_.Directory.Name] }

    # Ground truth for everything on the old volume.
    Get-ChildItem T:\ -Recurse -Force -ErrorAction SilentlyContinue | Where-Object { $_.FullName -notlike 'T:\System Volume Information*' } | ForEach-Object {
        $isDir = $_.PSIsContainer
        $e = [ordered]@{ path = $_.FullName; frn = [HdsFileId]::Get($_.FullName); directory = $isDir
            phase = $(if ($phaseOf.ContainsKey($_.FullName)) { $phaseOf[$_.FullName] } else { 'old' })
            size = $(if ($isDir) { $null } else { $_.Length })
            sha256 = $(if ($isDir) { $null } else { (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower() })
            created = $_.CreationTimeUtc.ToFileTimeUtc(); modified = $_.LastWriteTimeUtc.ToFileTimeUtc() }
        if ($e.phase -in @('robocopy', 'moved', 'copyitem')) {
            $key = $_.Name + '|' + @{ 'Robo' = 'src-robo'; 'Moved' = 'src-move'; 'Copied' = 'src-copy' }[$_.Directory.Name]
            $e['copy_started'] = $copyStarted
            if ($sourceTimes.ContainsKey($key)) { $e['source_created'] = $sourceTimes[$key][0]; $e['source_modified'] = $sourceTimes[$key][1] }
        }
        $entries.Add($e)
    }
    Write-VolumeCache T
    Start-Sleep -Seconds 2
    Write-VolumeCache T
    Run-Analyzer @('pre', '--raw', (Raw-Path $src), '--size', "$($src.Size)", '--partition-offset', "$($src.Offset)", '--out', (Join-Path $Out 'pre.json'))
    $lastOldWrite = Now-FileTime
    $facts['old_volume_fsutil'] = (fsutil fsinfo ntfsinfo T: | Out-String).Trim()
    Start-Sleep -Seconds 2   # a later whole second than the last write

    # The accident: quick format, as Explorer and Disk Management do it.
    Format-Volume -DriveLetter T -FileSystem NTFS -NewFileSystemLabel NEWVOL -Force -Confirm:$false | Out-Null
    $formatTime = Now-FileTime
    Start-Sleep -Seconds 3
    $facts['new_volume_fsutil_after_mount'] = (fsutil fsinfo ntfsinfo T: | Out-String).Trim()

    # Files written after the accident (one deleted again).
    New-Item -ItemType Directory -Path 'T:\NewFolder' | Out-Null
    Write-TestFile 'T:\NewFolder\a.txt' 2000 1
    Write-TestFile 'T:\NewFolder\b.txt' 2000 2
    Write-TestFile 'T:\new-1.bin' (5MB) 3
    foreach ($f in @('T:\NewFolder', 'T:\NewFolder\a.txt', 'T:\NewFolder\b.txt', 'T:\new-1.bin')) {
        $entries.Add([ordered]@{ path = $f; frn = [HdsFileId]::Get($f); phase = $(if ($f -like '*b.txt') { 'new_deleted' } else { 'new' }); directory = (Test-Path $f -PathType Container) })
    }
    Remove-Item 'T:\NewFolder\b.txt'
    Write-VolumeCache T
    $facts['new_volume_fsutil_end'] = (fsutil fsinfo ntfsinfo T: | Out-String).Trim()

    $truth = [ordered]@{ entries = $entries; format_time = $formatTime; last_old_write = $lastOldWrite }
    $truthFile = Join-Path $Out 'truth.json'
    $truth | ConvertTo-Json -Depth 6 -Compress | Set-Content -Path $truthFile -Encoding utf8
    # Analyse the disk while still attached (raw, read-only), after the cache was flushed.
    Run-Analyzer @('post', '--raw', (Raw-Path $src), '--size', "$($src.Size)", '--partition-offset', "$($src.Offset)",
        '--pre', (Join-Path $Out 'pre.json'), '--truth', $truthFile, '--out', (Join-Path $Out 'accident.json'))
    $facts['accident'] = Get-Content (Join-Path $Out 'accident.json') -Raw | ConvertFrom-Json
}
finally {
    if ($src) { Remove-TestDisk $src }
    if ($oth) { Remove-TestDisk $oth }
    fsutil behavior set DisableDeleteNotify NTFS 0 | Out-Null
}

$facts | ConvertTo-Json -Depth 8 | Set-Content -Path (Join-Path $Out 'facts.json') -Encoding utf8
Write-Host '== summary'
Write-Host ($facts | ConvertTo-Json -Depth 4)
