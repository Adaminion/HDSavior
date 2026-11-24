/// The imaging job (sector-by-sector copy of the source to a file).
library;

import '../engine/engine.dart';
import 'job_controller.dart';
import 'source_controller.dart';

/// Checks the destination of an image and runs `image_start`. Imaging needs
/// an open source but no scan.
class ImageController
    extends JobController<ImageStartResult, ImageProgress, ImageDone> {
  ImageController(super.api, this._source, {super.tracker})
    : super(kind: JobKind.image);

  final SourceController _source;

  String? _checkedDestination;
  DestinationCheck? _destinationCheck;
  CancelToken? _destinationRequest;

  /// The path of the last [checkDestination] (or refused [start]).
  String? get checkedDestination => _checkedDestination;

  /// The destination guard's verdict on [checkedDestination]; `null` while
  /// unknown.
  DestinationCheck? get destinationCheck => _destinationCheck;

  bool get isCheckingDestination => _destinationRequest != null;

  /// Asks the destination guard about the image file [path]
  /// (`check_destination` with purpose `image`). [needBytes]: the size of
  /// the range to copy. Nothing is created.
  Future<DestinationCheck> checkDestination(String path, {int? needBytes}) async {
    _destinationRequest?.cancel();
    final token = _destinationRequest = CancelToken();
    _checkedDestination = path;
    _destinationCheck = null;
    notifyListeners();
    try {
      final check = await api.checkDestination(
        path: path,
        purpose: DestinationPurpose.image,
        needBytes: needBytes,
        cancel: token,
      );
      if (identical(_destinationRequest, token)) _destinationCheck = check;
      return check;
    } finally {
      if (identical(_destinationRequest, token)) {
        _destinationRequest = null;
        notifyListeners();
      }
    }
  }

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
      if (check != null) {
        _destinationRequest?.cancel();
        _destinationRequest = null;
        _checkedDestination = dest;
        _destinationCheck = check;
        notifyListeners();
      }
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
    _destinationRequest?.cancel();
    super.dispose();
  }
}
