/// The source that is open in the engine, the disks to choose from, and the
/// session file in use.
library;

import 'package:flutter/foundation.dart';

import '../engine/engine.dart';
import '../platform/platform_services.dart';
import 'catalog_controller.dart';

/// What the engine has open: a disk or an image file, possibly without the
/// device itself (a session loaded for browsing).
@immutable
final class OpenedSource {
  const OpenedSource({
    required this.info,
    this.table,
    this.partitions = const [],
    this.warnings = const [],
    this.attached = true,
    this.attachError,
  });

  final SourceInfo info;

  /// The partition table; `null` when the source came with a loaded session
  /// (the engine reports partitions only for `open_source`).
  final PartitionTableKind? table;
  final List<Partition> partitions;

  /// The engine's warnings about the source: TRIM, mounted volume, page
  /// file, system disk, the app itself on the source.
  final List<Warning> warnings;

  /// `false`: the recorded source of a session that could not be attached.
  /// The tree can be browsed; recovery, previews, carving and resuming need
  /// [SourceController.attachSource] first.
  final bool attached;

  /// Why the source is not attached, if the engine said so.
  final EngineError? attachError;

  OpenedSource _detached(EngineError? error) => OpenedSource(
    info: info,
    table: table,
    partitions: partitions,
    warnings: warnings,
    attached: false,
    attachError: error,
  );
}

/// Opens and closes sources and sessions and keeps what the Source screen
/// shows.
///
/// Every method that talks to the engine rethrows the engine's
/// [EngineException] (`access_denied`, `not_found`, `source_mismatch`,
/// `busy`, ...) after leaving this controller in a consistent state.
class SourceController extends ChangeNotifier {
  SourceController(
    this._api,
    this._catalog,
    this._platform, {
    this._task,
  });

  final EngineApi _api;
  final CatalogController _catalog;
  final PlatformServices _platform;
  final ValueNotifier<TaskProgress?>? _task;

  List<Disk>? _disks;
  bool _loadingDisks = false;
  EngineException? _disksError;
  OpenedSource? _current;
  String? _sessionPath;
  DestinationCheck? _appDataCheck;
  int _busy = 0;

  /// Bumped whenever the source changes, so that answers to checks made for
  /// an earlier source are ignored.
  int _sourceGeneration = 0;

  // --- disks ---------------------------------------------------------------

  /// The physical disks, `null` until [refreshDisks] has answered.
  List<Disk>? get disks => _disks;

  bool get isLoadingDisks => _loadingDisks;

  /// Why the last [refreshDisks] failed.
  EngineException? get disksError => _disksError;

  /// Asks the engine for the disks of this machine (`list_disks`).
  Future<void> refreshDisks() async {
    if (_loadingDisks) return;
    _loadingDisks = true;
    notifyListeners();
    try {
      _disks = await _api.listDisks();
      _disksError = null;
    } on EngineException catch (e) {
      _disksError = e;
    } finally {
      _loadingDisks = false;
      notifyListeners();
    }
  }

  // --- the open source -----------------------------------------------------

  /// The open source, `null` if none.
  OpenedSource? get current => _current;

  /// An open, close, load or attach request is under way.
  bool get isBusy => _busy > 0;

  /// The session file in use: the one the session was loaded from, was last
  /// saved to, or the running scan saves to. `null` if the session is only in
  /// memory. It survives the loss of the engine, so the UI can offer to load
  /// it again.
  String? get sessionPath => _sessionPath;

  Future<T> _exclusive<T>(Future<T> Function() action) async {
    _busy++;
    notifyListeners();
    try {
      return await action();
    } finally {
      _busy--;
      notifyListeners();
    }
  }

  void _opened(OpenedSource source, {required String? sessionPath}) {
    _sourceGeneration++;
    _current = source;
    _sessionPath = sessionPath;
    _appDataCheck = null;
    if (source.attached) _checkAppData();
  }

  /// Opens the physical disk [disk] for reading (`open_source`).
  Future<OpenSourceResult> openDisk(Disk disk) =>
      _open(SourceKind.disk, disk.path, null);

  /// Opens a raw image file (`open_source`). [sectorSize]: 512 or 4096 if
  /// detection should be overridden.
  Future<OpenSourceResult> openImage(String path, {int? sectorSize}) =>
      _open(SourceKind.image, path, sectorSize);

  Future<OpenSourceResult> _open(SourceKind kind, String path, int? sectorSize) =>
      _exclusive(() async {
        final result = await _api.openSource(
          kind: kind,
          path: path,
          sectorSize: sectorSize,
        );
        // "Closes any previously open source and drops its session and
        // catalog."
        _catalog.clear();
        _opened(
          OpenedSource(
            info: result.source,
            table: result.table,
            partitions: result.partitions,
            warnings: result.warnings,
          ),
          sessionPath: null,
        );
        return result;
      });

  /// Closes the source and drops its session and catalog (`close_source`).
  Future<void> close() => _exclusive(() async {
    await _api.closeSource();
    _catalog.clear();
    _sourceGeneration++;
    _current = null;
    _sessionPath = null;
    _appDataCheck = null;
  });

