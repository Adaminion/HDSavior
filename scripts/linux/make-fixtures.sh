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
# accident -> before/after comparison -> manifest).  Every accident ends with
# the Windows-like rewrite of the new $MFT's slots up to 255 (zeros, as
# Windows 10/11 leave them; "wiped" uses the Windows 7 blank FILE records), and
# every old volume starts with the prelude, whose records that rewrite destroys:
#
#   basic           4 KiB clusters, the full tree; quick format, same parameters
#   fragmented      a file with $ATTRIBUTE_LIST + extension records, sparse
#                   files, a file with stale bytes behind its initialized size,
#                   a fragmented $MFT; quick format, same parameters
#   wiped           like basic, the slots rewritten as blank FILE records
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
#   generations     g1 (2600 files) quick-formatted into g2 (430 entries, the
#                   lost one); g1's records survive behind g2's
#   generations-restored
#                   the same, g2's files restored with g1-era creation times
#   embedded        a host holding a sparse, fragmented file that is itself an
#                   NTFS volume with more entries than the host
#   repartition     one 128 MiB volume replaced by two 56 MiB partitions, both
#                   quick-formatted; the nominal scan range is the first; the
#                   old volume was full, four files straddle the first's end
#   live-collision  quick format, then new folders created in exactly the
#                   record slots of destroyed old folders whose children survive
#
# Manifest schema (manifest_version 1)
# ------------------------------------
# The file is plain ASCII JSON (non-ASCII characters are \u-escaped), one
# top-level member per line and one entry per line.  Byte offsets are absolute
# offsets in the image file.  LCNs and cluster counts are in units of the OLD
# cluster size, counted from old_volume.offset (for entries of another file
# system: from that file system's own start, see file_systems).  Timestamps are
# Windows FILETIME integers (100 ns ticks since 1601-01-01 00:00:00 UTC); the
# *_unix_ms members are the same instants in Unix milliseconds, rounded towards
# minus infinity.
#
# Two builds on the same tool versions give identical manifests except for the
# members marked (volatile).  This revision only ADDED members (marked [new]);
# no existing member changed its meaning, so manifest_version is still 1.
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
#   "new_volume": { the same members for the file system the accident made
#                   (the first one, when it made several), plus
#                   [new] "format_time", "format_time_unix_ms" (volatile):
#                   its creation time (record 3, whole seconds) },
#   [new] "new_volumes": [ every file system the accident made, as new_volume
#                   but without mkntfs_args; repartition has two ],
#   [new] "scan_range": {"offset", "length", "what"}
#                   the nominal session range: the old partition (the whole
#                   image for a bare volume); repartition: the first new
#                   partition.  after.data_inside_scan_range, named_by[].
#                   inside_scan_range and after.ancestry refer to it.
#   "accident": {
#     "steps": ["quick-format", ...],        in order; further steps:
#                                            "wipe-mft-records", "new-files-written",
#                                            "live-collision", "repartition"
#     "wiped_records": null | {
#        "first": 0, "last": 255,            the new $MFT slots the model covers
#        [new] "fill": "zeros" | "blank",    Windows 10/11 or Windows 7 form
#        [new] "slots_written": [27, 255],   the slots the wipe wrote (the format
#                                            wrote the ones below)
#        [new] "byte_range": [start, end),   absolute bytes the wipe wrote
#        "overwritten_by_format": [...],     OLD record numbers in the window that
#        "overwritten_by_wipe": [...],       the format / the wipe changed
#        "method": "..."},
#     [new] "last_old_write", "last_old_write_unix_ms"            (volatile)
#                                            latest MFT-change time in the old
#                                            volume's records before the accident
#     [new] "format_time", "format_time_unix_ms"                 (volatile)
#                                            the accident's format: always in a
#                                            later whole second than last_old_write
#   },
#   [new] "history": [                       earlier re-formats (generations):
#     {"event": "re-format", "formatted_over": "g1",
#      "last_old_write", "last_old_write_unix_ms", "format_time",
#      "format_time_unix_ms" (volatile), "wiped_records": {as above}}],
#   [new] "file_systems": [                  every old file system of the scenario
#     {"name": "main" | "g2" | "g1" | "inner", "kind": "main" | "older" | "embedded",
#      the old_volume members, "format_time", "last_write" (+ _unix_ms, volatile),
#      "offset": absolute offset, or null for an embedded one, which has
#      "container" (path of the file holding it in the main file system),
#      "container_runs" ([[lcn, clusters]] in container_cluster_size units,
#      relative to the main file system; null lcn = hole) instead}],
#   "destroyed_records": [0, 1, ...],        old MFT record numbers (main file
#                                            system) whose bytes in the final image
#                                            differ from the pre-accident image.
#                                            (A never-used record rewritten with
#                                            identical bytes is not listed.)
#   "summary": {"files", "folders", "deleted_entries", "bytes_in_files",
#               "old_mft_records", "old_mft_fragments", "destroyed_records",
#               "live_entries_with_destroyed_record",
#               "live_files_with_damaged_content", "live_files_fully_intact",
#               "samples_with_destroyed_record_still_contiguous_and_intact",
#               "old_indx_blocks", "old_indx_blocks_surviving", "new_files",
#               [new] "live_entries_with_reconstructible_path",
#               "live_entries_below_a_lost_folder",
#               "destroyed_folders_named_by_index_entries",
#               "entries_whose_parent_reference_is_taken", "other_entries"},
#   "verified": ["..."],                     what the build checked and found
#   "notes": ["..."],                        free-form remarks
#   "samples_skipped": ["name: reason"],     samples this toolchain could not make
#   "new_files": [                           what was written to the NEW volume
#     {"path", "type", "size", "sha256",     after the accident (post-write,
#      "mft_record", "deleted",              live-collision).  mft_record counts
#      "created", "modified", "accessed",    in the NEW MFT; deleted = written and
#      "content_type",                       removed again before the end.
#      [new] "mft_sequence", "record_offset", "file_system": "new"}],
#   "entries": [ENTRY, ...]                  the lost (main) file system, by path
#   [new] "other_entries": [ENTRY, ...]      entries of the other file systems
#                                            (g1, the embedded volume), sorted by
#                                            file_system and path
# }
#
# ENTRY - one file or folder:
# {
#   "path": "names/unicode/Zażółć gęślą jaźń.txt",
#                        relative to its file system's root, "/" separators.
#                        Names are exactly as stored on the volume: they may
#                        contain backslashes, control characters, trailing dots
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
#                        the raw record.
#   [new] "mft_changed": FILETIME | null     (volatile) the MFT-change time in
#                        $STANDARD_INFORMATION (the build's wall clock); null
#                        when the record was reused before the accident
#   [new] "file_system": "main" | "g2" | "g1" | "inner"   see file_systems
#   [new] "embedded_in": "Virtual Machines/old-laptop.img"   entries of an
#                        embedded file system: path of the container file
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
#                        "dos-name", "deleted-early", "deleted-late",
#                        [new] "split" (at least two fragments), "container"
#                        (holds a file system), "collides" (new_files only: a
#                        new folder in the slot of a destroyed old folder)
#   "name_issues": [...],            only when the name cannot be re-created
#                        unchanged: "win32-illegal-char", "control-char",
#                        "trailing-dot-or-space", "reserved-device-name",
#                        "case-collision" (a sibling differs only in case),
#                        "utf8-over-255-bytes" (too long for most Linux file
#                        systems; fine on Windows).  A recovered copy of such
#                        an entry, or of anything below such a folder, cannot
#                        sit at exactly this path (ALGORITHMS.md H4).
#   "dos_name": "LONGDO~1.DOC",      only when a DOS (8.3) name exists
#   "streams": ["Zone.Identifier"],  only when named data streams exist
#   "ondisk": {          read from the raw volume before the accident
#     "record_offset": 123456,       absolute byte offset of the base MFT record
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
#     "index_entry_intact": bool     an old INDX block of this file system that
#                                    names this entry (record, sequence, name)
#                                    is byte-identical.  Entries of a folder
#                                    with a resident index never were in an
#                                    INDX block.
#     [new] "data_inside_scan_range": bool | null
#                                    files with a surviving record: all of
#                                    their data (resident: the record) lies in
#                                    scan_range
#     [new] "named_by": [            folders whose record is destroyed: every
#        {"name", "namespace",       surviving index entry with this folder's
#         "source": "indx" | "index_root",   (record, sequence): from an old
#         "offset",                  INDX block that is byte-identical (also
#         "parent_record",           stale ones), or from the $INDEX_ROOT of a
#         "parent_sequence",         surviving record of the same file system.
#         "usable",                  usable = ALGORITHMS C4 / E6: older than the
#         "inside_scan_range"}]      accident's format and not in a cluster the
#                                    new file system has allocated.  offset is
#                                    the block's / record's absolute offset.
#     [new] "foreign_index_entries": [{"file_system", "name"}]
#                                    surviving index entries of ANOTHER file
#                                    system of the scenario with the same
#                                    (record, sequence): names that must NOT
#                                    be given to this folder
#     [new] "ancestry": {            what a scan of scan_range can rebuild:
#        "path_reconstructible",     every ancestor (and the entry itself) is
#                                    known: its record survives inside the scan
#                                    range, or a usable index entry inside it
#                                    names it
#        "lost_ancestor",            the deepest ancestor that is not known
#                                    (null when none)
#        "surviving_suffix"}         the part of the path below lost_ancestor:
#                                    where a correct recovery puts the entry
#                                    when the path cannot be rebuilt
#     [new] "ancestry_whole_image": {the same, for a scan of the whole image;
#                                    only present where it differs}
#     [new] "parent_reference_taken_by": {"file_system", "path", "mft_record",
#        "mft_sequence", "in_use", "is_directory", "record_offset"}
#                                    the entry's parent record is destroyed and
#                                    its slot now holds a record its parent
#                                    reference RESOLVES to (ALGORITHMS reference
#                                    rule, a directory): the folder of the new
#                                    volume (or newer generation) the entry must
#                                    not be put into.  path is null when the
#                                    record belongs to nothing the manifest lists
#     [new] "parent_slot_contradiction": {same members}
#                                    the parent's slot holds an in-use record
#                                    the reference does NOT resolve to (other
#                                    sequence, or not a directory): the D2 (B)
#                                    contradiction
#     [new] "damage_evidence": [...] files with a surviving record whose content
#                                    is damaged: what on the disk shows it
#                                    (ALGORITHMS C5, G2): "current-allocation"
#                                    (a current $Bitmap), "post-format-record"
#                                    (a deleted record of a current $MFT maps
#                                    the clusters), "current-boot-sector" (a
#                                    current volume's backup boot sector, which
#                                    lies outside its $Bitmap), "newer-file" (an
#                                    intact file of the newer generation maps
#                                    them), or ["none"]
#     [new] "clusters_reused_by_newer": bool    entries of an "older" file
#                                    system: the newer generation's $Bitmap
#                                    (right before the accident) marks at least
#                                    one of the file's clusters
#   }
# }
#
# A file can be recovered bit-identical through its MFT record exactly when
# after.record_intact and after.content_intact are both true; where it belongs
# in the output is after.ancestry.  after.content_intact without
# after.record_intact, with ondisk.fragments == 1, is the carving case.
# check-recovery.py checks a recovery against all of this.

