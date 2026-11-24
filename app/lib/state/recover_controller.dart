/// The recovery: its options, what the selection amounts to, the
/// destination check and the job.
library;

import 'dart:async';

import 'package:flutter/foundation.dart';

import '../engine/engine.dart';
import 'catalog_controller.dart';
import 'job_controller.dart';
import 'source_controller.dart';

/// How the selected files are recovered.
@immutable
final class RecoverOptions {
  const RecoverOptions({
    this.includeDeleted = false,
    this.skipDamaged = false,
    this.onExists = OnExists.rename,
    this.filter,
    this.allowSameDisk = false,
    this.allowUnknownDisk = false,
  });

  /// Also recover selected files that had been deleted before the format.
  final bool includeDeleted;

  /// Do not write files rated `damaged`.
  final bool skipDamaged;

  /// What to do with files that already exist at the destination.
  final OnExists onExists;

  /// Recover only the selected files that match; `null`: all of them.
  final FileFilter? filter;

  /// The user's explicit consent to a destination on the source disk outside
  /// the scanned range (verdict `same_disk`).
  final bool allowSameDisk;

  /// The user's explicit consent to a destination whose disk cannot be
  /// determined (verdict `unknown_disk`).
  final bool allowUnknownDisk;

  RecoverOptions copyWith({
    bool? includeDeleted,
    bool? skipDamaged,
    OnExists? onExists,
    FileFilter? Function()? filter,
    bool? allowSameDisk,
    bool? allowUnknownDisk,
  }) => RecoverOptions(
    includeDeleted: includeDeleted ?? this.includeDeleted,
    skipDamaged: skipDamaged ?? this.skipDamaged,
    onExists: onExists ?? this.onExists,
    filter: filter == null ? this.filter : filter(),
    allowSameDisk: allowSameDisk ?? this.allowSameDisk,
    allowUnknownDisk: allowUnknownDisk ?? this.allowUnknownDisk,
  );

  @override
  bool operator ==(Object other) =>
      other is RecoverOptions &&
      other.includeDeleted == includeDeleted &&
      other.skipDamaged == skipDamaged &&
      other.onExists == onExists &&
      other.filter == filter &&
      other.allowSameDisk == allowSameDisk &&
      other.allowUnknownDisk == allowUnknownDisk;

  @override
  int get hashCode => Object.hash(
    includeDeleted,
    skipDamaged,
    onExists,
    filter,
    allowSameDisk,
    allowUnknownDisk,
  );
}

