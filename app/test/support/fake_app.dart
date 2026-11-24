/// An [AppController] on top of the fake engine, for state and widget tests.
library;

import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/engine/engine.dart';
import 'package:hdsavior/state/state.dart';

import '../fake_engine/fake_engine.dart';
import '../fake_engine/fake_transport.dart';
import 'fake_platform.dart';

export '../fake_engine/fake_engine.dart';
export '../fake_engine/fake_transport.dart';
export 'fake_platform.dart';

const String disk1 = r'\\.\PhysicalDrive1';

/// The app's state layer wired to a fake engine whose jobs only advance when
/// the test says so.
///
/// In a widget test (`testWidgets`) time is faked: a future from the engine
/// only completes while the test pumps. Never `await` an engine call there
/// directly. Create the fake with `FakeApp(settle: tester.pump)` and wrap the
/// calls in [run], which starts the call, lets the conversation with the
/// engine play out, and then returns the result:
///
///     await fake.run(() => fake.app.scan.start());
///     await fake.step(3);
///     await tester.pump(); // rebuild the widgets
///
/// The helpers of this class ([start], [openDisk], [scan], [step],
/// [finishJob]) all work that way, in plain tests and in widget tests.
final class FakeApp {
  FakeApp({
    int mainNodes = 3000,
    bool elevated = true,
    int protocol = 1,
    int pageSize = 200,
    int maxCachedPages = 64,
    Duration? tick,
    Future<void> Function()? settle,
  }) : transport = FakeEngineTransport(
         options: FakeEngineOptions(mainNodes: mainNodes, tick: tick, elevated: elevated, protocol: protocol),
       ),
       platform = InertPlatformServices(),
       _settle = settle ?? pumpEventQueue {
    app = AppController(
      transport: transport,
      platform: platform,
      pickers: pickers,
      pageSize: pageSize,
      maxCachedPages: maxCachedPages,
    );
  }

  final FakeEngineTransport transport;
  final InertPlatformServices platform;

  /// The dialogs: put the paths the "user" chooses into `pickers.answers`.
  final ScriptedFilePickers pickers = ScriptedFilePickers();
  final Future<void> Function() _settle;
  late final AppController app;

  /// The fake engine process that currently runs.
  FakeEngine get engine => transport.engine;

  FakeScript get script => transport.script;

  /// The requests the engine has received, as command names.
  List<String> get commands => [for (final request in engine.requests) request.command];

  /// The arguments of the last request of [command].
  Map<String, Object?> lastArgs(String command) =>
      engine.requests.lastWhere((request) => request.command == command).args;

  /// Lets the conversation with the engine play out.
  Future<void> settle() => _settle();

  /// Starts [action], lets the conversation with the engine play out and
  /// returns the result of [action] (or throws its error).
  Future<T> run<T>(Future<T> Function() action) async {
    final future = action();
    future.ignore(); // an error is reported when the future is awaited below
    await _settle();
    return future;
  }

  /// Starts the engine and lets everything settle.
  Future<void> start() => run(app.start);

  /// Opens disk 1, the freshly formatted 4 TB disk.
  Future<void> openDisk() => run(() => app.source.openDisk(app.source.disks![1]));

  /// Advances the running job by [steps] steps, letting the app react after
  /// each.
  Future<void> step([int steps = 1]) async {
    for (var i = 0; i < steps; i++) {
      engine.advance();
      await _settle();
    }
  }

  /// Advances until no job runs any more.
  Future<void> finishJob() async {
    for (var i = 0; i < 500 && engine.runningJob != null; i++) {
      await step();
    }
    expect(engine.runningJob, isNull, reason: 'the job did not end');
    await _settle();
  }

  /// Opens disk 1 and scans its partition to the end.
  Future<CatalogInfo> scan({String? sessionPath}) async {
    await openDisk();
    final partition = app.source.current!.partitions.single;
    await run(() => app.scan.start(range: partition.range, sessionPath: sessionPath));
    await finishJob();
    return app.catalog.info!;
  }

  FakeLandmarks get landmarks => engine.catalog!.tree.landmarks;

  /// Shuts the engine down and releases the controllers.
  Future<void> dispose() => run(app.dispose);
}

/// Matches an [EngineException] of [kind].
TypeMatcher<EngineException> engineError(EngineErrorCode kind) =>
    isA<EngineException>().having((e) => e.kind, 'kind', kind);
