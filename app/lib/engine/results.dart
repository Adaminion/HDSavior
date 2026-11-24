/// The results of the engine's commands (docs/PROTOCOL.md, section 3).
///
/// Same conventions as objects.dart: immutable, unknown members ignored,
/// 64-bit integers, nullable where the protocol says so.
library;

import 'dart:convert';
import 'dart:typed_data';

import 'enums.dart';
import 'json.dart';
import 'objects.dart';

/// Result of `hello`.
final class HelloResult {
  const HelloResult({
    required this.engineVersion,
    required this.protocol,
    required this.os,
    required this.elevated,
    required this.rawDiskSupport,
    required this.carveKinds,
  });

  factory HelloResult.fromJson(JsonObject json) => HelloResult(
    engineVersion: json.string('engine_version'),
    protocol: json.integer('protocol'),
    os: json.string('os'),
    elevated: json.boolean('elevated'),
    rawDiskSupport: json.boolean('raw_disk_support'),
    carveKinds: json.strings('carve_kinds'),
  );

  final String engineVersion;

  /// Protocol version; this app speaks version 1.
  final int protocol;
  final String os;

  /// Advisory, for messages only. The authoritative sign that the engine is
  /// not elevated is `access_denied` from opening a device.
  final bool elevated;

  /// Whether physical disks can be opened (Windows only).
  final bool rawDiskSupport;

  /// The file kinds carving can find, as `carve_start` takes them.
  final List<String> carveKinds;
}

/// Result of `open_source`.
final class OpenSourceResult {
  const OpenSourceResult({
    required this.source,
    required this.table,
    required this.partitions,
    required this.warnings,
  });

  factory OpenSourceResult.fromJson(JsonObject json) => OpenSourceResult(
    source: SourceInfo.fromJson(json.object('source')),
    table: PartitionTableKind.parse(json.string('table')),
    partitions: json.objects('partitions', Partition.fromJson),
    warnings: json.objects('warnings', Warning.fromJson),
  );

  final SourceInfo source;
  final PartitionTableKind table;
  final List<Partition> partitions;

  /// Read-only-computed warnings about the source: TRIM, mounted volume,
  /// page file, system disk, the app itself on the source.
  final List<Warning> warnings;
}

/// Result of `scan_start`.
final class ScanStartResult {
  const ScanStartResult({required this.job, required this.bytesTotal});

  factory ScanStartResult.fromJson(JsonObject json) => ScanStartResult(
    job: JobKind.parse(json.string('job')),
    bytesTotal: json.integer('bytes_total'),
  );

  final JobKind job;

  /// Size of the session range.
  final int bytesTotal;
}

/// Result of `session_save`.
final class SessionSaveResult {
  const SessionSaveResult({required this.path, required this.bytes});

  factory SessionSaveResult.fromJson(JsonObject json) =>
      SessionSaveResult(path: json.string('path'), bytes: json.integer('bytes'));

  final String path;

  /// Size of the written file.
  final int bytes;
}

/// The `source` argument of `session_load`: a source to open instead of the
/// recorded one.
final class SourceRef {
  const SourceRef({required this.kind, required this.path});

  final SourceKind kind;
  final String path;

  Map<String, Object?> toJson() => {'kind': kind.wire, 'path': path};
}

/// Result of `session_load`.
final class SessionLoadResult {
  const SessionLoadResult({
    required this.source,
    required this.sourceAttached,
    this.sourceError,
    required this.catalog,
  });

  factory SessionLoadResult.fromJson(JsonObject json) => SessionLoadResult(
    source: SourceInfo.fromJson(json.object('source')),
    sourceAttached: json.boolean('source_attached'),
    sourceError: EngineError.fromMember(json, 'source_error'),
    catalog: CatalogInfo.fromJson(json.object('catalog')),
  );

  /// The attached source, or the recorded one when none could be attached.
  final SourceInfo source;

