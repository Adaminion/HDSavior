<#
.SYNOPSIS
  Real-Windows checks of hds-io that wine cannot prove (run by CI, elevated).

.DESCRIPTION
  Creates two small virtual disks with diskpart:
    source disk  - MBR, one NTFS partition (drive letter T), also mounted as a
                   folder and reachable through a junction, a directory symlink
                   and a SUBST drive;
    other disk   - MBR, an exFAT partition (U) and a FAT32 partition (V).
  Then runs the winprobe example against them and fails on the first unmet
  expectation. Never touches any disk it did not create itself.

.PARAMETER Probe
  Path of winprobe.exe (cargo build -p hds-io --example winprobe --release).
#>
param(
    [Parameter(Mandatory = $true)][string]$Probe,
    [string]$WorkDir = $env:RUNNER_TEMP
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$failures = New-Object System.Collections.Generic.List[string]
function Fail([string]$what) { Write-Host "FAILED  $what"; $failures.Add($what) }
function Pass([string]$what) { Write-Host "ok      $what" }

function Invoke-Diskpart([string[]]$lines) {
    $script = Join-Path $WorkDir ("diskpart-" + [guid]::NewGuid().ToString('N') + '.txt')
    Set-Content -Path $script -Value $lines -Encoding ascii
    $out = & diskpart.exe /s $script 2>&1
    $code = $LASTEXITCODE
    Remove-Item $script -Force
    $out | ForEach-Object { Write-Host "  diskpart: $_" }
    if ($code -ne 0) { throw "diskpart failed with exit code $code" }
}

function Run-Probe([string[]]$probeArgs) {
    $out = & $Probe @probeArgs 2>&1
    return @{ Code = $LASTEXITCODE; Text = ($out | Out-String) }
}

function Expect-Verdict([string]$label, [string]$path, [int]$disk, [UInt64]$start, [UInt64]$end,
                        [string]$purpose, [string]$verdict) {
    $r = Run-Probe @('dest', $path, "$disk", "$start", "$end", $purpose, '--expect', $verdict)
    if ($r.Code -eq 0) { Pass "$label -> $verdict" }
    else { Fail "$label -> expected $verdict, got: $($r.Text.Trim())" }
}

$src = Join-Path $WorkDir 'hds-source.vhd'
$oth = Join-Path $WorkDir 'hds-other.vhd'
$mountFolder = 'C:\hds-ci-mounted'
$junction = 'C:\hds-ci-junction'
$dirLink = 'C:\hds-ci-dirlink'
$substDrive = 'S:'

try {
    foreach ($f in @($src, $oth)) { if (Test-Path $f) { Remove-Item $f -Force } }

    Write-Host '== creating virtual disks'
    Invoke-Diskpart @(
        "create vdisk file=""$src"" maximum=256 type=expandable",
        "select vdisk file=""$src""", 'attach vdisk', 'convert mbr',
        'create partition primary', 'format fs=ntfs quick label=HDSSRC', 'assign letter=T')
    Invoke-Diskpart @(
        "create vdisk file=""$oth"" maximum=256 type=expandable",
        "select vdisk file=""$oth""", 'attach vdisk', 'convert mbr',
        'create partition primary size=100', 'format fs=exfat quick label=HDSEXFAT', 'assign letter=U',
        'create partition primary size=100', 'format fs=fat32 quick label=HDSFAT32', 'assign letter=V')

    $srcDisk = (Get-DiskImage -ImagePath $src).Number
    $othDisk = (Get-DiskImage -ImagePath $oth).Number
    $srcSize = (Get-Disk -Number $srcDisk).Size
    $part = Get-Partition -DiskNumber $srcDisk | Where-Object { $_.DriveLetter -eq 'T' }
    $hostLetter = (Split-Path -Qualifier $src).TrimEnd(':')
    $hostDisk = (Get-Partition -DriveLetter $hostLetter).DiskNumber
    $hostSize = (Get-Disk -Number $hostDisk).Size
    $sysLetter = $env:SystemDrive.TrimEnd(':')
    $sysPart = Get-Partition -DriveLetter $sysLetter
    $sysDisk = $sysPart.DiskNumber
    Write-Host "source disk $srcDisk ($srcSize bytes, partition at $($part.Offset), size $($part.Size)); other disk $othDisk; VHD files on disk $hostDisk; system volume on disk $sysDisk at $($sysPart.Offset)"

    New-Item -ItemType Directory -Force -Path $mountFolder | Out-Null
    Add-PartitionAccessPath -DiskNumber $srcDisk -PartitionNumber $part.PartitionNumber -AccessPath "$mountFolder\"
    cmd /c mklink /J $junction 'T:\' | Out-Null
    cmd /c mklink /D $dirLink 'T:\' | Out-Null
    subst $substDrive 'T:\'

    Write-Host '== winprobe disks'
    $r = Run-Probe @('disks')
    if ($r.Code -ne 0) { Fail "disks exited $($r.Code)" }
    else {
        $disks = $r.Text | ConvertFrom-Json
        $d = @($disks | Where-Object { $_.number -eq $srcDisk })
        if ($d.Count -ne 1) { Fail "disks: source disk $srcDisk not listed exactly once" }
        else {
            $d = $d[0]
            if ($d.size -eq $srcSize) { Pass 'disks: size matches Get-Disk' } else { Fail "disks: size $($d.size) <> $srcSize" }
            if (@($d.flags) -contains 'virtual') { Pass 'disks: flag virtual' } else { Fail "disks: flags $($d.flags -join ',') lack virtual" }
            if (-not $d.is_system) { Pass 'disks: not the system disk' } else { Fail 'disks: VHD reported as system disk' }
            if ($d.table -eq 'mbr') { Pass 'disks: table mbr' } else { Fail "disks: table $($d.table)" }
            $p = @($d.partitions | Where-Object { @($_.letters) -contains 'T:' })
            if ($p.Count -eq 1 -and $p[0].fs -eq 'ntfs' -and $p[0].label -eq 'HDSSRC' -and $p[0].offset -eq $part.Offset) {
                Pass 'disks: partition T: ntfs HDSSRC at the right offset'
            } else { Fail "disks: partition T: not as expected: $($d.partitions | ConvertTo-Json -Compress -Depth 5)" }
        }
        $sys = @($disks | Where-Object { $_.number -eq $sysDisk })
        if ($sys.Count -eq 1 -and $sys[0].is_system) { Pass 'disks: system disk flagged' } else { Fail 'disks: system disk not flagged' }
    }

    Write-Host '== winprobe read (raw disk, elevated)'
    $r = Run-Probe @('read', "$srcDisk")
    Write-Host $r.Text
    if ($r.Code -eq 0) { Pass "read PhysicalDrive${srcDisk}: first and last sector readable" } else { Fail "read exited $($r.Code)" }
    if ($r.Text -match ('"disk_number":\s*' + $srcDisk + '\b')) { Pass 'read: disk number reported by the device' } else { Fail 'read: disk number missing' }
    if ($r.Text -match '"mounted_volume"') { Pass 'read: mounted_volume warning' } else { Fail 'read: no mounted_volume warning for T:' }
    if ($r.Text -notmatch '"system_disk"') { Pass 'read: no system_disk warning for the VHD' } else { Fail 'read: system_disk warning on the VHD' }

    Write-Host '== destination guard'
    $pStart = [UInt64]$part.Offset; $pEnd = [UInt64]($part.Offset + $part.Size)
    Expect-Verdict 'folder on the scanned partition' 'T:\Rescued' $srcDisk $pStart $pEnd 'recover' 'overlaps_source'
    Expect-Verdict 'same disk, outside the range' 'T:\Rescued' $srcDisk 0 512 'recover' 'same_disk'
    Expect-Verdict 'another disk' 'U:\Rescued' $srcDisk 0 $srcSize 'recover' 'ok'
    Expect-Verdict 'mounted folder of the scanned partition' "$mountFolder\Rescued" $srcDisk $pStart $pEnd 'recover' 'overlaps_source'
    Expect-Verdict 'junction to the scanned partition' "$junction\Rescued" $srcDisk $pStart $pEnd 'recover' 'overlaps_source'
    Expect-Verdict 'directory symlink to the scanned partition' "$dirLink\Rescued" $srcDisk $pStart $pEnd 'recover' 'overlaps_source'
    Expect-Verdict 'SUBST drive of the scanned partition' "$substDrive\Rescued" $srcDisk $pStart $pEnd 'recover' 'overlaps_source'
    Expect-Verdict 'image file on the scanned partition' 'T:\disk.img' $srcDisk $pStart $pEnd 'image' 'overlaps_source'
    Expect-Verdict 'VHD volume when its host disk is the source' 'T:\Rescued' $hostDisk 0 $hostSize 'recover' 'overlaps_source'
    Expect-Verdict 'relative path' 'Rescued' $srcDisk 0 512 'recover' 'invalid_path'
    Expect-Verdict 'unknown drive' 'Q:\Rescued' $srcDisk 0 512 'recover' 'not_found'
    Expect-Verdict 'session on the system volume, system disk as source' "$($env:SystemDrive)\hds-ci\s.hdsc" $sysDisk 0 512 'session' 'ok'
    Expect-Verdict 'recover folder there needs consent' "$($env:SystemDrive)\hds-ci\out" $sysDisk 0 512 'recover' 'same_disk'
    # Administrative shares of this machine lead back to local volumes.
    $r = Run-Probe @('dest', "\\localhost\$sysLetter`$\hds-ci", "$sysDisk", '0', '512', 'recover')
    Write-Host "  \\localhost share: $($r.Text.Trim() -replace '\s+', ' ')"
    if ($r.Text -match '"verdict":\s*"(unknown_disk|not_found)"') { Pass '\\localhost share is never simply ok' } else { Fail '\\localhost share judged ok' }

    Write-Host '== output primitives'
    foreach ($dir in @('T:\', 'U:\', 'V:\')) {
        $r = Run-Probe @('out', $dir)
        Write-Host $r.Text
        if ($r.Code -eq 0) { Pass "out on $dir" } else { Fail "out on $dir exited $($r.Code)" }
    }
}
catch {
    Fail "script error: $_"
}
finally {
    Write-Host '== cleanup'
    try { subst $substDrive /D 2>$null } catch {}
    foreach ($l in @($junction, $dirLink)) { if (Test-Path $l) { cmd /c rmdir $l | Out-Null } }
    foreach ($f in @($src, $oth)) {
        if (Test-Path $f) {
            try { Invoke-Diskpart @("select vdisk file=""$f""", 'detach vdisk') } catch { Write-Host "detach failed: $_" }
            Remove-Item $f -Force -ErrorAction SilentlyContinue
        }
    }
    if (Test-Path $mountFolder) { Remove-Item $mountFolder -Force -Recurse -ErrorAction SilentlyContinue }
}

if ($failures.Count -gt 0) {
    Write-Host "`n$($failures.Count) check(s) failed:"
    $failures | ForEach-Object { Write-Host "  - $_" }
    exit 1
}
Write-Host "`nall checks passed"
