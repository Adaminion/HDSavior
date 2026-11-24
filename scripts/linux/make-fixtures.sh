#!/usr/bin/env bash
#
# make-fixtures.sh - build the real-file-system test fixtures for HDSavior.
#
# Every fixture is an NTFS disk image that went through the accident HDSavior
# exists for, plus a JSON manifest that says what was on the old volume.
# Nothing here is synthetic NTFS: volumes are formatted by mkntfs, written
# through the ntfs-3g driver (FUSE mount of the image file) and "accidentally"
# quick-formatted by a second mkntfs -Q.  See README.md next to this script.
#
# Usage (as root):
#
#   make-fixtures.sh [--keep-pre] [--only NAME[,NAME...]] [--list] [OUTDIR]
#
#   OUTDIR       where images and manifests go (default: <repo>/fixtures-out,
#                which is git-ignored; images are never committed)
#   --keep-pre   also keep <scenario>.pre.img, the image right before the
#                accident (same layout as the final image), for debugging
#   --only       build only the named scenarios
#   --list       print the scenario names and exit
#
# Output per scenario:  OUTDIR/<name>.img  and  OUTDIR/<name>.manifest.json
#
# Needs: bash, python3 (3.8+) with Pillow, ntfs-3g (mkntfs, ntfs-3g, ntfsinfo),
# sfdisk, ffmpeg + ffprobe, coreutils, util-linux and /dev/fuse.  Optional: 7z
# and rar (two more sample types), blkid (one more cross-check).  On Ubuntu and
# on a GitHub-hosted ubuntu runner:
#
#   sudo apt-get update
#   sudo apt-get install -y ntfs-3g fuse3 fdisk ffmpeg python3-pil
#   sudo scripts/linux/make-fixtures.sh
#
# Scenarios (each: old volume created and populated -> ground truth captured
# through a read-only mount -> same volume re-read raw and cross-checked ->
# accident -> before/after comparison -> manifest):
#
#   basic           4 KiB clusters, the full tree; quick format, same parameters
#   fragmented      a file with $ATTRIBUTE_LIST + extension records, sparse
#                   files, a file with stale bytes behind its initialized size,
#                   a fragmented $MFT; quick format, same parameters
#   wiped           samples first, then the full tree; quick format, then old
#                   MFT records 0..255 overwritten as a Windows format would
#   cluster-change  old 4 KiB clusters, quick format with 64 KiB clusters
#   offset-mbr      NTFS partition at 1 MiB of an MBR whole-disk image
#   offset-mbr63    NTFS partition at sector 63 of an MBR whole-disk image (the
#                   Windows XP layout; nothing is 1 KiB-aligned in the image)
#   offset-gpt      NTFS partition at 16 MiB of a GPT whole-disk image (behind
#                   a Microsoft reserved partition, as Windows lays it out)
#   post-write      quick format, then new files written to (and partly
#                   deleted from) the new volume
#   small-clusters  512-byte clusters
#   big-clusters    64 KiB clusters
#   4k-records      4096-byte MFT records (mkntfs makes them on a volume with
#                   4096-byte sectors: -s 4096)
#
# Manifest schema (manifest_version 1)
# ------------------------------------
# The file is plain ASCII JSON (non-ASCII characters are \u-escaped), one
# top-level member per line and one entry per line.  Byte offsets are absolute
# offsets in the image file.  LCNs and cluster counts are in units of the OLD
# cluster size, counted from old_volume.offset.  Timestamps are Windows
# FILETIME integers (100 ns ticks since 1601-01-01 00:00:00 UTC); the *_unix_ms
# members are the same instants in Unix milliseconds, rounded towards minus
# infinity.
#
# Two builds on the same tool versions give identical manifests except for the
# members marked (volatile).
#
# {
#   "manifest_version": 1,
#   "scenario": "basic",
#   "description": "...",
#   "seed": "HDSavior-fixtures-v1",          seed of all generated content
#   "generated_utc": "2026-11-24T02:00:00Z",                       (volatile)
#   "tools": {"mkntfs", "ntfs-3g", "ffmpeg", "python", "pillow"},  versions
#   "image": {
#     "file": "basic.img",                   name inside OUTDIR
#     "size": 134217728,                     bytes
#     "sector_size": 512,                    4096 in the 4k-records scenario
#     "sha256": "...",                       of the final image    (volatile)
#     "pre_image": "basic.pre.img" | null,   only with --keep-pre
#     "partition_table": null | {            read back from the image
#       "type": "mbr" | "gpt",
#       "partitions": [{"index", "type", "start_sector", "sectors",
#                       "offset", "length", ("guid", "name" for GPT)}]}
#   },
#   "old_volume": {                          the volume that was lost
#     "offset": 0,                           byte offset of its boot sector in
#                                            the image (0 for bare volumes)
#     "length": 134217728,                   bytes of the partition it filled.
#                                            The last sector of that range held
#                                            the backup boot sector, so
#                                            total_sectors = length / sector_size - 1
#     "sector_size", "cluster_size", "mft_record_size", "index_block_size",
#     "total_sectors", "hidden_sectors", "mft_lcn", "mftmirr_lcn", "label",
#     "serial",                                                    (volatile)
#     "mft_runs": [[lcn, clusters], ...],    data runs of the old $MFT
#     "mft_fragments": 1, "mft_records": 3409,
#     "mkntfs_args": ["-Q", "-F", "-c", "4096", ...]
#   },
#   "new_volume": { the same members for the file system the accident made:
#                   the parameters of the new format },
#   "accident": {
#     "steps": ["quick-format", ...],        in order; further steps:
#                                            "wipe-mft-records", "new-files-written"
#     "wiped_records": null | {"first": 0, "last": 255, "method": "...",
#        "overwritten_by_format": [...], "overwritten_by_wipe": [...]}
#   },
#   "destroyed_records": [0, 1, ...],        old MFT record numbers whose bytes
#                                            in the final image differ from the
#                                            pre-accident image, whichever step
#                                            changed them.  (A never-used record
#                                            rewritten with identical bytes is
#                                            not listed.)
#   "summary": {"files", "folders", "deleted_entries", "bytes_in_files",
#               "old_mft_records", "old_mft_fragments", "destroyed_records",
#               "live_entries_with_destroyed_record",
#               "live_files_with_damaged_content", "live_files_fully_intact",
#               "samples_with_destroyed_record_still_contiguous_and_intact",
#               "old_indx_blocks", "old_indx_blocks_surviving", "new_files"},
#   "verified": ["..."],                     what the build checked and found
#   "notes": ["..."],                        free-form remarks
#   "samples_skipped": ["name: reason"],     samples this toolchain could not make
#   "new_files": [                           post-write only: what was written to
#     {"path", "type", "size", "sha256",     the NEW volume.  deleted = written
#      "mft_record", "deleted",              and removed again before the end.
#      "created", "modified", "accessed",    mft_record counts in the NEW MFT.
#      "content_type"}],
#   "entries": [ENTRY, ...]                  the old volume, sorted by path
# }
#
# ENTRY - one file or folder of the old volume:
# {
#   "path": "names/unicode/Zażółć gęślą jaźń.txt",
#                        relative to the volume root, "/" separators.  Names
#                        are exactly as stored on the volume: they may contain
#                        backslashes, control characters, trailing dots ...
#   "type": "file" | "dir",
#   "size": 1234 | null,             bytes; null for folders
#   "sha256": "..." | null,          of the content as read through the mounted
#                                    driver; null for folders
#   "mft_record": 70,                record number (what ntfs-3g reports as the
#                                    inode number: stat -c %i)
#   "mft_sequence": 1 | null,        sequence number that references to this
#                                    entry carry (parent references of its
#                                    children, index entries).  null when
#                                    unknown: a deleted entry whose record was
#                                    reused.  For a deleted entry with an intact
#                                    record the record itself now holds this
#                                    value plus one.
#   "created", "modified", "accessed"                         FILETIME
#   "created_unix_ms", "modified_unix_ms", "accessed_unix_ms"
#                        Trustworthy to the 100 ns tick: the build sets all
#                        three to fixed values through ntfs-3g, reads them back
#                        through the driver (raw, from the system.ntfs_times
#                        extended attribute; stat() clamps times before 1970
#                        to 0) and compares them with $STANDARD_INFORMATION in
#                        the raw record.  Not recorded and not reproducible:
#                        the MFT-change time (wall clock of the build).
#   "deleted": false,    true = deleted before the accident; the other members
#                        then describe the last state seen through the driver
#   "content_type": "jpeg" | "pdf" | ... | null
#                        the format of a valid file of a known type; null for
#                        arbitrary bytes
#   "tags": [...],       properties the build promised and checked on the raw
#                        volume: "sample" (made by Pillow / ffmpeg / an
#                        archiver / fixture_content.py's document writer),
#                        "contiguous" (one fragment), "resident", "limit" (size
#                        next to the resident limit), "sparse", "compressed",
#                        "initialized-short" (initialized size < size),
#                        "stale-tail" (non-zero bytes on disk behind the
#                        initialized size), "fragmented" ($ATTRIBUTE_LIST and
#                        extension records), "ads" (named data stream),
#                        "dos-name", "deleted-early", "deleted-late"
#   "name_issues": [...],            only when the name cannot be re-created
#                        unchanged: "win32-illegal-char", "control-char",
#                        "trailing-dot-or-space", "reserved-device-name",
#                        "case-collision" (a sibling differs only in case),
#                        "utf8-over-255-bytes" (too long for most Linux file
#                        systems; fine on Windows).  A recovered copy of such
#                        an entry, or of anything below such a folder, cannot
#                        sit at exactly this path.
#   "dos_name": "LONGDO~1.DOC",      only when a DOS (8.3) name exists
#   "streams": ["Zone.Identifier"],  only when named data streams exist
#   "ondisk": {          read from the raw volume before the accident
#     "record_offset": 123456,       byte offset of the base MFT record
#     "names": [{"name", "namespace", "record"}],
#                        every $FILE_NAME attribute and the record that holds
#                        it (which can be an extension record).  Sorted WIN32,
#                        POSIX, DOS: which of a WIN32 / DOS pair comes first
#                        on disk differs from build to build.
#     "extension_records": [68, 70],
#     "attribute_list": "none" | "resident" | "non-resident",
#     files:   "resident": bool,
#              "data_flags": [...],  of the unnamed $DATA attribute: "sparse"
#                                    (0x8000), "compressed" (0x0001),
#                                    "encrypted" (0x4000)
#              "fragments": n,       allocated extents; 0 when resident
#              "runs": [[lcn | null, clusters], ...],   null = sparse hole
#              "allocated_size", "initialized_size",
#              "compressed_size", "compression_unit"    only when the attribute
#                                    has the 72-byte header (sparse, compressed)
#              tagged files also: "stale_bytes_after_initialized_size",
#              "data_pieces", "data_runs", "data_extension_records",
#              "name_in_extension_record", "mft_fragments_spanned"
#     folders: "index": "resident" | "non-resident", "index_blocks": n
#     deleted: "record_state": "intact" (the record still describes the entry)
#                            | "reused" (it now belongs to something else)
#                            | "extension-records-lost",
#              "content_intact_before_accident": bool (files, intact record)
#   },
#   "after": {           the final image compared with the pre-accident image
#     "record_intact": bool,         base and extension records byte-identical
#                                    (false for a deleted entry whose record
#                                    had been reused before the accident)
#     "content_intact": bool | null, reading the file from where the old MFT
#                                    put it (old runs, old cluster size,
#                                    initialized-size rule; resident data from
#                                    its record) out of the final image still
#                                    gives sha256; null for folders
#     "clusters_changed": n,         non-resident files: how many of the file's
#                                    allocated clusters are not byte-identical
#                                    (can be non-zero with content_intact true
#                                    when only bytes behind the data changed)
#     "overlaps_new_allocation": bool | null,
#                                    the NEW file system's $Bitmap marks at
#                                    least one cluster of the file's data (for
#                                    resident data: of its record) allocated
#     "index_entry_intact": bool     an old INDX block that names this entry
#                                    (record, sequence, name) is byte-identical.
#                                    Entries of a folder with a resident index
#                                    never were in an INDX block.
#   }
# }
#
# A file can be recovered bit-identical through its MFT record exactly when
# after.record_intact and after.content_intact are both true (and its folder
# path can be rebuilt as far as its ancestors' records or index entries
# survive).  after.content_intact without after.record_intact and with
# ondisk.fragments == 1 is the carving case.

