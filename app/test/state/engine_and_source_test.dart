// Engine life cycle and the source: start, failure, crash, restart; disks,
// open, sessions, attach, the app-data check.
import 'dart:async';

import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/engine/engine.dart';
import 'package:hdsavior/state/state.dart';

import '../support/fake_app.dart';
import '../support/scripted_transport.dart';

void main() {
  late FakeApp fake;
  AppController app() => fake.app;

  tearDown(() => fake.dispose());

  group('engine', () {
    test('start: the engine is asked for its version and the disks are listed', () async {
      fake = FakeApp();
      expect(app().engine.status, EngineStatus.stopped);
      final statuses = <EngineStatus>[];
      app().engine.addListener(() => statuses.add(app().engine.status));
      await fake.start();
      expect(statuses, [EngineStatus.starting, EngineStatus.ready]);
      expect(app().engine.isReady, isTrue);
      expect(app().engine.hello!.engineVersion, '0.1.0-fake');
      expect(app().engine.hello!.carveKinds, hasLength(17));
      expect(app().engine.error, isNull);
      expect(app().source.disks, hasLength(7));
      expect(fake.commands, ['hello', 'list_disks']);
    });

    test('an engine that cannot be started: failed, with the reason; a retry works', () async {
      fake = FakeApp();
      fake.transport.startError = StateError('hdsavior-engine.exe not found');
      await fake.start();
      expect(app().engine.status, EngineStatus.failed);
      expect(app().engine.error!.kind, EngineErrorCode.engineStartFailed);
      expect(app().engine.error!.message, contains('hdsavior-engine.exe not found'));
      expect(app().source.disks, isNull);

      fake.transport.startError = null;
      await app().restartEngine();
      expect(app().engine.status, EngineStatus.ready);
      expect(app().source.disks, hasLength(7));
    });

    test('an engine with another protocol version is not used', () async {
      fake = FakeApp(protocol: 2);
      await fake.start();
      expect(app().engine.status, EngineStatus.incompatible);
      expect(app().engine.isReady, isFalse);
      expect(app().engine.hello!.protocol, 2);
      expect(fake.commands, ['hello'], reason: 'nothing else is asked of it');
    });

    test('start is idempotent', () async {
      fake = FakeApp();
      await Future.wait([app().start(), app().start(), app().engine.start()]);
      await app().start();
      expect(fake.transport.starts, 1);
      expect(fake.commands.where((c) => c == 'hello'), hasLength(1));
    });

    test('a crash: everything the engine knew is gone, and a fresh engine can be started', () async {
      fake = FakeApp();
      await fake.start();
      await fake.scan(sessionPath: r'C:\Users\Rescuer\AppData\Local\HDSavior\Sessions\a.hdsc');
      app().selection.setChecked(app().catalog.info!.mainInstance!.root, const [], true);
      final rows = app().browse.rows!;
      expect(app().source.current, isNotNull);

      final first = fake.engine;
      first.crash(exitCode: 139);
      await pumpEventQueue();

      expect(app().engine.status, EngineStatus.failed);
      expect(app().engine.error!.kind, EngineErrorCode.engineExited);
      expect(app().engine.error!.message, contains('139'));
      expect(app().engine.lastExit!.exitCode, 139);
      expect(app().engine.lastExit!.stderrTail.last, contains('simulated crash'));
      expect(app().engine.hello, isNull);
      expect(app().source.current, isNull);
      expect(app().source.disks, isNull);
      expect(app().catalog.info, isNull);
      expect(app().selection.isEmpty, isTrue);
      expect(rows.isClosed, isTrue);
      expect(app().browse.rows, isNull);
      expect(app().source.sessionPath, endsWith('a.hdsc'), reason: 'so that the UI can offer to load it again');
      await expectLater(app().source.refreshDisks(), completes);
      expect(app().source.disksError!.kind, EngineErrorCode.engineNotRunning);

      await app().restartEngine();
      expect(app().engine.status, EngineStatus.ready);
      expect(identical(fake.engine, first), isFalse);
      expect(app().source.disks, hasLength(7));
      expect(app().source.disksError, isNull);
      // The session file survived the crash and can be loaded again.
      final loaded = await app().source.loadSession(app().source.sessionPath!);
      expect(loaded.catalog.scanComplete, isTrue);
      expect(app().catalog.info, isNotNull);
    });

    test('restart of a healthy engine shuts the old one down first', () async {
      fake = FakeApp();
      await fake.start();
      final first = fake.engine;
      await app().restartEngine();
      expect(first.requests.last.command, 'shutdown');
      expect(await first.exitCode, 0);
      expect(app().engine.status, EngineStatus.ready);
      expect(fake.transport.starts, 2);
    });

    test('an engine that does not exit is given time, longer while a scan runs, and then killed', () async {
      final transport = ScriptedTransport();
      final scripted = AppController(transport: transport, platform: InertPlatformServices());
      addTearDown(scripted.dispose);
      unawaited(scripted.start());
      await pumpEventQueue();
      final connection = transport.connection..exitCodeOnInputClosed = null;

      final watch = Stopwatch()..start();
      await scripted.shutdown(grace: const Duration(milliseconds: 60));
      expect(watch.elapsedMilliseconds, greaterThanOrEqualTo(50));
      expect(connection.requests.last['cmd'], 'shutdown');
      expect(connection.inputClosed, isTrue);
      expect(connection.killed, isTrue);
      expect(scripted.engine.status, EngineStatus.stopped);
    });

    test('shutdown: the engine exits cleanly and the status says stopped', () async {
      fake = FakeApp();
      await fake.start();
      final engine = fake.engine;
      await app().shutdown();
      expect(engine.requests.last.command, 'shutdown');
      expect(await engine.exitCode, 0);
      expect(app().engine.status, EngineStatus.stopped);
      expect(app().engine.error, isNull, reason: 'an expected exit is not a failure');
    });
  });

  group('disks and open_source', () {
    setUp(() async {
      fake = FakeApp();
      await fake.start();
    });

    test('opening a disk: source, partitions and warnings; the catalog is gone', () async {
      await fake.scan();
      expect(app().catalog.hasCatalog, isTrue);
      final result = await app().source.openDisk(app().source.disks![0]);
      expect(result.source.diskNumber, 0);
      final current = app().source.current!;
      expect(current.info.path, r'\\.\PhysicalDrive0');
      expect(current.attached, isTrue);
      expect(current.table, PartitionTableKind.gpt);
      expect(current.partitions, hasLength(4));
      expect(current.warnings.map((w) => w.kind).toSet(), {
        WarningCode.trimSupported,
        WarningCode.mountedVolume,
        WarningCode.systemDisk,
        WarningCode.pageFileOnSource,
        WarningCode.appOnSource,
      });
      expect(app().catalog.hasCatalog, isFalse, reason: 'open_source drops the session and the catalog');
      expect(app().source.sessionPath, isNull);
      expect(fake.lastArgs('open_source'), {'kind': 'disk', 'path': r'\\.\PhysicalDrive0'});
    });

    test('a refused open leaves the current source as it was', () async {
      await fake.openDisk();
      await expectLater(app().source.openDisk(app().source.disks![5]), throwsA(engineError(EngineErrorCode.ioError)));
      await expectLater(app().source.openImage(r'D:\missing.img'), throwsA(engineError(EngineErrorCode.notFound)));
      expect(app().source.current!.info.diskNumber, 1);
      expect(app().source.isBusy, isFalse);
    });

    test('access_denied when the engine is not elevated', () async {
      await fake.dispose();
      fake = FakeApp(elevated: false);
      await fake.start();
      expect(app().engine.hello!.elevated, isFalse);
      await expectLater(app().source.openDisk(app().source.disks![1]), throwsA(engineError(EngineErrorCode.accessDenied)));
      expect(app().source.current, isNull);
    });

    test('opening an image, with a sector size', () async {
      final result = await app().source.openImage(r'D:\images\old disk.img', sectorSize: 4096);
      expect(result.source.kind, SourceKind.image);
      expect(app().source.current!.info.sectorSize, 4096);
      expect(app().source.current!.info.model, isNull);
      expect(fake.lastArgs('open_source'), {'kind': 'image', 'path': r'D:\images\old disk.img', 'sector_size': 4096});
    });

    test('isBusy while a request is under way', () async {
      final seen = <bool>[];
      app().source.addListener(() => seen.add(app().source.isBusy));
      await fake.openDisk();
      expect(seen.first, isTrue);
      expect(app().source.isBusy, isFalse);
    });

    test('the engine dying during an open: the error comes out and "busy" ends', () async {
      fake.engine.exitOn('open_source');
      await expectLater(
        app().source.openDisk(app().source.disks![1]),
        throwsA(engineError(EngineErrorCode.engineExited)),
      );
      await pumpEventQueue();
      expect(app().source.isBusy, isFalse);
      expect(app().source.current, isNull);

      await app().restartEngine();
      final busy = <bool>[];
      app().source.addListener(() => busy.add(app().source.isBusy));
      await fake.openDisk();
      expect(busy, contains(true));
      expect(app().source.isBusy, isFalse);
    });

    test('close', () async {
      await fake.scan();
      await app().source.close();
      expect(app().source.current, isNull);
      expect(app().catalog.hasCatalog, isFalse);
      expect(app().source.appDataCheck, isNull);
      expect(fake.commands.last, 'close_source');
    });

    test('refreshDisks reports a failure and recovers', () async {
      fake.engine.failNext('list_disks', 'internal', message: 'enumeration failed');
      await app().source.refreshDisks();
      expect(app().source.disksError!.message, 'enumeration failed');
      expect(app().source.disks, hasLength(7), reason: 'the previous list stays');
      await app().source.refreshDisks();
      expect(app().source.disksError, isNull);
      expect(app().source.isLoadingDisks, isFalse);
    });
  });

  group("the app's own data (S2)", () {
    setUp(() async {
      fake = FakeApp();
      await fake.start();
    });

    test('the session directory is checked with purpose app_data whenever a source is opened', () async {
      expect(app().source.appDataCheck, isNull);
      expect(app().source.canWriteAppData, isFalse);
      await fake.openDisk();
      expect(fake.lastArgs('check_destination'), {
        'path': fake.platform.defaultSessionDirectory,
        'purpose': 'app_data',
      });
      expect(app().source.appDataCheck!.verdict, DestinationVerdict.ok);
      expect(app().source.canWriteAppData, isTrue);
      expect(await app().source.ensureSessionDirectory(), isTrue);
      expect(fake.platform.createdDirectories, [fake.platform.defaultSessionDirectory]);
    });

    test('nothing is created when the directory is on the disk being rescued', () async {
      // Disk 0 holds C:, where the session directory is.
      await app().source.openDisk(app().source.disks![0]);
      await pumpEventQueue();
      expect(app().source.appDataCheck!.verdict, DestinationVerdict.overlapsSource);
      expect(app().source.canWriteAppData, isFalse);
      expect(await app().source.ensureSessionDirectory(), isFalse);
      expect(fake.platform.createdDirectories, isEmpty);
    });

    test('without a verdict nothing is created either', () async {
      fake.engine.failNext('check_destination', 'internal');
      await fake.openDisk();
      expect(app().source.appDataCheck, isNull);
      expect(await app().source.ensureSessionDirectory(), isFalse);
      expect(fake.platform.createdDirectories, isEmpty);
    });

    test('a check for an earlier source does not count for the next one', () async {
      // Open the system disk and, before its check is answered, disk 1.
      final first = app().source.openDisk(app().source.disks![0]);
      final second = app().source.openDisk(app().source.disks![1]);
      await Future.wait([first, second]);
      await pumpEventQueue();
      expect(app().source.current!.info.diskNumber, 1);
      expect(app().source.appDataCheck!.verdict, DestinationVerdict.ok);
    });

    test('suggestSessionPath names the source and the time', () async {
      expect(app().source.suggestSessionPath(), isNull);
      await fake.openDisk();
      expect(
        app().source.suggestSessionPath(now: DateTime.utc(2026, 11, 24, 3, 15, 0)),
        '${fake.platform.defaultSessionDirectory}\\Disk 1 WDC WD40EZRZ-00GXCB0 20261124-031500.hdsc',
      );
      await app().source.openImage(r'D:\images\my: "old" <disk>?.img');
      final path = app().source.suggestSessionPath(now: DateTime.utc(2026, 1, 2, 3, 4, 5))!;
      final name = path.substring(fake.platform.defaultSessionDirectory.length + 1);
      expect(name, 'Image my old disk .img 20260102-030405.hdsc');
      expect(name, isNot(matches(r'[<>:"/\\|?*]')));
    });
  });

  group('sessions', () {
    setUp(() async {
      fake = FakeApp();
      await fake.start();
    });

    test('loading a session adopts its source and catalog', () async {
      final result = await app().source.loadSession(r'C:\sessions\holiday.hdsc');
      expect(result.sourceAttached, isTrue);
      expect(app().source.current!.info.path, disk1);
      expect(app().source.current!.attached, isTrue);
      expect(app().source.current!.table, isNull, reason: 'session_load does not report partitions');
      expect(app().source.sessionPath, r'C:\sessions\holiday.hdsc');
      expect(app().catalog.info!.epoch, result.catalog.epoch);
      expect(app().catalog.sourceAttached, isTrue);
      expect(app().browse.folder!.id, result.catalog.mainInstance!.root);
      await pumpEventQueue();
      expect(app().source.appDataCheck, isNotNull);
    });

    test('a session whose disk is not connected can be browsed, then attached', () async {
      final result = await app().source.loadSession(r'C:\sessions\detached.hdsc');
      expect(result.sourceAttached, isFalse);
      final current = app().source.current!;
      expect(current.attached, isFalse);
      expect(current.attachError!.message, contains('not connected'));
      expect(app().catalog.sourceAttached, isFalse);
      await pumpEventQueue();
      expect(app().source.appDataCheck, isNull, reason: 'no source to check against');
      final browsing = await app().catalog.rows.open(ChildrenQuery(node: app().browse.folder!.id)).let((list) async {
        list.load();
        await list.idle;
        return list.total;
      });
      expect(browsing, greaterThan(0));

      final epoch = app().catalog.epoch;
      app().selection.setChecked(app().browse.folder!.id, const [], true);
      await expectLater(
        app().source.attachSource(kind: SourceKind.disk, path: r'\\.\PhysicalDrive2-mismatch'),
        throwsA(engineError(EngineErrorCode.sourceMismatch)),
      );
      expect(app().source.current!.attached, isFalse);
      expect(app().selection.isEmpty, isFalse, reason: 'a refused attach changes nothing');

      final attached = await app().source.attachSource(kind: SourceKind.disk, path: disk1);
      expect(app().source.current!.attached, isTrue);
      expect(app().source.current!.attachError, isNull);
      expect(app().catalog.sourceAttached, isTrue);
      expect(attached.catalog.epoch, isNot(epoch));
      expect(app().catalog.epoch, attached.catalog.epoch);
      expect(app().selection.isEmpty, isTrue, reason: 'a new epoch: the old ids are void');
      expect(app().source.sessionPath, r'C:\sessions\detached.hdsc');
    });

    test('load errors leave everything as it was', () async {
      await fake.openDisk();
      for (final (name, code) in [
        ('missing', EngineErrorCode.notFound),
        ('corrupt', EngineErrorCode.ioError),
        ('newer', EngineErrorCode.unsupported),
      ]) {
        await expectLater(app().source.loadSession('C:\\s\\$name.hdsc'), throwsA(engineError(code)));
      }
      expect(app().source.current!.info.diskNumber, 1);
      expect(app().source.sessionPath, isNull);
      expect(app().source.isBusy, isFalse);
    });

    test('a long load reports its progress and clears it at the end', () async {
      fake.script.taskTicks = 4;
      final fractions = <double?>[];
      app().task.addListener(() => fractions.add(app().task.value?.fraction));
      final loading = app().source.loadSession(r'C:\sessions\big.hdsc');
      await pumpEventQueue();
      expect(app().source.isBusy, isTrue);
      await fake.step(2);
      expect(app().task.value!.task, TaskKind.sessionLoad);
      expect(app().task.value!.fraction, 0.5);
      await fake.step(2);
      await loading;
      expect(fractions, [0.25, 0.5, 0.75, 1.0, null]);
      expect(app().task.value, isNull);
      expect(app().source.isBusy, isFalse);
    });

    test('saving the session makes the copy the session file in use', () async {
      await fake.scan();
      expect(app().source.sessionPath, isNull);
      final saved = await app().source.saveSession(r'D:\backup\copy.hdsc');
      expect(saved.path, r'D:\backup\copy.hdsc');
      expect(app().source.sessionPath, r'D:\backup\copy.hdsc');
      await expectLater(
        app().source.saveSession(r'E:\on-the-source.hdsc'),
        throwsA(engineError(EngineErrorCode.destinationRefused).having((e) => e.destinationCheck!.verdict, 'verdict', DestinationVerdict.overlapsSource)),
      );
      expect(app().source.sessionPath, r'D:\backup\copy.hdsc');
    });
  });
}

extension<T> on T {
  R let<R>(R Function(T value) f) => f(this);
}
