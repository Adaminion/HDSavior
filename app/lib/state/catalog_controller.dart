/// The catalog of the current scan: its info, its epoch, and everything that
/// is only valid for one epoch (cached rows, the selection).
library;

import 'dart:async';

import 'package:flutter/foundation.dart';

import '../engine/engine.dart';
import 'row_cache.dart';
import 'selection_model.dart';

/// Holds the [CatalogInfo] and enforces the epoch rules of docs/PROTOCOL.md
/// on the client side:
///
/// * Node ids are valid for one epoch. When a catalog with a different epoch
///   is adopted (or the catalog goes away), all cached rows and the selection
///   are dropped.
/// * When the epoch stays but rows may have changed (carving, a new geometry,
///   a recovery), the cached rows are dropped and fetched again on demand;
///   the selection is kept.
/// * A `stale_catalog` answer to any request made through [guarded] triggers
///   a [refresh], which adopts the engine's current catalog.
/// * [guardRevision] tells when the destination guard's answers may have
///   changed, so that verdicts on destinations are asked for again.
class CatalogController extends ChangeNotifier {
  CatalogController(
    this._api, {
    int pageSize = 200,
    int maxCachedPages = 64,
    this._task,
  }) {
    rows = RowCache(
      fetch: _fetchPage,
      pageSize: pageSize,
      maxCachedPages: maxCachedPages,
    );
  }

  final EngineApi _api;
  final ValueNotifier<TaskProgress?>? _task;

  /// The selection for recovery. Cleared with every new epoch.
  final SelectionModel selection = SelectionModel();

  /// The paged listings of the current epoch.
  late final RowCache rows;

  CatalogInfo? _info;
  Future<void>? _refreshing;
  int _changes = 0;
  int _guardRevision = 0;

  /// The current catalog, `null` if there is none (nothing scanned yet, or a
  /// scan is running).
  CatalogInfo? get info => _info;

  bool get hasCatalog => _info != null;

  /// The epoch that requests with node ids must carry.
  int? get epoch => _info?.epoch;

  /// Whether a source is attached, so that files can be read (recovery,
  /// previews, carving).
  bool get sourceAttached => _info?.sourceAttached ?? false;

  /// Whether a refresh from the engine is under way.
  bool get isRefreshing => _refreshing != null;

  /// Counts the adoptions of a different epoch (and the loss of the
  /// catalog): everything that was built from node ids must be rebuilt when
  /// it changes.
  int get epochChanges => _changes;

  /// Changes whenever the destination guard may answer differently: a
  /// catalog arrived or went away, a new epoch began, the protected ranges
  /// changed, or a geometry was set. The guard protects the session range
  /// and every lost volume the catalog knows (docs/ARCHITECTURE.md, S2), and
  /// `check_destination` counts the catalog files at a location, so a
  /// verdict must be asked for again when this changes ("ask again after
  /// `scan_done`, `session_load`, `session_attach_source`, `geometry_set`
  /// and `instance_set`").
  int get guardRevision => _guardRevision;

  /// The ranges of the source that no destination may touch; empty without
  /// a catalog.
  List<ProtectedRange> get protectedRanges => _info?.protected ?? const [];

  /// Takes over [next] as the current catalog (from `scan_done`,
  /// `session_load`, `session_attach_source`, `geometry_set`,
  /// `instance_set`, `carve_done`, `catalog_info`).
  void adopt(CatalogInfo next) => _adopt(next, guardChanged: false);

  void _adopt(CatalogInfo next, {required bool guardChanged}) {
    final previous = _info;
    _info = next;
    final newEpoch = previous == null || previous.epoch != next.epoch;
    if (newEpoch) {
      _epochEnded();
    } else {
      rows.invalidate();
    }
    if (guardChanged ||
        newEpoch ||
        !listEquals(previous.protected, next.protected)) {
      _guardRevision++;
    }
    notifyListeners();
  }

  /// There is no catalog any more (a scan started, the source was closed,
  /// the engine is gone).
  void clear() {
    if (_info == null) return;
    _info = null;
    _epochEnded();
    _guardRevision++;
    notifyListeners();
  }

  void _epochEnded() {
    _changes++;
    selection.clear();
    rows.reset();
  }

  /// The rows of the current epoch may have changed (a job re-evaluated what
  /// was overwritten): cached rows are dropped; ids and selection stay.
  void rowsChanged() => rows.invalidate();

  /// The carved nodes were (or are being) replaced: selection entries inside
  /// `[Carved files]` may name nodes that no longer exist and are forgotten;
  /// cached rows are dropped.
  void carvedNodesReplaced() {
    final carved = <int>{
      for (final instance in _info?.instances ?? const <Instance>[])
        ?instance.virtual.carved,
    };
    if (carved.isNotEmpty) {
      selection.removeWhere(
        (id, ancestors) => carved.contains(id) || ancestors.any(carved.contains),
      );
    }
    rows.invalidate();
  }

  /// Asks the engine for its current catalog and adopts it. If the engine
  /// has none, the catalog is cleared. Concurrent calls share one request.
  Future<void> refresh() {
    final running = _refreshing;
    if (running != null) return running;
    final started = _refresh();
    _refreshing = started;
    notifyListeners();
    return started;
  }

