<#
.SYNOPSIS
  Helper for the real-hardware test in docs/hardware-test.md.

.DESCRIPTION
  Fill        Writes known files to the test volume and records their SHA-256
              in a manifest kept on ANOTHER drive.
  HashDisk    Reads a whole physical disk (read-only) and stores one SHA-256
              per MiB, to prove later that nothing was written to it.
  CompareDisk Compares two HashDisk results and lists the MiB that differ.
  Compare     Compares the recovered files with the manifest and summarises
              the HDSavior report.

.EXAMPLE
  .\hardware-test.ps1 Fill -Drive E -Manifest D:\hds-test\manifest.json -CopyFrom "$env:USERPROFILE\Pictures\Holiday"
  .\hardware-test.ps1 HashDisk -Disk 3 -Out D:\hds-test\before.txt
  .\hardware-test.ps1 CompareDisk -Before D:\hds-test\before.txt -After D:\hds-test\after.txt
  .\hardware-test.ps1 Compare -Manifest D:\hds-test\manifest.json -Recovered D:\hds-test\recovered
#>
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [ValidateSet('Fill', 'HashDisk', 'CompareDisk', 'Compare')]
    [string]$Command,
    [string]$Drive,
    [string]$Manifest,
    [string]$CopyFrom,
    [int]$Disk = -1,
    [string]$Out,
    [string]$Before,
    [string]$After,
    [string]$Recovered
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Sha([string]$path) { (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash }

function Write-TestFile([string]$path, [int]$size, [int]$seed) {
    $rng = New-Object System.Random $seed
    $bytes = New-Object byte[] $size
    $rng.NextBytes($bytes)
    $head = [Text.Encoding]::ASCII.GetBytes("HDSAVIOR-HW $seed ")
    [Array]::Copy($head, $bytes, [Math]::Min($head.Length, $size))
    [IO.File]::WriteAllBytes($path, $bytes)
}

function Drive-Of([string]$path) { ([IO.Path]::GetPathRoot([IO.Path]::GetFullPath($path))).TrimEnd('\').ToUpperInvariant() }

switch ($Command) {
'Fill' {
    if (-not $Drive -or -not $Manifest) { throw 'Fill needs -Drive and -Manifest' }
    $Drive = $Drive.TrimEnd(':', '\').ToUpperInvariant()
    $root = "${Drive}:\"
    if ("${Drive}:" -eq $env:SystemDrive.ToUpperInvariant()) { throw "refusing to fill the Windows drive $root" }
    if ((Drive-Of $Manifest) -eq "${Drive}:") { throw 'the manifest must be on another drive than the test volume' }
    $vol = Get-Volume -DriveLetter $Drive
    if ($vol.FileSystem -ne 'NTFS') { throw "$root is $($vol.FileSystem), not NTFS" }
    Write-Host "Filling $root ($($vol.FileSystemLabel), $([Math]::Round($vol.Size / 1GB, 1)) GB)"

    # The first ~250 MFT records are rewritten by a quick format, so the files
    # written first are lost by design. These uncounted files take those slots,
    # as on any disk in use.
    New-Item -ItemType Directory -Force -Path "${root}early" | Out-Null
    for ($i = 0; $i -lt 300; $i++) { Write-TestFile "${root}early\e$i.txt" 64 (900000 + $i) }

    $files = [ordered]@{}
    $seed = 1
    foreach ($top in @('Documents', 'Photos', 'Music')) {
        foreach ($sub in @('', 'Archive', 'Archive\2024')) {
            $dir = if ($sub) { "$root$top\$sub" } else { "$root$top" }
            New-Item -ItemType Directory -Force -Path $dir | Out-Null
            for ($i = 0; $i -lt 30; $i++) {
                $size = if ($i % 3 -eq 0) { 120 + ($seed * 7) % 500 } else { 4096 + ($seed * 7919) % (3 * 1024 * 1024) }
                $rel = $dir.Substring(3) + "\file-$seed.bin"
                Write-TestFile "$root$rel" $size $seed
                $files[$rel] = Sha "$root$rel"
                $seed++
            }
        }
    }
    # Four files that grow in turns, so they end up fragmented.
    New-Item -ItemType Directory -Force -Path "${root}Video" | Out-Null
    $grow = @('Video\big-fragmented.bin', 'Video\grow-1.bin', 'Video\grow-2.bin', 'Video\grow-3.bin')
    $streams = @($grow | ForEach-Object { [IO.File]::Open("$root$_", 'CreateNew', 'Write') })
    $rng = New-Object System.Random 424242
    $chunk = New-Object byte[] (64 * 1024)
    for ($round = 0; $round -lt 160; $round++) {
        foreach ($fs in $streams) { $rng.NextBytes($chunk); $fs.Write($chunk, 0, $chunk.Length); $fs.Flush($true) }
    }
    $streams | ForEach-Object { $_.Close() }
    foreach ($rel in $grow) { $files[$rel] = Sha "$root$rel" }
    # ZIP files, so the disk layout can be confirmed by signatures.
    for ($i = 0; $i -lt 6; $i++) {
        $rel = "Documents\bundle-$i.zip"
        Compress-Archive -LiteralPath "${root}Documents\file-$($i * 3 + 2).bin" -DestinationPath "$root$rel"
        $files[$rel] = Sha "$root$rel"
    }
    # A non-ASCII name.
    $uni = 'Documents\' + (-join ([char[]](0x00F1, 0x0061, 0x006E, 0x0064, 0x00FA, 0x002D, 0x017C, 0x00F3, 0x0142, 0x0077))) + '.txt'
    Write-TestFile "$root$uni" 5000 777
    $files[$uni] = Sha "$root$uni"
    # Optional: your own real files (photos, documents, music).
    if ($CopyFrom) {
        $src = (Resolve-Path -LiteralPath $CopyFrom).Path.TrimEnd('\')
        $target = "${root}Real"
        Copy-Item -LiteralPath $src -Destination $target -Recurse
        Get-ChildItem -LiteralPath $target -Recurse -File | ForEach-Object {
            $files[$_.FullName.Substring(3)] = Sha $_.FullName
        }
    }
    Write-VolumeCache $Drive
    $frag = (fsutil file queryextents "${root}Video\big-fragmented.bin" | Out-String)
    $extents = ([regex]::Matches($frag, 'VCN:')).Count
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent ([IO.Path]::GetFullPath($Manifest))) | Out-Null
    [ordered]@{ volume = "${Drive}:"; label = $vol.FileSystemLabel; written_utc = [DateTime]::UtcNow.ToString('o'); files = $files } |
        ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $Manifest -Encoding utf8
    Write-Host "Wrote $($files.Count) counted files (plus 300 uncounted early ones)."
    Write-Host "The fragmented test file has $extents extents$(if ($extents -lt 2) { ' (NOT fragmented; the test still works)' })."
    Write-Host "Manifest: $Manifest"
}
'HashDisk' {
    if ($Disk -lt 0 -or -not $Out) { throw 'HashDisk needs -Disk N and -Out file' }
    $d = Get-Disk -Number $Disk
    if ($d.IsBoot -or $d.IsSystem) { throw "disk $Disk holds Windows; this test needs a separate disk" }
    if ((Drive-Of $Out) -in @(Get-Partition -DiskNumber $Disk | Where-Object DriveLetter | ForEach-Object { "$($_.DriveLetter):" })) {
        throw 'the output file must not be on the disk being hashed'
    }
    Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Security.Cryptography;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;
public static class HdsHwHash {
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern SafeFileHandle CreateFileW(string name, uint access, uint share, IntPtr sa,
                                             uint disposition, uint flags, IntPtr template);
    public static void Hash(string device, long length, string outPath) {
        // GENERIC_READ only: this never writes to the disk.
        using (var h = CreateFileW(device, 0x80000000, 3, IntPtr.Zero, 3, 0, IntPtr.Zero)) {
            if (h.IsInvalid) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error(), device);
            using (var fs = new FileStream(h, FileAccess.Read, 1 << 20))
            using (var sha = SHA256.Create())
            using (var w = new StreamWriter(outPath)) {
                var buf = new byte[1 << 20];
                long left = length, done = 0;
                var t = DateTime.UtcNow;
                while (left > 0) {
                    int want = (int)Math.Min(buf.Length, left), got = 0;
                    while (got < want) {
                        int n = fs.Read(buf, got, want - got);
                        if (n <= 0) throw new IOException("short read at " + (done + got));
                        got += n;
                    }
                    w.WriteLine(BitConverter.ToString(sha.ComputeHash(buf, 0, got)).Replace("-", ""));
                    left -= got; done += got;
                    if ((DateTime.UtcNow - t).TotalSeconds > 10) {
                        Console.WriteLine(string.Format("  {0:F1} %", 100.0 * done / length)); t = DateTime.UtcNow;
                    }
                }
            }
        }
    }
}
'@
    Write-Host "Hashing disk $Disk ($($d.FriendlyName), $([Math]::Round($d.Size / 1GB, 1)) GB), read-only..."
    [HdsHwHash]::Hash("\\.\PhysicalDrive$Disk", [Int64]$d.Size, [IO.Path]::GetFullPath($Out))
    Write-Host "Done: $Out"
}
'CompareDisk' {
    if (-not $Before -or -not $After) { throw 'CompareDisk needs -Before and -After' }
    $a = Get-Content -LiteralPath $Before
    $b = Get-Content -LiteralPath $After
    $diff = @()
    for ($i = 0; $i -lt [Math]::Max($a.Count, $b.Count); $i++) { if ($a[$i] -ne $b[$i]) { $diff += $i } }
    if ($a.Count -ne $b.Count) { Write-Host "Different lengths: $($a.Count) and $($b.Count) MiB" -ForegroundColor Red }
    if ($diff.Count -eq 0) { Write-Host "IDENTICAL: all $($a.Count) MiB match." -ForegroundColor Green }
    else {
        Write-Host "CHANGED: $($diff.Count) MiB differ. First ones: $(($diff | Select-Object -First 40) -join ', ')" -ForegroundColor Red
        Write-Host 'Please send this output: if nothing but HDSavior touched the disk between the two hashes, this is a bug.'
        exit 1
    }
}
'Compare' {
    if (-not $Manifest -or -not $Recovered) { throw 'Compare needs -Manifest and -Recovered' }
    $m = Get-Content -LiteralPath $Manifest -Raw | ConvertFrom-Json
    $byName = @{}
    Get-ChildItem -LiteralPath $Recovered -Recurse -File | ForEach-Object {
        $byName[$_.Name.ToLowerInvariant()] += @($_.FullName)
    }
    $n = 0; $exact = 0; $samePath = 0
    $missing = New-Object System.Collections.Generic.List[string]
    $wrong = New-Object System.Collections.Generic.List[string]
    foreach ($p in $m.files.PSObject.Properties) {
        $n++
        $rel = $p.Name; $want = $p.Value
        $cands = @($byName[(Split-Path $rel -Leaf).ToLowerInvariant()] | Where-Object { $_ })
        if ($cands.Count -eq 0) { $missing.Add($rel); continue }
        $match = $cands | Where-Object { (Sha $_) -eq $want } | Select-Object -First 1
        if ($match) {
            $exact++
            if ($match.ToLowerInvariant().EndsWith('\' + $rel.ToLowerInvariant())) { $samePath++ }
        } else { $wrong.Add($rel) }
    }
    Write-Host ''
    Write-Host "Counted files:            $n"
    Write-Host "Recovered byte-identical: $exact"
    Write-Host "  ...under the old path:  $samePath"
    Write-Host "Missing:                  $($missing.Count)"
    Write-Host "Present but different:    $($wrong.Count)"
    $missing | Select-Object -First 25 | ForEach-Object { Write-Host "  missing: $_" }
    $wrong | Select-Object -First 25 | ForEach-Object { Write-Host "  different: $_" -ForegroundColor Yellow }
    $reports = @(Get-ChildItem -LiteralPath $Recovered -Recurse -Filter 'HDSavior-report-*.csv')
    foreach ($r in $reports) {
        $rows = Import-Csv -LiteralPath $r.FullName
        Write-Host ''
        Write-Host "Report $($r.Name): $($rows.Count) rows"
        $rows | Group-Object status | Sort-Object Count -Descending | ForEach-Object { Write-Host ("  {0,6}  {1}" -f $_.Count, $_.Name) }
        foreach ($rel in $wrong) {
            $leaf = '\' + (Split-Path $rel -Leaf)
            $bad = @($rows | Where-Object { $_.status -eq 'ok' -and "$($_.output_path)".EndsWith($leaf, 'OrdinalIgnoreCase') })
            foreach ($row in $bad) { Write-Host "  BUG? rated ok but different: $($row.output_path)" -ForegroundColor Red }
        }
    }
    if ($n -gt 0 -and $exact -eq $n) { Write-Host "`nAll counted files came back intact." -ForegroundColor Green }
}
}