  /// `false`: the tree can be browsed, but recovery, previews, carving and
  /// resuming need `session_attach_source` first.
  final bool sourceAttached;

  /// Why no source could be attached; `null` when [sourceAttached].
  final EngineError? sourceError;
  final CatalogInfo catalog;
}

/// Result of `session_attach_source`. The catalog is rebuilt (new epoch).
final class SessionAttachResult {
  const SessionAttachResult({required this.source, required this.catalog});

  factory SessionAttachResult.fromJson(JsonObject json) => SessionAttachResult(
    source: SourceInfo.fromJson(json.object('source')),
    catalog: CatalogInfo.fromJson(json.object('catalog')),
  );

  final SourceInfo source;
  final CatalogInfo catalog;
}

/// Result of `list_children` and `search`: one page of a listing.
final class RowsPage {
  const RowsPage({required this.total, required this.rows});

  factory RowsPage.fromJson(JsonObject json) => RowsPage(
    total: json.integer('total'),
    rows: json.objects('rows', NodeRow.fromJson),
  );

  /// Rows of the whole listing after filters, not of this page.
  final int total;
  final List<NodeRow> rows;
}

/// One entry of `node_info.ancestors`.
final class NodeRef {
  const NodeRef({required this.id, required this.name});

  factory NodeRef.fromJson(JsonObject json) =>
      NodeRef(id: json.integer('id'), name: json.string('name'));

  final int id;
  final String name;
}

/// `node_info.record`: where the file's MFT record is.
final class RecordInfo {
  const RecordInfo({
    required this.number,
    required this.seq,
    required this.diskOffset,
    required this.extensionRecords,
  });

  factory RecordInfo.fromJson(JsonObject json) => RecordInfo(
    // Volumes formatted by Windows NT4/2000 store no record number.
    number: json.integerOrNull('number'),
    seq: json.integer('seq'),
    diskOffset: json.integer('disk_offset'),
    extensionRecords: json.integer('extension_records'),
  );

  /// MFT record number; `null` when the record does not store one.
  final int? number;
  final int seq;

  /// Absolute byte offset of the record on the source.
  final int diskOffset;
  final int extensionRecords;
}

/// `node_info.data`: where the file's content is.
final class DataInfo {
  const DataInfo({
    required this.resident,
    required this.realSize,
    required this.initializedSize,
    required this.allocatedSize,
    required this.runs,
    this.firstLcn,
    this.firstOffset,
  });

  factory DataInfo.fromJson(JsonObject json) => DataInfo(
    resident: json.boolean('resident'),
    realSize: json.integer('real_size'),
    initializedSize: json.integer('initialized_size'),
    allocatedSize: json.integer('allocated_size'),
    runs: json.integer('runs'),
    firstLcn: json.integerOrNull('first_lcn'),
    firstOffset: json.integerOrNull('first_offset'),
  );

  /// The content is stored inside the MFT record.
  final bool resident;
  final int realSize;
  final int initializedSize;
  final int allocatedSize;

  /// Number of runs (fragments).
  final int runs;

  /// First cluster of the content; `null` when there is none (resident or
  /// empty data).
  final int? firstLcn;

  /// Absolute byte offset of the first content byte on the source; `null`
  /// when there is none.
  final int? firstOffset;
}

/// One entry of `node_info.flag_details`: a flag explained in plain language.
final class FlagDetail {
  const FlagDetail({required this.flag, required this.text});

  factory FlagDetail.fromJson(JsonObject json) =>
      FlagDetail(flag: json.string('flag'), text: json.string('text'));

  /// The flag as sent; [kind] is the typed form.
  final String flag;
  final String text;

  FileFlag get kind => FileFlag.parse(flag);
}

/// Result of `node_info`: everything about one node.
final class NodeInfo {
  const NodeInfo({
    required this.row,
    required this.ancestors,
    required this.path,
    required this.instance,
    this.accessed,
    required this.otherNames,
    this.record,
    this.data,
    required this.flagDetails,
  });

