/// The engine's commands as typed methods (docs/PROTOCOL.md, section 3).
library;

import 'dart:math' as math;
import 'dart:typed_data';

import 'cancel.dart';
import 'engine_client.dart';
import 'enums.dart';
import 'errors.dart';
import 'json.dart';
import 'objects.dart';
import 'results.dart';

/// The largest `length` that `read_file` accepts.
const int maxReadFileLength = 8388608;

/// The largest `limit` that `list_children` and `search` accept.
const int maxListLimit = 1000;

/// The protocol version this app speaks (`hello.protocol`).
const int supportedProtocolVersion = 1;

/// The start of a file's content, as [EngineApi.readFileContent] returns it.
final class FileContent {
  const FileContent({
    required this.bytes,
    required this.size,
    required this.complete,
    this.kind,
  });

  /// The bytes read, from offset 0.
  final Uint8List bytes;

  /// Real size of the whole file.
  final int size;

  /// Whether [bytes] is the whole file.
  final bool complete;

  /// Sniffed file kind, for example `jpeg`; `null` if not recognised.
  final String? kind;
}

/// One method per command of the protocol.
///
/// Every method fails with an [EngineException] that carries the engine's
/// error code (`busy`, `stale_catalog`, `destination_refused`, ...) or one of
/// the client's own (`engine_exited`, `engine_not_running`,
/// `protocol_error`). Arguments that are left at `null` are omitted from the
/// request, so the engine's documented default applies.
final class EngineApi {
  EngineApi(this._client);

  final EngineClient _client;

  /// The client the commands are sent through (events, life cycle).
  EngineClient get client => _client;

  Future<T> _call<T>(
    String command,
    T Function(JsonObject json) fromJson, {
    Map<String, Object?>? args,
    CancelToken? cancel,
    bool urgent = false,
  }) async {
    final result = await _client.request(
      command,
      args: args,
      cancel: cancel,
      urgent: urgent,
    );
    try {
      return fromJson(JsonObject.root(result, command));
    } on ProtocolFormatException catch (e) {
      throw EngineException.client(
        EngineErrorCode.protocolError,
        'The engine answered "$command" in a form this app does not '
        'understand: ${e.message}.',
        command: command,
      );
    }
  }

  static void _nothing(JsonObject json) {}

  static void _checkPage(int offset, int limit) {
    RangeError.checkNotNegative(offset, 'offset');
    RangeError.checkValueInInterval(limit, 1, maxListLimit, 'limit');
  }

  // --- always answered -----------------------------------------------------

  /// `hello`: version and capabilities of the engine.
  Future<HelloResult> hello() => _call('hello', HelloResult.fromJson);

  /// `list_disks`: the physical disks of this machine (empty on hosts other
  /// than Windows).
  Future<List<Disk>> listDisks() => _call(
    'list_disks',
    (json) => json.objects('disks', Disk.fromJson),
  );

  /// `job_cancel`: asks the running job to stop. Answered at once; the job
  /// then ends with its `*_done` event (`stopped` for a scan, `cancelled`
  /// for the others). Without a job nothing happens.
  Future<void> jobCancel() => _call('job_cancel', _nothing, urgent: true);

  // --- source and session --------------------------------------------------

  /// `open_source`: opens a physical disk ([kind] disk, [path]
  /// `\\.\PhysicalDriveN`) or a raw image file for reading. Closes any
  /// previously open source and drops its session and catalog.
  ///
  /// [sectorSize] is for images only (default: detected, else 512).
  /// `access_denied` for a disk means the engine is not elevated.
  Future<OpenSourceResult> openSource({
    required SourceKind kind,
    required String path,
    int? sectorSize,
  }) => _call(
    'open_source',
    OpenSourceResult.fromJson,
    args: {'kind': _sourceKind(kind), 'path': path, 'sector_size': ?sectorSize},
  );

  /// `close_source`: closes the source only. A session and its catalog stay
  /// in the engine (browse only, like a session loaded without its source)
  /// until `open_source` or `session_load` replaces them; the epoch does not
  /// change.
  Future<void> closeSource() => _call('close_source', _nothing);

