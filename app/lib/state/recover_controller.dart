/// The recovery: its options, what the selection amounts to, the
/// destination check and the job.
library;

import 'dart:async';

import 'package:flutter/foundation.dart';

import '../engine/engine.dart';
import 'catalog_controller.dart';
import 'destination_checker.dart';
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
/// * [checkDestination] asks the destination guard and keeps its verdict
///   current: it is asked for again when the catalog changes (new epoch,
///   other protected ranges, a geometry) and when the space the selection
///   needs changes ([destination]).
/// * [start] starts the job; progress and outcome are in the inherited
///   members of [JobController].
class RecoverController
    extends JobController<RecoverStartResult, RecoverProgress, RecoverDone> {
  RecoverController(super.api, this._catalog, this._source, {super.tracker})
    : super(kind: JobKind.recover) {
    destination = DestinationChecker(api, purpose: DestinationPurpose.recover)
      ..addListener(notifyListeners);
    _seenInfo = _catalog.info;
    _seenGuard = _catalog.guardRevision;
    _catalog.selection.addListener(_scheduleSummary);
    _catalog.addListener(_onCatalogChanged);
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

  /// The verdict on the destination folder, kept current. Its members are
  /// also offered here: [checkedDestination], [destinationCheck],
  /// [destinationError], [isCheckingDestination].
  late final DestinationChecker destination;

  RecoverOptions _options = const RecoverOptions();
  SelectionSummary? _summary = _nothing;
  EngineException? _summaryError;
  CancelToken? _summaryRequest;
  bool _summaryScheduled = false;
  bool _disposed = false;
  CatalogInfo? _seenInfo;
  int _seenGuard = 0;

  // --- options and summary -------------------------------------------------

  RecoverOptions get options => _options;

  set options(RecoverOptions value) {
    if (value == _options) return;
    final previous = _options;
    _options = value;
    // Only these change what `selection_summary` counts; the consents and
    // `onExists` do not.
    if (value.includeDeleted != previous.includeDeleted ||
        value.skipDamaged != previous.skipDamaged ||
        value.filter != previous.filter) {
      _scheduleSummary();
    }
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

  void _onCatalogChanged() {
    final info = _catalog.info;
    if (!identical(info, _seenInfo)) {
      _seenInfo = info;
      _scheduleSummary();
    }
    if (_catalog.guardRevision != _seenGuard) {
      _seenGuard = _catalog.guardRevision;
      _guardChanged();
    }
  }

  /// The destination guard may answer differently now.
  void _guardChanged() {
    if (_catalog.hasCatalog && _catalog.sourceAttached) {
      destination.recheck(needBytes: _summary?.bytes);
    } else {
      destination.invalidate();
    }
  }

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
      _summaryRequest = null;
      // The space the recovery needs has changed: ask about the destination
      // with the new number, so that `enough_space` stays true to it.
      if (destination.path != null &&
          summary.bytes != destination.needBytes &&
          _catalog.sourceAttached) {
        destination.recheck(needBytes: summary.bytes);
      }
    } on RequestCancelledException {
      return;
    } on EngineException catch (e) {
      if (!identical(_summaryRequest, token)) return;
      _summaryError = e;
      _summaryRequest = null;
    }
    if (!_disposed) notifyListeners();
  }

  // --- destination ---------------------------------------------------------

  /// The path of the last [checkDestination] (or refused [start]).
  String? get checkedDestination => destination.path;

  /// The destination guard's verdict on [checkedDestination]; `null` while
  /// it is unknown (being asked for, no catalog or no source attached, or
  /// the check failed: [destinationError]).
  DestinationCheck? get destinationCheck => destination.check;

  /// Why the last check of the destination failed.
  EngineException? get destinationError => destination.error;

  bool get isCheckingDestination => destination.isChecking;

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
  /// created. A check of another path that overtakes this one fails it with
  /// [RequestCancelledException]; a re-check of the same path (the catalog
  /// changed meanwhile) answers for it.
  Future<DestinationCheck> checkDestination(String path) =>
      destination.checkPath(path, needBytes: _summary?.bytes);

  /// Whether everything the UI can know of is in place for [start]: no job
  /// runs, a catalog with its source attached, a selection with something
  /// in it, and a destination whose current verdict [allows] the start
  /// (with enough space if the engine said so). The engine checks all of it
  /// again; its refusal is what counts.
  bool get readyToStart {
    final summary = _summary;
    final check = destination.check;
    return !isActive &&
        !(tracker?.isBusy ?? false) &&
        _catalog.hasCatalog &&
        _catalog.sourceAttached &&
        summary != null &&
        (summary.files > 0 || summary.dirs > 0) &&
        check != null &&
        allows(check) &&
        check.enoughSpace != false;
  }

  // --- the job -------------------------------------------------------------

  /// Starts recovering the selection to the folder [dest]
  /// (`recover_start`).
  ///
  /// The engine's own destination check is authoritative. When it refuses
  /// (`destination_refused`), its verdict becomes [destinationCheck] and the
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
      if (check != null) destination.adoptRefusal(dest, check);
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
    _catalog.removeListener(_onCatalogChanged);
    _summaryRequest?.cancel();
    destination.dispose();
    super.dispose();
  }
}
