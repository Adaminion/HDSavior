/// The shared objects of the engine protocol (docs/PROTOCOL.md, section 2).
///
/// All classes are immutable. `fromJson` ignores members it does not know and
/// throws a [ProtocolFormatException] when a documented member is missing or
/// has the wrong type. Byte counts, offsets and ids are 64-bit `int`s (the
/// app runs on the Dart VM, never on the web). Timestamps are kept as the
/// protocol sends them, Unix time in milliseconds UTC, with [timeFromMs] to
/// convert for display.
library;

import 'enums.dart';
import 'json.dart';

/// Converts a protocol timestamp (Unix milliseconds, UTC) to a [DateTime].
///
/// Returns `null` for `null` ("unknown or not representable") and for values
/// outside the range of [DateTime].
DateTime? timeFromMs(int? milliseconds) {
  const limit = 8640000000000000; // DateTime's range is +/- 100 million days.
  if (milliseconds == null || milliseconds.abs() > limit) return null;
  return DateTime.fromMillisecondsSinceEpoch(milliseconds, isUtc: true);
}

bool _listEquals<T>(List<T>? a, List<T>? b) {
  if (identical(a, b)) return true;
  if (a == null || b == null || a.length != b.length) return false;
  for (var i = 0; i < a.length; i++) {
    if (a[i] != b[i]) return false;
  }
  return true;
}

/// `{"start", "end"}`: an absolute byte range `[start, end)` of the source.
final class ByteRange {
  const ByteRange(this.start, this.end);

  factory ByteRange.fromJson(JsonObject json) =>
      ByteRange(json.integer('start'), json.integer('end'));

  final int start;

  /// Exclusive.
  final int end;

  int get length => end - start;

  @override
  bool operator ==(Object other) =>
      other is ByteRange && other.start == start && other.end == end;

  @override
  int get hashCode => Object.hash(start, end);

  @override
  String toString() => 'ByteRange($start, $end)';
}

/// PROTOCOL `SourceInfo`: the disk or image file that is being rescued.
base class SourceInfo {
  const SourceInfo({
    required this.kind,
    required this.path,
    required this.size,
    required this.sectorSize,
    required this.physicalSectorSize,
    this.model,
    this.serial,
    this.diskNumber,
  });

  factory SourceInfo.fromJson(JsonObject json) => SourceInfo(
    kind: SourceKind.parse(json.string('kind')),
    path: json.string('path'),
    size: json.integer('size'),
    sectorSize: json.integer('sector_size'),
    physicalSectorSize: json.integer('physical_sector_size'),
    model: json.stringOrNull('model'),
    serial: json.stringOrNull('serial'),
    diskNumber: json.integerOrNull('disk_number'),
  );

  final SourceKind kind;

  /// Native path: `\\.\PhysicalDriveN` for disks, a file path for images.
  final String path;

  /// Bytes.
  final int size;

  /// Logical sector size: 512 or 4096.
  final int sectorSize;
  final int physicalSectorSize;

  /// `null` for images.
  final String? model;

  /// `null` for images.
  final String? serial;

  /// What the opened device reports; `null` for images.
  final int? diskNumber;
}

/// `Partition.ntfs`: values from the NTFS boot sector of a partition.
final class NtfsBootInfo {
  const NtfsBootInfo({
    required this.clusterSize,
    required this.totalSectors,
    required this.serial,
  });

  factory NtfsBootInfo.fromJson(JsonObject json) => NtfsBootInfo(
    clusterSize: json.integer('cluster_size'),
    totalSectors: json.integer('total_sectors'),
    serial: json.string('serial'),
  );

  final int clusterSize;
  final int totalSectors;

  /// Volume serial number as hexadecimal digits.
  final String serial;
}

/// PROTOCOL `Partition`.
final class Partition {
  const Partition({
    required this.index,
    required this.offset,
    required this.size,
    required this.type,
    required this.typeId,
    required this.fs,
    this.ntfs,
    this.letters = const [],
    this.label,
  });

  factory Partition.fromJson(JsonObject json) {
    final fs = json.stringOrNull('fs');
    return Partition(
      index: json.integer('index'),
      offset: json.integer('offset'),
      size: json.integer('size'),
      // The pseudo-partition of a source without partition table (index 0)
      // has no type; the protocol does not say how that is sent.
      type: json.stringOrNull('type') ?? '',
      typeId: json.stringOrNull('type_id') ?? '',
      fs: fs == null ? null : FileSystemKind.parse(fs),
      ntfs: switch (json.objectOrNull('ntfs')) {
        null => null,
        final ntfs => NtfsBootInfo.fromJson(ntfs),
      },
      letters: json.strings('letters'),
      label: json.stringOrNull('label'),
    );
  }

