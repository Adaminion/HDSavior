/// The root of the application state.
library;

import 'dart:async';

import 'package:flutter/foundation.dart';

import '../engine/engine.dart';
import '../platform/file_pickers.dart';
import '../platform/platform_services.dart';
import 'browse_controller.dart';
import 'carve_controller.dart';
import 'catalog_controller.dart';
import 'engine_controller.dart';
import 'image_controller.dart';
import 'job_controller.dart';
import 'recover_controller.dart';
import 'scan_controller.dart';
import 'selection_model.dart';
import 'source_controller.dart';

/// The five screens of the one window (docs/ARCHITECTURE.md, section 6).
enum AppScreen { source, scan, results, recover, image }

/// Creates the engine client and every controller, and wires them together.
///
/// One instance lives for the life of the app (`main.dart` creates it with
/// the process transport; tests create it with a fake transport). Screens
/// reach all state through it and listen to the individual controllers,
/// which are `ChangeNotifier`s.
class AppController {
  AppController({
    required EngineTransport transport,
    required this.platform,
    this.pickers = const SystemFilePickers(),
    int pageSize = 200,
    int maxCachedPages = 64,
    Duration helloTimeout = const Duration(seconds: 15),
  }) : client = EngineClient(transport) {
    api = EngineApi(client);
    engine = EngineController(api, helloTimeout: helloTimeout);
    catalog = CatalogController(
      api,
      pageSize: pageSize,
      maxCachedPages: maxCachedPages,
      task: task,
    );
    source = SourceController(api, catalog, platform, task: task);
    browse = BrowseController(catalog);
    scan = ScanController(api, catalog, source, tracker: jobs);
    recover = RecoverController(api, catalog, source, tracker: jobs);
    carve = CarveController(api, catalog, source, tracker: jobs);
    image = ImageController(api, source, catalog, tracker: jobs);

    _subscriptions = [
      client.on<TaskProgress>().listen((event) => task.value = event),
      client.exits.listen(_onEngineExit),
    ];
  }

  /// Operating-system services (session directory, file manager).
  final PlatformServices platform;

  /// The file and folder dialogs.
  final FilePickers pickers;

  /// The connection to the engine process.
  final EngineClient client;

  /// The engine's commands as typed methods.
  late final EngineApi api;

  /// Engine process: status, version, restart.
  late final EngineController engine;

  /// Disks, the open source and its warnings, sessions.
  late final SourceController source;

  /// Catalog info and epoch; owns [selection] and the row cache.
  late final CatalogController catalog;

  /// Current folder, filters, sort order and the listings for them.
  late final BrowseController browse;

  late final ScanController scan;
  late final RecoverController recover;
  late final CarveController carve;
  late final ImageController image;

  /// Which job is active, if any (the engine runs one at a time).
  final JobTracker jobs = JobTracker();

  /// Progress of a long plain request (`session_load`, `geometry_set`);
  /// `null` when none is running.
  final ValueNotifier<TaskProgress?> task = ValueNotifier(null);

  /// The screen the window shows.
  final ValueNotifier<AppScreen> screen = ValueNotifier(AppScreen.source);

  late final List<StreamSubscription<Object?>> _subscriptions;
  bool _disposed = false;

  /// The selection for recovery (shorthand for `catalog.selection`).
  SelectionModel get selection => catalog.selection;

  /// The engine's `log` events (informational).
  Stream<LogEvent> get log => client.on<LogEvent>();

  /// Starts the engine and loads the list of disks. Never throws; failures
  /// show in [engine].
  Future<void> start() async {
    await engine.start();
    if (engine.isReady) await source.refreshDisks();
  }

  /// Starts a fresh engine after the old one died or hung, and loads the
  /// list of disks again. All engine-side state (source, session, catalog,
  /// jobs) is gone; `source.sessionPath` still names the last session file.
  Future<void> restartEngine({bool graceful = true}) async {
    await engine.restart(graceful: graceful);
    if (engine.isReady) await source.refreshDisks();
  }

  /// Ends the engine cleanly. Call before the app exits: a running scan
  /// saves its session, other jobs are cancelled and remove their unfinished
  /// file.
  ///
  /// The engine is killed only if it has not exited after [grace]. By
  /// default that is 10 seconds, and a minute while a scan runs: writing the
  /// session of a large disk takes a while, and a kill would cost the scan
  /// everything since its last automatic save.
  Future<void> shutdown({Duration? grace}) => engine.shutdown(
    grace: grace ?? (scan.isActive ? const Duration(minutes: 1) : const Duration(seconds: 10)),
  );

  void _onEngineExit(EngineExit exit) {
    // The engine's state died with it. (Job controllers notice by
    // themselves.)
    catalog.clear();
    source.engineLost();
    task.value = null;
  }

  /// Shuts the engine down and releases everything.
  Future<void> dispose() async {
    if (_disposed) return;
    _disposed = true;
    await client.dispose();
    for (final subscription in _subscriptions) {
      // Not awaited on purpose: cancelling a subscription of a stream that is
      // already closed returns a future of the root zone, and awaiting it
      // would leave a widget test's fake-async zone (and hang the test).
      unawaited(subscription.cancel());
    }
    scan.dispose();
    recover.dispose();
    carve.dispose();
    image.dispose();
    browse.dispose();
    source.dispose();
    catalog.dispose();
    engine.dispose();
    jobs.dispose();
    task.dispose();
    screen.dispose();
  }
}
