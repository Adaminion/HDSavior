/// The carving job ("Find more files by signature").
library;

import 'dart:async';

import '../engine/engine.dart';
import 'catalog_controller.dart';
import 'job_controller.dart';
import 'source_controller.dart';

/// Starts carving and hands the changed catalog back.
///
/// Carving keeps the catalog epoch: node ids and the selection stay valid,
/// except for carved nodes, which `carve_start` replaces. Selection entries
/// inside `[Carved files]` are therefore forgotten when a run starts.
class CarveController
    extends JobController<CarveStartResult, CarveProgress, CarveDone> {
  CarveController(super.api, this._catalog, this._source, {super.tracker})
    : super(kind: JobKind.carve);

  final CatalogController _catalog;
  final SourceController _source;

  /// Whether the scan collected signature hits; without them `carve_start`
  /// answers `unsupported`.
  bool get hasCarveHits => (_catalog.info?.carveHits ?? 0) > 0;

  /// Looks for files of [kinds] (`null`: every kind of `hello.carve_kinds`)
  /// where no file record survived (`carve_start`).
  Future<CarveStartResult> start({List<String>? kinds}) =>
      begin(() => api.carveStart(kinds: kinds));

  @override
  void onAccepted() => _catalog.carvedNodesReplaced();

  @override
  void onDone(CarveDone done) {
    if (done.error?.kind == EngineErrorCode.sourceLost) {
      _source.sourceLost(done.error);
    }
    final catalog = done.catalog;
    if (catalog != null) {
      _catalog.adopt(catalog);
    } else {
      unawaited(_catalog.refresh().catchError((Object _) {}));
    }
  }
}