  /// In the result of `open_source`: 1-based in offset order; 0 for the
  /// pseudo-partition that covers a source without partition table. In
  /// `list_disks` (which reads no sector): Windows' partition number.
  final int index;

  /// Absolute byte offset on the source.
  final int offset;

  /// Bytes.
  final int size;

  /// Display name of the partition type, for example `Basic data`. Empty in
  /// `list_disks`.
  final String type;

  /// The MBR type byte as two hex digits, or the GPT type GUID. Empty in
  /// `list_disks`.
  final String typeId;

  /// In the result of `open_source`: sniffed from the first sector, `null`
  /// when that is unreadable or empty. In `list_disks`: the mounted file
  /// system, `null` for a raw volume.
  final FileSystemKind? fs;

  /// `null` unless [fs] is NTFS; always `null` in `list_disks`.
  final NtfsBootInfo? ntfs;

  /// Drive letters (`E:`) of the volume currently mounted there; empty if
  /// none, and always empty for images.
  final List<String> letters;

  /// Label of the mounted volume, `null` if none.
  final String? label;

  /// Exclusive end offset on the source.
  int get end => offset + size;

  /// The byte range a scan of this partition covers.
  ByteRange get range => ByteRange(offset, end);
}

/// PROTOCOL `Disk`: one entry of `list_disks` (the members of [SourceInfo]
/// plus what enumeration knows about the disk).
final class Disk extends SourceInfo {
  const Disk({
    required super.kind,
    required super.path,
    required super.size,
    required super.sectorSize,
    required super.physicalSectorSize,
    super.model,
    super.serial,
    super.diskNumber,
    required this.number,
    required this.bus,
    required this.removable,
    required this.isSystem,
    required this.media,
    required this.trim,
    required this.flagNames,
    required this.table,
    required this.partitions,
  });

  factory Disk.fromJson(JsonObject json) {
    final source = SourceInfo.fromJson(json);
    return Disk(
      kind: source.kind,
      path: source.path,
      size: source.size,
      sectorSize: source.sectorSize,
      physicalSectorSize: source.physicalSectorSize,
      model: source.model,
      serial: source.serial,
      diskNumber: source.diskNumber,
      number: json.integer('number'),
      bus: json.string('bus'),
      removable: json.boolean('removable'),
      isSystem: json.boolean('is_system'),
      media: MediaKind.parse(json.string('media')),
      trim: json.booleanOrNull('trim'),
      flagNames: json.strings('flags'),
      table: PartitionTableKind.parse(json.string('table')),
      partitions: json.objects('partitions', Partition.fromJson),
    );
  }

  /// N of `\\.\PhysicalDriveN`.
  final int number;

  /// Bus type as reported by Windows, for example `USB`, `SATA`, `NVMe`.
  final String bus;
  final bool removable;

  /// The disk carries the Windows system volume.
  final bool isSystem;
  final MediaKind media;

  /// Whether the device supports TRIM; `null` when unknown. A format on a
  /// TRIM-capable device discards the data.
  final bool? trim;

  /// The flags as sent (see [flags]).
  final List<String> flagNames;
  final PartitionTableKind table;

  /// The volumes Windows exposes on this disk. The list is made without
  /// reading a sector, so partitions without a volume are missing and the
  /// entries carry no type and no NTFS details; the complete table is the
  /// result of `open_source`.
  final List<Partition> partitions;

  /// The flags this version of the app knows.
  Set<DiskFlag> get flags => {
    for (final name in flagNames)
      if (DiskFlag.parse(name) != DiskFlag.unknown) DiskFlag.parse(name),
  };
}

/// PROTOCOL `Warning`.
final class Warning {
  const Warning({required this.code, required this.message});

  factory Warning.fromJson(JsonObject json) =>
      Warning(code: json.string('code'), message: json.string('message'));

  /// The code as sent; [kind] is the typed form.
  final String code;

  /// Plain-language text from the engine.
  final String message;

  WarningCode get kind => WarningCode.parse(code);

  @override
  String toString() => 'Warning($code: $message)';
}

