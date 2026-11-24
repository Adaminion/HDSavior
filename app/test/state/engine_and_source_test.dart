// Engine life cycle and the source: start, failure, crash, restart; disks,
// open, sessions, attach, the app-data check.
import 'dart:async';

import 'package:flutter_test/flutter_test.dart';
import 'package:hdsavior/engine/engine.dart';
import 'package:hdsavior/state/state.dart';

import '../engine/protocol_examples.dart';
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

    test('a hello that cannot be read: failed, and the failure stays after the engine is ended', () async {
      final transport = ScriptedTransport();
      final scripted = AppController(transport: transport, platform: InertPlatformServices());
      addTearDown(scripted.dispose);
      final started = scripted.engine.start();
      await pumpEventQueue();
      final connection = transport.connection;
      expect(connection.lastRequest['cmd'], 'hello');
      // An engine of another build: its hello lacks members.
      connection.respond(connection.lastId, {'engine_version': '0.1.0'});
      await started;
      await pumpEventQueue();
      expect(connection.killed, isTrue, reason: 'an engine that cannot say hello is of no use');
      await Future<void>.delayed(const Duration(milliseconds: 50));
      expect(scripted.engine.status, EngineStatus.failed, reason: 'not the endless "Starting…"');
      expect(scripted.engine.error, isNotNull);
    });

    test('an engine that never answers hello: failed after the timeout', () async {
      final transport = ScriptedTransport();
      final scripted = AppController(
        transport: transport,
        platform: InertPlatformServices(),
        helloTimeout: const Duration(milliseconds: 50),
      );
      addTearDown(scripted.dispose);
      await scripted.engine.start();
      await pumpEventQueue();
      expect(transport.connection.killed, isTrue);
      expect(scripted.engine.status, EngineStatus.failed);
      expect(scripted.engine.error!.kind, EngineErrorCode.engineStartFailed);
      expect(scripted.engine.error!.message, contains('did not answer'));
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
      await expectLater(
        app().source.openDisk(app().source.disks![1]),
        throwsA(engineError(EngineErrorCode.accessDenied)),
      );
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
      // The engine keeps no session the app no longer shows (state N).
      expect(fake.commands.sublist(fake.commands.length - 2), ['close_source', 'close_session']);
      expect(fake.engine.catalog, isNull);
      await expectLater(app().api.catalogInfo(), throwsA(engineError(EngineErrorCode.noCatalog)));
      await expectLater(
        app().api.sessionSave(path: r'C:\s\after-close.hdsc'),
        throwsA(engineError(EngineErrorCode.noSession)),
      );
      expect(app().source.sessionPath, isNull);
    });

    test('close after a session was loaded without its source drops that session too', () async {
      await app().source.loadSession(r'C:\sessions\detached.hdsc');
      expect(app().source.current!.attached, isFalse);
      expect(app().catalog.hasCatalog, isTrue);
      await app().source.close();
      expect(app().source.current, isNull);
      expect(app().catalog.hasCatalog, isFalse);
      await expectLater(app().api.catalogInfo(), throwsA(engineError(EngineErrorCode.noCatalog)));
    });

    test('app_on_source is the app\'s own warning: only for the disk its program runs from', () async {
      // The system disk holds C:, where the program is.
      await app().source.openDisk(app().source.disks![0]);
      expect(app().source.current!.warnings.map((w) => w.kind), contains(WarningCode.appOnSource));
      expect(
        fake.engine.requests.any(
          (r) =>
              r.command == 'check_destination' &&
              r.args['purpose'] == 'app_data' &&
              r.args['path'] == fake.platform.programDirectory,
        ),
        isTrue,
      );
      final warning = app().source.current!.warnings.firstWhere((w) => w.kind == WarningCode.appOnSource);
      expect(warning.message, contains(fake.platform.programDirectory!));
      // Another disk, and an image file, do not carry it.
      await app().source.openDisk(app().source.disks![1]);
      expect(app().source.current!.warnings.map((w) => w.kind), isNot(contains(WarningCode.appOnSource)));
      await app().source.openImage(r'C:\images\disk.img');
      expect(app().source.current!.warnings.map((w) => w.kind), isNot(contains(WarningCode.appOnSource)));
    });

    test('app_on_source also when the check cannot tell where the program is', () async {
      // `unknown_disk` (U:) and `not_writable` (R:, which the guard can give
      // before it looks at the disk) do not rule the disk out; `ok` does.
      for (final (dir, warned) in [(r'U:\HDSavior', true), (r'R:\HDSavior', true), (r'D:\HDSavior', false)]) {
        await fake.dispose();
        fake = FakeApp(programDirectory: dir);
        await fake.start();
        await app().source.openDisk(app().source.disks![1]);
        final own = app().source.current!.warnings.where((w) => w.kind == WarningCode.appOnSource).toList();
        expect(own, hasLength(warned ? 1 : 0), reason: dir);
        if (warned) {
          expect(own.single.message, startsWith('HDSavior runs from $dir. Whether that folder is on this disk'));
          expect(own.single.message, endsWith('.'));
          expect(own.single.message, isNot(endsWith('..')));
        }
      }
      // A failed check leaves it open too.
      await fake.dispose();
      fake = FakeApp(programDirectory: r'D:\HDSavior');
      await fake.start();
      fake.engine.failNext('check_destination', 'internal', message: 'no answer');
      await app().source.openDisk(app().source.disks![1]);
      expect(app().source.current!.warnings.map((w) => w.kind), contains(WarningCode.appOnSource));
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
      // (Both checks of an open fail: the program folder's, then this one.)
      fake.engine.failNext('check_destination', 'internal', times: 2);
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

    test('asked again whenever a catalog arrives, a geometry is set or a file system changes', () async {
      // (Not counting the check of the program's folder when a disk opens.)
      int appDataChecks() => fake.engine.requests
          .where(
            (r) =>
                r.command == 'check_destination' &&
                r.args['purpose'] == 'app_data' &&
                r.args['path'] != fake.platform.programDirectory,
          )
          .length;
      await fake.openDisk();
      expect(appDataChecks(), 1);
      await fake.run(() => app().scan.start(range: app().source.current!.partitions.single.range));
      expect(appDataChecks(), 1, reason: 'there was no catalog to lose');
      await fake.finishJob();
      expect(appDataChecks(), 2, reason: 'a catalog arrived');
      expect(app().source.canWriteAppData, isTrue);
      await app().catalog.setGeometry(instance: 0, clusterSize: 8192, volumeOffset: 1048576);
      expect(app().source.appDataCheck, isNull, reason: 'unknown while it is asked again');
      expect(app().source.canWriteAppData, isFalse);
      await pumpEventQueue();
      expect(appDataChecks(), 3);
      expect(app().source.canWriteAppData, isTrue);
      await app().catalog.setInstanceKind(instance: 1, kind: InstanceOverride.other);
      await pumpEventQueue();
      expect(appDataChecks(), 4);
      await app().catalog.refresh();
      await pumpEventQueue();
      expect(appDataChecks(), 4, reason: 'nothing the guard depends on changed');
      await fake.run(() => app().scan.start(range: app().source.current!.partitions.single.range));
      expect(appDataChecks(), 5, reason: 'the catalog went away when the new scan was accepted');
      expect(app().source.canWriteAppData, isTrue);
    });

    test('the verdict follows the catalog: a directory that turns out to be inside the lost volume', () async {
      final inLostVolume = FakeApp(sessionDirectory: r'L:\HDSavior\Sessions');
      addTearDown(inLostVolume.dispose);
      await inLostVolume.start();
      await inLostVolume.openDisk();
      final source = inLostVolume.app.source;
      expect(
        source.appDataCheck!.verdict,
        DestinationVerdict.sameDisk,
        reason: 'nobody knows yet how far the lost volume reaches',
      );
      expect(source.canWriteAppData, isFalse, reason: 'only an "ok" lets the app write there');
      await inLostVolume.scan();
      expect(source.appDataCheck!.verdict, DestinationVerdict.overlapsSource);
      expect(source.appDataCheck!.reason, ProtectionReason.lostVolume);
      expect(await source.ensureSessionDirectory(), isFalse);
      expect(inLostVolume.platform.createdDirectories, isEmpty);
    });

    test('a verdict that a newer question has overtaken never counts, not even for a moment', () async {
      // Engine answers arrive in order, but the client may already have asked
      // again (a catalog arrived) when an older answer comes in. An old "ok"
      // must not let the app create its directory until the new answer is in.
      final examples = ProtocolExamples.load();
      final transport = ScriptedTransport();
      final scripted = AppController(transport: transport, platform: InertPlatformServices());
      addTearDown(scripted.dispose);
      unawaited(scripted.start());
      await pumpEventQueue();
      transport.connection.respond(transport.connection.lastId, examples.object('hello.result'));
      await pumpEventQueue();
      transport.connection.respond(transport.connection.lastId, {'disks': <Object?>[]});
      await pumpEventQueue();

      final opening = scripted.source.openImage(r'D:\disk.img');
      await pumpEventQueue();
      transport.connection.respond(transport.connection.lastId, examples.object('open_source.result'));
      await pumpEventQueue();
      // A disk: the app asks whether its own program is on it (app_on_source).
      final program = transport.connection.lastRequest;
      expect(program['cmd'], 'check_destination');
      expect(program['args'], {'path': InertPlatformServices().programDirectory, 'purpose': 'app_data'});
      transport.connection.respond(program['id']! as int, examples.object('check_destination.result'));
      await opening;
      await pumpEventQueue();
      final first = transport.connection.lastRequest;
      expect(first['cmd'], 'check_destination');
      expect((first['args']! as Map)['purpose'], 'app_data');

      // A catalog arrives before the first answer: the guard may answer differently now.
      scripted.catalog.adopt(CatalogInfo.fromJson(JsonObject.root(examples.object('CatalogInfo'), 'c')));
      final ok = examples.object('check_destination.result');
      transport.connection.respond(first['id']! as int, ok);
      await pumpEventQueue();
      expect(scripted.source.appDataCheck, isNull, reason: 'the answer to the older question');
      expect(scripted.source.canWriteAppData, isFalse);
      expect(await scripted.source.ensureSessionDirectory(), isFalse);

      final second = transport.connection.lastRequest;
      expect(second['cmd'], 'check_destination');
      expect(second['id'], isNot(first['id']));
      transport.connection.respond(second['id']! as int, {
        ...ok,
        'verdict': 'overlaps_source',
        'reason': 'lost_volume',
        'message': 'inside the lost volume',
      });
      await pumpEventQueue();
      expect(scripted.source.appDataCheck!.reason, ProtectionReason.lostVolume);
      expect(scripted.source.canWriteAppData, isFalse);
    });

    test('suggestSessionPath names model, serial, range start and local time', () async {
      expect(app().source.suggestSessionPath(), isNull);
      await fake.openDisk();
      expect(
        app().source.suggestSessionPath(now: DateTime(2026, 11, 24, 3, 15, 0)),
        '${fake.platform.defaultSessionDirectory}\\WDC WD40EZRZ-00GXCB0 WD-WCC7K0000000 at 0 2026-11-24 031500.hdsc',
      );
      final partition = app().source.current!.partitions.single;
      expect(
        app().source.suggestSessionPath(range: partition.range, now: DateTime(2026, 11, 24, 3, 15, 0)),
        endsWith('\\WDC WD40EZRZ-00GXCB0 WD-WCC7K0000000 at ${partition.offset} 2026-11-24 031500.hdsc'),
      );
      await app().source.openImage(r'D:\images\my: "old" <disk>?.img');
      final path = app().source.suggestSessionPath(now: DateTime(2026, 1, 2, 3, 4, 5))!;
      final name = path.substring(fake.platform.defaultSessionDirectory.length + 1);
      expect(name, 'Image my old disk .img at 0 2026-01-02 030405.hdsc');
      expect(name, isNot(matches(r'[<>:"/\\|?*]')));
    });

    test('prepareSessionFile checks with purpose session and creates the directory only on ok', () async {
      await fake.openDisk();
      final plan = await app().source.prepareSessionFile(now: DateTime(2026, 11, 24, 3, 15, 0));
      expect(plan.check.isOk, isTrue);
      expect(plan.fileName, 'WDC WD40EZRZ-00GXCB0 WD-WCC7K0000000 at 0 2026-11-24 031500.hdsc');
      expect(fake.lastArgs('check_destination'), {'path': plan.path, 'purpose': 'session', 'start': 0});
      expect(fake.platform.createdDirectories, [fake.platform.defaultSessionDirectory]);
    });

    test('prepareSessionFile joins with the platform separator (not always a backslash)', () async {
      await fake.dispose();
      const directory = '/home/rescuer/.local/share/hdsavior/sessions';
      fake = FakeApp(sessionDirectory: directory, pathSeparator: '/');
      await fake.start();
      await fake.openDisk();
      final plan = await app().source.prepareSessionFile(now: DateTime(2026, 11, 24, 3, 15, 0));
      expect(plan.path, '$directory/WDC WD40EZRZ-00GXCB0 WD-WCC7K0000000 at 0 2026-11-24 031500.hdsc');
      expect(plan.path, isNot(contains(r'\')));
      expect(plan.fileName, 'WDC WD40EZRZ-00GXCB0 WD-WCC7K0000000 at 0 2026-11-24 031500.hdsc');
      expect(fake.lastArgs('check_destination'), {'path': plan.path, 'purpose': 'session', 'start': 0});
    });

    test('prepareSessionFile protects the new scan range, not only the current session range', () async {
      // The system disk: its first partition was scanned, with the session elsewhere.
      await app().source.openDisk(app().source.disks![0]);
      final efi = app().source.current!.partitions.first;
      await fake.run(() => app().scan.start(range: efi.range, sessionPath: r'D:\sessions\efi.hdsc'));
      await fake.finishJob();
      expect(app().catalog.hasCatalog, isTrue);
      fake.platform.createdDirectories.clear();

      // Now the whole disk: the usual place on C: lies inside the new range.
      final plan = await app().source.prepareSessionFile();
      final args = fake.lastArgs('check_destination');
      expect(args['start'], 0);
      expect(args.containsKey('end'), isFalse, reason: 'the whole source');
      expect(plan.check.verdict, DestinationVerdict.overlapsSource);
      expect(fake.platform.createdDirectories, isEmpty, reason: 'nothing is created inside the new range');

      // A partial scan names its range.
      final partition = app().source.current!.partitions.last;
      await app().source.prepareSessionFile(range: partition.range);
      expect(fake.lastArgs('check_destination')['start'], partition.offset);
      expect(fake.lastArgs('check_destination')['end'], partition.end);
    });

    test('prepareSessionFile creates nothing when the guard refuses', () async {
      await fake.dispose();
      fake = FakeApp(sessionDirectory: r'E:\HDSavior\Sessions'); // on the disk being rescued
      await fake.start();
      await fake.openDisk();
      final plan = await app().source.prepareSessionFile();
      expect(plan.check.verdict, DestinationVerdict.overlapsSource);
      expect(fake.platform.createdDirectories, isEmpty);
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
        throwsA(
          engineError(EngineErrorCode.destinationRefused)
              .having((e) => e.destinationCheck!.verdict, 'verdict', DestinationVerdict.overlapsSource),
        ),
      );
      expect(app().source.sessionPath, r'D:\backup\copy.hdsc');
    });

    test('a successful save clears the warning about the session file on screen', () async {
      await fake.openDisk();
      final partition = app().source.current!.partitions.single;
      // Inside the lost volume, which nobody knew before the scan.
      await fake.run(
        () => app().scan.start(range: partition.range, sessionPath: r'L:\scans\s.hdsc', allowSameDisk: true),
      );
      await fake.finishJob();
      List<String> codes() => [for (final w in app().catalog.info!.warnings) w.code];
      expect(codes(), contains('session_file_at_risk'));
      final epoch = app().catalog.info!.epoch;
      await app().source.saveSession(r'D:\backup\copy.hdsc');
      expect(codes(), isNot(contains('session_file_at_risk')));
      expect(app().catalog.info!.epoch, epoch, reason: 'the same catalog, its warnings up to date');
    });
  });
}

extension<T> on T {
  R let<R>(R Function(T value) f) => f(this);
}
