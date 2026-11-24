/// The imaging job (sector-by-sector copy of the source to a file).
library;

import '../engine/engine.dart';
import 'catalog_controller.dart';
import 'destination_checker.dart';
import 'job_controller.dart';
import 'source_controller.dart';

/// Checks the destination of an image and runs `image_start`. Imaging needs
/// an open source but no scan.
///
/// The verdict on the image file's location ([destination]) is kept
/// current: it is asked for again when another source is opened and when
/// the catalog changes (the guard protects the imaged range and the
/// session's protected ranges, docs/ALGORITHMS.md J), and it is unknown
/// while no source is attached.
class ImageController
    extends JobController<ImageStartResult, ImageProgress, ImageDone> {
  ImageController(super.api, this._source, this._catalog, {super.tracker})
    : super(kind: JobKind.image) {
    destination = DestinationChecker(api, purpose: DestinationPurpose.image)
      ..addListener(notifyListeners);
    _seenSource = _source.revision;
    _seenAttached = _source.current?.attached ?? false;
    _seenGuard = _catalog.guardRevision;
    _source.addListener(_onSourceOrCatalogChanged);
    _catalog.addListener(_onSourceOrCatalogChanged);
  }

  final SourceController _source;
  final CatalogController _catalog;

  /// The verdict on the image file's location, kept current. Its members
  /// are also offered here: [checkedDestination], [destinationCheck],
  /// [destinationError], [isCheckingDestination].
  late final DestinationChecker destination;

  int _seenSource = 0;
  bool _seenAttached = false;
  int _seenGuard = 0;
  String? _jobDestination;

  /// The image file the running (or last) job writes to, as given to
  /// [start].
  String? get jobDestination => _jobDestination;

  void _onSourceOrCatalogChanged() {
    final attached = _source.current?.attached ?? false;
    if (_source.revision == _seenSource &&
        attached == _seenAttached &&
        _catalog.guardRevision == _seenGuard) {
      return;
    }
    _seenSource = _source.revision;
    _seenAttached = attached;
    _seenGuard = _catalog.guardRevision;
    if (attached) {
      destination.recheck();
    } else {
      destination.invalidate();
    }
  }

  /// The path of the last [checkDestination] (or refused [start]).
  String? get checkedDestination => destination.path;

  /// The destination guard's verdict on [checkedDestination]; `null` while
  /// it is unknown (being asked for, no source attached, or the check
  /// failed: [destinationError]).
  DestinationCheck? get destinationCheck => destination.check;

  /// Why the last check of the destination failed.
  EngineException? get destinationError => destination.error;

  bool get isCheckingDestination => destination.isChecking;

  /// Asks the destination guard about the image file [path]
  /// (`check_destination` with purpose `image`). [needBytes]: the size of
  /// the range to copy. Nothing is created. A check of another path that
  /// overtakes this one fails it with [RequestCancelledException]; a
  /// re-check of the same path answers for it.
  Future<DestinationCheck> checkDestination(String path, {int? needBytes}) =>
      destination.checkPath(path, needBytes: needBytes);

  /// Whether a start can succeed with the verdict [check] and the user's
  /// consents: `ok`, or `same_disk` / `unknown_disk` with the matching
  /// consent. `overlaps_source` can never be started.
  bool allows(
    DestinationCheck check, {
    bool allowSameDisk = false,
    bool allowUnknownDisk = false,
  }) => switch (check.verdict) {
    DestinationVerdict.ok => true,
    DestinationVerdict.sameDisk => allowSameDisk,
    DestinationVerdict.unknownDisk => allowUnknownDisk,
    _ => false,
  };

  /// Copies the byte range [range] of the open source (default: all of it)
  /// to the file [dest] (`image_start`).
  ///
  /// Without [resume] an existing file is refused (`exists`); with it, an
  /// interrupted image of the same source is continued. A refused
  /// destination (`destination_refused`) becomes [destinationCheck].
  Future<ImageStartResult> start({
    ByteRange? range,
    required String dest,
    bool resume = false,
    bool allowSameDisk = false,
    bool allowUnknownDisk = false,
  }) async {
    if (!isActive) _jobDestination = dest;
    try {
      return await begin(
        () => api.imageStart(
          start: range?.start ?? 0,
          end: range?.end,
          dest: dest,
          resume: resume,
          allowSameDisk: allowSameDisk,
          allowUnknownDisk: allowUnknownDisk,
        ),
      );
    } on EngineException catch (e) {
      final check = e.destinationCheck;
      if (check != null) destination.adoptRefusal(dest, check);
      rethrow;
    }
  }

  @override
  void onDone(ImageDone done) {
    if (done.error?.kind == EngineErrorCode.sourceLost) {
      _source.sourceLost(done.error);
    }
  }

  @override
  void dispose() {
    _source.removeListener(_onSourceOrCatalogChanged);
    _catalog.removeListener(_onSourceOrCatalogChanged);
    destination.dispose();
    super.dispose();
  }
}