  factory NodeInfo.fromJson(JsonObject json) => NodeInfo(
    row: NodeRow.fromJson(json.object('row')),
    ancestors: json.objects('ancestors', NodeRef.fromJson),
    path: json.string('path'),
    instance: json.integer('instance'),
    accessed: json.integerOrNull('accessed'),
    otherNames: json.strings('other_names'),
    record: switch (json.objectOrNull('record')) {
      null => null,
      final record => RecordInfo.fromJson(record),
    },
    data: switch (json.objectOrNull('data')) {
      null => null,
      final data => DataInfo.fromJson(data),
    },
    flagDetails: json.objects('flag_details', FlagDetail.fromJson),
  );

  final NodeRow row;

  /// Root first, without the node itself.
  final List<NodeRef> ancestors;

  /// Display path inside the rescued tree (`/`).
  final String path;

  /// `Instance.id` of the tree the node belongs to.
  final int instance;

  /// Last access time, Unix milliseconds; `null` if unknown.
  final int? accessed;

  /// Other hard-link names of the file.
  final List<String> otherNames;

  /// `null` for virtual, lost and carved nodes.
  final RecordInfo? record;

  /// `null` for folders.
  final DataInfo? data;

  /// Every flag of the row, explained.
  final List<FlagDetail> flagDetails;

  /// Ids of the ancestors, root first: what the selection model needs.
  List<int> get ancestorIds => [for (final a in ancestors) a.id];
}

/// Result of `selection_summary`: what a recovery of the selection would do.
final class SelectionSummary {
  const SelectionSummary({
    required this.files,
    required this.bytes,
    required this.dirs,
    required this.good,
    required this.uncertain,
    required this.damaged,
    required this.unsupported,
  });

  factory SelectionSummary.fromJson(JsonObject json) => SelectionSummary(
    files: json.integer('files'),
    bytes: json.integer('bytes'),
    dirs: json.integer('dirs'),
    good: json.integer('good'),
    uncertain: json.integer('uncertain'),
    damaged: json.integer('damaged'),
    unsupported: json.integer('unsupported'),
  );

  /// Selected files after the filter and `include_deleted`; the four rating
  /// counts add up to it.
  final int files;

  /// Sizes of the files that will be attempted (not `unsupported`; not
  /// `damaged` when `skip_damaged`).
  final int bytes;

  /// Directories that will be created.
  final int dirs;
  final int good;
  final int uncertain;
  final int damaged;
  final int unsupported;
}

/// Result of `check_destination`, and the `data` of a `destination_refused`
/// error.
///
/// The answer depends on the catalog (the protected ranges grow when a lost
/// file system is found): ask again after `scan_done`, `session_load`,
/// `session_attach_source`, `geometry_set` and `instance_set`.
final class DestinationCheck {
  const DestinationCheck({
    required this.verdict,
    this.reasonName,
    required this.message,
    this.lostFiles = 0,
    this.lostBytes = 0,
    this.freeBytes,
    this.enoughSpace,
    this.fs,
    this.maxFileBytes,
    this.diskNumbers = const [],
  });

  factory DestinationCheck.fromJson(JsonObject json) => DestinationCheck(
    verdict: DestinationVerdict.parse(json.string('verdict')),
    reasonName: json.stringOrNull('reason'),
    message: json.stringOrNull('message') ?? '',
    lostFiles: json.integerOrNull('lost_files') ?? 0,
    lostBytes: json.integerOrNull('lost_bytes') ?? 0,
    // What the engine knows about a location depends on the verdict (nothing
    // for `invalid_path`), so everything below is optional here.
    freeBytes: json.integerOrNull('free_bytes'),
    enoughSpace: json.booleanOrNull('enough_space'),
    fs: json.stringOrNull('fs'),
    maxFileBytes: json.integerOrNull('max_file_bytes'),
    diskNumbers: json.integersOrNull('disk_numbers') ?? const [],
  );

  final DestinationVerdict verdict;