set -euo pipefail
export LC_ALL=C.UTF-8

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/../.." && pwd)
HELPER="$SCRIPT_DIR/fixture_helper.py"
ALL_SCENARIOS="basic fragmented wiped cluster-change offset-mbr offset-mbr63 offset-gpt post-write small-clusters big-clusters 4k-records"

KEEP_PRE=0
ONLY=""
OUT=""
MNT=""
WORK=""
NTFS3G_PID=""

die() {
    echo "make-fixtures.sh: $*" >&2
    exit 1
}

usage() {
    sed -n '2,/^# Output per scenario/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --keep-pre) KEEP_PRE=1 ;;
        --only) [ $# -ge 2 ] || die "--only needs a value"; ONLY="$2"; shift ;;
        --only=*) ONLY="${1#--only=}" ;;
        --list) echo "$ALL_SCENARIOS" | tr ' ' '\n'; exit 0 ;;
        -h | --help) usage; exit 0 ;;
        -*) die "unknown option $1 (try --help)" ;;
        *) [ -z "$OUT" ] || die "more than one output directory given"; OUT="$1" ;;
    esac
    shift
done
[ -n "$OUT" ] || OUT="$REPO_ROOT/fixtures-out"

SCENARIOS="$ALL_SCENARIOS"
if [ -n "$ONLY" ]; then
    SCENARIOS=""
    for s in ${ONLY//,/ }; do
        case " $ALL_SCENARIOS " in
            *" $s "*) SCENARIOS="$SCENARIOS $s" ;;
            *) die "unknown scenario '$s' (see --list)" ;;
        esac
    done