/// PROTOCOL `Geometry`: cluster size and volume offset of one file system,
/// with the evidence for them.
final class Geometry {
  const Geometry({
    required this.clusterSize,
    required this.volumeOffset,
    required this.clusterSource,
    required this.offsetSource,
    required this.indexDirs,
    required this.indexBlocks,
    required this.signatureChecked,
    required this.signatureOk,
    required this.validated,
    required this.totalClusters,
  });

  factory Geometry.fromJson(JsonObject json) => Geometry(
    clusterSize: json.integer('cluster_size'),
    volumeOffset: json.integer('volume_offset'),
    clusterSource: ClusterSource.parse(json.string('cluster_source')),
    offsetSource: OffsetSource.parse(json.string('offset_source')),
    indexDirs: json.integer('index_dirs'),
    indexBlocks: json.integer('index_blocks'),
    signatureChecked: json.integer('signature_checked'),
    signatureOk: json.integer('signature_ok'),
    validated: json.boolean('validated'),
    totalClusters: json.integer('total_clusters'),
  );

  /// Bytes.
  final int clusterSize;

  /// Absolute byte offset on the source of the volume's cluster 0.
  final int volumeOffset;
  final ClusterSource clusterSource;
  final OffsetSource offsetSource;

  /// Distinct directories whose index blocks voted for this offset.
  final int indexDirs;
  final int indexBlocks;

  /// Files whose first sector was compared with their extension.
  final int signatureChecked;

  /// How many of [signatureChecked] matched.
  final int signatureOk;

  /// Whether the evidence is strong enough (docs/ALGORITHMS.md, F6). Files of
  /// an instance without validated geometry are rated at most `uncertain`.
  final bool validated;
  final int totalClusters;

  /// Whether [other] names the same cluster size and volume offset.
  bool sameValues(Geometry other) =>
      other.clusterSize == clusterSize && other.volumeOffset == volumeOffset;
}

/// `Instance.virtual`: node ids of an instance's virtual folders.
final class InstanceVirtual {
  const InstanceVirtual({this.lost, this.orphans, this.carved});

  factory InstanceVirtual.fromJson(JsonObject json) => InstanceVirtual(
    lost: json.integerOrNull('lost'),
    orphans: json.integerOrNull('orphans'),
    carved: json.integerOrNull('carved'),
  );

  /// `[Lost folders]`, `null` if there is none.
  final int? lost;

  /// `[Orphaned files]`, `null` if there is none.
  final int? orphans;

  /// `[Carved files]`, `null` until carving has found something.
  final int? carved;
}

/// PROTOCOL `Instance`: one file system found by the scan. Every instance is
/// a tree of its own; [root] is its root node.
final class Instance {
  const Instance({
    required this.id,
    required this.kind,
    this.manual = false,
    required this.label,
    required this.root,
    required this.records,
    required this.files,
    required this.dirs,
    required this.deletedFiles,
    required this.lostDirs,
    required this.bytes,
    this.timeFrom,
    this.timeTo,
    this.embeddedIn,
    required this.geometry,
    required this.geometryCandidates,
    required this.virtual,
  });

  factory Instance.fromJson(JsonObject json) => Instance(
    id: json.integer('id'),
    kind: InstanceKind.parse(json.string('kind')),
    manual: json.boolean('manual'),
    label: json.string('label'),
    root: json.integer('root'),
    records: json.integer('records'),
    files: json.integer('files'),
    dirs: json.integer('dirs'),
    deletedFiles: json.integer('deleted_files'),
    lostDirs: json.integer('lost_dirs'),
    bytes: json.integer('bytes'),
    timeFrom: json.integerOrNull('time_from'),
    timeTo: json.integerOrNull('time_to'),
    embeddedIn: json.stringOrNull('embedded_in'),
    geometry: Geometry.fromJson(json.object('geometry')),
    geometryCandidates: json.objects('geometry_candidates', Geometry.fromJson),
    virtual: InstanceVirtual.fromJson(json.object('virtual')),
  );

  /// The `instance` argument of `geometry_set` and `instance_set`.
  final int id;
  final InstanceKind kind;

  /// Whether the user set [kind] (`instance_set`) instead of the engine.
  final bool manual;

  /// Display name of the root; empty for the main instance.
  final String label;

  /// Node id of the root folder.
  final int root;
  final int records;

  /// Non-deleted files.
  final int files;

  /// Non-deleted folders.
  final int dirs;
  final int deletedFiles;
  final int lostDirs;

  /// Total size of the non-deleted files.
  final int bytes;

