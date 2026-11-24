/// The scan job.
library;

import '../engine/engine.dart';
import 'catalog_controller.dart';
import 'job_controller.dart';
import 'source_controller.dart';

/// Starts, stops and resumes the scan and hands its result to the catalog.
///
/// While a scan runs there is no catalog: the catalog (with the selection
/// and all cached rows) is cleared as soon as the engine accepts
/// `scan_start`, and the catalog of `scan_done` is adopted when it arrives.
class ScanController
    extends JobController<ScanStartResult, ScanProgress, ScanDone> {
  ScanController(super.api, this._catalog, this._source, {super.tracker})
    : super(kind: JobKind.scan);

  final CatalogController _catalog;
  final SourceController _source;
  String? _sessionPath;

  /// Scans the byte range [range] of the open source (default: all of it).
  ///
  /// [sessionPath] is where the engine saves the session every 5 minutes and
  /// at the end; `null`: nothing is saved automatically. The engine checks it
  /// with the destination guard before it reads anything
  /// (`destination_refused`).
  Future<ScanStartResult> start({
    ByteRange? range,
    String? sessionPath,
    bool collectCarveHits = true,
    bool allowSameDisk = false,
    bool allowUnknownDisk = false,
  }) {
    _sessionPath = sessionPath;
    return begin(
      () => api.scanStart(
        start: range?.start,
        end: range?.end,
        sessionPath: sessionPath,
        collectCarveHits: collectCarveHits,
        allowSameDisk: allowSameDisk,
        allowUnknownDisk: allowUnknownDisk,
      ),
    );
  }

  /// Continues the loaded session's scan where it stopped (a stopped scan,
  /// or one that failed with `source_lost` once the source is attached
  /// again). [sessionPath] defaults to the file the session was loaded from
  /// or last saved to.
  Future<ScanStartResult> resume({
    String? sessionPath,
    bool allowSameDisk = false,
    bool allowUnknownDisk = false,
  }) {
    _sessionPath = sessionPath;
    return begin(
      () => api.scanStart(
        resume: true,
        sessionPath: sessionPath,
        allowSameDisk: allowSameDisk,
        allowUnknownDisk: allowUnknownDisk,
      ),
    );
  }

  /// "Stop and show results": the scan ends with status `stopped`, a catalog
  /// of what was read so far, and can be resumed later.
  Future<void> stop() => cancel();

  /// The phase of the running scan.
  ScanPhase? get scanPhase => isActive ? progress?.phase : null;

  /// Whether stopping still has an effect: the engine ignores `job_cancel`
  /// in phase `analyzing`.
  bool get canStop =>
      phase == JobPhase.running && progress?.phase != ScanPhase.analyzing;

  @override
  void onAccepted() {
    // "Any scan_start discards the current catalog at once."
    _catalog.clear();
    final path = _sessionPath;
    if (path != null) _source.noteSessionPath(path);
  }

  @override
  void onDone(ScanDone done) {
    final path = done.sessionPath;
    if (path != null) _source.noteSessionPath(path);
    final catalog = done.catalog;
    if (catalog != null) _catalog.adopt(catalog);
    if (done.error?.kind == EngineErrorCode.sourceLost) {
      _source.sourceLost(done.error);
    }
  }
}