  /// For `overlaps_source`: which protected range the location is in, as
  /// sent; [reason] is the typed form. `null` otherwise.
  final String? reasonName;

  /// Explains the verdict in plain language.
  final String message;

  /// Catalog files stored in the part of the source disk that the
  /// location's volume occupies: writing there would overwrite them. 0
  /// without a catalog.
  final int lostFiles;

  /// The bytes of [lostFiles].
  final int lostBytes;
  final int? freeBytes;

  /// `null` when the check was made without `need_bytes`.
  final bool? enoughSpace;

  /// Name of the destination's file system, for example `NTFS`.
  final String? fs;

  /// The file system's maximum file size where that is a practical limit
  /// (FAT32: 4294967295), else `null`.
  final int? maxFileBytes;

  /// Physical disks the location is on.
  final List<int> diskNumbers;

  bool get isOk => verdict == DestinationVerdict.ok;

  /// For `overlaps_source`: `sessionRange` (inside the scanned or imaged
  /// range), `lostVolume` (inside a lost file system that reaches beyond the
  /// scanned range: scanning the whole disk reaches those files, and the
  /// destination has to be on another disk) or `sourceImage` (the image file
  /// that is the source). `null` for the other verdicts.
  ProtectionReason? get reason => switch (reasonName) {
    null => null,
    final name => ProtectionReason.parse(name),
  };

  /// The location is on the source disk outside the session range: usable
  /// only with the user's explicit consent (`allow_same_disk`).
  bool get needsSameDiskConsent => verdict == DestinationVerdict.sameDisk;

  /// The disk cannot be determined: usable only with the user's explicit
  /// consent (`allow_unknown_disk`).
  bool get needsUnknownDiskConsent => verdict == DestinationVerdict.unknownDisk;
}

/// Result of `recover_start`.
final class RecoverStartResult {
  const RecoverStartResult({
    required this.job,
    required this.files,
    required this.bytes,
  });

  factory RecoverStartResult.fromJson(JsonObject json) => RecoverStartResult(
    job: JobKind.parse(json.string('job')),
    files: json.integer('files'),
    bytes: json.integer('bytes'),
  );

  final JobKind job;

  /// The numbers of `selection_summary`.
  final int files;
  final int bytes;
}

/// Result of `carve_start`.
final class CarveStartResult {
  const CarveStartResult({required this.job, required this.candidates});

  factory CarveStartResult.fromJson(JsonObject json) => CarveStartResult(
    job: JobKind.parse(json.string('job')),
    candidates: json.integer('candidates'),
  );

  final JobKind job;
  final int candidates;
}

/// Result of `image_start`.
final class ImageStartResult {
  const ImageStartResult({required this.job, required this.bytes});

  factory ImageStartResult.fromJson(JsonObject json) => ImageStartResult(
    job: JobKind.parse(json.string('job')),
    bytes: json.integer('bytes'),
  );

  final JobKind job;

  /// Bytes the image will hold.
  final int bytes;
}

/// Result of `read_file`: a piece of a file's content.
final class ReadFileResult {
  const ReadFileResult({
    required this.bytes,
    required this.size,
    required this.eof,
    this.kind,
  });

  factory ReadFileResult.fromJson(JsonObject json) {
    final Uint8List bytes;
    try {
      bytes = base64.decode(json.string('data_b64'));
    } on FormatException catch (e) {
      throw ProtocolFormatException(
        '${json.path}.data_b64',
        'is not valid base64 (${e.message})',
      );
    }
    return ReadFileResult(
      bytes: bytes,
      size: json.integer('size'),
      eof: json.boolean('eof'),
      kind: json.stringOrNull('kind'),
    );
  }

  /// The content read (`data_b64`, decoded).
  final Uint8List bytes;

  /// Real size of the whole file.
  final int size;

  /// Whether the end of the file was reached.
  final bool eof;

  /// Sniffed file kind of the returned bytes when the read started at offset
  /// 0, else `null`.
  final String? kind;
}