set -euo pipefail
export LC_ALL=C.UTF-8

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/../.." && pwd)
HELPER="$SCRIPT_DIR/fixture_helper.py"
ALL_SCENARIOS="basic fragmented wiped cluster-change offset-mbr offset-mbr63 offset-gpt post-write small-clusters big-clusters 4k-records generations generations-restored embedded repartition live-collision"

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

# begin NAME SIZE EXPECTATIONS DESCRIPTION [helper init options]
begin() {
    NAME=$1
    STATE="$WORK/$NAME.state.json"
    VOL="$WORK/$NAME.vol.img"
    PREVOL="$WORK/$NAME.pre.vol.img"
    T0=$SECONDS
    echo "== $NAME"
    rm -f "$VOL" "$PREVOL"
    truncate -s "$2" "$VOL"
    helper init --state "$STATE" --scenario "$NAME" --expect "$3" --samples "$SAMPLES" --description "$4" "${@:5}"
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

# build_old PROFILE [--straddle BYTES] [helper populate options: --compression, --prelude N, ...]
# populate through a read-write mount (in two phases with a remount in
# between), capture through a read-only mount, cross-check raw, keep a copy of
# the pre-accident volume.  --straddle BYTES (profile repart): three phases;
# after the first, straddle-plan reads the unmounted volume and picks the fill
# files whose cuts make room for files across byte BYTES of the volume.
build_old() {
    local profile=$1 phase phases="1 2" mount_opt="" straddle=""
    shift
    if [ "${1:-}" = "--straddle" ]; then
        straddle=$2 phases="1 2 3"
        shift 2
    fi
    case " $* " in *" --compression "*) mount_opt=compression ;; esac
    step "populate ($profile${*:+ $*}) through ntfs-3g"
    for phase in $phases; do
        mount_vol "$VOL" rw "$mount_opt"
        helper populate --state "$STATE" --root "$MNT" --samples "$SAMPLES" --profile "$profile" \
            --phase "$phase" "$@"
        umount_vol
        if [ "$phase" = 1 ] && [ -n "$straddle" ]; then
            helper straddle-plan --state "$STATE" --volume "$VOL" --boundary "$straddle"
        fi
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

# wait_after_last_write VOLUME RECORD
# Record the latest MFT-change time on VOLUME (the file system about to be
# formatted over) and wait until the clock is in a later whole second: mkntfs
# stamps whole seconds, and ALGORITHMS C3 compares MFT-change times with the
# new volume's creation time.  RECORD is "accident" or the name of the file
# system that an earlier (historical) re-format went over.
wait_after_last_write() {
    local last
    last=$(helper lastwrite --state "$STATE" --volume "$1" --record "$2")
    while [ "$(date +%s)" -le "$last" ]; do sleep 0.1; done
}

# accident FILL MKNTFS-OPTIONS...
# The accidental quick format of the whole volume file, then the Windows-like
# rewrite of the new $MFT's slots up to 255 (FILL: zeros = Windows 10/11,
# blank = Windows 7).
accident() {
    local fill=$1
    shift
    wait_after_last_write "$VOL" accident
    format_volume new "$@"
    step "rewrite the new \$MFT's slots up to 255 ($fill)"
    helper wipe --state "$STATE" --pre-volume "$PREVOL" --volume "$VOL" --fill "$fill"
}

# discard_work_files: remove the scenario's work files (volume copies, state)
discard_work_files() {
    local f
    for f in "${WORK:?}/${NAME:?}".*; do
        [ ! -f "$f" ] || rm -f -- "$f"
    done
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
        --image "$img" --pre-image-name "$pre_name" --manifest "$OUT/$NAME.manifest.json" \
        --scan-range-what "the whole image (a bare volume)"
    if [ "$KEEP_PRE" -eq 1 ]; then
        mv "$PREVOL" "$OUT/$NAME.pre.img"
    else
        rm -f "$PREVOL"
    fi
    discard_work_files
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
    discard_work_files
    echo "    done in $((SECONDS - T0)) s"
}

GEO=(-s 512 -H 255 -S 63)     # 512-byte sectors and the usual fake CHS geometry

# ---------------------------------------------------------------------------
# scenarios
# ---------------------------------------------------------------------------

# Every scenario's accident ends with the Windows-like rewrite of the new
# $MFT's slots up to 255 (see "accident"), and every old volume starts with the
# prelude (helper populate --prelude): its first files and folders land in the
# slots the accident destroys, their children behind them.  WIPED is the
# expectation that goes with it.
WIPED="prelude-lost"

scenario_basic() {
    begin basic 128M "resident-limit,indx,deleted,compressed,$WIPED,content-survives" \
        "4 KiB clusters. Nested folders, non-ASCII / long / case-only-different / Win32-illegal names, DOS names, resident and non-resident sizes around every boundary, a folder with 3000 files, named streams, compressed files, deleted files and folders. Accident: quick format with identical parameters, then the new \$MFT's slots up to 255 zeroed as Windows 10/11 leave them." \
        --boundary 255
    format_volume old -c 4096 "${GEO[@]}" -p 0 -L OLD-BASIC
    build_old full --prelude 255 --compression
    accident zeros -c 4096 "${GEO[@]}" -p 0 -L NEW-BASIC
    finish_bare
}

scenario_fragmented() {
    begin fragmented 64M "resident-limit,indx,fragmented-file,sparse,mft-fragmented,deleted-ballast,$WIPED,content-survives" \
        "4 KiB clusters. One file written into the holes of a full volume (its runlist needs an \$ATTRIBUTE_LIST and extension records), sparse files, a file whose initialized size is smaller than its size with stale bytes behind it, and an old \$MFT that is itself fragmented. Accident: quick format with identical parameters, new \$MFT slots up to 255 zeroed." \
        --boundary 255
    format_volume old -c 4096 "${GEO[@]}" -p 0 -L OLD-FRAG
    build_old fragmented --prelude 255
    accident zeros -c 4096 "${GEO[@]}" -p 0 -L NEW-FRAG
    finish_bare
}

scenario_wiped() {
    begin wiped 128M "resident-limit,indx,deleted,compressed,samples-contiguous,$WIPED,content-survives,carving-samples" \
        "Like basic, but the new \$MFT's slots up to 255 receive blank FILE records (the Windows 7 form) instead of zeros. Accident: quick format with identical parameters, then old MFT records 0..255 overwritten the way a Windows 7 format does." \
        --boundary 255
    format_volume old -c 4096 "${GEO[@]}" -p 0 -L OLD-WIPED
    build_old full --prelude 255 --compression
    accident blank -c 4096 "${GEO[@]}" -p 0 -L NEW-WIPED
    finish_bare
}

scenario_cluster_change() {
    # The new $MFT starts at byte 131072 (64 KiB clusters), the old one at 16384:
    # its slots 0..255 cover old records 112..367, and the format itself writes
    # old records 0..175.  The prelude therefore reaches record 367.
    begin cluster-change 128M "resident-limit,indx,deleted,samples-contiguous,cluster-change,$WIPED,content-survives" \
        "Old volume with 4 KiB clusters. Accident: quick format with 64 KiB clusters, so the new boot sector describes a different geometry and the old cluster size has to be found from the old structures; then the new \$MFT's slots up to 255 zeroed, which covers old records up to 367." \
        --boundary 367
    format_volume old -c 4096 "${GEO[@]}" -p 0 -L OLD-4K
    build_old standard --prelude 367 --compression
    accident zeros -c 65536 "${GEO[@]}" -p 0 -L NEW-64K
    finish_bare
}

scenario_offset_mbr() {
    # sector 2048 = 1 MiB, the alignment partitioning tools have used since Vista
    begin offset-mbr 64M "resident-limit,indx,deleted,partition,$WIPED,content-survives" \
        "Whole-disk image with an MBR; the NTFS partition (type 07) starts at sector 2048 (1 MiB). 4 KiB clusters. Accident: quick format of the partition with identical parameters, new \$MFT slots up to 255 zeroed." \
        --boundary 255
    format_volume old -c 4096 "${GEO[@]}" -p 2048 -L OLD-MBR
    build_old tiny --prelude 255 --compression
    accident zeros -c 4096 "${GEO[@]}" -p 2048 -L NEW-MBR
    finish_disk mbr 2048
}

scenario_offset_mbr63() {
    # Windows XP and older: first partition at sector 63, ending on a "cylinder"
    # boundary of the fake 255 x 63 geometry: 8 cylinders - 63 = 128457 sectors
    begin offset-mbr63 $((128457 * 512)) "resident-limit,indx,deleted,partition,$WIPED,content-survives" \
        "Whole-disk image with an MBR laid out the Windows XP way: the NTFS partition starts at sector 63 and has an odd number of sectors (128457), so nothing in it is aligned to 1 KiB or 4 KiB in the image; MFT records lie at odd multiples of 512 bytes. 4 KiB clusters. Accident: quick format of the partition with identical parameters, new \$MFT slots up to 255 zeroed." \
        --boundary 255
    format_volume old -c 4096 "${GEO[@]}" -p 63 -L OLD-MBR63
    build_old tiny --prelude 255 --compression
    accident zeros -c 4096 "${GEO[@]}" -p 63 -L NEW-MBR63
    finish_disk mbr 63
}

scenario_offset_gpt() {
    # sector 32768 = 16 MiB: where Windows puts the first data partition of a
    # GPT disk, behind the Microsoft reserved partition
    begin offset-gpt 64M "resident-limit,indx,deleted,partition,$WIPED,content-survives" \
        "Whole-disk image with a GPT: a Microsoft reserved partition at sector 34 and the NTFS partition (basic data) at sector 32768 (16 MiB), the layout Windows gives a GPT data disk. 4 KiB clusters. Accident: quick format of the partition with identical parameters, new \$MFT slots up to 255 zeroed." \
        --boundary 255
    format_volume old -c 4096 "${GEO[@]}" -p 32768 -L OLD-GPT
    build_old tiny --prelude 255 --compression
    accident zeros -c 4096 "${GEO[@]}" -p 32768 -L NEW-GPT
    finish_disk gpt 32768
}

scenario_post_write() {
    begin post-write 128M "resident-limit,indx,deleted,samples-contiguous,post-write,$WIPED" \
        "4 KiB clusters. Accident: quick format with identical parameters (new \$MFT slots up to 255 zeroed), after which the new volume was mounted and about 25 MiB of new files were written to it; six of the new files were deleted again. Old content under the new data is gone, whether or not the new file system still has those clusters allocated." \
        --boundary 255
    format_volume old -c 4096 "${GEO[@]}" -p 0 -L OLD-POSTW
    build_old standard --prelude 255 --compression
    accident zeros -c 4096 "${GEO[@]}" -p 0 -L NEW-POSTW
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
    begin small-clusters 64M "resident-limit,indx,deleted,$WIPED,content-survives" \
        "512-byte clusters (an MFT record spans two clusters, an index block eight). Accident: quick format with identical parameters, new \$MFT slots up to 255 zeroed." \
        --boundary 255
    format_volume old -c 512 "${GEO[@]}" -p 0 -L OLD-512
    build_old tiny --prelude 255 --compression
    accident zeros -c 512 "${GEO[@]}" -p 0 -L NEW-512
    finish_bare
}

scenario_big_clusters() {
    begin big-clusters 64M "resident-limit,indx,deleted,$WIPED,content-survives" \
        "64 KiB clusters (sixteen index blocks per cluster; index block VCNs count 512-byte units). Accident: quick format with identical parameters, new \$MFT slots up to 255 zeroed." \
        --boundary 255
    format_volume old -c 65536 "${GEO[@]}" -p 0 -L OLD-64K
    build_old tiny --prelude 255
    accident zeros -c 65536 "${GEO[@]}" -p 0 -L NEW-64K
    finish_bare
}

scenario_4k_records() {
    begin 4k-records 64M "resident-limit,indx,deleted,$WIPED,content-survives" \
        "4096-byte MFT records. mkntfs has no option for the record size; it uses max(1024, sector size), so this volume is formatted with 4096-byte sectors (-s 4096) like a 4Kn disk. 4 KiB clusters. Accident: quick format with identical parameters, new \$MFT slots up to 255 zeroed (256 records of 4 KiB: how large Windows makes the initial \$MFT with 4 KiB records is not known)." \
        --boundary 255
    format_volume old -c 4096 -s 4096 -H 255 -S 63 -p 0 -L OLD-4KREC
    build_old tiny --prelude 255 --compression
    accident zeros -c 4096 -s 4096 -H 255 -S 63 -p 0 -L NEW-4KREC
    finish_bare
}

# generations NAME DESCRIPTION [populate options for g2]
# g1 (a few thousand files) is quick-formatted (with the Windows-like rewrite)
# and becomes g2 (a few hundred files in other folders), which is lost in the
# accident.  g1's records behind g2's last one survive at the same MFT origin.
generations() {
    local name=$1 desc=$2 expect="generations,$WIPED,content-survives"
    shift 2
    [ $# -eq 0 ] || expect="$expect,restored-times"
    begin "$name" 128M "$expect" "$desc" --boundary 255 --fs-name g2
    format_volume old -c 4096 "${GEO[@]}" -p 0 -L G1
    build_old gen1
    local g1pre="$WORK/$NAME.g1.pre.img"
    mv "$PREVOL" "$g1pre"
    helper stash --state "$STATE" --name g1 --kind older --pre-volume "$g1pre"
    step "quick format over g1 (g2 is made)"
    wait_after_last_write "$VOL" g1
    format_volume old -c 4096 "${GEO[@]}" -p 0 -L G2
    helper wipe --state "$STATE" --pre-volume "$g1pre" --volume "$VOL" --fill zeros --record history
    build_old gen2 --prelude 255 "$@"
    accident zeros -c 4096 "${GEO[@]}" -p 0 -L NEW-GEN
    finish_bare
}

scenario_generations() {
    generations generations \
        "Two generations on one volume. g1 (4 KiB clusters, about 2600 files created 2008-2013) was quick-formatted and became g2 (about 450 entries in other folders, created 2019-2022), which is the lost file system. Both formats rewrote the new \$MFT's slots up to 255 (zeros). g1's records behind g2's last record survive at the same MFT origin, many of its clusters were reused by g2. Accident: quick format of g2 with identical parameters."
}

scenario_generations_restored() {
    generations generations-restored \
        "As generations, but g2 was filled by restoring files from a backup: their creation and modification times are as old as g1's (2008-2013), so only the MFT-change times tell the generations apart." \
        --restored
}

scenario_embedded() {
    begin embedded 128M "embedded,$WIPED,content-survives" \
        "Host volume (4 KiB clusters, a few hundred entries) holding Virtual Machines/old-laptop.img, a raw 32 MiB NTFS volume with more entries than its host; the image file is stored sparse (holes where the volume is empty) and in several fragments. Accident: quick format of the host with identical parameters, new \$MFT slots up to 255 zeroed." \
        --boundary 255
    local host_vol=$VOL host_pre=$PREVOL
    step "the volume that becomes a file of the host"
    VOL="$WORK/$NAME.inner.img"
    PREVOL="$WORK/$NAME.inner.pre.img"
    truncate -s 32M "$VOL"
    format_volume old -c 4096 "${GEO[@]}" -p 0 -L INNER
    build_old inner
    helper stash --state "$STATE" --name inner --kind embedded --pre-volume "$PREVOL" \
        --container "Virtual Machines/old-laptop.img"
    local inner_pre=$PREVOL
    VOL=$host_vol
    PREVOL=$host_pre
    step "the host"
    format_volume old -c 4096 "${GEO[@]}" -p 0 -L OLD-HOST
    build_old host --prelude 255 --compression --source "$inner_pre"
    accident zeros -c 4096 "${GEO[@]}" -p 0 -L NEW-HOST
    finish_bare
}

scenario_repartition() {
    # old partition: sector 2048, 128 MiB.  New layout: P1 = its first 56 MiB,
    # P2 = the next 56 MiB, the last 16 MiB unpartitioned (the old backup boot
    # sector survives there).  The nominal scan range is P1.
    local start=2048 p1=$((56 << 20)) p2=$((56 << 20))
    begin repartition 128M "repartition,$WIPED" \
        "Whole-disk image with an MBR. The old NTFS volume filled a 128 MiB partition at sector 2048 (4 KiB clusters, data all over it). Accident: the disk was re-partitioned into two 56 MiB partitions (the last 16 MiB stay unpartitioned) and both were quick-formatted; the first one starts where the old volume did, and its new \$MFT's slots up to 255 were zeroed. The nominal scan range is the first partition." \
        --boundary 255
    format_volume old -c 4096 "${GEO[@]}" -p "$start" -L OLD-REPART
    build_old repart --straddle "$p1" --prelude 255 --compression
    wait_after_last_write "$VOL" accident
    local old_vol=$VOL p1vol="$WORK/$NAME.p1.img" p2vol="$WORK/$NAME.p2.img"
    step "re-partition: cut the two new partitions out of the old one, format them, put them back"
    dd if="$old_vol" of="$p1vol" bs=1M count=$((p1 >> 20)) status=none
    dd if="$old_vol" of="$p2vol" bs=1M skip=$((p1 >> 20)) count=$((p2 >> 20)) status=none
    VOL=$p1vol
    format_volume new -c 4096 "${GEO[@]}" -p "$start" -L NEW-P1
    step "rewrite the new \$MFT's slots up to 255 (zeros)"
    helper wipe --state "$STATE" --pre-volume "$PREVOL" --volume "$p1vol" --fill zeros \
        --volume-offset $((start * 512))
    # mkntfs derives the serial number from the clock: format the second one a second later
    sleep 1
    step "mkntfs -Q -F -c 4096 ${GEO[*]} -p $((start + p1 / 512)) -L NEW-P2 (second new partition)"
    mkntfs -Q -F -q -c 4096 "${GEO[@]}" -p $((start + p1 / 512)) -L NEW-P2 "$p2vol" >>"$LOG" 2>&1 \
        || die "mkntfs failed (see $LOG)"
    dd if="$p1vol" of="$old_vol" bs=1M conv=notrunc status=none
    dd if="$p2vol" of="$old_vol" bs=1M seek=$((p1 >> 20)) conv=notrunc status=none
    VOL=$old_vol
    helper note --state "$STATE" --step repartition \
        "The disk was re-partitioned: partition 1 now covers the first $p1 bytes of the old volume, partition 2 the next $p2 bytes; both were quick-formatted (the second without the slot rewrite: Windows would allocate those slots to its own \$MFT, here they would only destroy old file data that no structure of the new volume accounts for). The rest of the old partition is unpartitioned."
    local img="$OUT/$NAME.img" sectors=$(($(stat -c %s "$VOL") / 512)) pre_name=""
    discard_outputs
    step "assemble the whole-disk image (mbr, partitions at sectors $start and $((start + p1 / 512)))"
    : >"$img"
    truncate -s $(((start + sectors + 16065) * 512)) "$img"
    sfdisk --quiet --no-reread --no-tell-kernel "$img" >>"$LOG" 2>&1 <<EOF
label: dos
label-id: 0x48445332
unit: sectors
start=$start, size=$((p1 / 512)), type=7
start=$((start + p1 / 512)), size=$((p2 / 512)), type=7
EOF
    helper embed --volume "$VOL" --image "$img" --offset $((start * 512))
    if [ "$KEEP_PRE" -eq 1 ]; then
        build_disk mbr "$start" "$PREVOL" "$OUT/$NAME.pre.img"
        pre_name="$NAME.pre.img"
    fi
    step "compare before / after, write the manifest"
    helper finalize --state "$STATE" --pre-volume "$PREVOL" --volume "$VOL" --volume-offset $((start * 512)) \
        --image "$img" --pre-image-name "$pre_name" --manifest "$OUT/$NAME.manifest.json" \
        --new-volume "$p1" --scan-range "$((start * 512)):$p1" --scan-range-what "the first new partition"
    discard_work_files
    echo "    done in $((SECONDS - T0)) s"
}

scenario_live_collision() {
    begin live-collision 128M "resident-limit,indx,deleted,live-collision,$WIPED" \
        "4 KiB clusters. Accident: quick format with identical parameters (new \$MFT slots up to 255 zeroed); then the new volume was used: new folders were created in exactly the record slots of destroyed old folders whose children survived, so these old children's parent references (record N, sequence 1) now resolve to live folders of the new volume; files were written into some of them, large files written, and some files created and deleted again." \
        --boundary 255
    format_volume old -c 4096 "${GEO[@]}" -p 0 -L OLD-LIVE
    build_old standard --prelude 255 --compression
    accident zeros -c 4096 "${GEO[@]}" -p 0 -L NEW-LIVE
    step "use the new volume: folders in the slots of destroyed old folders"
    mount_vol "$VOL" rw
    helper collide --state "$STATE" --root "$MNT" --samples "$SAMPLES"
    umount_vol
    mount_vol "$VOL" ro
    helper capture-new --state "$STATE" --root "$MNT"
    umount_vol
    helper note --state "$STATE" --step live-collision \
        "After the quick format the new volume was mounted read-write with ntfs-3g; the entries listed under new_files were created, those marked deleted removed again before unmounting."
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