  /// Oldest change time found, Unix milliseconds; `null` if unknown.
  final int? timeFrom;

  /// Newest change time found, Unix milliseconds; `null` if unknown.
  final int? timeTo;

  /// Display path of the containing file for embedded instances.
  final String? embeddedIn;
  final Geometry geometry;
  final List<Geometry> geometryCandidates;
  final InstanceVirtual virtual;
}

/// One entry of `CatalogInfo.live_volumes`: an NTFS file system that exists in
/// the session range now (normally the result of the accidental format).
final class LiveVolume {
  const LiveVolume({
    required this.offset,
    required this.size,
    this.formatTime,
    this.snapshotTime,
  });

  factory LiveVolume.fromJson(JsonObject json) => LiveVolume(
    offset: json.integer('offset'),
    size: json.integer('size'),
    formatTime: json.integerOrNull('format_time'),
    snapshotTime: json.integerOrNull('snapshot_time'),
  );

  final int offset;
  final int size;

  /// When the volume was formatted, Unix milliseconds.
  final int? formatTime;

  /// When the engine last read its allocation state, Unix milliseconds.
  final int? snapshotTime;
}

/// One entry of `CatalogInfo.protected`: a byte range of the source that no
/// destination may touch (docs/ARCHITECTURE.md, S2).
final class ProtectedRange {
  const ProtectedRange({
    required this.start,
    required this.end,
    required this.reasonName,
  });

  factory ProtectedRange.fromJson(JsonObject json) => ProtectedRange(
    start: json.integer('start'),
    end: json.integer('end'),
    reasonName: json.string('reason'),
  );

  final int start;

  /// Exclusive.
  final int end;

  /// The reason as sent; [reason] is the typed form.
  final String reasonName;

  /// `session_range` for the scanned range, `lost_volume` for a lost file
  /// system with validated geometry.
  ProtectionReason get reason => ProtectionReason.parse(reasonName);

  ByteRange get range => ByteRange(start, end);
}

/// PROTOCOL `CatalogInfo`: what the scan found. Node ids are valid for one
/// [epoch] only.
final class CatalogInfo {
  const CatalogInfo({
    required this.epoch,
    required this.instances,
    required this.nodes,
    required this.liveRecords,
    required this.postFormatRecords,
    required this.liveVolumes,
    required this.range,
    this.protected = const [],
    required this.scannedBytes,
    required this.scanComplete,
    required this.badBytes,
    required this.outOfRangeFiles,
    required this.outOfRangeBytes,
    required this.carveHits,
    required this.carved,
    required this.sourceAttached,
    required this.warnings,
  });

  factory CatalogInfo.fromJson(JsonObject json) => CatalogInfo(
    epoch: json.integer('epoch'),
    instances: json.objects('instances', Instance.fromJson),
    nodes: json.integer('nodes'),
    liveRecords: json.integer('live_records'),
    postFormatRecords: json.integer('post_format_records'),
    liveVolumes: json.objects('live_volumes', LiveVolume.fromJson),
    range: ByteRange.fromJson(json.object('range')),
    protected: json.objects('protected', ProtectedRange.fromJson),
    scannedBytes: json.integer('scanned_bytes'),
    scanComplete: json.boolean('scan_complete'),
    badBytes: json.integer('bad_bytes'),
    outOfRangeFiles: json.integer('out_of_range_files'),
    outOfRangeBytes: json.integer('out_of_range_bytes'),
    carveHits: json.integer('carve_hits'),
    carved: json.integer('carved'),
    sourceAttached: json.boolean('source_attached'),
    warnings: json.objects('warnings', Warning.fromJson),
  );

  /// Changes whenever the node set is rebuilt; every command that takes node
  /// ids must send it.
  final int epoch;

  /// `instances[0]` is the main instance.
  final List<Instance> instances;

  /// All nodes of all roots.
  final int nodes;

  /// Records of the current file system; not shown.
  final int liveRecords;

  /// Files that were created and deleted again on the current file system
  /// since the format; not shown, but what they occupied counts as
  /// overwritten.
  final int postFormatRecords;
  final List<LiveVolume> liveVolumes;

  /// The session range.
  final ByteRange range;

  /// The ranges no destination may touch: the session range, then one
  /// `lost_volume` entry per lost file system with validated geometry.
  final List<ProtectedRange> protected;
  final int scannedBytes;

  /// `false` after a stopped scan (which can be resumed).
  final bool scanComplete;