  /// `scan_start`: starts (or with [resume] continues) the scan of the byte
  /// range [start]..[end] of the open source (default: all of it).
  ///
  /// Any `scan_start` discards the current catalog at once. [sessionPath] is
  /// where the session is saved every 5 minutes and at the end (`null`:
  /// nothing is saved automatically; when resuming: the file the session was
  /// loaded from or last saved to). When resuming, [start], [end] and
  /// [collectCarveHits] must be left out or equal the session's.
  Future<ScanStartResult> scanStart({
    int? start,
    int? end,
    String? sessionPath,
    bool resume = false,
    bool? collectCarveHits,
    bool allowSameDisk = false,
    bool allowUnknownDisk = false,
  }) => _call(
    'scan_start',
    ScanStartResult.fromJson,
    args: {
      'start': ?start,
      'end': ?end,
      'session_path': ?sessionPath,
      'resume': resume,
      'collect_carve_hits': ?collectCarveHits,
      'allow_same_disk': allowSameDisk,
      'allow_unknown_disk': allowUnknownDisk,
    },
  );

  /// `session_save`: writes a compacted copy of the session to [path]
  /// (guarded like every destination).
  Future<SessionSaveResult> sessionSave({
    required String path,
    bool allowSameDisk = false,
    bool allowUnknownDisk = false,
  }) => _call(
    'session_save',
    SessionSaveResult.fromJson,
    args: {
      'path': path,
      'allow_same_disk': allowSameDisk,
      'allow_unknown_disk': allowUnknownDisk,
    },
  );

  /// `session_load`: opens a saved session, attaching the recorded source if
  /// it can be found ([source]: a source to open instead). Can take long on
  /// very large sessions; `task_progress` events report how far it is.
  Future<SessionLoadResult> sessionLoad({
    required String path,
    SourceRef? source,
  }) => _call(
    'session_load',
    SessionLoadResult.fromJson,
    args: {'path': path, 'source': ?source?.toJson()},
  );

  /// `session_attach_source`: attaches a source to a session that was loaded
  /// without one, or after `source_lost`. The catalog is rebuilt (new
  /// epoch). Fails with `source_mismatch` when the source is not the scanned
  /// one.
  Future<SessionAttachResult> sessionAttachSource({
    required SourceKind kind,
    required String path,
    bool force = false,
  }) => _call(
    'session_attach_source',
    SessionAttachResult.fromJson,
    args: {'kind': _sourceKind(kind), 'path': path, 'force': force},
  );

  // --- catalog -------------------------------------------------------------

  /// `catalog_info`.
  Future<CatalogInfo> catalogInfo() =>
      _call('catalog_info', CatalogInfo.fromJson);

  /// `geometry_set`: overrides the geometry of one file system. Health and
  /// the protected ranges are recomputed; the tree and the epoch stay; carved
  /// nodes are discarded when the main instance or its geometry changes. Can
  /// take long (`task_progress`). [instance] belongs to [epoch]
  /// (`stale_catalog` otherwise).
  Future<CatalogInfo> geometrySet({
    required int epoch,
    required int instance,
    required int clusterSize,
    required int volumeOffset,
  }) => _call(
    'geometry_set',
    CatalogInfo.fromJson,
    args: {
      'epoch': epoch,
      'instance': instance,
      'cluster_size': clusterSize,
      'volume_offset': volumeOffset,
    },
  );

  /// `instance_set`: overrules the engine's verdict about one file system.
  ///
  /// [InstanceOverride.main] makes that instance the main one (the previous
  /// main becomes `other`, or, if it was main only by an earlier
  /// [InstanceOverride.main], returns to the kind the engine gives it); [InstanceOverride.other] turns an `embedded` or
  /// `older` instance into a volume of its own, so that its files are read
  /// with its own geometry and can be recovered; [InstanceOverride.auto]
  /// withdraws the override. The catalog is rebuilt (new epoch) and carved
  /// nodes are discarded. `strays` cannot be changed (`bad_request`).
  /// [instance] belongs to [epoch] (`stale_catalog` otherwise, so that a
  /// repeated request cannot hit another instance).
  Future<CatalogInfo> instanceSet({
    required int epoch,
    required int instance,
    required InstanceOverride kind,
  }) => _call(
    'instance_set',
    CatalogInfo.fromJson,
    args: {'epoch': epoch, 'instance': instance, 'kind': kind.wire},
  );