fi

# ---------------------------------------------------------------------------
# prerequisites
# ---------------------------------------------------------------------------

[ "$(id -u)" -eq 0 ] || die "must run as root (it mounts file systems): run it with sudo"
missing=""
for tool in mkntfs ntfs-3g ntfsinfo sfdisk ffmpeg ffprobe python3 mountpoint truncate; do
    command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
[ -z "$missing" ] || die "missing tools:$missing
install them with: apt-get install -y ntfs-3g fuse3 fdisk ffmpeg python3-pil"
python3 -c 'import sys; sys.exit(0 if sys.version_info >= (3, 8) else 1)' || die "python3 must be 3.8 or newer"
python3 -c 'import PIL' 2>/dev/null || die "python3 has no Pillow (apt-get install -y python3-pil)"
if [ ! -c /dev/fuse ]; then
    modprobe fuse 2>/dev/null || true
    [ -c /dev/fuse ] || die "/dev/fuse is missing: FUSE is required to mount the images with ntfs-3g
(in a container, start it with --device /dev/fuse --cap-add SYS_ADMIN)"
fi

mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
WORK="$OUT/.work"
MNT="$WORK/mnt"
LOG="$WORK/tools.log"
SAMPLES="$WORK/samples"

cleanup() {
    local rc=$?
    trap - EXIT INT TERM
    if [ -n "$MNT" ] && mountpoint -q "$MNT" 2>/dev/null; then
        umount "$MNT" 2>/dev/null || umount -l "$MNT" 2>/dev/null || true
    fi
    if [ -n "$NTFS3G_PID" ]; then
        wait "$NTFS3G_PID" 2>/dev/null || true
    fi
    if [ "$rc" -eq 0 ]; then
        rm -rf -- "${WORK:?}"
    else
        echo "make-fixtures.sh: FAILED (exit $rc); work files and tool output are in $WORK" >&2
    fi
    exit "$rc"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# a previous run that was killed may have left a mount behind
if [ -d "$MNT" ] && mountpoint -q "$MNT" 2>/dev/null; then
    umount "$MNT" 2>/dev/null || umount -l "$MNT"
fi
rm -rf -- "${WORK:?}"
mkdir -p "$MNT" "$SAMPLES"
: >"$LOG"

# ---------------------------------------------------------------------------
# building blocks
# ---------------------------------------------------------------------------

helper() {
    python3 -B "$HELPER" "$@"
}

step() {
    echo "    $*"
}

# mount_vol VOLUME rw|ro [extra ntfs-3g options]
# ntfs-3g stays in the foreground (no_detach) as a job of this shell so that
# umount_vol can wait for it: for an image file "umount" returns before the
# driver has finished writing, and the image must not be read before that.
mount_vol() {
    local vol=$1 opts="$2,noatime,no_detach"
    [ -z "${3:-}" ] || opts="$opts,$3"
    ntfs-3g -o "$opts" "$vol" "$MNT" >>"$LOG" 2>&1 &
    NTFS3G_PID=$!
    local i
    for ((i = 0; i < 400; i++)); do
        if mountpoint -q "$MNT"; then
            return 0
        fi
        kill -0 "$NTFS3G_PID" 2>/dev/null || break
        sleep 0.05
    done
    die "could not mount $vol (see $LOG)"
}

umount_vol() {
    umount "$MNT"
    local rc=0
    wait "$NTFS3G_PID" || rc=$?
    NTFS3G_PID=""
    [ "$rc" -eq 0 ] || die "ntfs-3g exited with status $rc (see $LOG)"
}

# begin NAME SIZE EXPECTATIONS DESCRIPTION
begin() {
    NAME=$1
    STATE="$WORK/$NAME.state.json"
    VOL="$WORK/$NAME.vol.img"
    PREVOL="$WORK/$NAME.pre.vol.img"
    T0=$SECONDS
    echo "== $NAME"
    rm -f "$VOL" "$PREVOL"
    truncate -s "$2" "$VOL"
    helper init --state "$STATE" --scenario "$NAME" --expect "$3" --samples "$SAMPLES" --description "$4"
}

# format_volume old|new MKNTFS-OPTIONS...
# A quick format in both cases: the image starts out as zeros, so the full
# format's zero-fill would change nothing except making the image non-sparse.
format_volume() {
    local which=$1
    shift
    # mkntfs seeds the volume serial number with the time in seconds; make sure
    # the new volume cannot get the old volume's serial number
    if [ "$which" = old ]; then
        OLD_FORMAT_SECOND=$(date +%s)
    else
        while [ "$(date +%s)" -le "$OLD_FORMAT_SECOND" ]; do sleep 0.1; done
    fi
    step "mkntfs -Q -F $* ($which volume)"
    mkntfs -Q -F -q "$@" "$VOL" >>"$LOG" 2>&1 || die "mkntfs failed (see $LOG)"
    helper format --state "$STATE" --which "$which" --volume "$VOL" -- -Q -F "$@" >/dev/null
}

# build_old PROFILE [compression]
# populate through a read-write mount (in two phases with a remount in
# between), capture through a read-only mount, cross-check raw, keep a copy of
# the pre-accident volume
build_old() {
    local profile=$1 comp=${2:-} phase
    step "populate ($profile) through ntfs-3g"
    for phase in 1 2; do
        if [ -n "$comp" ]; then
            mount_vol "$VOL" rw compression
            helper populate --state "$STATE" --root "$MNT" --samples "$SAMPLES" --profile "$profile" \
                --phase "$phase" --compression
        else
            mount_vol "$VOL" rw
            helper populate --state "$STATE" --root "$MNT" --samples "$SAMPLES" --profile "$profile" \
                --phase "$phase"
        fi
        umount_vol
    done
    step "capture ground truth through a read-only mount"
    mount_vol "$VOL" ro
    helper capture --state "$STATE" --root "$MNT"
    umount_vol
    step "re-read the volume raw and cross-check"
    helper analyze --state "$STATE" --volume "$VOL"
    helper crosscheck --state "$STATE" --volume "$VOL"
    cp --sparse=always "$VOL" "$PREVOL"
}

# discard_outputs: remove what an earlier run left behind for this scenario
discard_outputs() {
    local base="${OUT:?}/${NAME:?}" f
    for f in "$base.img" "$base.pre.img" "$base.manifest.json"; do
        [ ! -f "$f" ] || rm -f -- "$f"
    done
}

# finish_bare: the volume file is the image
finish_bare() {
    local img="$OUT/$NAME.img" pre_name=""
    discard_outputs
    mv "$VOL" "$img"
    [ "$KEEP_PRE" -eq 0 ] || pre_name="$NAME.pre.img"
    step "compare before / after, write the manifest"
    helper finalize --state "$STATE" --pre-volume "$PREVOL" --volume "$img" --volume-offset 0 \
        --image "$img" --pre-image-name "$pre_name" --manifest "$OUT/$NAME.manifest.json"
    if [ "$KEEP_PRE" -eq 1 ]; then
        mv "$PREVOL" "$OUT/$NAME.pre.img"
    else
        rm -f "$PREVOL"
    fi
    rm -f "$STATE"
    echo "    done in $((SECONDS - T0)) s"
}

# build_disk TABLE START VOLUME IMAGE
# Wrap a volume file into a whole-disk image whose NTFS partition begins at
# sector START.  The volume is built and re-formatted as a file of its own and
# copied into its partition here.  That is byte-identical to formatting the
# partition in place (checked with losetup --offset and mkntfs -T, see
# README.md) and needs no loop device, which containers often lack.
build_disk() {
    local table=$1 start=$2 vol=$3 img=$4
    local sectors=$(($(stat -c %s "$vol") / 512))
    # one spare "cylinder" (16065 sectors) of unallocated space behind the partition
    : >"$img"
    truncate -s $(((start + sectors + 16065) * 512)) "$img"
    if [ "$table" = mbr ]; then
        sfdisk --quiet --no-reread --no-tell-kernel "$img" >>"$LOG" 2>&1 <<EOF
label: dos
label-id: 0x48445331
unit: sectors
start=$start, size=$sectors, type=7
EOF
    else
        sfdisk --quiet --no-reread --no-tell-kernel "$img" >>"$LOG" 2>&1 <<EOF
label: gpt
label-id: 48445361-7669-4F72-8000-0000000000A0
first-lba: 34
unit: sectors
start=34, size=$((start - 34)), type=E3C9E316-0B5C-4DB8-817D-F92DF00215AE, uuid=48445361-7669-4F72-8000-0000000000A1, name="Microsoft reserved partition"
start=$start, size=$sectors, type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7, uuid=48445361-7669-4F72-8000-0000000000A2, name="Basic data partition"
EOF
    fi
    helper embed --volume "$vol" --image "$img" --offset $((start * 512))
}

# finish_disk mbr|gpt START: the image is a whole disk with the volume in a partition
finish_disk() {
    local table=$1 start=$2 img="$OUT/$NAME.img" pre_name=""
    discard_outputs
    step "assemble the whole-disk image ($table, partition at sector $start)"
    build_disk "$table" "$start" "$VOL" "$img"
    if [ "$KEEP_PRE" -eq 1 ]; then
        build_disk "$table" "$start" "$PREVOL" "$OUT/$NAME.pre.img"
        pre_name="$NAME.pre.img"
    fi
    step "compare before / after, write the manifest"
    helper finalize --state "$STATE" --pre-volume "$PREVOL" --volume "$VOL" --volume-offset $((start * 512)) \
        --image "$img" --pre-image-name "$pre_name" --manifest "$OUT/$NAME.manifest.json"
    rm -f "$VOL" "$PREVOL" "$STATE"
    echo "    done in $((SECONDS - T0)) s"
}

GEO=(-s 512 -H 255 -S 63)     # 512-byte sectors and the usual fake CHS geometry

# ---------------------------------------------------------------------------
# scenarios
# ---------------------------------------------------------------------------

scenario_basic() {
    begin basic 128M "resident-limit,indx,deleted,compressed,user-records-survive" \
        "4 KiB clusters. Nested folders, non-ASCII / long / case-only-different / Win32-illegal names, DOS names, resident and non-resident sizes around every boundary, a folder with 3000 files, named streams, compressed files, deleted files and folders. Accident: quick format with identical parameters."
    format_volume old -c 4096 "${GEO[@]}" -p 0 -L OLD-BASIC
    build_old full compression
    format_volume new -c 4096 "${GEO[@]}" -p 0 -L NEW-BASIC
    finish_bare
}

scenario_fragmented() {
    begin fragmented 64M "resident-limit,indx,fragmented-file,sparse,mft-fragmented,deleted-ballast" \
        "4 KiB clusters. One file written into the holes of a full volume (its runlist needs an \$ATTRIBUTE_LIST and extension records), sparse files, a file whose initialized size is smaller than its size with stale bytes behind it, and an old \$MFT that is itself fragmented. Accident: quick format with identical parameters."
    format_volume old -c 4096 "${GEO[@]}" -p 0 -L OLD-FRAG
    build_old fragmented
    format_volume new -c 4096 "${GEO[@]}" -p 0 -L NEW-FRAG
    finish_bare
}

scenario_wiped() {
    begin wiped 128M "resident-limit,indx,deleted,compressed,samples-contiguous,mft-contiguous-256,wiped" \
        "Like basic, but the first user files are real media and document samples and the folders created right after them get their children later. Accident: quick format with identical parameters, then the old MFT records 0..255 are overwritten the way a Windows format does (256-record initial MFT)."
    format_volume old -c 4096 "${GEO[@]}" -p 0 -L OLD-WIPED
    build_old wiped compression
    format_volume new -c 4096 "${GEO[@]}" -p 0 -L NEW-WIPED
    step "overwrite old MFT record slots 0..255"
    helper wipe --state "$STATE" --pre-volume "$PREVOL" --volume "$VOL" --first 0 --last 255
    finish_bare
}

scenario_cluster_change() {
    begin cluster-change 128M "resident-limit,indx,deleted,samples-contiguous,cluster-change" \
        "Old volume with 4 KiB clusters. Accident: quick format with 64 KiB clusters, so the new boot sector describes a different geometry and the old cluster size has to be found from the old structures."
    format_volume old -c 4096 "${GEO[@]}" -p 0 -L OLD-4K
    build_old standard compression
    format_volume new -c 65536 "${GEO[@]}" -p 0 -L NEW-64K
    finish_bare
}

scenario_offset_mbr() {
    # sector 2048 = 1 MiB, the alignment partitioning tools have used since Vista
    begin offset-mbr 64M "resident-limit,indx,deleted,user-records-survive,partition" \
        "Whole-disk image with an MBR; the NTFS partition (type 07) starts at sector 2048 (1 MiB). 4 KiB clusters. Accident: quick format of the partition with identical parameters."
    format_volume old -c 4096 "${GEO[@]}" -p 2048 -L OLD-MBR
    build_old tiny compression
    format_volume new -c 4096 "${GEO[@]}" -p 2048 -L NEW-MBR
    finish_disk mbr 2048
}

scenario_offset_mbr63() {
    # Windows XP and older: first partition at sector 63, ending on a "cylinder"
    # boundary of the fake 255 x 63 geometry: 8 cylinders - 63 = 128457 sectors
    begin offset-mbr63 $((128457 * 512)) "resident-limit,indx,deleted,user-records-survive,partition" \
        "Whole-disk image with an MBR laid out the Windows XP way: the NTFS partition starts at sector 63 and has an odd number of sectors (128457), so nothing in it is aligned to 1 KiB or 4 KiB in the image; MFT records lie at odd multiples of 512 bytes. 4 KiB clusters. Accident: quick format of the partition with identical parameters."
    format_volume old -c 4096 "${GEO[@]}" -p 63 -L OLD-MBR63
    build_old tiny compression
    format_volume new -c 4096 "${GEO[@]}" -p 63 -L NEW-MBR63
    finish_disk mbr 63
}

scenario_offset_gpt() {
    # sector 32768 = 16 MiB: where Windows puts the first data partition of a
    # GPT disk, behind the Microsoft reserved partition
    begin offset-gpt 64M "resident-limit,indx,deleted,user-records-survive,partition" \
        "Whole-disk image with a GPT: a Microsoft reserved partition at sector 34 and the NTFS partition (basic data) at sector 32768 (16 MiB), the layout Windows gives a GPT data disk. 4 KiB clusters. Accident: quick format of the partition with identical parameters."
    format_volume old -c 4096 "${GEO[@]}" -p 32768 -L OLD-GPT
    build_old tiny compression
    format_volume new -c 4096 "${GEO[@]}" -p 32768 -L NEW-GPT
    finish_disk gpt 32768
}

scenario_post_write() {
    begin post-write 128M "resident-limit,indx,deleted,samples-contiguous,post-write" \
        "4 KiB clusters. Accident: quick format with identical parameters, after which the new volume was mounted and about 25 MiB of new files were written to it; six of the new files were deleted again. Old content under the new data is gone, whether or not the new file system still has those clusters allocated."
    format_volume old -c 4096 "${GEO[@]}" -p 0 -L OLD-POSTW
    build_old standard compression
    format_volume new -c 4096 "${GEO[@]}" -p 0 -L NEW-POSTW
    step "write new files to the new volume"
    mount_vol "$VOL" rw
    helper newfiles --state "$STATE" --root "$MNT" --samples "$SAMPLES"
    umount_vol
    mount_vol "$VOL" ro
    helper capture-new --state "$STATE" --root "$MNT"
    umount_vol
    helper note --state "$STATE" --step new-files-written \
        "After the quick format the new volume was mounted read-write with ntfs-3g and the files listed under new_files were written; those marked deleted were removed again before unmounting."
    finish_bare
}

scenario_small_clusters() {
    begin small-clusters 64M "resident-limit,indx,deleted,user-records-survive" \
        "512-byte clusters (an MFT record spans two clusters, an index block eight). Accident: quick format with identical parameters."
    format_volume old -c 512 "${GEO[@]}" -p 0 -L OLD-512
    build_old tiny compression
    format_volume new -c 512 "${GEO[@]}" -p 0 -L NEW-512
    finish_bare
}

scenario_big_clusters() {
    begin big-clusters 64M "resident-limit,indx,deleted,user-records-survive" \
        "64 KiB clusters (sixteen index blocks per cluster; index block VCNs count 512-byte units). Accident: quick format with identical parameters."
    format_volume old -c 65536 "${GEO[@]}" -p 0 -L OLD-64K
    build_old tiny
    format_volume new -c 65536 "${GEO[@]}" -p 0 -L NEW-64K
    finish_bare
}

scenario_4k_records() {
    begin 4k-records 64M "resident-limit,indx,deleted,user-records-survive" \
        "4096-byte MFT records. mkntfs has no option for the record size; it uses max(1024, sector size), so this volume is formatted with 4096-byte sectors (-s 4096) like a 4Kn disk. 4 KiB clusters. Accident: quick format with identical parameters."
    format_volume old -c 4096 -s 4096 -H 255 -S 63 -p 0 -L OLD-4KREC
    build_old tiny compression
    format_volume new -c 4096 -s 4096 -H 255 -S 63 -p 0 -L NEW-4KREC
    finish_bare
}

# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------

echo "make-fixtures.sh: output in $OUT"
echo "== samples (Pillow, ffmpeg)"
helper samples --out "$SAMPLES"

for s in $SCENARIOS; do
    "scenario_${s//-/_}"
done

# Under sudo, hand the results to the invoking user so that tests can read
# (and CI can delete) them without root.
if [ -n "${SUDO_UID:-}" ] && [ "$SUDO_UID" != 0 ]; then
    chown "$SUDO_UID:${SUDO_GID:-$SUDO_UID}" "$OUT"
    for s in $SCENARIOS; do
        for f in "$OUT/$s.img" "$OUT/$s.pre.img" "$OUT/$s.manifest.json"; do
            [ ! -f "$f" ] || chown "$SUDO_UID:${SUDO_GID:-$SUDO_UID}" "$f"
        done
    done
fi

echo "== summary"
helper report --dir "$OUT" --scenarios "$SCENARIOS"
echo "make-fixtures.sh: finished in $SECONDS s"