  /// Bytes in unreadable sectors.
  final int badBytes;

  /// Files with data outside the session range (scan the whole disk to reach
  /// them).
  final int outOfRangeFiles;
  final int outOfRangeBytes;

  /// Signature hits collected for carving.
  final int carveHits;

  /// Carved files in the catalog.
  final int carved;

  /// `false`: the tree can be browsed, but nothing can be read until
  /// `session_attach_source` succeeds.
  final bool sourceAttached;
  final List<Warning> warnings;

  /// The file system that was lost, `null` if the scan found none.
  Instance? get mainInstance => instances.isEmpty ? null : instances.first;

  /// The instance whose root is [node], `null` if [node] is not a root.
  Instance? instanceOfRoot(int node) {
    for (final instance in instances) {
      if (instance.root == node) return instance;
    }
    return null;
  }
}

/// PROTOCOL `Row`: one file or folder in a listing. (Named `NodeRow` because
/// Flutter has a `Row` widget.)
final class NodeRow {
  NodeRow({
    required this.id,
    required this.parent,
    required this.name,
    required this.kind,
    required this.ext,
    required this.size,
    this.modified,
    this.created,
    required this.deleted,
    this.rating,
    this.flagNames = const [],
    this.files = 0,
    this.deletedFiles = 0,
    this.deletedSize = 0,
    this.hasChildren = false,
    this.path,
    this.ancestorIds,
  });

  factory NodeRow.fromJson(JsonObject json) {
    final rating = json.stringOrNull('rating');
    return NodeRow(
      id: json.integer('id'),
      parent: json.integerOrNull('parent'),
      name: json.string('name'),
      kind: NodeKind.parse(json.string('kind')),
      ext: json.string('ext'),
      size: json.integer('size'),
      modified: json.integerOrNull('modified'),
      created: json.integerOrNull('created'),
      deleted: json.boolean('deleted'),
      rating: rating == null ? null : Rating.parse(rating),
      flagNames: json.strings('flags'),
      files: json.integer('files'),
      deletedFiles: json.integer('deleted_files'),
      deletedSize: json.integer('deleted_size'),
      hasChildren: json.boolean('has_children'),
      path: json.stringOrNull('path'),
      ancestorIds: json.integersOrNull('ancestor_ids'),
    );
  }

  /// Node id, valid for one catalog epoch.
  final int id;

  /// `null` for an instance root.
  final int? parent;

  /// Display name. An unpaired surrogate in the NTFS name arrives as U+FFFD.
  final String name;
  final NodeKind kind;

  /// Lower case, without the dot; empty if none.
  final String ext;

  /// Files: the real size. Folders: total size of the non-deleted files
  /// below.
  final int size;

  /// Unix milliseconds, `null` if unknown.
  final int? modified;

  /// Unix milliseconds, `null` if unknown.
  final int? created;
  final bool deleted;

  /// `null` for folders.
  final Rating? rating;

  /// The flags as sent, including any this version does not know.
  final List<String> flagNames;

  /// Folders: non-deleted files below, recursively. Files: 0.
  final int files;

  /// Folders: deleted files below, recursively. Files: 0.
  final int deletedFiles;

  /// Folders: total size of the deleted files below. Files: 0.
  final int deletedSize;

  /// Folders: whether a `dirs_only` listing with the same options would
  /// return at least one child folder (the tree shows an expander).
  final bool hasChildren;

  /// Search results only: display path inside the rescued tree (`/`).
  final String? path;

  /// Search results only: ids of the ancestors, root first. This is what the
  /// selection model needs to show tri-state checkboxes for search rows.
  final List<int>? ancestorIds;

  /// The flags this version of the app knows.
  late final Set<FileFlag> flags = {
    for (final name in flagNames)
      if (FileFlag.parse(name) != FileFlag.unknown) FileFlag.parse(name),
  };

  bool get isFolder => kind.isFolder;

  bool get isFile => !kind.isFolder;

  DateTime? get modifiedTime => timeFromMs(modified);

  DateTime? get createdTime => timeFromMs(created);

  @override
  String toString() => 'NodeRow($id, ${kind.wire}, "$name")';
}

/// PROTOCOL `Filter`: which files a search returns, or which selected files a
/// recovery takes. `null` members mean "any".
final class FileFilter {
  const FileFilter({
    this.query,
    this.exts,
    this.minSize,
    this.maxSize,
    this.ratings,
  });