  Future<void> _refresh() async {
    try {
      adopt(await _api.catalogInfo());
    } on EngineException catch (e) {
      if (e.kind == EngineErrorCode.noCatalog || e.isEngineGone) {
        clear();
      } else {
        rethrow;
      }
    } finally {
      _refreshing = null;
      notifyListeners();
    }
  }

  /// Runs a request that carries the catalog epoch.
  ///
  /// [request] receives the current epoch. If the engine answers
  /// `stale_catalog`, a [refresh] is started (the new catalog arrives through
  /// the listeners) and the error is rethrown: the caller's node ids are
  /// void. `no_catalog` clears the catalog. Without a catalog the request is
  /// not sent and fails with `no_catalog`.
  Future<T> guarded<T>(Future<T> Function(int epoch) request) async {
    final epoch = this.epoch;
    if (epoch == null) {
      throw EngineException.client(
        EngineErrorCode.noCatalog,
        'There is no catalog.',
      );
    }
    try {
      return await request(epoch);
    } on EngineException catch (e) {
      // Only if the refused epoch is still the one we believe in: otherwise
      // the news has already arrived.
      if (this.epoch == epoch) {
        if (e.kind == EngineErrorCode.staleCatalog) {
          unawaited(refresh().catchError((Object _) {}));
        } else if (e.kind == EngineErrorCode.noCatalog) {
          clear();
        }
      }
      rethrow;
    }
  }

  Future<RowsPage> _fetchPage(
    RowQuery query,
    int offset,
    int limit,
    CancelToken cancel,
  ) => guarded(
    (epoch) => switch (query) {
      ChildrenQuery() => _api.listChildren(
        epoch: epoch,
        node: query.node,
        offset: offset,
        limit: limit,
        sort: query.sort,
        desc: query.desc,
        dirsOnly: query.dirsOnly,
        showDeleted: query.showDeleted,
        exts: query.exts,
        ratings: query.ratings,
        cancel: cancel,
      ),
      SearchQuery() => _api.search(
        epoch: epoch,
        under: query.under,
        filter: query.filter,
        showDeleted: query.showDeleted,
        sort: query.sort,
        desc: query.desc,
        offset: offset,
        limit: limit,
        cancel: cancel,
      ),
    },
  );

  /// `node_info` for the details pane.
  Future<NodeInfo> nodeInfo(int node, {CancelToken? cancel}) => guarded(
    (epoch) => _api.nodeInfo(epoch: epoch, node: node, cancel: cancel),
  );

  /// The first [maxBytes] of a file's content, for the preview pane (see
  /// [EngineApi.readFileContent]).
  Future<FileContent> readContent(
    int node, {
    required int maxBytes,
    CancelToken? cancel,
  }) => guarded(
    (epoch) => _api.readFileContent(
      epoch: epoch,
      node: node,
      maxBytes: maxBytes,
      cancel: cancel,
    ),
  );

  /// `geometry_set`: overrides the geometry of the file system [instance]
  /// (an `Instance.id`; a pair equal to one of its `geometryCandidates`
  /// selects that candidate). The epoch stays; ratings and the protected
  /// ranges are recomputed, so rows are fetched again and destinations are
  /// checked again; carved files are discarded when the geometry of the main
  /// instance changes. Can take long: progress arrives as `task_progress`.
  Future<CatalogInfo> setGeometry({
    required int instance,
    required int clusterSize,
    required int volumeOffset,
  }) async {
    try {
      final carvedBefore = _info?.mainInstance?.virtual.carved;
      final info = await guarded(
        (epoch) => _api.geometrySet(
          epoch: epoch,
          instance: instance,
          clusterSize: clusterSize,
          volumeOffset: volumeOffset,
        ),
      );
      // "Carved nodes are discarded when the main instance or its geometry
      // changes": the `[Carved files]` folder is gone then.
      if (carvedBefore != null &&
          info.mainInstance?.virtual.carved != carvedBefore) {
        carvedNodesReplaced();
      }
      _adopt(info, guardChanged: true);
      return info;
    } finally {
      _task?.value = null;
    }
  }

  /// `instance_set`: overrules the engine's verdict about the file system
  /// [instance] (an `Instance.id`).
  ///
  /// * [InstanceOverride.main]: "this is the one I want" - it becomes the
  ///   main instance (`instances[0]`), the previous main one becomes
  ///   `other` (or, if it was main only by an earlier choice, returns to
  ///   the kind the engine gives it).
  /// * [InstanceOverride.other]: "treat as a separate volume" - an
  ///   `embedded` or `older` instance becomes a volume of its own whose
  ///   files are read with its own geometry and can be recovered.
  /// * [InstanceOverride.auto]: withdraws the override (`Instance.manual`).
  ///
  /// The catalog is rebuilt with a new epoch: the selection and every
  /// cached row are dropped and the browser returns to the main root.
  /// Carved files are discarded. `strays` cannot be changed (`bad_request`).
  Future<CatalogInfo> setInstanceKind({
    required int instance,
    required InstanceOverride kind,
  }) async {
    final info = await guarded(
      (epoch) => _api.instanceSet(epoch: epoch, instance: instance, kind: kind),
    );
    _adopt(info, guardChanged: true);
    return info;
  }

  @override
  void dispose() {
    selection.dispose();
    super.dispose();
  }
}