/// Everything around `recover_start`.
///
/// * [summary] follows the selection and the [options] automatically
///   (`selection_summary`; requests that a newer change has made obsolete are
///   cancelled).
/// * [checkDestination] asks the destination guard and keeps its verdict.
/// * [start] starts the job; progress and outcome are in the inherited
///   members of [JobController].
class RecoverController
    extends JobController<RecoverStartResult, RecoverProgress, RecoverDone> {
  RecoverController(super.api, this._catalog, this._source, {super.tracker})
    : super(kind: JobKind.recover) {
    _catalog.selection.addListener(_scheduleSummary);
    _catalog.addListener(_scheduleSummary);
  }

  final CatalogController _catalog;
  final SourceController _source;

  static const SelectionSummary _nothing = SelectionSummary(
    files: 0,
    bytes: 0,
    dirs: 0,
    good: 0,
    uncertain: 0,
    damaged: 0,
    unsupported: 0,
  );

  RecoverOptions _options = const RecoverOptions();
  SelectionSummary? _summary = _nothing;
  EngineException? _summaryError;
  CancelToken? _summaryRequest;
  bool _summaryScheduled = false;
  bool _disposed = false;

  String? _checkedDestination;
  DestinationCheck? _destinationCheck;
  CancelToken? _destinationRequest;

  // --- options and summary -------------------------------------------------

  RecoverOptions get options => _options;

  set options(RecoverOptions value) {
    if (value == _options) return;
    _options = value;
    _scheduleSummary();
    notifyListeners();
  }

  /// What recovering the current selection with the current [options] would
  /// do. `null` while it is being computed (see [isSummaryLoading]) or after
  /// a failure (see [summaryError]). All zeros when nothing is selected.
  SelectionSummary? get summary => _summary;

  bool get isSummaryLoading => _summaryRequest != null || _summaryScheduled;

  /// Why the summary could not be computed, for example `not_found` when a
  /// selected node no longer exists.
  EngineException? get summaryError => _summaryError;

  /// Computes [summary] again (it is refreshed automatically when the
  /// selection, the options or the catalog change).
  void refreshSummary() => _scheduleSummary();

  void _scheduleSummary() {
    // Several changes in a row (a click that updates selection and options)
    // become one request.
    if (_summaryScheduled || _disposed) return;
    _summaryScheduled = true;
    scheduleMicrotask(() {
      _summaryScheduled = false;
      if (!_disposed) unawaited(_loadSummary());
    });
  }

  Future<void> _loadSummary() async {
    _summaryRequest?.cancel();
    _summaryRequest = null;
    final selection = _catalog.selection.spec;
    if (selection.isEmpty || !_catalog.hasCatalog) {
      // Nothing selected: known without asking.
      if (!identical(_summary, _nothing) || _summaryError != null) {
        _summary = _nothing;
        _summaryError = null;
        notifyListeners();
      }
      return;
    }
    final token = _summaryRequest = CancelToken();
    final options = _options;
    _summary = null;
    _summaryError = null;
    notifyListeners();
    try {
      final summary = await _catalog.guarded(
        (epoch) => api.selectionSummary(
          epoch: epoch,
          include: selection.include,
          exclude: selection.exclude,
          filter: options.filter,
          includeDeleted: options.includeDeleted,
          skipDamaged: options.skipDamaged,
          cancel: token,
        ),
      );
      if (!identical(_summaryRequest, token)) return;
      _summary = summary;
    } on RequestCancelledException {
      return;
    } on EngineException catch (e) {
      if (!identical(_summaryRequest, token)) return;
      _summaryError = e;
    }
    _summaryRequest = null;
    if (!_disposed) notifyListeners();
  }

  // --- destination ---------------------------------------------------------

  /// The path of the last [checkDestination] (or refused [start]).
  String? get checkedDestination => _checkedDestination;

  /// The destination guard's verdict on [checkedDestination]; `null` while
  /// unknown.
  DestinationCheck? get destinationCheck => _destinationCheck;

  bool get isCheckingDestination => _destinationRequest != null;

  /// Whether a recovery to a destination with verdict [check] can be started
  /// with the current [options]: `ok`, or a verdict the user has consented
  /// to. `overlaps_source` can never be started.
  bool allows(DestinationCheck check) => switch (check.verdict) {
    DestinationVerdict.ok => true,
    DestinationVerdict.sameDisk => _options.allowSameDisk,
    DestinationVerdict.unknownDisk => _options.allowUnknownDisk,
    _ => false,
  };

  /// Asks the destination guard about [path] (`check_destination` with
  /// purpose `recover` and the size of the current [summary]). Nothing is
  /// created. A check that a newer one has overtaken fails with
  /// [RequestCancelledException].
  Future<DestinationCheck> checkDestination(String path) async {
    _destinationRequest?.cancel();
    final token = _destinationRequest = CancelToken();
    _checkedDestination = path;
    _destinationCheck = null;
    notifyListeners();
    try {
      final check = await api.checkDestination(
        path: path,
        purpose: DestinationPurpose.recover,
        needBytes: _summary?.bytes,
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

  // --- the job -------------------------------------------------------------

  /// Starts recovering the selection to the folder [dest]
  /// (`recover_start`).
  ///
  /// The engine's own destination check is authoritative. When it refuses
  /// (`destination_refused`), the verdict becomes [destinationCheck] and the
  /// exception is rethrown.
  Future<RecoverStartResult> start({required String dest}) async {
    final selection = _catalog.selection.spec;
    final options = _options;
    try {
      return await begin(
        () => _catalog.guarded(
          (epoch) => api.recoverStart(
            epoch: epoch,
            include: selection.include,
            exclude: selection.exclude,
            filter: options.filter,
            dest: dest,
            includeDeleted: options.includeDeleted,
            skipDamaged: options.skipDamaged,
            allowSameDisk: options.allowSameDisk,
            allowUnknownDisk: options.allowUnknownDisk,
            onExists: options.onExists,
          ),
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
  void onAccepted() {
    // The engine re-evaluates what has been overwritten before it reads
    // anything, so ratings may have changed.
    _catalog.rowsChanged();
  }

  @override
  void onDone(RecoverDone done) {
    if (done.error?.kind == EngineErrorCode.sourceLost) {
      _source.sourceLost(done.error);
    }
    // Reading the files updates their flags (signature, bad sectors).
    unawaited(_catalog.refresh().catchError((Object _) {}));
  }

  @override
  void dispose() {
    _disposed = true;
    _catalog.selection.removeListener(_scheduleSummary);
    _catalog.removeListener(_scheduleSummary);
    _summaryRequest?.cancel();
    _destinationRequest?.cancel();
    super.dispose();
  }
}