  /// Matches every file.
  static const FileFilter any = FileFilter();

  /// Case-insensitive substring of the name.
  final String? query;

  /// Lower-case extensions without the dot (see [normalizeExts]).
  final List<String>? exts;

  /// Bytes, inclusive.
  final int? minSize;

  /// Bytes, inclusive.
  final int? maxSize;
  final List<Rating>? ratings;

  /// Whether the filter restricts nothing.
  bool get isAny =>
      (query == null || query!.isEmpty) &&
      exts == null &&
      minSize == null &&
      maxSize == null &&
      ratings == null;

  /// Brings extensions typed by a user into the protocol's form: lower case,
  /// no leading dot, no duplicates, no empty entries. Returns `null` when
  /// nothing is left, because the protocol rejects an empty list.
  static List<String>? normalizeExts(Iterable<String> exts) {
    final result = <String>{};
    for (final ext in exts) {
      var cleaned = ext.trim().toLowerCase();
      while (cleaned.startsWith('.')) {
        cleaned = cleaned.substring(1);
      }
      if (cleaned.isNotEmpty) result.add(cleaned);
    }
    return result.isEmpty ? null : result.toList(growable: false);
  }

  /// The wire form. Members that are `null` (and an empty [query]) are
  /// omitted. An empty list is a programming error: the protocol answers it
  /// with `bad_request`, and "any" is spelled `null`.
  Map<String, Object?> toJson() {
    if (exts != null && exts!.isEmpty) {
      throw ArgumentError.value(exts, 'exts', 'must be null or non-empty');
    }
    if (ratings != null && ratings!.isEmpty) {
      throw ArgumentError.value(ratings, 'ratings', 'must be null or non-empty');
    }
    if (ratings != null && ratings!.contains(Rating.unknown)) {
      throw ArgumentError.value(ratings, 'ratings', 'must not hold "unknown"');
    }
    return {
      if (query != null && query!.isNotEmpty) 'query': query,
      'exts': ?exts,
      'min_size': ?minSize,
      'max_size': ?maxSize,
      if (ratings != null) 'ratings': [for (final r in ratings!) r.wire],
    };
  }

  @override
  bool operator ==(Object other) =>
      other is FileFilter &&
      (other.query ?? '') == (query ?? '') &&
      _listEquals(other.exts, exts) &&
      other.minSize == minSize &&
      other.maxSize == maxSize &&
      _listEquals(other.ratings, ratings);

  @override
  int get hashCode => Object.hash(
    query ?? '',
    exts == null ? null : Object.hashAll(exts!),
    minSize,
    maxSize,
    ratings == null ? null : Object.hashAll(ratings!),
  );

  @override
  String toString() =>
      'FileFilter(query: $query, exts: $exts, minSize: $minSize, '
      'maxSize: $maxSize, ratings: $ratings)';
}

/// The error object of the protocol: `{"code", "message", "data"}`.
///
/// It is the `error` member of a failed response and of the `*_done` events
/// of a failed job.
final class EngineError {
  const EngineError({required this.code, this.message = '', this.data});

  factory EngineError.fromJson(JsonObject json) {
    final data = json.raw['data'];
    return EngineError(
      code: json.string('code'),
      message: json.stringOrNull('message') ?? '',
      // "error.data is an optional object": anything else is ignored rather
      // than allowed to hide the error itself.
      data: data is Map<String, Object?> ? data : null,
    );
  }

  /// Reads an optional error member such as `scan_done.error`.
  ///
  /// The protocol shows these members only as `null`. An error object is
  /// expected when they are set; a bare string is accepted too and taken as
  /// the code if it is one, otherwise as the message.
  static EngineError? fromMember(JsonObject json, String key) {
    final value = json.raw[key];
    if (value == null) return null;
    if (value is String) {
      return EngineErrorCode.parse(value) == EngineErrorCode.unknown
          ? EngineError(code: '', message: value)
          : EngineError(code: value);
    }
    return EngineError.fromJson(json.object(key));
  }

  /// The code as sent; [kind] is the typed form.
  final String code;

  /// Plain-language explanation from the engine; may be empty.
  final String message;

  /// Optional details, for example the `check_destination` result of a
  /// `destination_refused` error.
  final Map<String, Object?>? data;

  EngineErrorCode get kind => EngineErrorCode.parse(code);

  @override
  String toString() =>
      message.isEmpty ? 'EngineError($code)' : 'EngineError($code: $message)';
}