  /// `list_children`: one page of the children of folder [node].
  ///
  /// Folders sort before files. [exts] and [ratings] filter files only.
  /// Without [showDeleted], deleted files are hidden, and so are deleted
  /// folders with no non-deleted file below them. [limit] is at most
  /// [maxListLimit].
  Future<RowsPage> listChildren({
    required int epoch,
    required int node,
    int offset = 0,
    int limit = 200,
    SortKey sort = SortKey.name,
    bool desc = false,
    bool dirsOnly = false,
    bool showDeleted = false,
    List<String>? exts,
    List<Rating>? ratings,
    CancelToken? cancel,
  }) {
    _checkPage(offset, limit);
    return _call(
      'list_children',
      RowsPage.fromJson,
      args: {
        'epoch': epoch,
        'node': node,
        'offset': offset,
        'limit': limit,
        'sort': sort.wire,
        'desc': desc,
        'dirs_only': dirsOnly,
        'show_deleted': showDeleted,
        'exts': ?_nonEmpty(exts, 'exts'),
        'ratings': ?_ratings(ratings),
      },
      cancel: cancel,
    );
  }

  /// `node_info`: everything about one node.
  Future<NodeInfo> nodeInfo({
    required int epoch,
    required int node,
    CancelToken? cancel,
  }) => _call(
    'node_info',
    NodeInfo.fromJson,
    args: {'epoch': epoch, 'node': node},
    cancel: cancel,
  );

  /// `search`: one page of the files (and carved files) below [under]
  /// (default: the main root) that match [filter]. Rows carry `path` and
  /// `ancestorIds`.
  Future<RowsPage> search({
    required int epoch,
    int? under,
    FileFilter filter = FileFilter.any,
    bool showDeleted = false,
    SortKey sort = SortKey.name,
    bool desc = false,
    int offset = 0,
    int limit = 200,
    CancelToken? cancel,
  }) {
    _checkPage(offset, limit);
    return _call(
      'search',
      RowsPage.fromJson,
      args: {
        'epoch': epoch,
        'under': ?under,
        'filter': filter.toJson(),
        'show_deleted': showDeleted,
        'sort': sort.wire,
        'desc': desc,
        'offset': offset,
        'limit': limit,
      },
      cancel: cancel,
    );
  }

  /// `selection_summary`: what a recovery of the selection would do.
  ///
  /// A node is selected when its nearest ancestor-or-self that appears in
  /// [include] or [exclude] is in [include]. [filter], [includeDeleted] and
  /// [skipDamaged] apply to every selected file.
  Future<SelectionSummary> selectionSummary({
    required int epoch,
    required List<int> include,
    required List<int> exclude,
    FileFilter? filter,
    bool includeDeleted = false,
    bool skipDamaged = false,
    CancelToken? cancel,
  }) => _call(
    'selection_summary',
    SelectionSummary.fromJson,
    args: {
      'epoch': epoch,
      'include': include,
      'exclude': exclude,
      'filter': ?_filter(filter),
      'include_deleted': includeDeleted,
      'skip_damaged': skipDamaged,
    },
    cancel: cancel,
  );

  /// `read_file`: reads [length] bytes (at most [maxReadFileLength]) of a
  /// file's content at [offset], through the same code path as recovery.
  Future<ReadFileResult> readFile({
    required int epoch,
    required int node,
    int offset = 0,
    int length = 65536,
    CancelToken? cancel,
  }) {
    RangeError.checkNotNegative(offset, 'offset');
    RangeError.checkValueInInterval(length, 1, maxReadFileLength, 'length');
    return _call(
      'read_file',
      ReadFileResult.fromJson,
      args: {'epoch': epoch, 'node': node, 'offset': offset, 'length': length},
      cancel: cancel,
    );
  }

  /// Reads the first [maxBytes] of a file (or all of it, if it is smaller)
  /// with several `read_file` requests of [chunkSize] bytes.
  ///
  /// Small chunks keep each response line short, so that decoding it never
  /// stalls the UI, and let [cancel] take effect between them (a preview the
  /// user has already moved away from).
  Future<FileContent> readFileContent({
    required int epoch,
    required int node,
    required int maxBytes,
    int chunkSize = 262144,
    CancelToken? cancel,
  }) async {
    RangeError.checkValueInInterval(chunkSize, 1, maxReadFileLength, 'chunkSize');
    if (maxBytes < 1) throw RangeError.range(maxBytes, 1, null, 'maxBytes');

    final builder = BytesBuilder(copy: false);
    String? kind;
    var size = 0;
    var reachedEnd = false;
    while (builder.length < maxBytes) {
      final offset = builder.length;
      final part = await readFile(
        epoch: epoch,
        node: node,
        offset: offset,
        length: math.min(chunkSize, maxBytes - offset),
        cancel: cancel,
      );
      if (offset == 0) kind = part.kind;
      size = part.size;
      builder.add(part.bytes);
      if (part.eof) {
        reachedEnd = true;
        break;
      }
      if (part.bytes.isEmpty) break; // No progress: do not loop forever.
    }
    return FileContent(
      // The file may end exactly at the budget, where no read saw `eof`.
      complete: reachedEnd || builder.length >= size,
      size: size,
      kind: kind,
      bytes: builder.takeBytes(),
    );
  }

