/// The destination guard's verdict on the path the user chose, kept current.
library;

import 'dart:async';

import 'package:flutter/foundation.dart';

import '../engine/engine.dart';

/// Asks the engine's destination guard (`check_destination`) about one path
/// and keeps the verdict up to date.
///
/// The verdict depends on the open source and on the catalog: the guard
/// protects the session range and every lost volume the catalog knows, and
/// it counts the catalog files at the location (docs/ARCHITECTURE.md, S2;
/// PROTOCOL `check_destination`: "ask again after `scan_done`,
/// `session_load`, `session_attach_source`, `geometry_set` and
/// `instance_set`"). The owner therefore calls [recheck] when those change,
/// and [invalidate] when no answer can be had (no source attached). While a
/// verdict is not known, [check] is `null`: a screen must never show an old
/// "ok" as if it were current.
///
/// A newer check overtakes an older one: the older request is cancelled and
/// its answer dropped. Nothing is created at the destination.
///
/// The verdict only informs the user. The engine checks again when the job
/// starts, and that check is the one that counts ([adoptRefusal]).
class DestinationChecker extends ChangeNotifier {
  DestinationChecker(this._api, {required this.purpose});

  final EngineApi _api;

  /// The purpose sent with every check (`recover`, `image`, ...).
  final DestinationPurpose purpose;

  String? _path;
  int? _needBytes;
  DestinationCheck? _check;
  EngineException? _error;
  CancelToken? _request;
  Future<DestinationCheck>? _latest;
  CancelToken? _latestToken;
  bool _disposed = false;

  /// The path the verdict is about; `null` until a path was checked.
  String? get path => _path;

  /// The `need_bytes` the checks are made with (`null`: none, and
  /// `DestinationCheck.enoughSpace` is `null`).
  int? get needBytes => _needBytes;

  /// The verdict on [path]; `null` while it is being asked for, after
  /// [invalidate], and after a failure ([error]).
  DestinationCheck? get check => _check;

  /// Why the last check failed (for example `no_source`); `null` if it did
  /// not.
  EngineException? get error => _error;

  /// A check is on its way to the engine.
  bool get isChecking => _request != null;

  /// Asks about [path] (with [needBytes], the space the job needs) and
  /// returns the verdict.
  ///
  /// When a check of the same path overtakes this one (the catalog changed
  /// meanwhile), the returned future completes with that newer verdict. When
  /// a check of another path overtakes it, or [invalidate] or [adoptRefusal]
  /// ends it, it fails with [RequestCancelledException]: there is nothing to
  /// show for it. Engine errors are rethrown and kept in [error].
  Future<DestinationCheck> checkPath(String path, {int? needBytes}) {
    _path = path;
    _needBytes = needBytes;
    return _start();
  }

  /// Asks about the current [path] again; with [needBytes] when the space
  /// the job needs has changed. Does nothing before a path was checked.
  /// Failures are kept in [error], never thrown.
  void recheck({int? needBytes}) {
    if (_path == null || _disposed) return;
    if (needBytes != null) _needBytes = needBytes;
    _start().ignore();
  }

  /// The verdict cannot be had right now (no source attached, no catalog):
  /// forgets it and cancels a check on its way. [path] stays, so that a
  /// later [recheck] asks again.
  void invalidate() {
    final changed = _request != null || _check != null || _error != null;
    _forgetRequest();
    _check = null;
    _error = null;
    if (changed) notifyListeners();
  }

  /// The engine refused a job's destination (`destination_refused`): its
  /// own check of [path], made at the start of the job, becomes the verdict.
  void adoptRefusal(String path, DestinationCheck check) {
    _forgetRequest();
    _path = path;
    _check = check;
    _error = null;
    notifyListeners();
  }

  /// Cancels the check on its way; its caller gets a
  /// [RequestCancelledException] (no newer check answers for it).
  void _forgetRequest() {
    _latest = null;
    _latestToken = null;
    _request?.cancel();
    _request = null;
  }

  Future<DestinationCheck> _start() {
    final path = _path!;
    final needBytes = _needBytes;
    _request?.cancel();
    final token = _request = CancelToken();
    _check = null;
    _error = null;
    notifyListeners();
    final future = _run(path, needBytes, token);
    _latest = future;
    _latestToken = token;
    return future;
  }

  Future<DestinationCheck> _run(String path, int? needBytes, CancelToken token) async {
    try {
      final check = await _api.checkDestination(
        path: path,
        purpose: purpose,
        needBytes: needBytes,
        cancel: token,
      );
      if (identical(_request, token)) _check = check;
      return check;
    } on RequestCancelledException {
      // Overtaken. A newer check of the same path answers for this one.
      final latest = _latest;
      if (!_disposed && latest != null && _path == path && !identical(_latestToken, token)) {
        return latest;
      }
      rethrow;
    } on EngineException catch (e) {
      if (identical(_request, token)) _error = e;
      rethrow;
    } finally {
      if (identical(_request, token)) {
        _request = null;
        if (!_disposed) notifyListeners();
      }
    }
  }

  @override
  void dispose() {
    _disposed = true;
    _forgetRequest();
    super.dispose();
  }
}
