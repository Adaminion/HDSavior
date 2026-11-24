<#
.SYNOPSIS
  End-to-end rescue of a volume that Windows itself quick-formatted (CI, elevated).

.DESCRIPTION
  ARCHITECTURE.md 7.4. On a fixed VHD (a disk without TRIM, like a hard disk):
   1. Windows formats an NTFS volume and the script fills it: nested folders,
      small (resident) and large files, a fragmented file, a non-ASCII name.
      Each file's SHA-256 is recorded.
   2. Windows quick-formats the volume again (the accident), and the volume's
      drive letter is removed.
   3. The SHA-256 of the whole source disk is taken through \\.\PhysicalDriveN.
   4. hdsavior-engine scans \\.\PhysicalDriveN (partition 1) and recovers
      everything to a second virtual disk; then the source disk is hashed
      again.
   5. Checks:
       - the source disk is byte-identical before and after (the hard rule);
       - at least MinRecovered of the files come back byte-identical, under
         their old folder path;
       - the fragmented and the non-ASCII files come back byte-identical;
       - no recovered file with an old name has wrong content while the
         report rates it ok.
  The engine's output, the report and a summary go to <Out>.
#>
param(
    [Parameter(Mandatory = $true)][string]$Engine,
    [string]$WorkDir = $env:RUNNER_TEMP,
    [string]$Out = (Join-Path $env:RUNNER_TEMP 'roundtrip'),
    [double]$MinRecovered = 0.95
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
New-Item -ItemType Directory -Force -Path $Out | Out-Null
$Engine = (Resolve-Path $Engine).Path

$failures = New-Object System.Collections.Generic.List[string]
function Fail([string]$what) { Write-Host "FAILED  $what"; $failures.Add($what) }
function Pass([string]$what) { Write-Host "ok      $what" }

Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Security.Cryptography;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;
public static class HdsRawHash {
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern SafeFileHandle CreateFileW(string name, uint access, uint share, IntPtr sa,
                                             uint disposition, uint flags, IntPtr template);
    // SHA-256 of the first `length` bytes of a device, read-only, in 1 MiB steps.
    public static string Hash(string device, long length) {
        // GENERIC_READ, share read|write, OPEN_EXISTING.
        using (var h = CreateFileW(device, 0x80000000, 3, IntPtr.Zero, 3, 0, IntPtr.Zero)) {
            if (h.IsInvalid) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error(), device);
            using (var fs = new FileStream(h, FileAccess.Read, 1 << 20))
            using (var sha = SHA256.Create()) {
                var buf = new byte[1 << 20];
                long left = length;
                while (left > 0) {
                    int want = (int)Math.Min(buf.Length, left);
                    int got = fs.Read(buf, 0, want);
                    if (got <= 0) throw new IOException("short read at " + (length - left));
                    sha.TransformBlock(buf, 0, got, null, 0);
                    left -= got;
                }
                sha.TransformFinalBlock(buf, 0, 0);
                return BitConverter.ToString(sha.Hash).Replace("-", "");
            }
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
    $o | ForEach-Object { Write-Host "  diskpart: $_" }
    if ($code -ne 0) { throw "diskpart exit $code" }
}

function New-TestDisk([string]$name, [int]$mb, [string]$letter) {
    $file = Join-Path $WorkDir $name
    if (Test-Path $file) { Remove-Item $file -Force }
    Invoke-Diskpart @("create vdisk file=""$file"" maximum=$mb type=fixed", "select vdisk file=""$file""",
        'attach vdisk', 'convert mbr', 'create partition primary', 'format fs=ntfs quick label=OLDVOL',
        "assign letter=$letter")
    $disk = (Get-DiskImage -ImagePath $file).Number
    return @{ File = $file; Disk = $disk; Size = [Int64](Get-Disk -Number $disk).Size; Letter = $letter }
}

function Remove-TestDisk($d) {
    if (-not $d) { return }
    try { Invoke-Diskpart @("select vdisk file=""$($d.File)""", 'detach vdisk') } catch { Write-Host "detach: $_" }
    Remove-Item $d.File -Force -ErrorAction SilentlyContinue
}

# Deterministic content: a header naming the file, then seeded random bytes.
function Write-TestFile([string]$path, [int]$size, [int]$seed) {
    $rng = New-Object System.Random $seed
    $bytes = New-Object byte[] $size
    $rng.NextBytes($bytes)
    $head = [Text.Encoding]::ASCII.GetBytes("HDSAVIOR-RT $seed ")
    [Array]::Copy($head, $bytes, [Math]::Min($head.Length, $size))
    [IO.File]::WriteAllBytes($path, $bytes)
}

function Sha([string]$path) { (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash }

function Run-Engine([string]$label, [string[]]$a) {
    $log = Join-Path $Out "$label.jsonl"
    $err = Join-Path $Out "$label.stderr.txt"
    Write-Host "> hdsavior-engine $($a -join ' ')"
    $p = Start-Process -FilePath $Engine -ArgumentList $a -NoNewWindow -Wait -PassThru `
        -RedirectStandardOutput $log -RedirectStandardError $err
    Get-Content $err -Tail 15 | ForEach-Object { Write-Host "  $_" }
    Get-Content $log -Tail 3 | ForEach-Object { Write-Host "  $_" }
    return $p.ExitCode
}

$deleteNotify = (fsutil behavior query DisableDeleteNotify | Out-String).Trim()
Write-Host "DisableDeleteNotify before: $deleteNotify"
fsutil behavior set DisableDeleteNotify NTFS 1 | Out-Null

$src = $null
$dst = $null
try {
    $src = New-TestDisk 'hds-rt-source.vhd' 1024 'Q'
    $dst = New-TestDisk 'hds-rt-dest.vhd' 2048 'W'
    Write-Host "source: disk $($src.Disk), $($src.Size) bytes; destination: disk $($dst.Disk)"

    # ------------------------------------------------------------ 1. populate
    $files = [ordered]@{}   # relative path -> sha256
    $seed = 1
    foreach ($top in @('Documents', 'Photos', 'Music')) {
        foreach ($sub in @('', 'Archive', 'Archive\2024')) {
            $dir = if ($sub) { "Q:\$top\$sub" } else { "Q:\$top" }
            New-Item -ItemType Directory -Force -Path $dir | Out-Null
            for ($i = 0; $i -lt 30; $i++) {
                # A third are small enough to be resident in the MFT record.
                $size = if ($i % 3 -eq 0) { 120 + ($seed * 7) % 500 } else { 4096 + ($seed * 7919) % (3 * 1024 * 1024) }
                $rel = ($dir.Substring(3) + "\file-$seed.bin")
                Write-TestFile "Q:\$rel" $size $seed
                $files[$rel] = Sha "Q:\$rel"
                $seed++
            }
        }
    }
    # Fragmentation: fill with spacers, delete every other one, then write a
    # large file into the holes.
    New-Item -ItemType Directory -Force -Path 'Q:\spacers' | Out-Null
    for ($i = 0; $i -lt 200; $i++) { Write-TestFile "Q:\spacers\s$i.bin" (256 * 1024) (100000 + $i) }
    for ($i = 0; $i -lt 200; $i += 2) { Remove-Item "Q:\spacers\s$i.bin" -Force }
    $frag = 'Video\big-fragmented.bin'
    New-Item -ItemType Directory -Force -Path 'Q:\Video' | Out-Null
    Write-TestFile "Q:\$frag" (20 * 1024 * 1024) 424242
    $files[$frag] = Sha "Q:\$frag"
    for ($i = 1; $i -lt 200; $i += 2) {
        $rel = "spacers\s$i.bin"
        $files[$rel] = Sha "Q:\$rel"
    }
    # A non-ASCII name (Spanish and Polish letters), built from code points.
    $uni = 'Documents\' + (-join ([char[]](0x00F1, 0x0061, 0x006E, 0x0064, 0x00FA, 0x002D, 0x017C, 0x00F3, 0x0142, 0x0077))) + '.txt'
    Write-TestFile "Q:\$uni" 5000 777
    $files[$uni] = Sha "Q:\$uni"
    Write-VolumeCache Q
    $fragments = (fsutil file queryextents "Q:\$frag" | Measure-Object -Line).Lines
    Write-Host "populated $($files.Count) files; the fragmented file has $fragments extents"
    if ($fragments -lt 2) { Fail "the fragmented file was written in one piece ($fragments extents)" }

    # ------------------------------------------------------------ 2. the accident
    Format-Volume -DriveLetter Q -FileSystem NTFS -NewFileSystemLabel NEWVOL -Force -Confirm:$false | Out-Null
    Start-Sleep -Seconds 2
    $part = Get-Partition -DiskNumber $src.Disk | Where-Object { $_.DriveLetter -eq 'Q' }
    Remove-PartitionAccessPath -DiskNumber $src.Disk -PartitionNumber $part.PartitionNumber -AccessPath 'Q:\'
    Write-Host 'quick-formatted the source volume and removed its drive letter'

    # ------------------------------------------------------------ 3-4. rescue
    $raw = "\\.\PhysicalDrive$($src.Disk)"
    $before = [HdsRawHash]::Hash($raw, $src.Size)
    $session = 'W:\rescue\source.hds'
    $dest = 'W:\rescue\recovered'
    New-Item -ItemType Directory -Force -Path 'W:\rescue' | Out-Null

    $code = Run-Engine 'scan' @('scan', $raw, '--session', $session, '--kind', 'disk', '--partition', '1')
    if ($code -ne 0) { throw "scan exited $code" }
    Run-Engine 'info' @('info', '--session', $session) | Out-Null
    $code = Run-Engine 'recover' @('recover', '--session', $session, '--dest', $dest, '--all')
    if ($code -ne 0) { Fail "recover exited $code" }

    $after = [HdsRawHash]::Hash($raw, $src.Size)
    if ($before -eq $after) { Pass 'the source disk is byte-identical after scan and recover' }
    else { Fail "the source disk changed: $before -> $after" }

    # ------------------------------------------------------------ 5. compare
    $recovered = @{}   # lower-case relative path (without the volume folder) -> full path
    $byName = @{}
    Get-ChildItem -LiteralPath $dest -Recurse -File | ForEach-Object {
        $byName[$_.Name.ToLowerInvariant()] += @($_.FullName)
    }
    $exact = 0; $samePath = 0; $wrong = New-Object System.Collections.Generic.List[string]
    $missing = New-Object System.Collections.Generic.List[string]
    foreach ($rel in $files.Keys) {
        $name = (Split-Path $rel -Leaf).ToLowerInvariant()
        $cands = @($byName[$name] | Where-Object { $_ })
        if ($cands.Count -eq 0) { $missing.Add($rel); continue }
        $match = $cands | Where-Object { (Sha $_) -eq $files[$rel] } | Select-Object -First 1
        if ($match) {
            $exact++
            if ($match.ToLowerInvariant().EndsWith('\' + $rel.ToLowerInvariant())) { $samePath++ }
        } else {
            $wrong.Add($rel)
        }
    }
    $n = $files.Count
    $summary = [ordered]@{ files = $n; byte_identical = $exact; under_old_path = $samePath
        missing = $missing; wrong_content = $wrong; source_hash_before = $before; source_hash_after = $after }
    $summary | ConvertTo-Json -Depth 4 | Set-Content (Join-Path $Out 'summary.json')
    Write-Host "files: $n, byte-identical: $exact, under the old path: $samePath, missing: $($missing.Count), wrong content: $($wrong.Count)"
    $missing | Select-Object -First 20 | ForEach-Object { Write-Host "  missing: $_" }
    $wrong | Select-Object -First 20 | ForEach-Object { Write-Host "  wrong:   $_" }

    if ($exact -ge [Math]::Ceiling($MinRecovered * $n)) { Pass "$exact of $n files recovered byte-identical (minimum $MinRecovered)" }
    else { Fail "only $exact of $n files recovered byte-identical (minimum $MinRecovered)" }
    if ($samePath -ge [Math]::Ceiling($MinRecovered * $n)) { Pass "$samePath of $n under their old folder path" }
    else { Fail "only $samePath of $n under their old folder path" }
    foreach ($must in @($frag, $uni)) {
        if ($missing.Contains($must) -or $wrong.Contains($must)) { Fail "$must was not recovered byte-identical" }
        else { Pass "$must recovered byte-identical" }
    }

    # Wrong content must never be rated ok in the report.
    $report = Get-ChildItem -LiteralPath $dest -Filter 'HDSavior-report-*.csv' | Select-Object -First 1
    if (-not $report) { Fail 'no HDSavior-report-*.csv in the destination' }
    else {
        Copy-Item $report.FullName $Out
        $rows = Import-Csv $report.FullName
        Write-Host "report: $($rows.Count) rows; columns: $(($rows | Select-Object -First 1).PSObject.Properties.Name -join ', ')"
        $statuses = $rows | Group-Object status | ForEach-Object { "$($_.Name)=$($_.Count)" }
        Write-Host "report statuses: $($statuses -join ', ')"
        foreach ($rel in $wrong) {
            $leaf = '\' + (Split-Path $rel -Leaf)
            foreach ($row in @($rows | Where-Object { "$($_.output_path)".EndsWith($leaf, 'OrdinalIgnoreCase') })) {
                $path = if ([IO.Path]::IsPathRooted($row.output_path)) { $row.output_path } else { Join-Path $dest $row.output_path }
                if ($row.status -eq 'ok' -and (Test-Path -LiteralPath $path) -and (Sha $path) -ne $files[$rel]) {
                    Fail "$rel has wrong content but the report says ok ($($row.output_path))"
                }
            }
        }
    }
}
finally {
    Remove-TestDisk $src
    Remove-TestDisk $dst
}

if ($failures.Count -gt 0) {
    Write-Host "$($failures.Count) check(s) failed"
    exit 1
}
Write-Host 'round trip passed'