  /// Opens the saved session at [path] (`session_load`) and adopts its
  /// catalog. The engine attaches the recorded source if it finds it;
  /// [source] names one to open instead. Can take long: progress arrives as
  /// `task_progress`.
  Future<SessionLoadResult> loadSession(String path, {SourceRef? source}) =>
      _exclusive(() async {
        try {
          final result = await _api.sessionLoad(path: path, source: source);
          _opened(
            OpenedSource(
              info: result.source,
              attached: result.sourceAttached,
              attachError: result.sourceError,
            ),
            sessionPath: path,
          );
          _catalog.adopt(result.catalog);
          return result;
        } finally {
          _task?.value = null;
        }
      });

  /// Attaches a source to the loaded session (`session_attach_source`), after
  /// a load without source or after `source_lost`. The catalog is rebuilt
  /// (new epoch). `source_mismatch`: it is not the scanned disk.
  Future<SessionAttachResult> attachSource({
    required SourceKind kind,
    required String path,
    bool force = false,
  }) => _exclusive(() async {
    final result = await _api.sessionAttachSource(
      kind: kind,
      path: path,
      force: force,
    );
    // What open_source said about this very source is still true; for
    // another path the engine has reported no partitions.
    final previous = _current;
    final same = previous != null && previous.info.path == result.source.path;
    _opened(
      OpenedSource(
        info: result.source,
        table: same ? previous.table : null,
        partitions: same ? previous.partitions : const [],
        warnings: same ? previous.warnings : const [],
      ),
      sessionPath: _sessionPath,
    );
    _catalog.adopt(result.catalog);
    return result;
  });

  /// Writes a compacted copy of the session to [path] (`session_save`),
  /// which becomes the session file in use.
  Future<SessionSaveResult> saveSession(
    String path, {
    bool allowSameDisk = false,
    bool allowUnknownDisk = false,
  }) async {
    final result = await _api.sessionSave(
      path: path,
      allowSameDisk: allowSameDisk,
      allowUnknownDisk: allowUnknownDisk,
    );
    _sessionPath = result.path;
    notifyListeners();
    return result;
  }

  /// A scan saves its session to [path] (called by the scan controller).
  void noteSessionPath(String path) {
    if (_sessionPath == path) return;
    _sessionPath = path;
    notifyListeners();
  }

  /// The source stopped answering (a job ended with `source_lost`): it stays
  /// known but is no longer attached.
  void sourceLost(EngineError? error) {
    final current = _current;
    if (current == null || !current.attached) return;
    _current = current._detached(error);
    notifyListeners();
  }

  /// The engine process is gone: it has no source any more. [sessionPath] is
  /// kept.
  void engineLost() {
    _sourceGeneration++;
    // (Requests that were under way fail by themselves and end their own
    // busy state.)
    _current = null;
    _appDataCheck = null;
    _disks = null;
    _disksError = null;
    _task?.value = null;
    notifyListeners();
  }

  // --- the app's own data (S2) ---------------------------------------------

  /// The directory proposed for session files.
  String get defaultSessionDirectory => _platform.defaultSessionDirectory;

  /// The destination guard's verdict on [defaultSessionDirectory] for the
  /// open source (purpose `app_data`); `null` while unknown or without an
  /// attached source.
  ///
  /// The UI must not create anything of its own inside the session range
  /// (docs/ARCHITECTURE.md, S2), so it asks whenever a source is opened.
  DestinationCheck? get appDataCheck => _appDataCheck;

  /// Whether the app may create [defaultSessionDirectory] and write there
  /// without asking the user: only with the verdict `ok`.
  bool get canWriteAppData => _appDataCheck?.isOk ?? false;

  void _checkAppData() {
    final generation = _sourceGeneration;
    _api
        .checkDestination(
          path: _platform.defaultSessionDirectory,
          purpose: DestinationPurpose.appData,
        )
        .then<void>(
          (check) {
            if (generation != _sourceGeneration) return;
            _appDataCheck = check;
            notifyListeners();
          },
          // Without a verdict nothing is written: `canWriteAppData` stays
          // false.
          onError: (Object _) {},
        );
  }

  /// Creates [defaultSessionDirectory] if the destination guard allows it
  /// ([canWriteAppData]). Returns whether the directory may be used.
  Future<bool> ensureSessionDirectory() async {
    if (!canWriteAppData) return false;
    await _platform.createDirectory(_platform.defaultSessionDirectory);
    return true;
  }

  /// A session file path in [defaultSessionDirectory] for the open source,
  /// for example `...\Sessions\Disk 1 WDC WD40EZRZ 20261124-031500.hdsc`.
  /// `null` without a source.
  String? suggestSessionPath({DateTime? now}) {
    final source = _current?.info;
    if (source == null) return null;
    final time = (now ?? DateTime.now()).toUtc();
    String two(int value) => value.toString().padLeft(2, '0');
    final stamp =
        '${time.year}${two(time.month)}${two(time.day)}-'
        '${two(time.hour)}${two(time.minute)}${two(time.second)}';
    final String what;
    if (source.kind == SourceKind.disk) {
      what = 'Disk ${source.diskNumber ?? ''} ${source.model ?? ''}';
    } else {
      final cut = source.path.lastIndexOf(RegExp(r'[\\/]'));
      what = 'Image ${source.path.substring(cut + 1)}';
    }
    // Only characters that are safe in a Windows file name.
    final safe = what
        .replaceAll(RegExp(r'[<>:"/\\|?*\x00-\x1f]'), ' ')
        .replaceAll(RegExp(r'\s+'), ' ')
        .trim();
    final separator = _platform.pathSeparator;
    return '${_platform.defaultSessionDirectory}$separator$safe $stamp.hdsc';
  }
}