  // --- destinations and jobs -----------------------------------------------

  /// `check_destination`: asks the destination guard about [path] without
  /// creating anything. With [needBytes] the result says whether the space
  /// suffices.
  Future<DestinationCheck> checkDestination({
    required String path,
    required DestinationPurpose purpose,
    int? needBytes,
    CancelToken? cancel,
  }) => _call(
    'check_destination',
    DestinationCheck.fromJson,
    args: {'path': path, 'purpose': purpose.wire, 'need_bytes': ?needBytes},
    cancel: cancel,
  );

  /// `recover_start`: starts writing the selected files to [dest].
  ///
  /// Fails with `destination_refused` (the exception's `destinationCheck`
  /// says why), `no_space`, `bad_request` for an empty selection, `busy`.
  /// Progress arrives as `recover_progress` events, the end as
  /// `recover_done`.
  Future<RecoverStartResult> recoverStart({
    required int epoch,
    required List<int> include,
    required List<int> exclude,
    FileFilter? filter,
    required String dest,
    bool includeDeleted = false,
    bool skipDamaged = false,
    bool allowSameDisk = false,
    bool allowUnknownDisk = false,
    OnExists onExists = OnExists.rename,
  }) => _call(
    'recover_start',
    RecoverStartResult.fromJson,
    args: {
      'epoch': epoch,
      'include': include,
      'exclude': exclude,
      'filter': ?_filter(filter),
      'dest': dest,
      'include_deleted': includeDeleted,
      'skip_damaged': skipDamaged,
      'allow_same_disk': allowSameDisk,
      'allow_unknown_disk': allowUnknownDisk,
      'on_exists': onExists.wire,
    },
  );

  /// `carve_start`: looks for files by signature where no MFT record
  /// survived. [kinds] `null` means all kinds of `hello.carve_kinds`.
  /// Replaces the carved nodes of the given kinds (same epoch).
  Future<CarveStartResult> carveStart({List<String>? kinds}) => _call(
    'carve_start',
    CarveStartResult.fromJson,
    args: {'kinds': ?_nonEmpty(kinds, 'kinds')},
  );

  /// `image_start`: copies the byte range [start]..[end] of the open source
  /// (default: all of it) sector by sector to the file [dest]. Without
  /// [resume] an existing file is refused (`exists`).
  Future<ImageStartResult> imageStart({
    int start = 0,
    int? end,
    required String dest,
    bool resume = false,
    bool allowSameDisk = false,
    bool allowUnknownDisk = false,
  }) => _call(
    'image_start',
    ImageStartResult.fromJson,
    args: {
      'start': start,
      'end': ?end,
      'dest': dest,
      'resume': resume,
      'allow_same_disk': allowSameDisk,
      'allow_unknown_disk': allowUnknownDisk,
    },
  );

  /// `shutdown`: the engine saves a running scan's session, cancels other
  /// jobs and exits. Prefer [EngineClient.shutdown], which also waits for
  /// the process to end.
  Future<void> shutdown() => _call('shutdown', _nothing, urgent: true);

  // --- argument helpers ----------------------------------------------------

  static String _sourceKind(SourceKind kind) {
    if (kind == SourceKind.unknown) {
      throw ArgumentError.value(kind, 'kind', 'must be disk or image');
    }
    return kind.wire;
  }

  /// "In filters `null` means any; an empty list is `bad_request`."
  static List<String>? _nonEmpty(List<String>? list, String name) {
    if (list != null && list.isEmpty) {
      throw ArgumentError.value(list, name, 'must be null or non-empty');
    }
    return list;
  }

  static List<String>? _ratings(List<Rating>? ratings) {
    if (ratings == null) return null;
    if (ratings.isEmpty || ratings.contains(Rating.unknown)) {
      throw ArgumentError.value(
        ratings,
        'ratings',
        'must be null or a non-empty list of known ratings',
      );
    }
    return [for (final rating in ratings) rating.wire];
  }

  /// A filter that restricts nothing is sent as "no filter".
  static Map<String, Object?>? _filter(FileFilter? filter) =>
      filter == null || filter.isAny ? null : filter.toJson();
}
